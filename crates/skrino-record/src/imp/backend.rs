//! Windows recording pipeline: Windows.Graphics.Capture -> region crop -> Media
//! Foundation H.264 encoder, all in-process via the `windows-capture` crate.
//!
//! Threading: three players share `Arc<Shared>`.
//! - The WGC capture thread (`start_free_threaded`) delivers frames; the
//!   handler crops them and writes through the shared encoder.
//! - A keepalive watchdog thread re-sends the last written frame when the
//!   screen goes static: WGC only fires on change, so without duplication an
//!   idle recording's mp4 would end at the last screen change instead of at
//!   `elapsed()` (mvhd duration collapse).
//! - The UI thread drives pause/resume/stop/cancel; `stop()` emits one final
//!   frame stamped with the closing active-clock time and finalizes the mp4.
//!
//! The encoder lives inside `Shared` behind one mutex together with the pacing
//! state and the last-frame copy, so real frames, keepalive duplicates, and the
//! final frame are serialized and their timestamps stay strictly monotonic.
//!
//! Pause timeline excision: WGC stamps each frame with its own capture time,
//! which we ignore. Every written frame gets a timestamp from
//! [`RecordClock::active_hns`] (wall time minus paused spans, 100ns ticks);
//! frames arriving while paused are dropped, and the frozen clock makes the
//! monotonicity check suppress keepalives too. The result has no frozen
//! stretch and no jump.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, HMONITOR, MONITORINFO};
use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::encoder::{
    AudioSettingsBuilder, ContainerSettingsBuilder, VideoEncoder, VideoSettingsBuilder,
    VideoSettingsSubType,
};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};

use super::clock::RecordClock;
use super::flip;
use super::geometry::{self, CropPlan, Rect};
use super::pacing::FramePacer;
use crate::{RecordError, RecordOptions};

/// Cheap WGC availability probe. `GraphicsCaptureSession::IsSupported` is a
/// static metadata check (Windows 10 1903+); on any failure we assume no.
pub(crate) fn is_supported() -> bool {
    windows::Graphics::Capture::GraphicsCaptureSession::IsSupported().unwrap_or(false)
}

/// Encoder bitrate. 12 Mbps is plenty for screen content at 1080p/30 and keeps
/// files reasonable; the app does not expose this yet.
const BITRATE_BPS: u32 = 12_000_000;

/// `MONITORINFO::dwFlags` bit marking the primary monitor. Win32 defines it as
/// 1; the `windows` crate does not re-export the constant.
const MONITORINFOF_PRIMARY: u32 = 1;

/// Encoder + write bookkeeping, guarded by one mutex so real, keepalive, and
/// final frames serialize and share the monotonic pacing state.
struct EncodeState {
    /// `None` after stop/cancel has taken it for finalize/teardown.
    encoder: Option<VideoEncoder>,
    pacer: FramePacer,
    /// Copy of the last written (cropped, depadded BGRA) frame, re-sent by the
    /// keepalive and the final stop() frame. Empty until the first real frame.
    last_frame: Vec<u8>,
}

/// State shared between the UI thread, the capture thread, and the watchdog.
struct Shared {
    clock: Mutex<RecordClock>,
    encode: Mutex<EncodeState>,
    /// Last background-pipeline error, polled by `take_error`.
    error: Mutex<Option<String>>,
    /// Fast path for the frame callback to drop paused frames without locking.
    paused: AtomicBool,
    /// Set by `cancel`/drop so no further frames are written.
    cancelled: AtomicBool,
    /// Tells the watchdog thread to exit.
    shutdown: AtomicBool,
    /// Fixed crop rectangle, in monitor-local pixels.
    crop: CropPlan,
}

impl Shared {
    fn store_error(&self, msg: String) {
        let mut slot = self.error.lock().unwrap();
        // Keep the first error; later ones are usually cascade noise.
        if slot.is_none() {
            *slot = Some(msg);
        }
    }
}

