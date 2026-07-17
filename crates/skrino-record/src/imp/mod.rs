//! Implementation root for the recording engine.
//!
//! Two backends selected by target:
//! - `backend` (Windows): Windows.Graphics.Capture frames cropped to the
//!   requested region and pushed into a Media Foundation H.264 encoder via the
//!   `windows-capture` crate.
//! - `stub` (everything else): compiles and returns `Unsupported`, keeping the
//!   workspace buildable off-Windows the same way the other crates degrade.
//!
//! The geometry and clock logic is deliberately split into pure modules with no
//! OS dependencies so the tricky parts (multi-monitor region clamping, pause
//! timeline excision) are unit-tested on any platform.

mod clock;
mod geometry;
mod pacing;

#[cfg(windows)]
mod backend;
#[cfg(windows)]
pub(crate) use backend::{RecorderImpl, is_supported};

#[cfg(not(windows))]
mod stub;
#[cfg(not(windows))]
pub(crate) use stub::{RecorderImpl, is_supported};
