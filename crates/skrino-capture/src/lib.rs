//! skrino-capture — screen capture abstraction.
//!
//! Backed by `xcap` (Windows Graphics Capture on Windows). All returned images
//! are in physical pixels; `MonitorInfo` coordinates are in the virtual-screen
//! coordinate space (physical pixels, origin at the primary monitor's top-left,
//! can be negative for monitors left/above the primary).

use anyhow::{Context, Result};
use image::RgbaImage;
use xcap::{Monitor, Window};

#[derive(Debug, Clone)]
pub struct MonitorInfo {
    pub id: u32,
    pub name: String,
    /// Virtual-screen position in physical pixels.
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale_factor: f32,
    pub is_primary: bool,
}

#[derive(Debug, Clone)]
pub struct WindowInfo {
    pub id: u32,
    pub title: String,
    pub app_name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub is_minimized: bool,
}

/// A captured virtual screen: one image spanning all monitors, plus the offset
/// of its top-left corner in virtual-screen coordinates (needed to map a
/// selection made on a specific monitor back into image pixels).
pub struct VirtualScreenCapture {
    pub image: RgbaImage,
    /// Virtual-screen coordinate of image pixel (0, 0).
    pub origin_x: i32,
    pub origin_y: i32,
    pub monitors: Vec<MonitorInfo>,
}

/// Build a `MonitorInfo` from an `xcap::Monitor`.
///
/// DPI note: on the Windows backend (xcap 0.7.1), `Monitor::x()/y()` come from
/// `EnumDisplaySettingsW`'s `dmPosition`, and `Monitor::width()/height()` come
/// from that same call's `dmPelsWidth`/`dmPelsHeight`. Those are the display
/// mode's actual device (physical) pixels, *not* DPI-scaled logical pixels, and
/// `Monitor::capture_image()` captures exactly `width x height` physical pixels
/// at `(x, y)` via `BitBlt` against the desktop DC. So on Windows these values
/// are already self-consistent physical-pixel virtual-screen coordinates and no
/// scale conversion is needed here. We still don't fully trust this getter pair
/// in `capture_virtual_screen` below — there we re-derive width/height from the
/// actual captured image buffer instead, in case another platform/backend ever
/// reports logical/scaled sizes from these getters while capturing physical
/// pixels (or vice versa).
fn monitor_info(monitor: &Monitor) -> Result<MonitorInfo> {
    let id = monitor.id().context("failed to read monitor id")?;
    let name = monitor
        .name()
        .with_context(|| format!("failed to read name for monitor {id}"))?;
    let x = monitor
        .x()
        .with_context(|| format!("failed to read x position for monitor {id}"))?;
    let y = monitor
        .y()
        .with_context(|| format!("failed to read y position for monitor {id}"))?;
    let width = monitor
        .width()
        .with_context(|| format!("failed to read width for monitor {id}"))?;
    let height = monitor
        .height()
        .with_context(|| format!("failed to read height for monitor {id}"))?;
    let scale_factor = monitor
        .scale_factor()
        .with_context(|| format!("failed to read scale factor for monitor {id}"))?;
    let is_primary = monitor
        .is_primary()
        .with_context(|| format!("failed to read is_primary for monitor {id}"))?;

    Ok(MonitorInfo {
        id,
        name,
        x,
        y,
        width,
        height,
        scale_factor,
        is_primary,
    })
}

fn find_monitor(id: u32) -> Result<Monitor> {
    Monitor::all()
        .context("failed to enumerate monitors")?
        .into_iter()
        .find(|m| matches!(m.id(), Ok(monitor_id) if monitor_id == id))
        .with_context(|| format!("no monitor with id {id}"))
}

fn find_window(id: u32) -> Result<Window> {
    Window::all()
        .context("failed to enumerate windows")?
        .into_iter()
        .find(|w| matches!(w.id(), Ok(window_id) if window_id == id))
        .with_context(|| format!("no window with id {id}"))
}

