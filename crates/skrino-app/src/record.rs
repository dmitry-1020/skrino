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
//! shrunk from the fullscreen overlay to the tiny control bar, and the accent
//! border frame (separate layered Win32 windows) is up. Both are excluded from
//! capture — the control bar by `app.rs` when it applies the `ControlBar` window
//! mode (see [`crate::capture_shield`]), the frame strips by `record_frame.rs`.
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

use crate::capture_shield::WindowId;
use crate::record_frame::BorderFrame;
use crate::theme::Palette;

/// Control-bar size in logical points. Slightly wider than the original 224 to
/// fit the stop-hotkey hint under the timer, and a touch taller for that second
/// line; kept a fixed constant so `window_size`/placement math stay in sync.
const BAR_SIZE: Vec2 = Vec2::new(296.0, 52.0);
/// Gap (logical points) between the recorded region and the control bar.
const BAR_GAP: f32 = 8.0;
/// Layered-window translucency on the glow control-bar window. The bar is an
/// OpenGL (glow) window, so uniform `LWA_ALPHA` (whole-window alpha) is used;
/// it composes correctly regardless of per-pixel rendering and coexists with the
/// window's `WDA_EXCLUDEFROMCAPTURE` affinity (display affinity and extended
/// window styles are independent). The idle alpha is kept intentionally high so
/// the bar stays clearly legible even if a GPU renders the layered GL surface
/// imperfectly.
const BAR_ALPHA_IDLE: u8 = 215;
/// Fully opaque while the pointer is over the bar.
const BAR_ALPHA_HOVER: u8 = 255;
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
    /// Precomputed control-bar window geometry (logical points).
    bar_pos: Pos2,
    /// Stop-hotkey label shown as a hint in the Active view (e.g. "Ctrl+Shift+6").
    stop_hotkey: String,
    /// Whether the pointer was over the bar last frame. Drives the layered-window
    /// alpha: the alpha is pushed to Win32 only when this flips, never per-frame.
    hover: bool,
    /// Whether `WS_EX_LAYERED` has been applied and the initial alpha pushed.
    layered_ready: bool,
}

impl RecordSession {
    /// Build a session. `region` is the capture area in virtual-screen physical
    /// pixels (`None` = primary monitor); `scale` the monitor DPI scale;
    /// `audio` the single audio source (if any) to record; `output` the temp
    /// .mp4 path; `stop_hotkey` the label of the hotkey that also stops the
    /// recording (shown as a hint); `stop_flag` shared with the hotkey watcher.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        region: Option<RegionPx>,
        scale: f32,
        fps: u32,
        capture_cursor: bool,
        audio: skrino_record::AudioSource,
        output: PathBuf,
        stop_hotkey: String,
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
            bar_pos,
            stop_hotkey,
            hover: false,
            layered_ready: false,
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

