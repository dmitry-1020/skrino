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

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
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

use super::audio_dsp::{DST_SAMPLE_RATE, StereoResampler};
use super::audio_wasapi::{AudioCapturer, AudioSel};
use super::clock::RecordClock;
use super::flip;
use super::geometry::{self, CropPlan, Rect};
use super::pacing::FramePacer;
use crate::{AudioSource, RecordError, RecordOptions};

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

// --- Single-source audio capture ---------------------------------------------
//
// The encoder derives audio time from the cumulative count of sample frames it
// has been handed (`audio_samples_sent * 10^7 / sample_rate`), so the stream
// must be continuous at 48 kHz with no gaps. The audio thread therefore feeds
// the encoder purely by the active clock: it queues resampled real audio and,
// each tick, tops the queue up with silence so cumulative frames sent tracks
// `RecordClock::active` wall time. While paused the active clock is frozen, so
// no frames are due and none are sent, keeping A/V aligned across pauses without
// injecting a silent gap.

/// Encoder audio frame = 2 channels * 2 bytes (i16 stereo at [`DST_SAMPLE_RATE`]).
const AUDIO_BYTES_PER_FRAME: usize = 4;

/// Audio feed cadence. 10 ms (~480 frames at 48 kHz) keeps latency low without
/// spinning; the WASAPI client buffer (200 ms) absorbs any poll jitter.
const AUDIO_TICK: Duration = Duration::from_millis(10);

/// Cap on queued-but-unsent real audio (~0.5 s). A capture device clock running
/// slightly faster than the system clock would otherwise build unbounded
/// latency; excess oldest frames are dropped, trading a sub-perceptible skip for
/// staying locked to the clock-driven target.
const AUDIO_MAX_PENDING_FRAMES: usize = (DST_SAMPLE_RATE as usize) / 2;

/// Number of 48 kHz stereo frames that should have been emitted by active time
/// `active_hns` (100 ns ticks). Saturates at zero for a non-positive clock.
fn audio_target_frames(active_hns: i64) -> u64 {
    if active_hns <= 0 {
        return 0;
    }
    ((active_hns as i128 * i128::from(DST_SAMPLE_RATE)) / 10_000_000i128) as u64
}

/// Feed `need` stereo frames to the encoder: real audio from `pending` first,
/// then silence for any shortfall. Returns `false` if the encoder rejected the
/// buffer (audio then stops; video is unaffected).
fn emit_audio_frames(
    shared: &Shared,
    pending: &mut VecDeque<i16>,
    need: usize,
    scratch: &mut Vec<u8>,
) -> bool {
    if need == 0 {
        return true;
    }
    scratch.clear();
    scratch.reserve(need * AUDIO_BYTES_PER_FRAME);
    let mut remaining = need;
    while remaining > 0 {
        let (Some(l), Some(r)) = (pending.pop_front(), pending.pop_front()) else {
            break;
        };
        scratch.extend_from_slice(&l.to_le_bytes());
        scratch.extend_from_slice(&r.to_le_bytes());
        remaining -= 1;
    }
    // Silence pads the remaining shortfall (leading warm-up, gaps, drained queue).
    scratch.resize(need * AUDIO_BYTES_PER_FRAME, 0);

    let mut state = shared.encode.lock().unwrap();
    if let Some(encoder) = state.encoder.as_mut() {
        // The encoder ignores the timestamp and clocks off the sample count.
        if let Err(e) = encoder.send_audio_buffer(scratch, 0) {
            log::warn!("skrino-record: audio send failed, stopping audio track: {e}");
            return false;
        }
    }
    true
}

