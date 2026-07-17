//! Accent-coloured border frame drawn around the recorded region.
//!
//! The frame is four thin plain Win32 *layered* windows created directly with
//! the winapi (NOT egui viewports): each is `WS_EX_LAYERED | WS_EX_TRANSPARENT |
//! WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE` so it is click-through,
//! always on top, and never grabs focus or a taskbar button. A solid accent
//! fill comes from the class background brush plus `SetLayeredWindowAttributes`
//! (opaque alpha), and `SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)` keeps
//! the frame itself out of the recording.
//!
//! Coordinates are virtual-screen *physical* pixels — the same space as
//! [`RegionPx`] and the capture crate — which is what `CreateWindowExW` expects
//! for a per-monitor-DPI-aware process (eframe/winit makes the process aware).
//!
//! Lifetime is RAII: [`BorderFrame::new`] creates the windows, `Drop` destroys
//! them. Creation is best-effort and never panics; on any failure the frame is
//! simply absent (an empty struct).

use skrino_record::RegionPx;

/// Frame thickness in physical pixels.
#[cfg(windows)]
const THICKNESS: i32 = 2;

/// Accent colour #F8BB10 as a Win32 `COLORREF` (0x00BBGGRR).
#[cfg(windows)]
const ACCENT_COLORREF: u32 = 0x00_10_BB_F8;

#[cfg(windows)]
mod win {
    use super::{ACCENT_COLORREF, RegionPx, THICKNESS};
    use std::sync::atomic::{AtomicBool, Ordering};

    use winapi::shared::minwindef::{ATOM, UINT};
    use winapi::shared::windef::HWND;
    use winapi::um::libloaderapi::GetModuleHandleW;
    use winapi::um::wingdi::CreateSolidBrush;
    use winapi::um::winuser::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW, SetLayeredWindowAttributes,
        SetWindowDisplayAffinity, ShowWindow, LWA_ALPHA, SW_SHOWNOACTIVATE, WNDCLASSW, WS_EX_LAYERED,
        WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
    };

    /// `WDA_EXCLUDEFROMCAPTURE` (winapi 0.3 only ships `WDA_NONE`/`WDA_MONITOR`).
    const WDA_EXCLUDEFROMCAPTURE: UINT = 0x11;

    static CLASS_REGISTERED: AtomicBool = AtomicBool::new(false);

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn class_name() -> Vec<u16> {
        wide("SkrinoRecordFrame")
    }

    /// Register the window class once (idempotent). The class owns a solid
    /// accent background brush so each strip paints itself with no WM_PAINT code.
    fn ensure_class() {
        if CLASS_REGISTERED.load(Ordering::Acquire) {
            return;
        }
        unsafe {
            let hinstance = GetModuleHandleW(std::ptr::null());
            let brush = CreateSolidBrush(ACCENT_COLORREF);
            let name = class_name();
            let wc = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(DefWindowProcW),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinstance,
                hIcon: std::ptr::null_mut(),
                hCursor: std::ptr::null_mut(),
                hbrBackground: brush,
                lpszMenuName: std::ptr::null(),
                lpszClassName: name.as_ptr(),
            };
            let atom: ATOM = RegisterClassW(&wc);
            // A zero atom may just mean "already registered" from a previous
            // frame; either way further CreateWindowExW calls succeed.
            let _ = atom;
        }
        CLASS_REGISTERED.store(true, Ordering::Release);
    }

    /// Create one accent strip at the given physical-pixel rectangle.
    fn create_strip(x: i32, y: i32, w: i32, h: i32) -> Option<HWND> {
        if w <= 0 || h <= 0 {
            return None;
        }
        unsafe {
            let hinstance = GetModuleHandleW(std::ptr::null());
            let name = class_name();
            let ex_style = WS_EX_LAYERED
                | WS_EX_TRANSPARENT
                | WS_EX_TOOLWINDOW
                | WS_EX_TOPMOST
                | WS_EX_NOACTIVATE;
            let hwnd = CreateWindowExW(
                ex_style,
                name.as_ptr(),
                std::ptr::null(),
                WS_POPUP,
                x,
                y,
                w,
                h,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                hinstance,
                std::ptr::null_mut(),
            );
            if hwnd.is_null() {
                return None;
            }
            // Opaque: the layered attributes just make the class brush show.
            SetLayeredWindowAttributes(hwnd, 0, 255, LWA_ALPHA);
            // Keep the frame out of the recording it surrounds.
            SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            Some(hwnd)
        }
    }

    /// The four strips surrounding `region` (drawn just outside its edges).
    pub(super) fn create_frame(region: RegionPx) -> Vec<HWND> {
        ensure_class();
        let t = THICKNESS;
        let (rx, ry) = (region.x, region.y);
        let (rw, rh) = (region.width as i32, region.height as i32);
        let rects = [
            (rx - t, ry - t, rw + 2 * t, t), // top
            (rx - t, ry + rh, rw + 2 * t, t), // bottom
            (rx - t, ry, t, rh),             // left
            (rx + rw, ry, t, rh),            // right
        ];
        rects
            .into_iter()
            .filter_map(|(x, y, w, h)| create_strip(x, y, w, h))
            .collect()
    }

    /// Destroy every strip window.
    pub(super) fn destroy_frame(hwnds: &[HWND]) {
        for &hwnd in hwnds {
            unsafe {
                DestroyWindow(hwnd);
            }
        }
    }
}

/// RAII accent frame around a recorded region. Dropping it destroys the strips.
#[cfg(windows)]
pub struct BorderFrame {
    hwnds: Vec<winapi::shared::windef::HWND>,
}

#[cfg(windows)]
impl BorderFrame {
    /// Create the four accent strips surrounding `region` (best effort).
    pub fn new(region: RegionPx) -> Self {
        Self {
            hwnds: win::create_frame(region),
        }
    }
}

#[cfg(windows)]
impl Drop for BorderFrame {
    fn drop(&mut self) {
        win::destroy_frame(&self.hwnds);
        self.hwnds.clear();
    }
}

// --- non-Windows stub (the app targets Windows; keep it compiling elsewhere) ---

#[cfg(not(windows))]
pub struct BorderFrame;

#[cfg(not(windows))]
impl BorderFrame {
    pub fn new(_region: RegionPx) -> Self {
        BorderFrame
    }
}
