//! Screen video recording: Windows.Graphics.Capture frames fed into a
//! Media Foundation hardware H.264 encoder, written straight to an .mp4.
//! No external binaries (no ffmpeg) — everything ships inside skrino.exe.
//!
//! The public API is intentionally tiny and synchronous from the caller's
//! point of view: `Recorder::start` spins up the capture + encode machinery
//! on background threads and returns immediately; the UI polls `elapsed()`
//! for the timer and calls `pause`/`resume`/`stop`/`cancel`.

use std::path::PathBuf;
use std::time::Duration;

/// A rectangle in *physical* pixels, in virtual-screen coordinates (the same
/// space the capture crate and the selection overlay use). May span negative
/// coordinates when a monitor sits left/above the primary one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegionPx {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Everything needed to start a recording.
#[derive(Debug, Clone)]
pub struct RecordOptions {
    /// Area to record, in virtual-screen physical pixels. `None` records the
    /// full primary monitor. A region is clamped to the single monitor that
    /// contains its center (WGC captures one monitor at a time); width/height
    /// are rounded down to even values (H.264 requirement).
    pub region: Option<RegionPx>,
    /// Target frame rate (frames per second), e.g. 30.
    pub fps: u32,
    /// Whether the mouse cursor is drawn into the recording.
    pub capture_cursor: bool,
    /// Destination .mp4 path. Parent directory must exist.
    pub output: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    /// Windows.Graphics.Capture is unavailable (needs Windows 10 1903+).
    #[error("запись экрана не поддерживается этой версией Windows: {0}")]
    Unsupported(String),
    #[error("ошибка захвата экрана: {0}")]
    Capture(String),
    #[error("ошибка видеокодировщика: {0}")]
    Encoder(String),
    #[error("ошибка записи файла: {0}")]
    Io(#[from] std::io::Error),
}

/// Quick availability probe so the UI can fail fast with a friendly message
/// instead of erroring mid-flow. Cheap to call.
pub fn is_supported() -> bool {
    imp::is_supported()
}

/// A recording in progress. Dropping without `stop`/`cancel` behaves like
/// `cancel` (best effort: capture torn down, partial file removed).
pub struct Recorder {
    inner: imp::RecorderImpl,
}

impl Recorder {
    /// Start capturing and encoding. Returns as soon as the pipeline is up.
    pub fn start(opts: RecordOptions) -> Result<Recorder, RecordError> {
        Ok(Recorder {
            inner: imp::RecorderImpl::start(opts)?,
        })
    }

    /// Freeze the recording: frames are dropped and the pause span is excised
    /// from the output timeline (no frozen-frame stretch in the video).
    pub fn pause(&self) {
        self.inner.pause();
    }

    /// Resume after `pause`. No-op when not paused.
    pub fn resume(&self) {
        self.inner.resume();
    }

    pub fn is_paused(&self) -> bool {
        self.inner.is_paused()
    }

    /// Wall-clock recording time excluding paused spans — what the UI timer
    /// shows and (approximately) the duration of the final video.
    pub fn elapsed(&self) -> Duration {
        self.inner.elapsed()
    }

    /// If the pipeline died in the background (device lost, encoder error),
    /// returns the error message so the UI can surface it without waiting
    /// for `stop`. `None` while healthy.
    pub fn take_error(&self) -> Option<String> {
        self.inner.take_error()
    }

    /// Finish the recording: flush the encoder, finalize the mp4, return its
    /// path. Blocks briefly (encoder drain, typically well under a second).
    pub fn stop(self) -> Result<PathBuf, RecordError> {
        self.inner.stop()
    }

    /// Abort: tear down capture and delete the partial output file.
    pub fn cancel(self) {
        self.inner.cancel();
    }
}

mod imp;