/// Values handed to the capture thread to build its handler.
struct HandlerFlags {
    shared: Arc<Shared>,
}

#[derive(Debug, thiserror::Error)]
enum HandlerError {
    #[error("{0}")]
    Encoder(String),
}

/// Per-capture-thread state: a reusable de-pad scratch buffer plus the shared
/// pipeline state.
struct Handler {
    shared: Arc<Shared>,
    scratch: Vec<u8>,
}

impl GraphicsCaptureApiHandler for Handler {
    type Flags = HandlerFlags;
    type Error = HandlerError;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            shared: ctx.flags.shared,
            scratch: Vec::new(),
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        // Once cancelled or paused, write nothing.
        if self.shared.cancelled.load(Ordering::Acquire)
            || self.shared.paused.load(Ordering::Acquire)
        {
            return Ok(());
        }

        let hns = self.shared.clock.lock().unwrap().active_hns(Instant::now());

        // Cheap pacing pre-check before paying for the crop; re-checked under
        // the same lock at write time, so a watchdog race cannot break
        // monotonicity.
        if !self.shared.encode.lock().unwrap().pacer.accept_real(hns) {
            return Ok(());
        }

        let crop = self.shared.crop;
        let mut fb = match frame.buffer_crop(
            crop.local_x,
            crop.local_y,
            crop.local_x + crop.width,
            crop.local_y + crop.height,
        ) {
            Ok(fb) => fb,
            Err(e) => {
                // Transient size mismatch (e.g. a resolution change mid-record):
                // skip this frame rather than tear the whole pipeline down.
                log::warn!("skrino-record: crop failed, dropping frame: {e}");
                return Ok(());
            }
        };

        // Depad and reverse row order in one pass: WGC rows are top-down, but
        // send_frame_buffer's uncompressed BGRA sample follows the Media
        // Foundation DIB convention (bottom-up); feeding top-down rows flips
        // the video vertically. See `flip::pack_rows_bottom_up`.
        let row_pitch = fb.row_pitch() as usize;
        let src = fb.as_raw_buffer();
        if !flip::pack_rows_bottom_up(
            src,
            crop.width as usize * 4,
            row_pitch,
            crop.height as usize,
            &mut self.scratch,
        ) {
            log::warn!("skrino-record: unexpected frame layout, dropping frame");
            return Ok(());
        }
        let bytes = self.scratch.as_slice();

        let mut state = self.shared.encode.lock().unwrap();
        if !state.pacer.accept_real(hns) {
            return Ok(());
        }
        let EncodeState { encoder, pacer, last_frame } = &mut *state;
        if let Some(encoder) = encoder.as_mut() {
            if let Err(e) = encoder.send_frame_buffer(bytes, hns) {
                let msg = format!("сбой кодирования кадра: {e}");
                self.shared.store_error(msg.clone());
                return Err(HandlerError::Encoder(msg));
            }
            pacer.record_write(hns);
            last_frame.clear();
            last_frame.extend_from_slice(bytes);
        }
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        // Finalization happens in stop()/cancel() on the caller thread (the
        // encoder lives in Shared, not here); nothing to do when WGC closes.
        Ok(())
    }
}

/// Write the stored last frame stamped `hns` if `due` approves it. Shared by
/// the keepalive watchdog and the final stop() frame. Returns `false` when an
/// encoder error occurred (already stored for `take_error`).
fn write_duplicate_frame(
    shared: &Shared,
    hns: i64,
    due: impl Fn(&FramePacer, i64) -> bool,
) -> bool {
    let mut state = shared.encode.lock().unwrap();
    let EncodeState { encoder, pacer, last_frame } = &mut *state;
    if last_frame.is_empty() || !due(pacer, hns) {
        return true;
    }
    let Some(encoder) = encoder.as_mut() else {
        return true;
    };
    if let Err(e) = encoder.send_frame_buffer(last_frame, hns) {
        shared.store_error(format!("сбой кодирования кадра: {e}"));
        return false;
    }
    pacer.record_write(hns);
    true
}

