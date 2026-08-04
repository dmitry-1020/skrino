//! Keeping Skrino's own windows out of Skrino's own captures.
//!
//! Windows 10 2004+ exposes `SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)`:
//! the window keeps rendering on the physical monitor but is invisible to every
//! screen-capture path — the desktop `BitBlt` behind `skrino-capture` and the
//! Windows Graphics Capture session behind `skrino-record` alike. That is what
//! keeps the launcher, the selection overlay and the recording control bar out
//! of the shot with no hide-flicker and no settle delay.
//!
//! The exclusion is applied per window mode (see `WindowMode` in
//! [`crate::app`]): capture-adjacent chrome is excluded, the editor and the
//! settings screen are not — those are ordinary windows the user may well want
//! visible in a screen share.
//!
//! Where the affinity call is unavailable (Windows older than 2004, non-Windows
//! builds) the caller must actually hide the window and let the desktop settle
//! before capturing — see `SkrinoApp::start_capture_job`.

/// The platform's native window handle.
#[cfg(windows)]
pub type WindowId = winapi::shared::windef::HWND;
#[cfg(not(windows))]
pub type WindowId = ();

/// The root viewport's native window handle.
#[cfg(windows)]
pub fn window_id(frame: &eframe::Frame) -> Option<WindowId> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match frame.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get() as WindowId),
        _ => None,
    }
}

#[cfg(not(windows))]
pub fn window_id(_frame: &eframe::Frame) -> Option<WindowId> {
    None
}

/// Exclude `window` from (or restore it to) screen captures. Returns `true`
/// only when the platform actually honoured the request — a `false` for
/// `excluded = true` means the caller must fall back to hiding the window.
#[cfg(windows)]
pub fn set_excluded(window: WindowId, excluded: bool) -> bool {
    use winapi::shared::minwindef::UINT;
    // winapi 0.3 only ships WDA_NONE / WDA_MONITOR.
    const WDA_NONE: UINT = 0x00;
    const WDA_EXCLUDEFROMCAPTURE: UINT = 0x11;

    let affinity = if excluded {
        WDA_EXCLUDEFROMCAPTURE
    } else {
        WDA_NONE
    };
    let ok = unsafe { winapi::um::winuser::SetWindowDisplayAffinity(window, affinity) != 0 };
    if !ok && excluded {
        log::warn!("SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE) failed; hiding instead");
    }
    ok
}

#[cfg(not(windows))]
pub fn set_excluded(_window: WindowId, _excluded: bool) -> bool {
    false
}

/// Does the exclusion actually keep a window out of `skrino-capture`'s output?
/// This is the one assumption the whole "never photograph yourself" design rests
/// on, so it is checked against the real desktop rather than assumed: a small
/// solid-colour probe window is created on the primary monitor and the monitor
/// is captured before and after the affinity is set.
#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    use winapi::shared::windef::HWND;
    use winapi::um::libloaderapi::GetModuleHandleW;
    use winapi::um::wingdi::CreateSolidBrush;
    use winapi::um::winuser::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, LWA_ALPHA, MSG, PM_REMOVE,
        PeekMessageW, RegisterClassW, SW_SHOWNOACTIVATE, SetLayeredWindowAttributes, ShowWindow,
        TranslateMessage, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
        WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
    };

    /// A colour an ordinary desktop pixel is very unlikely to hit.
    const PROBE_RGB: [i32; 3] = [253, 2, 1];
    /// The same colour as a Win32 `COLORREF` (0x00BBGGRR).
    const PROBE_COLORREF: u32 = 0x00_01_02_FD;
    /// Probe-window edge in window units.
    const PROBE_SIZE: i32 = 60;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// A topmost, click-through, solid-colour window at `(x, y)`.
    fn create_probe(x: i32, y: i32) -> Option<HWND> {
        let name = wide("SkrinoCaptureShieldProbe");
        unsafe {
            let hinstance = GetModuleHandleW(std::ptr::null());
            let wc = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(DefWindowProcW),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinstance,
                hIcon: std::ptr::null_mut(),
                hCursor: std::ptr::null_mut(),
                hbrBackground: CreateSolidBrush(PROBE_COLORREF),
                lpszMenuName: std::ptr::null(),
                lpszClassName: name.as_ptr(),
            };
            // A zero atom just means the class is already registered.
            let _ = RegisterClassW(&wc);
            let hwnd = CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_TOOLWINDOW
                    | WS_EX_TOPMOST
                    | WS_EX_NOACTIVATE,
                name.as_ptr(),
                std::ptr::null(),
                WS_POPUP,
                x,
                y,
                PROBE_SIZE,
                PROBE_SIZE,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                hinstance,
                std::ptr::null_mut(),
            );
            if hwnd.is_null() {
                return None;
            }
            SetLayeredWindowAttributes(hwnd, 0, 255, LWA_ALPHA);
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            Some(hwnd)
        }
    }

    /// Drain the message queue for `dur` so the probe window paints and the
    /// desktop composition settles.
    fn pump(dur: Duration) {
        let end = Instant::now() + dur;
        let mut msg: MSG = unsafe { std::mem::zeroed() };
        while Instant::now() < end {
            unsafe {
                while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Pixels close enough to the probe colour (tolerant of colour management).
    fn probe_pixels(image: &image::RgbaImage) -> u32 {
        image
            .pixels()
            .filter(|p| (0..3).all(|c| (p[c] as i32 - PROBE_RGB[c]).abs() <= 12))
            .count() as u32
    }

    #[test]
    fn excluded_window_is_absent_from_a_capture_of_the_same_monitor() {
        let Ok(monitors) = skrino_capture::list_monitors() else {
            return; // No desktop session (headless CI): nothing to assert.
        };
        let Some(primary) = monitors.iter().find(|m| m.is_primary) else {
            return;
        };
        // The primary monitor's origin is the virtual-screen origin, so the
        // probe lands well inside it whatever the DPI virtualisation does to
        // this (unmanifested, DPI-unaware) test binary's coordinates.
        let Some(hwnd) = create_probe(80, 80) else {
            return;
        };

        pump(Duration::from_millis(400));
        let before = skrino_capture::capture_monitor(primary.id).map(|i| probe_pixels(&i));

        let applied = set_excluded(hwnd, true);
        pump(Duration::from_millis(400));
        let after = skrino_capture::capture_monitor(primary.id).map(|i| probe_pixels(&i));

        unsafe { DestroyWindow(hwnd) };
        pump(Duration::from_millis(50));

        let (Ok(before), Ok(after)) = (before, after) else {
            return; // Capture unavailable; the shield itself is untestable here.
        };
        let area = (PROBE_SIZE * PROBE_SIZE) as u32;

        assert!(applied, "SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE) failed");
        assert!(
            before >= area / 2,
            "probe window never showed up in the capture ({before} px, expected ~{area}); \
             the test can't tell exclusion from a window that was never visible"
        );
        assert!(
            after <= area / 50,
            "excluded window is still in the capture ({after} px of {area})"
        );
    }
}