/// Audio streaming loop: drain WASAPI, resample to 48 kHz stereo i16, and feed
/// the encoder paced by the active clock. Runs on the dedicated audio thread and
/// exits on `shutdown`/`cancelled`.
fn audio_loop(shared: &Shared, cap: &mut AudioCapturer) {
    let mut resampler = StereoResampler::new(cap.sample_rate(), DST_SAMPLE_RATE);
    let mut capture_buf: Vec<f32> = Vec::new();
    let mut resampled: Vec<i16> = Vec::new();
    let mut pending: VecDeque<i16> = VecDeque::new();
    let mut send_scratch: Vec<u8> = Vec::new();
    let mut frames_emitted: u64 = 0;
    let mut was_paused = false;

    loop {
        std::thread::sleep(AUDIO_TICK);

        if shared.cancelled.load(Ordering::Acquire) {
            return;
        }
        let shutting_down = shared.shutdown.load(Ordering::Acquire);

        // Always drain the device so its buffer never overflows, even if we then
        // discard the data (paused).
        capture_buf.clear();
        if let Err(e) = cap.capture_into(&mut capture_buf) {
            // A transient device read failure must not crash the recording; keep
            // the stream alive by padding silence for this span.
            log::warn!("skrino-record: audio capture failed: {e}");
            capture_buf.clear();
        }

        if shared.paused.load(Ordering::Acquire) {
            // Excise this span: drop captured audio and the queued backlog, and
            // re-prime the resampler so resumed audio does not interpolate across
            // the discarded gap. The active clock is frozen, so no frames are due.
            pending.clear();
            resampler.reset();
            was_paused = true;
            if shutting_down {
                return;
            }
            continue;
        }
        if was_paused {
            // On resume, realign the emit target to the current active time so
            // the pause span is not back-filled with silence.
            frames_emitted = audio_target_frames(shared.clock.lock().unwrap().active_hns(Instant::now()));
            was_paused = false;
        }

        if !capture_buf.is_empty() {
            resampled.clear();
            resampler.process(&capture_buf, &mut resampled);
            pending.extend(resampled.iter().copied());
            // Bound queued latency (interleaved i16: two entries per frame).
            let max_entries = AUDIO_MAX_PENDING_FRAMES * 2;
            while pending.len() > max_entries {
                pending.pop_front();
            }
        }

        let active_hns = shared.clock.lock().unwrap().active_hns(Instant::now());
        let target = audio_target_frames(active_hns);
        if target > frames_emitted {
            let need = (target - frames_emitted) as usize;
            if !emit_audio_frames(shared, &mut pending, need, &mut send_scratch) {
                return; // encoder rejected audio; leave video running
            }
            frames_emitted = target;
        }

        if shutting_down {
            return;
        }
    }
}

/// Map the public audio choice to the capture selector; `None` means no track.
fn audio_selection(source: AudioSource) -> Option<AudioSel> {
    match source {
        AudioSource::None => None,
        AudioSource::System => Some(AudioSel::Loopback),
        AudioSource::Microphone => Some(AudioSel::Microphone),
    }
}

/// Start the audio capture thread and block until WASAPI has initialized, so the
/// caller knows whether to enable the encoder's AAC track. On success the thread
/// waits for `run_rx` to deliver the shared pipeline, then streams audio.
/// On init failure the thread has already exited and `Err` is returned so the
/// recording proceeds video-only.
fn spawn_audio_thread(
    source: AudioSel,
    run_rx: mpsc::Receiver<Arc<Shared>>,
) -> Result<JoinHandle<()>, String> {
    let (init_tx, init_rx) = mpsc::channel::<Result<(), String>>();
    let handle = std::thread::Builder::new()
        .name("skrino-record-audio".into())
        .spawn(move || {
            // Phase A: initialize WASAPI on this thread (owns the COM apartment).
            let mut cap = match AudioCapturer::new(source) {
                Ok(cap) => {
                    let _ = init_tx.send(Ok(()));
                    cap
                }
                Err(e) => {
                    let _ = init_tx.send(Err(e));
                    return;
                }
            };
            // Phase B: wait for the pipeline. A dropped sender means start() gave
            // up after our init; just exit.
            if let Ok(shared) = run_rx.recv() {
                audio_loop(&shared, &mut cap);
            }
        })
        .map_err(|e| format!("не удалось запустить аудиопоток: {e}"))?;

    match init_rx.recv() {
        Ok(Ok(())) => Ok(handle),
        Ok(Err(e)) => {
            let _ = handle.join();
            Err(e)
        }
        Err(_) => Err("аудиопоток завершился до инициализации".into()),
    }
}