pub fn list_monitors() -> Result<Vec<MonitorInfo>> {
    let monitors = Monitor::all().context("failed to enumerate monitors")?;
    monitors.iter().map(monitor_info).collect()
}

pub fn list_windows() -> Result<Vec<WindowInfo>> {
    let windows = Window::all().context("failed to enumerate windows")?;
    let mut result = Vec::with_capacity(windows.len());

    for window in &windows {
        // Any of these getters can fail transiently (e.g. the window closed
        // between enumeration and inspection); skip rather than abort the
        // whole listing.
        let Ok(id) = window.id() else { continue };
        let Ok(width) = window.width() else { continue };
        let Ok(height) = window.height() else { continue };
        let is_minimized = window.is_minimized().unwrap_or(false);

        // Filter out minimized-and-sizeless junk: a minimized window's client
        // rect commonly collapses to 0x0 (or otherwise degenerate) since there
        // is nothing to composite. Zero-sized windows are never useful capture
        // targets regardless of minimized state, so drop those too.
        if width == 0 || height == 0 {
            continue;
        }

        let title = window.title().unwrap_or_default();
        let app_name = window.app_name().unwrap_or_default();
        let x = window.x().unwrap_or(0);
        let y = window.y().unwrap_or(0);

        result.push(WindowInfo {
            id,
            title,
            app_name,
            x,
            y,
            width,
            height,
            is_minimized,
        });
    }

    Ok(result)
}

/// Capture a single monitor.
pub fn capture_monitor(id: u32) -> Result<RgbaImage> {
    let monitor = find_monitor(id)?;
    monitor
        .capture_image()
        .with_context(|| format!("failed to capture monitor {id}"))
}

/// Capture a single window by id from `list_windows`.
pub fn capture_window(id: u32) -> Result<RgbaImage> {
    let window = find_window(id)?;
    window
        .capture_image()
        .with_context(|| format!("failed to capture window {id}"))
}