/// Keepalive watchdog: on a static screen WGC stops delivering frames, so this
/// thread duplicates the last frame (with a fresh active-clock timestamp) once
/// the silence exceeds the pacer's keepalive threshold. Exits on `shutdown`.
fn watchdog_loop(shared: &Shared) {
    // Poll at half the keepalive threshold (bounded), enough resolution to
    // keep the duplicated stream near the nominal cadence without spinning.
    let keepalive_hns = shared.encode.lock().unwrap().pacer.keepalive_after_hns();
    let poll = Duration::from_nanos((keepalive_hns as u64 * 100) / 2)
        .clamp(Duration::from_millis(5), Duration::from_millis(250));

    while !shared.shutdown.load(Ordering::Acquire) {
        std::thread::sleep(poll);
        if shared.shutdown.load(Ordering::Acquire)
            || shared.cancelled.load(Ordering::Acquire)
            || shared.paused.load(Ordering::Acquire)
        {
            continue;
        }
        let hns = shared.clock.lock().unwrap().active_hns(Instant::now());
        if !write_duplicate_frame(shared, hns, FramePacer::keepalive_due) {
            // Encoder is broken; the error is stored, stop trying.
            return;
        }
    }
}

pub(crate) struct RecorderImpl {
    control: Option<CaptureControl<Handler, HandlerError>>,
    watchdog: Option<JoinHandle<()>>,
    shared: Arc<Shared>,
    output: PathBuf,
}

impl RecorderImpl {
    pub(crate) fn start(opts: RecordOptions) -> Result<Self, RecordError> {
        if !is_supported() {
            return Err(RecordError::Unsupported(
                "Windows.Graphics.Capture недоступен (нужна Windows 10 1903+)".into(),
            ));
        }
        let fps = opts.fps.clamp(1, 240);

        let monitors = Monitor::enumerate()
            .map_err(|e| RecordError::Capture(format!("не удалось перечислить мониторы: {e}")))?;
        if monitors.is_empty() {
            return Err(RecordError::Capture("мониторы не найдены".into()));
        }

        // Virtual-screen rectangle + primary flag for each monitor.
        let mut rects = Vec::with_capacity(monitors.len());
        let mut primary_index = 0usize;
        for (i, monitor) in monitors.iter().enumerate() {
            let (rect, is_primary) = monitor_rect(monitor)?;
            if is_primary {
                primary_index = i;
            }
            rects.push(rect);
        }

        let region = opts.region.map(|r| Rect {
            x: r.x,
            y: r.y,
            width: r.width,
            height: r.height,
        });
        let plan = geometry::plan_region(&rects, region, primary_index)
            .ok_or_else(|| RecordError::Capture("не удалось определить область записи".into()))?;

        // Consume the vec to move the chosen monitor into the capture settings
        // (Monitor is not Copy), no clone needed.
        let target = monitors
            .into_iter()
            .nth(plan.monitor_index)
            .ok_or_else(|| RecordError::Capture("выбранный монитор недоступен".into()))?;

        // Create the encoder up front so a bad output path or missing codec
        // fails fast here instead of asynchronously on the capture thread.
        let video = VideoSettingsBuilder::new(plan.width, plan.height)
            .frame_rate(fps)
            .bitrate(BITRATE_BPS)
            .sub_type(VideoSettingsSubType::H264);
        let audio = AudioSettingsBuilder::new().disabled(true);
        let container = ContainerSettingsBuilder::new();
        let encoder = VideoEncoder::new(video, audio, container, &opts.output)
            .map_err(|e| RecordError::Encoder(format!("не удалось создать кодировщик: {e}")))?;

        let shared = Arc::new(Shared {
            clock: Mutex::new(RecordClock::new(Instant::now())),
            encode: Mutex::new(EncodeState {
                encoder: Some(encoder),
                pacer: FramePacer::new(fps),
                last_frame: Vec::new(),
            }),
            error: Mutex::new(None),
            paused: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
            crop: plan,
        });

        let flags = HandlerFlags {
            shared: Arc::clone(&shared),
        };

        let cursor = if opts.capture_cursor {
            CursorCaptureSettings::WithCursor
        } else {
            CursorCaptureSettings::WithoutCursor
        };

        let settings = Settings::new(
            target,
            cursor,
            // The app draws its own frame; suppress the system yellow border.
            DrawBorderSettings::WithoutBorder,
            SecondaryWindowSettings::Default,
            MinimumUpdateIntervalSettings::Default,
            DirtyRegionSettings::Default,
            // BGRA8 matches the encoder input and `send_frame_buffer`'s
            // width*height*4 expectation.
            ColorFormat::Bgra8,
            flags,
        );

        let control = Handler::start_free_threaded(settings)
            .map_err(|e| RecordError::Capture(format!("не удалось запустить захват: {e}")))?;

        let watchdog_shared = Arc::clone(&shared);
        let watchdog = std::thread::Builder::new()
            .name("skrino-record-keepalive".into())
            .spawn(move || watchdog_loop(&watchdog_shared))
            .map_err(|e| {
                RecordError::Capture(format!("не удалось запустить поток записи: {e}"))
            })?;

        Ok(Self {
            control: Some(control),
            watchdog: Some(watchdog),
            shared,
            output: opts.output,
        })
    }

