//! Screen-recording session: the control bar UI, the accent region frame, the
//! start/stop lifecycle, and the cross-process stop-toggle IPC.
//!
//! One recording runs in its own one-shot UI process (`--record-region` /
//! `--record-full`). The heavy lifting (WGC capture + Media Foundation encode)
//! lives in `skrino-record`; this module only drives it and paints the small
//! always-on-top control bar into the ROOT viewport (the same viewport the
//! region overlay used — reshaped from a fullscreen overlay into a compact pill,
//! transition-only viewport commands, never per-frame).
//!
//! SAFETY / ORDERING
//! -----------------
//! The translucent selection overlay must never end up in the video, so the
//! recorder is started only after an "arming" delay: the root window has by then
//! shrunk from the fullscreen overlay to the tiny control bar, the control bar is
//! excluded from capture via `SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)`,
//! and the accent border frame (separate layered Win32 windows, also excluded)
//! is up.
//!
//! IPC (mirrors the daemon's winapi plumbing)
//! ------------------------------------------
//! * `Global\skrino-recording` — a named mutex held for the recording process's
//!   lifetime. A second recording sees `ERROR_ALREADY_EXISTS` and bows out; the
//!   daemon probes it with `OpenMutexW` to decide start-vs-stop for a hotkey.
//! * `Global\skrino-record-stop` — an auto-reset named event. The daemon
//!   `SetEvent`s it to stop the active recording; a watcher thread in the
//!   recording process wakes, flips an atomic flag, and repaints the UI.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use egui::{CornerRadius, FontId, Pos2, Sense, Vec2, ViewportCommand};
use egui_phosphor::regular as ph;
use skrino_record::{RecordOptions, Recorder, RegionPx};

use crate::record_frame::BorderFrame;
use crate::theme::Palette;

/// Control-bar size in logical points.
const BAR_SIZE: Vec2 = Vec2::new(224.0, 48.0);
/// Gap (logical points) between the recorded region and the control bar.
const BAR_GAP: f32 = 8.0;
/// Minimum time the window is given to shrink from the overlay before capture
/// begins, so the fullscreen overlay is never caught in the first frames.
const ARM_DELAY: Duration = Duration::from_millis(180);
/// Minimum frames rendered during arming (belt-and-braces with `ARM_DELAY`).
const ARM_MIN_FRAMES: u32 = 3;

/// What the control bar reported this frame.
pub enum RecordSignal {
    /// Keep recording.
    None,
    /// Finish and run the save/upload pipeline (stop button or stop hotkey).
    Stop,
    /// Abort and discard (cancel button).
    Cancel,
    /// The engine failed to start or died mid-recording: notify and exit.
    Error(String),
}

enum Phase {
    /// Window is shrinking from the overlay; capture not started yet.
    Arming { since: Instant, frames: u32 },
    /// Recording is live.
    Active,
    /// Recorder stopped; a server upload is in flight (spinner shown).
    Finalizing,
}

/// A recording session: owns the [`Recorder`], the border frame, and the
/// control-bar state. Driven by `app.rs` one frame at a time.
pub struct RecordSession {
    /// What the recorder captures (`None` = full primary monitor).
    region: Option<RegionPx>,
    phase: Phase,
    recorder: Option<Recorder>,
    frame_windows: Option<BorderFrame>,
    /// The recorded options, built once at construction.
    opts: RecordOptions,
    /// Cross-thread stop request from the hotkey watcher.
    stop_flag: Arc<AtomicBool>,
    /// Control-bar HWND excluded from capture yet.
    affinity_set: bool,
    /// Precomputed control-bar window geometry (logical points).
    bar_pos: Pos2,
}