    /// Draw the control bar and advance the lifecycle. `bar_hwnd` is the root
    /// (control-bar) window handle, used to drive the layered-window translucency
    /// on hover changes.
    pub fn ui(
        &mut self,
        ctx: &egui::Context,
        palette: &Palette,
        bar_hwnd: Option<WindowId>,
    ) -> RecordSignal {
        // Hover = pointer over the bar (the whole window IS the bar). Detected via
        // egui, not Win32. The alpha is only pushed to Win32 when it flips.
        let hovered = ctx.input(|i| i.pointer.hover_pos()).is_some();
        self.update_bar_alpha(bar_hwnd, hovered);

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

    /// Keep the layered-window alpha in step with hover. Adds `WS_EX_LAYERED`
    /// once (and pushes the initial alpha), then pushes a new alpha ONLY when the
    /// hover state changes, never every frame.
    fn update_bar_alpha(&mut self, bar_hwnd: Option<WindowId>, hovered: bool) {
        #[cfg(windows)]
        if let Some(hwnd) = bar_hwnd {
            if !self.layered_ready {
                ensure_layered(hwnd);
                self.layered_ready = true;
                self.hover = hovered;
                set_bar_alpha(hwnd, alpha_for(hovered));
                return;
            }
            if hovered != self.hover {
                self.hover = hovered;
                set_bar_alpha(hwnd, alpha_for(hovered));
            }
        }
        #[cfg(not(windows))]
        {
            let _ = (bar_hwnd, hovered);
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
                    .inner_margin(egui::Margin::symmetric(12, 6)),
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

        // Timer mm:ss, with the stop-hotkey hint tucked underneath (discoverable
        // backstop: pressing the record hotkey again also stops the recording).
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            ui.label(
                egui::RichText::new(format_elapsed(elapsed))
                    .font(FontId::monospace(15.0))
                    .color(palette.text),
            );
            if !self.stop_hotkey.is_empty() {
                ui.label(
                    egui::RichText::new(format!("стоп: {}", self.stop_hotkey))
                        .size(9.5)
                        .color(palette.text_secondary),
                );
            }
        });

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

/// Plain monitor geometry (physical pixels) fed to the placement math, so the
/// geometry is unit-testable without a real desktop. `w`/`h` are the full monitor
/// bounds; `wx`/`wy`/`ww`/`wh` are the work area (taskbar excluded).
#[derive(Clone, Copy)]
struct MonGeom {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    wx: i32,
    wy: i32,
    ww: i32,
    wh: i32,
    scale: f32,
    is_primary: bool,
}

impl MonGeom {
    fn scale(&self) -> f32 {
        if self.scale > 0.0 { self.scale } else { 1.0 }
    }

    /// Work-area edges in logical points.
    fn work_pt(&self) -> (f32, f32, f32, f32) {
        let s = self.scale();
        (
            self.wx as f32 / s,
            self.wy as f32 / s,
            (self.wx + self.ww) as f32 / s,
            (self.wy + self.wh) as f32 / s,
        )
    }

    fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }

    fn same_as(&self, other: &MonGeom) -> bool {
        self.x == other.x && self.y == other.y && self.w == other.w && self.h == other.h
    }
}

/// Compute the control-bar outer position (logical points). Region recordings
/// keep the bar just below the region when there is room on that region's own
/// monitor; otherwise (full-screen, or a region hugging the bottom) the bar docks
/// off the recorded content: onto a second monitor's work area if one exists,
/// else onto the bottom-centre of the recorded monitor's own work area. Always
/// clamped to the chosen monitor's work area so it can never go off-screen.
fn control_bar_pos(region: Option<RegionPx>, scale: f32) -> Pos2 {
    control_bar_pos_geom(region, &gather_monitor_geom(scale))
}

/// Pure placement math (see [`control_bar_pos`]); split out so it can be tested
/// with injected monitor data.
fn control_bar_pos_geom(region: Option<RegionPx>, monitors: &[MonGeom]) -> Pos2 {
    // No monitor data at all (enumeration failed / headless): best-effort.
    if monitors.is_empty() {
        return match region {
            Some(r) => Pos2::new(r.x as f32, (r.y + r.height as i32) as f32 + BAR_GAP),
            None => Pos2::new(40.0, 40.0),
        };
    }

    // The recorded monitor: the one under the region's centre; for a full-screen
    // (`region == None`) recording, the primary monitor.
    let recorded = region
        .and_then(|r| {
            let cx = r.x + r.width as i32 / 2;
            let cy = r.y + r.height as i32 / 2;
            monitors.iter().find(|m| m.contains(cx, cy))
        })
        .or_else(|| monitors.iter().find(|m| m.is_primary))
        .or_else(|| monitors.first())
        .copied()
        .expect("monitors is non-empty");

    // Region recording with room below on its own monitor's work area: the
    // well-liked default, just below the region and left-aligned.
    if let Some(r) = region {
        let s = recorded.scale();
        let region_bottom = (r.y + r.height as i32) as f32 / s;
        let (_, _, _, work_bottom) = recorded.work_pt();
        if region_bottom + BAR_GAP + BAR_SIZE.y <= work_bottom {
            let mut pos = Pos2::new(r.x as f32 / s, region_bottom + BAR_GAP);
            clamp_to_work(&mut pos, &recorded);
            return pos;
        }
    }

    // Otherwise dock off the recorded content: a second monitor if present, else
    // the bottom-centre of the recorded monitor's own work area.
    let host = monitors
        .iter()
        .find(|m| !m.same_as(&recorded))
        .copied()
        .unwrap_or(recorded);

    let mut pos = bottom_center(&host);
    clamp_to_work(&mut pos, &host);
    pos
}

/// Bottom-centre of a monitor's work area (just above the taskbar), in points.
fn bottom_center(m: &MonGeom) -> Pos2 {
    let (wl, _, wr, wb) = m.work_pt();
    let x = wl + ((wr - wl) - BAR_SIZE.x) / 2.0;
    let y = wb - BAR_SIZE.y - BAR_GAP;
    Pos2::new(x, y)
}

/// Clamp a position so the whole bar stays inside `m`'s work area.
fn clamp_to_work(pos: &mut Pos2, m: &MonGeom) {
    let (wl, wt, wr, wb) = m.work_pt();
    pos.x = pos.x.clamp(wl, (wr - BAR_SIZE.x).max(wl));
    pos.y = pos.y.clamp(wt, (wb - BAR_SIZE.y).max(wt));
}

/// Enumerate monitors with their Win32 work areas (physical pixels). Empty when
/// enumeration fails (headless / non-Windows), which the caller handles.
#[cfg(windows)]
fn gather_monitor_geom(fallback_scale: f32) -> Vec<MonGeom> {
    let Ok(monitors) = skrino_capture::list_monitors() else {
        return Vec::new();
    };
    monitors
        .into_iter()
        .map(|m| {
            let full = (m.x, m.y, m.width as i32, m.height as i32);
            let (wx, wy, ww, wh) = work_area_px(m.x, m.y, m.width as i32, m.height as i32)
                .unwrap_or(full);
            MonGeom {
                x: m.x,
                y: m.y,
                w: m.width as i32,
                h: m.height as i32,
                wx,
                wy,
                ww,
                wh,
                scale: if m.scale_factor > 0.0 {
                    m.scale_factor
                } else {
                    fallback_scale
                },
                is_primary: m.is_primary,
            }
        })
        .collect()
}

#[cfg(not(windows))]
fn gather_monitor_geom(_fallback_scale: f32) -> Vec<MonGeom> {
    Vec::new()
}

/// The work area (taskbar excluded) of the monitor containing the centre of the
/// given physical-pixel rect, via `GetMonitorInfoW`'s `rcWork`.
#[cfg(windows)]
fn work_area_px(x: i32, y: i32, w: i32, h: i32) -> Option<(i32, i32, i32, i32)> {
    use winapi::shared::windef::POINT;
    use winapi::um::winuser::{
        GetMonitorInfoW, MONITORINFO, MONITOR_DEFAULTTONULL, MonitorFromPoint,
    };
    let pt = POINT {
        x: x + w / 2,
        y: y + h / 2,
    };
    unsafe {
        let hmon = MonitorFromPoint(pt, MONITOR_DEFAULTTONULL);
        if hmon.is_null() {
            return None;
        }
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(hmon, &mut mi) == 0 {
            return None;
        }
        let r = mi.rcWork;
        Some((r.left, r.top, r.right - r.left, r.bottom - r.top))
    }
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

// --- layered-window translucency helpers ---

/// The layered alpha for a given hover state.
#[cfg(windows)]
fn alpha_for(hovered: bool) -> u8 {
    if hovered {
        BAR_ALPHA_HOVER
    } else {
        BAR_ALPHA_IDLE
    }
}

/// Add `WS_EX_LAYERED` to the bar window's extended style once (OR-ed in, so it
/// never disturbs the existing ex-styles or the capture affinity).
#[cfg(windows)]
fn ensure_layered(hwnd: WindowId) {
    use winapi::um::winuser::{GWL_EXSTYLE, GetWindowLongPtrW, SetWindowLongPtrW, WS_EX_LAYERED};
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        if ex & WS_EX_LAYERED as isize == 0 {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | WS_EX_LAYERED as isize);
        }
    }
}

