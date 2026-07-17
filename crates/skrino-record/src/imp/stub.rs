//! Non-Windows stub: keeps the crate compiling everywhere. Recording relies on
//! Windows.Graphics.Capture, so every entry point reports `Unsupported`.

use std::path::PathBuf;
use std::time::Duration;

use crate::{RecordError, RecordOptions};

const MSG: &str = "запись экрана поддерживается только в Windows";

pub(crate) fn is_supported() -> bool {
    false
}

pub(crate) struct RecorderImpl;

impl RecorderImpl {
    pub(crate) fn start(_opts: RecordOptions) -> Result<Self, RecordError> {
        Err(RecordError::Unsupported(MSG.into()))
    }

    pub(crate) fn pause(&self) {}

    pub(crate) fn resume(&self) {}

    pub(crate) fn is_paused(&self) -> bool {
        false
    }

    pub(crate) fn elapsed(&self) -> Duration {
        Duration::ZERO
    }

    pub(crate) fn take_error(&self) -> Option<String> {
        None
    }

    pub(crate) fn stop(self) -> Result<PathBuf, RecordError> {
        Err(RecordError::Unsupported(MSG.into()))
    }

    pub(crate) fn cancel(self) {}
}