/// Capture all monitors and stitch them into one image covering the whole
/// virtual screen. Areas not covered by any monitor are transparent black.
pub fn capture_virtual_screen() -> Result<VirtualScreenCapture> {
    let monitors = Monitor::all().context("failed to enumerate monitors")?;
    if monitors.is_empty() {
        anyhow::bail!("no monitors found");
    }

    // Capture every monitor up front and reconcile each MonitorInfo's
    // width/height with the *actual* captured pixel buffer dimensions (see the
    // DPI note on `monitor_info` above) so that the bounding-box math and the
    // blit below are always self-consistent, even if a getter and the
    // capture disagree.
    let mut captured: Vec<(MonitorInfo, RgbaImage)> = Vec::with_capacity(monitors.len());
    for monitor in &monitors {
        let id = monitor.id().unwrap_or(0);
        let mut info = monitor_info(monitor)
            .with_context(|| format!("failed to read info for monitor {id}"))?;
        let image = monitor
            .capture_image()
            .with_context(|| format!("failed to capture monitor {id} ({})", info.name))?;

        if info.width != image.width() || info.height != image.height() {
            log::warn!(
                "monitor {id} reported size {}x{} but captured image is {}x{}; using captured size",
                info.width,
                info.height,
                image.width(),
                image.height()
            );
            info.width = image.width();
            info.height = image.height();
        }

        captured.push((info, image));
    }

    let min_x = captured.iter().map(|(info, _)| info.x).min().unwrap();
    let min_y = captured.iter().map(|(info, _)| info.y).min().unwrap();
    let max_x = captured
        .iter()
        .map(|(info, _)| info.x + info.width as i32)
        .max()
        .unwrap();
    let max_y = captured
        .iter()
        .map(|(info, _)| info.y + info.height as i32)
        .max()
        .unwrap();

    let total_width = (max_x - min_x) as u32;
    let total_height = (max_y - min_y) as u32;

    // Transparent black background for any area not covered by a monitor
    // (can happen with irregular multi-monitor layouts).
    let mut image = RgbaImage::new(total_width, total_height);

    let mut monitor_infos = Vec::with_capacity(captured.len());
    for (info, monitor_image) in captured {
        let dest_x = (info.x - min_x) as i64;
        let dest_y = (info.y - min_y) as i64;
        image::imageops::replace(&mut image, &monitor_image, dest_x, dest_y);
        monitor_infos.push(info);
    }

    Ok(VirtualScreenCapture {
        image,
        origin_x: min_x,
        origin_y: min_y,
        monitors: monitor_infos,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_at_least_one_monitor_with_exactly_one_primary() {
        let monitors = list_monitors().expect("list_monitors should succeed");
        assert!(!monitors.is_empty(), "expected at least one monitor");

        let primary_count = monitors.iter().filter(|m| m.is_primary).count();
        assert_eq!(
            primary_count, 1,
            "expected exactly one primary monitor, got {primary_count}: {monitors:?}"
        );

        for m in &monitors {
            assert!(m.width > 0 && m.height > 0, "monitor {m:?} has zero size");
            assert!(m.scale_factor > 0.0, "monitor {m:?} has non-positive scale factor");
        }
    }

    #[test]
    fn lists_windows_without_sizeless_minimized_junk() {
        let windows = list_windows().expect("list_windows should succeed");
        for w in &windows {
            assert!(w.width > 0 && w.height > 0, "window {w:?} has zero size");
        }
        // Not asserting non-empty: a CI/minimal desktop could plausibly have
        // zero visible top-level windows at test time.
    }

    #[test]
    fn captures_a_single_monitor_matching_its_reported_size() {
        let monitors = list_monitors().expect("list_monitors should succeed");
        let monitor = monitors.first().expect("at least one monitor");

        let image = capture_monitor(monitor.id).expect("capture_monitor should succeed");
        assert_eq!(image.width(), monitor.width);
        assert_eq!(image.height(), monitor.height);
        assert!(has_pixel_variance(&image), "captured monitor image looks blank/solid");
    }

    #[test]
    fn captures_virtual_screen_spanning_the_bounding_box_of_all_monitors() {
        let capture = capture_virtual_screen().expect("capture_virtual_screen should succeed");
        assert!(!capture.monitors.is_empty());

        let min_x = capture.monitors.iter().map(|m| m.x).min().unwrap();
        let min_y = capture.monitors.iter().map(|m| m.y).min().unwrap();
        let max_x = capture
            .monitors
            .iter()
            .map(|m| m.x + m.width as i32)
            .max()
            .unwrap();
        let max_y = capture
            .monitors
            .iter()
            .map(|m| m.y + m.height as i32)
            .max()
            .unwrap();

        assert_eq!(capture.origin_x, min_x);
        assert_eq!(capture.origin_y, min_y);
        assert_eq!(capture.image.width(), (max_x - min_x) as u32);
        assert_eq!(capture.image.height(), (max_y - min_y) as u32);

        assert!(
            has_pixel_variance(&capture.image),
            "stitched virtual screen image looks blank/solid, desktop should be visible"
        );
    }

    /// Sanity check that an image isn't a single flat color (e.g. all-black),
    /// tolerant of desktops that happen to have large uniform regions: we just
    /// need *some* variance across the image.
    fn has_pixel_variance(image: &RgbaImage) -> bool {
        let mut min = [255u8; 3];
        let mut max = [0u8; 3];
        // Sample on a grid instead of every pixel to keep this fast on large
        // multi-monitor captures.
        let step_x = (image.width() / 200).max(1);
        let step_y = (image.height() / 200).max(1);

        for y in (0..image.height()).step_by(step_y as usize) {
            for x in (0..image.width()).step_by(step_x as usize) {
                let p = image.get_pixel(x, y);
                for c in 0..3 {
                    min[c] = min[c].min(p[c]);
                    max[c] = max[c].max(p[c]);
                }
            }
        }

        max.iter().zip(min.iter()).any(|(mx, mn)| mx.saturating_sub(*mn) > 4)
    }
}