/// Push a whole-window alpha via `LWA_ALPHA` (glow-window friendly).
#[cfg(windows)]
fn set_bar_alpha(hwnd: WindowId, alpha: u8) {
    use winapi::um::winuser::{LWA_ALPHA, SetLayeredWindowAttributes};
    unsafe {
        SetLayeredWindowAttributes(hwnd, 0, alpha, LWA_ALPHA);
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

    /// A single 1920x1080 primary monitor with a 40px bottom taskbar.
    fn primary_1080() -> MonGeom {
        MonGeom {
            x: 0,
            y: 0,
            w: 1920,
            h: 1080,
            wx: 0,
            wy: 0,
            ww: 1920,
            wh: 1040,
            scale: 1.0,
            is_primary: true,
        }
    }

    /// A second 1920x1080 monitor to the right, no taskbar (full work area).
    fn secondary_right() -> MonGeom {
        MonGeom {
            x: 1920,
            y: 0,
            w: 1920,
            h: 1080,
            wx: 1920,
            wy: 0,
            ww: 1920,
            wh: 1080,
            scale: 1.0,
            is_primary: false,
        }
    }

    #[test]
    fn below_region_when_room() {
        // A small region high on the monitor: the bar sits just below it.
        let region = RegionPx {
            x: 100,
            y: 100,
            width: 400,
            height: 200,
        };
        let pos = control_bar_pos_geom(Some(region), &[primary_1080()]);
        assert_eq!(pos.x, 100.0);
        assert_eq!(pos.y, 300.0 + BAR_GAP);
    }

    #[test]
    fn below_region_kept_even_with_second_monitor() {
        // Room below on the region's own monitor wins over moving to monitor 2.
        let region = RegionPx {
            x: 100,
            y: 100,
            width: 400,
            height: 200,
        };
        let pos = control_bar_pos_geom(Some(region), &[primary_1080(), secondary_right()]);
        assert_eq!(pos.x, 100.0);
        assert_eq!(pos.y, 300.0 + BAR_GAP);
    }

    #[test]
    fn full_screen_single_monitor_docks_bottom_center() {
        // Full-screen (no region), single monitor: bottom-centre of work area,
        // clear of the taskbar.
        let pos = control_bar_pos_geom(None, &[primary_1080()]);
        assert_eq!(pos.x, (1920.0 - BAR_SIZE.x) / 2.0);
        assert_eq!(pos.y, 1040.0 - BAR_SIZE.y - BAR_GAP);
        // Wholly inside the work area (above the taskbar at y=1040).
        assert!(pos.y + BAR_SIZE.y <= 1040.0);
    }

    #[test]
    fn region_hugging_bottom_docks_bottom_center() {
        // A region whose bottom reaches the taskbar leaves no room below, so on a
        // single monitor the bar docks to the bottom-centre of the work area.
        let region = RegionPx {
            x: 100,
            y: 1000,
            width: 400,
            height: 40,
        };
        let pos = control_bar_pos_geom(Some(region), &[primary_1080()]);
        assert_eq!(pos.x, (1920.0 - BAR_SIZE.x) / 2.0);
        assert_eq!(pos.y, 1040.0 - BAR_SIZE.y - BAR_GAP);
    }

    #[test]
    fn full_screen_moves_to_second_monitor() {
        // Full-screen region on the primary: with a second monitor present the
        // bar lands entirely on that other monitor's work area.
        let region = RegionPx {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let pos = control_bar_pos_geom(Some(region), &[primary_1080(), secondary_right()]);
        // Bottom-centre of the second monitor, off the recorded (primary) screen.
        assert!(pos.x >= 1920.0, "bar should be on the second monitor");
        assert_eq!(pos.x, 1920.0 + (1920.0 - BAR_SIZE.x) / 2.0);
        assert_eq!(pos.y, 1080.0 - BAR_SIZE.y - BAR_GAP);
    }

    #[test]
    fn no_monitor_data_falls_back() {
        // Region: below the region; full-screen: the modest default corner.
        let region = RegionPx {
            x: 100,
            y: 100,
            width: 400,
            height: 200,
        };
        let pos = control_bar_pos_geom(Some(region), &[]);
        assert_eq!(pos, Pos2::new(100.0, 300.0 + BAR_GAP));
        assert_eq!(control_bar_pos_geom(None, &[]), Pos2::new(40.0, 40.0));
    }
}