pub(crate) struct RecorderImpl {
    control: Option<CaptureControl<Handler, HandlerError>>,
    watchdog: Option<JoinHandle<()>>,
    /// Audio capture thread; `None` when no source was selected or init failed.
    audio: Option<JoinHandle<()>>,
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

        // Bring up audio capture before the encoder: the AAC track must be
        // enabled at encoder-creation time, and we only enable it once WASAPI has
        // actually initialized. On any audio failure we log and record video-only
        // rather than aborting the whole recording. The thread is handed the
        // shared pipeline via `run_tx` once the encoder exists.
        let (run_tx, run_rx) = mpsc::channel::<Arc<Shared>>();
        let mut audio_handle: Option<JoinHandle<()>> = None;
        let audio_enabled = match audio_selection(opts.audio) {
            Some(sel) => match spawn_audio_thread(sel, run_rx) {
                Ok(handle) => {
                    audio_handle = Some(handle);
                    true
                }
                Err(e) => {
                    log::warn!("skrino-record: звук недоступен, запись без звука: {e}");
                    false
                }
            },
            None => false,
        };

        // Create the encoder up front so a bad output path or missing codec
        // fails fast here instead of asynchronously on the capture thread.
        let video = VideoSettingsBuilder::new(plan.width, plan.height)
            .frame_rate(fps)
            .bitrate(BITRATE_BPS)
            .sub_type(VideoSettingsSubType::H264);
        // Defaults (48 kHz / stereo / 16-bit / AAC) match the audio thread's
        // output format; enabled only when a source is actually capturing.
        let audio = AudioSettingsBuilder::new().disabled(!audio_enabled);
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

        // Hand the live pipeline to the waiting audio thread so it starts
        // streaming into the encoder. (On the `?` error paths above, `run_tx` is
        // dropped instead, and the audio thread exits cleanly on the closed
        // channel.)
        if audio_handle.is_some() && run_tx.send(Arc::clone(&shared)).is_err() {
            log::warn!("skrino-record: аудиопоток недоступен, запись без звука");
            audio_handle = None;
        }

        Ok(Self {
            control: Some(control),
            watchdog: Some(watchdog),
            audio: audio_handle,
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
        // Join the audio thread before the encoder is finalized/dropped so its
        // final flush completes and it stops touching the encoder. It observes
        // `shutdown`/`cancelled` and drains within one `AUDIO_TICK`.
        if let Some(audio) = self.audio.take() {
            let _ = audio.join();
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
            audio: AudioSource::None,
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

    /// Real end-to-end audio test: records ~2s of the primary monitor with system
    /// (loopback) audio and asserts the mp4 carries an AAC audio track. Ignored by
    /// default (needs a live desktop, hardware encoder, and a default render
    /// device); run with:
    ///   cargo test -p skrino-record -- --ignored records_with_system_audio
    #[test]
    #[ignore = "records the real screen + system audio; run explicitly with --ignored"]
    fn records_with_system_audio_writes_aac_track() {
        let dir = std::env::temp_dir();
        let output = dir.join(format!("skrino-record-audio-{}.mp4", std::process::id()));
        let _ = std::fs::remove_file(&output);

        let opts = RecordOptions {
            region: None,
            fps: 30,
            capture_cursor: false,
            audio: AudioSource::System,
            output: output.clone(),
        };

        let recorder = RecorderImpl::start(opts).expect("recording should start");
        std::thread::sleep(Duration::from_millis(2000));
        assert!(recorder.take_error().is_none(), "no mid-recording error");
        let path = recorder.stop().expect("stop should finalize the mp4");

        let bytes = std::fs::read(&path).expect("output file should exist");
        assert!(bytes.len() > 10_240, "mp4 too small: {} bytes", bytes.len());

        // The AAC sample-entry box type `mp4a` appears in the audio track's stsd;
        // its presence proves a muxed audio track exists (no full mp4 parser
        // needed). Video-only files never contain it.
        let has_aac = bytes.windows(4).any(|w| w == b"mp4a");
        assert!(has_aac, "expected an AAC (mp4a) audio track in the mp4");

        eprintln!(
            "recorded {} bytes to {} with audio track (mp4a: {has_aac})",
            bytes.len(),
            path.display(),
        );
        let _ = std::fs::remove_file(&output);
    }
}