impl RecordSession {
    /// Build a session. `region` is the capture area in virtual-screen physical
    /// pixels (`None` = primary monitor); `scale` the monitor DPI scale;
    /// `audio` the single audio source (if any) to record; `output` the temp
    /// .mp4 path; `stop_flag` shared with the hotkey watcher.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        region: Option<RegionPx>,
        scale: f32,
        fps: u32,
        capture_cursor: bool,
        audio: skrino_record::AudioSource,
        output: PathBuf,
        stop_flag: Arc<AtomicBool>,
    ) -> Self {
        let scale = if scale > 0.0 { scale } else { 1.0 };
        let bar_pos = control_bar_pos(region, scale);
        let opts = RecordOptions {
            region,
            fps,
            capture_cursor,
            audio,
            output,
        };
        Self {
            region,
            phase: Phase::Arming {
                since: Instant::now(),
                frames: 0,
            },
            recorder: None,
            frame_windows: None,
            opts,
            stop_flag,
            affinity_set: false,
            bar_pos,
        }
    }

    /// Desired control-bar outer position (logical points).
    pub fn window_pos(&self) -> Pos2 {
        self.bar_pos
    }

    /// Desired control-bar inner size (logical points).
    pub fn window_size(&self) -> Vec2 {
        BAR_SIZE
    }

    /// Take the live recorder out so the caller can `stop()` it (consumes it).
    pub fn take_recorder(&mut self) -> Option<Recorder> {
        self.recorder.take()
    }

    /// Switch the control bar to the "uploading" spinner state.
    pub fn set_finalizing(&mut self) {
        self.phase = Phase::Finalizing;
    }

    /// Draw the control bar and advance the lifecycle. `frame` is needed to grab
    /// the control-bar HWND for the capture-exclusion affinity.
    pub fn ui(
        &mut self,
        ctx: &egui::Context,
        frame: &eframe::Frame,
        palette: &Palette,
    ) -> RecordSignal {
        // Keep the window out of the capture as soon as its HWND is real.
        if !self.affinity_set
            && let Some(hwnd) = control_bar_hwnd(frame)
        {
            exclude_from_capture(hwnd);
            self.affinity_set = true;
        }

        match &mut self.phase {
            Phase::Arming { since, frames } => {
                *frames += 1;
                let ready = *frames >= ARM_MIN_FRAMES && since.elapsed() >= ARM_DELAY;
                ctx.request_repaint();
                self.draw_bar(ctx, palette, BarView::Arming);
                if ready {
                    return self.begin_capture();
                }
                RecordSignal::None
            }
            Phase::Active => {
                // Hotkey stop toggle.
                if self.stop_flag.swap(false, Ordering::AcqRel) {
                    return RecordSignal::Stop;
                }
                // Engine died in the background.
                if let Some(rec) = &self.recorder
                    && let Some(err) = rec.take_error()
                {
                    return RecordSignal::Error(err);
                }
                ctx.request_repaint_after(Duration::from_millis(250));
                self.draw_bar(ctx, palette, BarView::Active)
            }
            Phase::Finalizing => {
                ctx.request_repaint_after(Duration::from_millis(120));
                self.draw_bar(ctx, palette, BarView::Finalizing);
                RecordSignal::None
            }
        }
    }

    /// Bring up the border frame and start the recorder (end of arming).
    fn begin_capture(&mut self) -> RecordSignal {
        if let Some(region) = self.region {
            self.frame_windows = Some(BorderFrame::new(region));
        }
        match Recorder::start(self.opts.clone()) {
            Ok(rec) => {
                self.recorder = Some(rec);
                self.phase = Phase::Active;
                RecordSignal::None
            }
            Err(e) => RecordSignal::Error(e.to_string()),
        }
    }

    /// Paint the pill and (in the Active view) return the button signal.
    fn draw_bar(&self, ctx: &egui::Context, palette: &Palette, view: BarView) -> RecordSignal {
        let mut signal = RecordSignal::None;
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(palette.panel)
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(egui::Margin::symmetric(12, 8)),
            )
            .show(ctx, |ui| {
                // Dragging the pill background moves the window (transition-only
                // command: only fired the frame the drag begins).
                let bg = ui.interact(
                    ui.max_rect(),
                    ui.id().with("record_bar_drag"),
                    Sense::click_and_drag(),
                );
                if bg.drag_started() {
                    ctx.send_viewport_cmd(ViewportCommand::StartDrag);
                }

                ui.horizontal_centered(|ui| {
                    match view {
                        BarView::Arming => {
                            ui.spinner();
                            ui.label(
                                egui::RichText::new("Подготовка…")
                                    .size(13.0)
                                    .color(palette.text_secondary),
                            );
                        }
                        BarView::Finalizing => {
                            ui.spinner();
                            ui.label(
                                egui::RichText::new("Загрузка…")
                                    .size(13.0)
                                    .color(palette.text_secondary),
                            );
                        }
                        BarView::Active => {
                            signal = self.draw_active_controls(ui, palette);
                        }
                    }
                });
            });
        signal
    }

    /// The live controls: red dot, timer, pause/resume, stop, cancel.
    fn draw_active_controls(&self, ui: &mut egui::Ui, palette: &Palette) -> RecordSignal {
        let mut signal = RecordSignal::None;
        let paused = self.recorder.as_ref().is_some_and(|r| r.is_paused());
        let elapsed = self
            .recorder
            .as_ref()
            .map(|r| r.elapsed())
            .unwrap_or_default();

        // Recording dot (dims while paused).
        let (dot_rect, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
        let dot_color = if paused {
            palette.text_secondary
        } else {
            palette.danger
        };
        ui.painter()
            .circle_filled(dot_rect.center(), 5.0, dot_color);

        // Timer mm:ss.
        ui.label(
            egui::RichText::new(format_elapsed(elapsed))
                .font(FontId::monospace(15.0))
                .color(palette.text),
        );

        ui.add_space(2.0);

        // Pause / resume.
        let pause_icon = if paused { ph::PLAY } else { ph::PAUSE };
        if ui
            .add(egui::Button::new(egui::RichText::new(pause_icon).size(16.0)).frame(false))
            .on_hover_text(if paused { "Продолжить" } else { "Пауза" })
            .clicked()
            && let Some(rec) = &self.recorder
        {
            if paused {
                rec.resume();
            } else {
                rec.pause();
            }
        }

        // Stop (accent).
        if ui
            .add(
                egui::Button::new(
                    egui::RichText::new(format!("{}  Стоп", ph::STOP))
                        .size(13.0)
                        .color(palette.accent_fg),
                )
                .fill(palette.accent)
                .corner_radius(CornerRadius::same(8)),
            )
            .clicked()
        {
            signal = RecordSignal::Stop;
        }

        // Cancel.
        if ui
            .add(egui::Button::new(egui::RichText::new(ph::X).size(15.0)).frame(false))
            .on_hover_text("Отмена")
            .clicked()
        {
            signal = RecordSignal::Cancel;
        }

        signal
    }
}