    pub(crate) fn pause(&self) {
        self.shared.clock.lock().unwrap().pause(Instant::now());
        self.shared.paused.store(true, Ordering::Release);
    }

    pub(crate) fn resume(&self) {
        self.shared.clock.lock().unwrap().resume(Instant::now());
        self.shared.paused.store(false, Ordering::Release);
    }

    pub(crate) fn is_paused(&self) -> bool {
        self.shared.clock.lock().unwrap().is_paused()
    }

    pub(crate) fn elapsed(&self) -> Duration {
        self.shared.clock.lock().unwrap().active(Instant::now())
    }

    pub(crate) fn take_error(&self) -> Option<String> {
        self.shared.error.lock().unwrap().take()
    }

    /// Stop the watchdog and the WGC capture; after this no thread writes to
    /// the encoder anymore.
    fn shutdown_pipeline(&mut self) {
        self.shared.shutdown.store(true, Ordering::Release);
        if let Some(control) = self.control.take()
            && let Err(e) = control.stop()
        {
            self.shared
                .store_error(format!("не удалось остановить захват: {e}"));
        }
        if let Some(watchdog) = self.watchdog.take() {
            let _ = watchdog.join();
        }
    }

    pub(crate) fn stop(mut self) -> Result<PathBuf, RecordError> {
        // Close the timeline at the exact elapsed() the UI last showed.
        let final_hns = self.shared.clock.lock().unwrap().active_hns(Instant::now());

        self.shutdown_pipeline();

        // The video must end at elapsed(): stamp the last frame once more at
        // the closing time (mvhd duration tracks the last sample, and on a
        // static screen no real frame arrived near the end).
        write_duplicate_frame(&self.shared, final_hns, FramePacer::accept_final);

        // Finalize the mp4.
        let encoder = self.shared.encode.lock().unwrap().encoder.take();
        if let Some(encoder) = encoder {
            encoder.finish().map_err(|e| {
                RecordError::Encoder(format!("не удалось финализировать файл записи: {e}"))
            })?;
        }

        if let Some(msg) = self.shared.error.lock().unwrap().take() {
            return Err(RecordError::Encoder(msg));
        }
        Ok(self.output.clone())
    }

    pub(crate) fn cancel(mut self) {
        self.teardown_and_discard();
    }

    /// Best-effort teardown: stop everything, drop the encoder without
    /// finalizing, delete the partial file. Used by cancel() and Drop.
    fn teardown_and_discard(&mut self) {
        self.shared.cancelled.store(true, Ordering::Release);
        self.shutdown_pipeline();
        drop(self.shared.encode.lock().unwrap().encoder.take());
        let _ = std::fs::remove_file(&self.output);
    }
}

impl Drop for RecorderImpl {
    fn drop(&mut self) {
        // Dropped without stop/cancel: behave like cancel. After an explicit
        // stop/cancel, control and watchdog are already None (and for stop the
        // file must survive), so teardown is skipped.
        if self.control.is_some() || self.watchdog.is_some() {
            self.teardown_and_discard();
        }
    }
}

/// Read a monitor's virtual-screen rectangle (physical pixels) and primary flag
/// via `GetMonitorInfoW` on its raw `HMONITOR`.
fn monitor_rect(monitor: &Monitor) -> Result<(Rect, bool), RecordError> {
    let hmonitor = HMONITOR(monitor.as_raw_hmonitor());
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: `hmonitor` comes straight from `windows-capture`'s live monitor
    // enumeration and `info` is a correctly sized, owned MONITORINFO.
    let ok = unsafe { GetMonitorInfoW(hmonitor, &mut info) };
    if !ok.as_bool() {
        return Err(RecordError::Capture(
            "не удалось получить геометрию монитора".into(),
        ));
    }
    let r = info.rcMonitor;
    let rect = Rect {
        x: r.left,
        y: r.top,
        width: (r.right - r.left).max(0) as u32,
        height: (r.bottom - r.top).max(0) as u32,
    };
    let is_primary = info.dwFlags & MONITORINFOF_PRIMARY != 0;
    Ok((rect, is_primary))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_supported_is_true_on_this_windows() {
        // The dev/CI box is Windows 11, so WGC must be available.
        assert!(is_supported());
    }

    /// Real end-to-end smoke test: records ~2s of the primary monitor to a temp
    /// mp4 and checks the file exists and is non-trivial. Ignored by default
    /// (needs a live desktop + hardware encoder); run with:
    ///   cargo test -p skrino-record -- --ignored
    #[test]
    #[ignore = "records the real screen; run explicitly with --ignored"]
    fn records_primary_monitor_to_mp4() {
        let dir = std::env::temp_dir();
        let output = dir.join(format!("skrino-record-test-{}.mp4", std::process::id()));
        let _ = std::fs::remove_file(&output);

        let opts = RecordOptions {
            region: None,
            fps: 30,
            capture_cursor: true,
            output: output.clone(),
        };

        let recorder = RecorderImpl::start(opts).expect("recording should start");

        // Record ~1s, pause ~0.5s (must not appear in the file), record ~1s.
        std::thread::sleep(Duration::from_millis(1000));
        recorder.pause();
        assert!(recorder.is_paused());
        std::thread::sleep(Duration::from_millis(500));
        recorder.resume();
        assert!(!recorder.is_paused());
        std::thread::sleep(Duration::from_millis(1000));

        let elapsed = recorder.elapsed();
        assert!(
            elapsed >= Duration::from_millis(1500) && elapsed < Duration::from_millis(2600),
            "elapsed should exclude the paused span, got {elapsed:?}"
        );

        let path = recorder.stop().expect("stop should finalize the mp4");
        assert_eq!(path, output);

        let meta = std::fs::metadata(&output).expect("output file should exist");
        assert!(
            meta.len() > 10_240,
            "mp4 should be larger than 10 KiB, got {} bytes",
            meta.len()
        );

        eprintln!(
            "recorded {} bytes to {} (active elapsed {:?})",
            meta.len(),
            output.display(),
            elapsed
        );
        let _ = std::fs::remove_file(&output);
    }
}