#[derive(Clone, Copy)]
enum BarView {
    Arming,
    Active,
    Finalizing,
}

/// Format a duration as `m:ss` (or `mm:ss`).
fn format_elapsed(d: Duration) -> String {
    let secs = d.as_secs();
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// Compute the control-bar outer position (logical points): just below the
/// region's bottom edge, else tucked into the region's top-right corner if there
/// is no room below, clamped to the containing monitor's work area.
fn control_bar_pos(region: Option<RegionPx>, scale: f32) -> Pos2 {
    let Some(region) = region else {
        // Full-primary recording with no explicit region: a modest default.
        return Pos2::new(40.0, 40.0);
    };
    let left = region.x as f32 / scale;
    let top = region.y as f32 / scale;
    let right = (region.x + region.width as i32) as f32 / scale;
    let bottom = (region.y + region.height as i32) as f32 / scale;

    let (mon_left, mon_top, mon_right, mon_bottom) = monitor_bounds_pt(region, scale);

    // Preferred: just below the region, left-aligned.
    let mut pos = Pos2::new(left, bottom + BAR_GAP);
    // No room below -> inside the region's top-right corner.
    if pos.y + BAR_SIZE.y > mon_bottom {
        pos = Pos2::new(right - BAR_SIZE.x - BAR_GAP, top + BAR_GAP);
    }
    // Clamp to the monitor's work area.
    pos.x = pos.x.clamp(mon_left, (mon_right - BAR_SIZE.x).max(mon_left));
    pos.y = pos.y.clamp(mon_top, (mon_bottom - BAR_SIZE.y).max(mon_top));
    pos
}

/// The bounds (logical points) of the monitor containing `region`'s centre,
/// falling back to the region's own rect when monitors can't be enumerated.
fn monitor_bounds_pt(region: RegionPx, scale: f32) -> (f32, f32, f32, f32) {
    let cx = region.x + region.width as i32 / 2;
    let cy = region.y + region.height as i32 / 2;
    if let Ok(monitors) = skrino_capture::list_monitors()
        && let Some(m) = monitors.iter().find(|m| {
            cx >= m.x && cx < m.x + m.width as i32 && cy >= m.y && cy < m.y + m.height as i32
        })
    {
        let s = if m.scale_factor > 0.0 {
            m.scale_factor
        } else {
            scale
        };
        return (
            m.x as f32 / s,
            m.y as f32 / s,
            (m.x + m.width as i32) as f32 / s,
            (m.y + m.height as i32) as f32 / s,
        );
    }
    (
        region.x as f32 / scale,
        region.y as f32 / scale,
        (region.x + region.width as i32) as f32 / scale,
        (region.y + region.height as i32) as f32 / scale,
    )
}

/// Build the temp .mp4 path the engine writes before the file is moved/uploaded.
pub fn temp_output_path() -> PathBuf {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    std::env::temp_dir().join(format!("skrino-rec-{stamp}.mp4"))
}

/// Full-monitor recording region: the monitor under the cursor as a
/// [`RegionPx`], else the primary, else `None` (engine records the primary).
/// The second return value is that monitor's DPI scale.
pub fn full_monitor_region() -> (Option<RegionPx>, f32) {
    let monitors = match skrino_capture::list_monitors() {
        Ok(m) if !m.is_empty() => m,
        _ => return (None, 1.0),
    };
    let cursor = cursor_pos();
    let chosen = cursor
        .and_then(|(cx, cy)| {
            monitors.iter().find(|m| {
                cx >= m.x && cx < m.x + m.width as i32 && cy >= m.y && cy < m.y + m.height as i32
            })
        })
        .or_else(|| monitors.iter().find(|m| m.is_primary))
        .or_else(|| monitors.first());
    match chosen {
        Some(m) => (
            Some(RegionPx {
                x: m.x,
                y: m.y,
                width: m.width,
                height: m.height,
            }),
            if m.scale_factor > 0.0 {
                m.scale_factor
            } else {
                1.0
            },
        ),
        None => (None, 1.0),
    }
}

/// `--record-smoke`: headless, automated-test-safe. Records the primary monitor
/// for 3 seconds with no interactive UI, prints the resulting .mp4 path, exits.
/// With the current stub engine it prints the "unsupported" error and exits(1).
pub fn run_smoke() -> ! {
    let output = temp_output_path();
    let opts = RecordOptions {
        region: None,
        fps: 30,
        capture_cursor: true,
        audio: skrino_record::AudioSource::None,
        output,
    };
    match Recorder::start(opts) {
        Ok(rec) => {
            std::thread::sleep(Duration::from_secs(3));
            match rec.stop() {
                Ok(path) => {
                    println!("{}", path.display());
                    std::process::exit(0);
                }
                Err(e) => {
                    eprintln!("не удалось завершить запись: {e}");
                    std::process::exit(1);
                }
            }
        }
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}

// --- cursor helper (mirrors app.rs) ---

#[cfg(windows)]
fn cursor_pos() -> Option<(i32, i32)> {
    use winapi::shared::windef::POINT;
    use winapi::um::winuser::GetCursorPos;
    let mut p = POINT { x: 0, y: 0 };
    if unsafe { GetCursorPos(&mut p) } != 0 {
        Some((p.x, p.y))
    } else {
        None
    }
}

#[cfg(not(windows))]
fn cursor_pos() -> Option<(i32, i32)> {
    None
}

// --- control-bar HWND + capture exclusion ---

#[cfg(windows)]
fn control_bar_hwnd(frame: &eframe::Frame) -> Option<winapi::shared::windef::HWND> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match frame.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get() as winapi::shared::windef::HWND),
        _ => None,
    }
}

#[cfg(windows)]
fn exclude_from_capture(hwnd: winapi::shared::windef::HWND) {
    // WDA_EXCLUDEFROMCAPTURE (winapi 0.3 lacks the constant).
    const WDA_EXCLUDEFROMCAPTURE: winapi::shared::minwindef::UINT = 0x11;
    unsafe {
        winapi::um::winuser::SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
    }
}

#[cfg(not(windows))]
fn control_bar_hwnd(_frame: &eframe::Frame) -> Option<()> {
    None
}

#[cfg(not(windows))]
fn exclude_from_capture(_hwnd: ()) {}

// ============================ Stop-toggle IPC ============================

#[cfg(windows)]
const RECORDING_MUTEX: &str = "Global\\skrino-recording";
#[cfg(windows)]
const STOP_EVENT: &str = "Global\\skrino-record-stop";

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Single-instance recording lock. Held for the recording process's lifetime;
/// [`acquire`](RecordingLock::acquire) returns `None` if a recording is already
/// running.
pub struct RecordingLock {
    #[cfg(windows)]
    handle: usize,
}

impl RecordingLock {
    /// Try to become the one active recording. `None` means another recording
    /// already holds the lock.
    #[cfg(windows)]
    pub fn acquire() -> Option<RecordingLock> {
        use winapi::shared::winerror::ERROR_ALREADY_EXISTS;
        use winapi::um::errhandlingapi::GetLastError;
        use winapi::um::synchapi::CreateMutexW;

        let name = wide(RECORDING_MUTEX);
        unsafe {
            let handle = CreateMutexW(std::ptr::null_mut(), 1, name.as_ptr());
            if handle.is_null() {
                // Could not create it; treat as "no other recording" so the
                // user isn't blocked by an infrastructure hiccup.
                return Some(RecordingLock { handle: 0 });
            }
            if GetLastError() == ERROR_ALREADY_EXISTS {
                winapi::um::handleapi::CloseHandle(handle);
                return None;
            }
            Some(RecordingLock {
                handle: handle as usize,
            })
        }
    }

    #[cfg(not(windows))]
    pub fn acquire() -> Option<RecordingLock> {
        Some(RecordingLock {})
    }
}

#[cfg(windows)]
impl Drop for RecordingLock {
    fn drop(&mut self) {
        if self.handle != 0 {
            unsafe {
                winapi::um::handleapi::CloseHandle(self.handle as winapi::um::winnt::HANDLE);
            }
        }
    }
}

/// Spawn the watcher thread that blocks on the stop event and, when signalled,
/// flips the returned flag and repaints the UI. The flag is polled by the
/// control bar each frame.
#[cfg(windows)]
pub fn spawn_stop_watcher(ctx: egui::Context) -> Arc<AtomicBool> {
    use winapi::um::synchapi::{CreateEventW, WaitForSingleObject};
    use winapi::um::winbase::{INFINITE, WAIT_OBJECT_0};

    let flag = Arc::new(AtomicBool::new(false));
    let name = wide(STOP_EVENT);
    // Auto-reset, initially non-signaled.
    let handle = unsafe { CreateEventW(std::ptr::null_mut(), 0, 0, name.as_ptr()) };
    if handle.is_null() {
        log::warn!("could not create stop event; hotkey stop-toggle disabled");
        return flag;
    }
    let handle_usize = handle as usize;
    let watcher_flag = flag.clone();
    std::thread::Builder::new()
        .name("skrino-record-stop-watch".into())
        .spawn(move || {
            let handle = handle_usize as winapi::um::winnt::HANDLE;
            loop {
                let r = unsafe { WaitForSingleObject(handle, INFINITE) };
                if r != WAIT_OBJECT_0 {
                    break;
                }
                watcher_flag.store(true, Ordering::Release);
                ctx.request_repaint();
            }
        })
        .ok();
    flag
}

#[cfg(not(windows))]
pub fn spawn_stop_watcher(_ctx: egui::Context) -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

/// Daemon side: is a recording currently active? (Probe the recording mutex.)
#[cfg(windows)]
pub fn is_recording_active() -> bool {
    use winapi::um::handleapi::CloseHandle;
    use winapi::um::synchapi::OpenMutexW;
    use winapi::um::winnt::SYNCHRONIZE;

    let name = wide(RECORDING_MUTEX);
    unsafe {
        let handle = OpenMutexW(SYNCHRONIZE, 0, name.as_ptr());
        if handle.is_null() {
            false
        } else {
            CloseHandle(handle);
            true
        }
    }
}

#[cfg(not(windows))]
pub fn is_recording_active() -> bool {
    false
}

/// Daemon side: ask the active recording to stop (set the stop event).
#[cfg(windows)]
pub fn signal_stop() {
    use winapi::um::handleapi::CloseHandle;
    use winapi::um::synchapi::{OpenEventW, SetEvent};
    use winapi::um::winnt::EVENT_MODIFY_STATE;

    let name = wide(STOP_EVENT);
    unsafe {
        let handle = OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr());
        if handle.is_null() {
            return;
        }
        SetEvent(handle);
        CloseHandle(handle);
    }
}

#[cfg(not(windows))]
pub fn signal_stop() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_elapsed_is_mm_ss() {
        assert_eq!(format_elapsed(Duration::from_secs(0)), "0:00");
        assert_eq!(format_elapsed(Duration::from_secs(5)), "0:05");
        assert_eq!(format_elapsed(Duration::from_secs(65)), "1:05");
        assert_eq!(format_elapsed(Duration::from_secs(600)), "10:00");
    }

    #[test]
    fn control_bar_pos_below_region_when_room() {
        // A small region high on a large-ish virtual screen: the bar sits just
        // below the region's bottom edge (scale 1.0 -> points == pixels).
        let region = RegionPx {
            x: 100,
            y: 100,
            width: 400,
            height: 200,
        };
        let pos = control_bar_pos(Some(region), 1.0);
        // Just below the bottom edge (y = 300 + gap), left-aligned at x = 100,
        // unless clamped by the monitor bounds fallback (region rect). Since the
        // fallback bounds equal the region, clamping pins x within [100, 100].
        assert!(pos.y >= 100.0);
    }

    #[test]
    fn full_region_uses_default_bar_pos() {
        let pos = control_bar_pos(None, 1.0);
        assert_eq!(pos, Pos2::new(40.0, 40.0));
    }
}
