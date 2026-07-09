//! `SkrinoApp`: the eframe UI application. A single UI process is launched in one
//! of several one-shot modes (see [`LaunchMode`]); it opens straight to the point,
//! does its job, and exits when its window closes. The tray icon and global
//! hotkey do NOT live here — they run in the separate `--tray` daemon (see
//! [`crate::daemon`]).
//!
//! The region-selection overlay is drawn into the ROOT viewport, which the app
//! reshapes into a borderless, always-on-top fullscreen surface on entry and
//! restores on exit. Window reshaping is driven off state transitions, never
//! per-frame, to avoid fighting the OS (the previous freeze).

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::time::Duration;

use egui::{
    Color32, CornerRadius, FontId, Pos2, RichText, Sense, Stroke, Vec2, ViewportCommand,
    WindowLevel,
};
use egui_phosphor::regular as ph;
use image::RgbaImage;
use skrino_capture::{MonitorInfo, VirtualScreenCapture};
use skrino_core::render::{RenderOptions, render_document};

use crate::config::{AppConfig, ImageFormat, ShareDestination};
use crate::editor::{EditorSignal, EditorState};
use crate::overlay::{OverlayOutcome, OverlayState};
use crate::settings_ui::{SettingsResult, SettingsWindow};
use crate::share::{ShareHandle, ShareResult};
use crate::theme::{self, Palette, Theme};
use crate::toast::{ToastAction, Toasts};

/// Start-window size.
pub const START_SIZE: Vec2 = Vec2::new(420.0, 356.0);
/// Editor window default size.
const EDITOR_SIZE: Vec2 = Vec2::new(1120.0, 780.0);
/// Settings-window host size (fits the settings dialog).
const SETTINGS_SIZE: Vec2 = Vec2::new(540.0, 620.0);

/// How the UI process was launched. Each mode is one-shot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchMode {
    /// Interactive launcher window; returns here after each job.
    Start,
    /// Capture + region overlay, straight to the editor.
    CaptureRegion,
    /// Capture the full screen straight to the editor.
    CaptureFull,
    /// File-open dialog straight to the editor.
    OpenFile,
    /// Settings window alone.
    Settings,
    /// Region overlay that auto-cancels after 3s then exits (safe overlay test).
    OverlaySmoke,
}

/// Current UI state.
enum AppState {
    /// First frame not yet dispatched.
    Boot,
    /// Small launcher window.
    Start,
    /// Region-selection overlay drawn into the (reshaped) root viewport.
    Overlay(Box<OverlayState>),
    /// Main editor.
    Editor(Box<EditorState>),
    /// Settings-only launch: nothing behind the settings window.
    SettingsOnly,
}

/// Desired OS window configuration for the current state. Applied only when it
/// changes from the previously applied mode.
#[derive(Clone, PartialEq)]
enum WindowMode {
    Hidden,
    Start,
    Editor,
    Settings,
    Overlay { pos: Pos2, size: Vec2 },
}

pub struct SkrinoApp {
    config: AppConfig,
    launch: LaunchMode,
    applied_theme: Option<Theme>,
    applied_window: Option<WindowMode>,
    toasts: Toasts,
    state: AppState,
    settings: SettingsWindow,
    share: Option<ShareHandle>,
    /// Last server-share payload, kept so "Повторить" can re-upload.
    pending_share: Option<(skrino_upload::UploadConfig, String, Vec<u8>)>,
}

impl SkrinoApp {
    pub fn new(cc: &eframe::CreationContext<'_>, config: AppConfig, launch: LaunchMode) -> Self {
        cc.egui_ctx.set_fonts(theme::build_fonts());
        theme::apply(&cc.egui_ctx, config.theme);
        let applied_theme = Some(config.theme);

        Self {
            config,
            launch,
            applied_theme,
            applied_window: None,
            toasts: Toasts::default(),
            state: AppState::Boot,
            settings: SettingsWindow::default(),
            share: None,
            pending_share: None,
        }
    }

    fn is_interactive(&self) -> bool {
        matches!(self.launch, LaunchMode::Start)
    }

    /// Save config and terminate the process. Used when a one-shot job finishes.
    fn quit(&self) -> ! {
        self.config.save();
        std::process::exit(0);
    }

    /// Job finished: interactive launch returns to Start, one-shot launches exit.
    fn finish(&mut self) -> AppState {
        if self.is_interactive() {
            AppState::Start
        } else {
            self.quit();
        }
    }

    /// First-frame dispatch based on the launch mode.
    fn dispatch_launch(&mut self) {
        self.state = match self.launch {
            LaunchMode::Start => AppState::Start,
            LaunchMode::CaptureRegion => self.begin_region_capture(false),
            LaunchMode::OverlaySmoke => self.begin_region_capture(true),
            LaunchMode::CaptureFull => self.begin_full_capture(),
            LaunchMode::OpenFile => self.begin_open_file(),
            LaunchMode::Settings => {
                self.settings.open = true;
                AppState::SettingsOnly
            }
        };
    }

    // --- capture / open entry points ---

    fn begin_region_capture(&mut self, smoke: bool) -> AppState {
        match catch_unwind(AssertUnwindSafe(skrino_capture::capture_virtual_screen)) {
            Ok(Ok(cap)) => match build_overlay(cap, smoke) {
                Some(ov) => AppState::Overlay(Box::new(ov)),
                None => {
                    self.toasts.error("Не удалось подготовить область выделения");
                    self.finish()
                }
            },
            Ok(Err(e)) => {
                self.toasts.error(format!("Не удалось сделать снимок: {e}"));
                self.finish()
            }
            Err(_) => {
                self.toasts.error("Модуль захвата ещё не готов");
                self.finish()
            }
        }
    }

    fn begin_full_capture(&mut self) -> AppState {
        match catch_unwind(AssertUnwindSafe(skrino_capture::capture_virtual_screen)) {
            Ok(Ok(cap)) => AppState::Editor(Box::new(EditorState::new(cap.image))),
            Ok(Err(e)) => {
                self.toasts.error(format!("Не удалось сделать снимок: {e}"));
                self.finish()
            }
            Err(_) => {
                self.toasts.error("Модуль захвата ещё не готов");
                self.finish()
            }
        }
    }

    fn begin_open_file(&mut self) -> AppState {
        let file = rfd::FileDialog::new()
            .set_title("Открыть изображение")
            .add_filter("Изображения", &["png", "jpg", "jpeg", "bmp", "gif", "webp"])
            .pick_file();
        let Some(path) = file else {
            // Cancel = job done.
            return self.finish();
        };
        match image::open(&path) {
            Ok(img) => AppState::Editor(Box::new(EditorState::new(img.to_rgba8()))),
            Err(e) => {
                self.toasts.error(format!("Не удалось открыть файл: {e}"));
                self.finish()
            }
        }
    }

    /// Spawn a background tray daemon (`--tray`) and exit this UI process.
    fn go_background(&self) -> ! {
        if let Ok(exe) = std::env::current_exe() {
            let _ = std::process::Command::new(exe).arg("--tray").spawn();
        }
        self.quit();
    }

    // --- editor actions ---

    fn render(&mut self, ed: &EditorState) -> Option<RgbaImage> {
        let opts = RenderOptions {
            font_data: theme::INTER_REGULAR_BYTES,
        };
        match catch_unwind(AssertUnwindSafe(|| render_document(&ed.doc, &opts))) {
            Ok(img) => Some(img),
            Err(_) => {
                self.toasts.error("Модуль отрисовки ещё не готов");
                None
            }
        }
    }

    fn do_copy(&mut self, ed: &EditorState) {
        let Some(img) = self.render(ed) else { return };
        if let Err(e) = copy_image_to_clipboard(&img) {
            self.toasts.error(format!("Буфер обмена: {e}"));
        } else {
            self.toasts.success("Скопировано");
        }
    }

    fn do_save(&mut self, ed: &EditorState) {
        let Some(img) = self.render(ed) else { return };
        let ext = self.config.format.extension();
        let default_name = match catch_unwind(|| skrino_upload::generate_filename(ext)) {
            Ok(name) => name,
            Err(_) => format!("skrino.{ext}"),
        };
        let dir = self.config.default_save_dir();
        let _ = std::fs::create_dir_all(&dir);

        let path = rfd::FileDialog::new()
            .set_title("Сохранить скриншот")
            .set_directory(&dir)
            .set_file_name(&default_name)
            .add_filter("Изображение", &[ext])
            .save_file();
        let Some(path) = path else { return };

        let bytes = match encode_image(&img, self.config.format, self.config.jpeg_quality) {
            Ok(b) => b,
            Err(e) => {
                self.toasts.error(format!("Кодирование: {e}"));
                return;
            }
        };
        match std::fs::write(&path, bytes) {
            Ok(()) => {
                if let Some(parent) = path.parent() {
                    self.config.last_save_dir = Some(parent.to_path_buf());
                    self.config.save();
                }
                self.toasts.success("Сохранено");
            }
            Err(e) => self.toasts.error(format!("Не удалось сохранить: {e}")),
        }
    }

    /// «Поделиться»: local folder or server, per the configured destination.
    fn do_share(&mut self, ed: &EditorState) {
        match self.config.share_dest.clone() {
            ShareDestination::LocalDir { path } => self.share_local(ed, &path),
            ShareDestination::Server => self.share_server(ed),
        }
    }

    fn share_local(&mut self, ed: &EditorState, dir: &Path) {
        let Some(img) = self.render(ed) else { return };
        let ext = self.config.format.extension();
        let filename = match catch_unwind(|| skrino_upload::generate_filename(ext)) {
            Ok(name) => name,
            Err(_) => format!("skrino.{ext}"),
        };
        let bytes = match encode_image(&img, self.config.format, self.config.jpeg_quality) {
            Ok(b) => b,
            Err(e) => {
                self.toasts.error(format!("Кодирование: {e}"));
                return;
            }
        };
        if let Err(e) = std::fs::create_dir_all(dir) {
            self.toasts.error(format!("Не удалось создать папку: {e}"));
            return;
        }
        let path = dir.join(&filename);
        if let Err(e) = std::fs::write(&path, &bytes) {
            self.toasts.error(format!("Не удалось сохранить: {e}"));
            return;
        }
        // Copy the rendered image (not a link) to the clipboard.
        let _ = copy_image_to_clipboard(&img);
        self.toasts
            .saved(format!("Сохранено: {}", path.display()), path);
    }

    fn share_server(&mut self, ed: &EditorState) {
        if self.share.as_ref().is_some_and(|h| h.in_flight()) {
            return;
        }
        let Some(config) = self.config.upload.to_upload_config() else {
            self.toasts
                .error("Настройте загрузку в параметрах, чтобы делиться ссылкой");
            return;
        };
        let Some(img) = self.render(ed) else { return };
        let filename = match catch_unwind(|| skrino_upload::generate_filename("png")) {
            Ok(name) => name,
            Err(_) => "skrino.png".to_string(),
        };
        let bytes = match encode_image(&img, ImageFormat::Png, 100) {
            Ok(b) => b,
            Err(e) => {
                self.toasts.error(format!("Кодирование: {e}"));
                return;
            }
        };
        self.pending_share = Some((config.clone(), filename.clone(), bytes.clone()));
        self.share = Some(ShareHandle::spawn(
            config,
            filename,
            bytes,
            AppConfig::fallback_dir(),
        ));
        self.toasts.info("Отправка…");
    }

    fn retry_share(&mut self) {
        if self.share.as_ref().is_some_and(|h| h.in_flight()) {
            return;
        }
        let Some((config, filename, bytes)) = self.pending_share.clone() else {
            return;
        };
        self.share = Some(ShareHandle::spawn(
            config,
            filename,
            bytes,
            AppConfig::fallback_dir(),
        ));
        self.toasts.info("Повторная отправка…");
    }

    fn poll_share(&mut self, ctx: &egui::Context) {
        let Some(handle) = &mut self.share else { return };
        if handle.in_flight() {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
        if let Some(res) = handle.poll() {
            match res {
                ShareResult::Success(url) => {
                    match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(url.clone())) {
                        Ok(()) => self.toasts.link(format!("Ссылка скопирована: {url}"), url),
                        Err(_) => self.toasts.link(format!("Готово: {url}"), url),
                    }
                }
                ShareResult::Failure { error, saved_to } => {
                    let extra = saved_to
                        .map(|p| format!(" • сохранено локально: {}", p.display()))
                        .unwrap_or_default();
                    self.toasts
                        .error_retry(format!("Не удалось отправить: {error}{extra}"));
                }
            }
            self.share = None;
        }
    }

    // --- settings side-effects ---

    fn handle_settings_result(&mut self, res: SettingsResult) {
        if res.dirty {
            self.config.configured = true;
            self.config.save();
        }
        // The daemon owns the global hotkey; ask it to reload after we persisted.
        if res.hotkey_changed {
            crate::daemon::reload_if_running();
        }
        if res.autostart_changed
            && let Err(e) = crate::autostart::set_autostart(self.config.autostart)
        {
            self.toasts.error(format!("Автозапуск: {e}"));
        }
    }

    // --- window mode ---

    fn window_mode_for_state(&self) -> WindowMode {
        match &self.state {
            AppState::Boot => WindowMode::Hidden,
            AppState::Overlay(ov) => WindowMode::Overlay {
                pos: ov.window_pos(),
                size: ov.window_size(),
            },
            AppState::Editor(_) => WindowMode::Editor,
            AppState::SettingsOnly => WindowMode::Settings,
            AppState::Start => {
                if self.settings.open {
                    WindowMode::Settings
                } else {
                    WindowMode::Start
                }
            }
        }
    }

    fn apply_window_mode(ctx: &egui::Context, mode: &WindowMode) {
        let send = |c| ctx.send_viewport_cmd(c);
        match mode {
            WindowMode::Hidden => send(ViewportCommand::Visible(false)),
            WindowMode::Start => windowed(ctx, START_SIZE),
            WindowMode::Editor => windowed(ctx, EDITOR_SIZE),
            WindowMode::Settings => windowed(ctx, SETTINGS_SIZE),
            WindowMode::Overlay { pos, size } => {
                send(ViewportCommand::Decorations(false));
                send(ViewportCommand::Resizable(false));
                send(ViewportCommand::WindowLevel(WindowLevel::AlwaysOnTop));
                send(ViewportCommand::OuterPosition(*pos));
                send(ViewportCommand::InnerSize(*size));
                send(ViewportCommand::Visible(true));
                send(ViewportCommand::Focus);
            }
        }
    }

    // --- main content dispatch ---

    fn draw_main(&mut self, ctx: &egui::Context, palette: &Palette) {
        let mut state = std::mem::replace(&mut self.state, AppState::Boot);
        let mut next: Option<AppState> = None;

        match &mut state {
            AppState::Boot => {}
            AppState::SettingsOnly => {
                egui::CentralPanel::default()
                    .frame(egui::Frame::new().fill(palette.window))
                    .show(ctx, |_ui| {});
            }
            AppState::Start => match draw_start(ctx, palette) {
                StartSignal::Region => next = Some(self.begin_region_capture(false)),
                StartSignal::Full => next = Some(self.begin_full_capture()),
                StartSignal::Open => next = Some(self.begin_open_file()),
                StartSignal::Settings => self.settings.open = true,
                StartSignal::Background => self.go_background(),
                StartSignal::None => {}
            },
            AppState::Editor(ed) => {
                let sharing = self.share.as_ref().is_some_and(|h| h.in_flight());
                let signal = ed.ui(ctx, palette, sharing);
                match signal {
                    EditorSignal::Close => next = Some(self.finish()),
                    EditorSignal::Copy => self.do_copy(ed),
                    EditorSignal::Save => self.do_save(ed),
                    EditorSignal::Share => self.do_share(ed),
                    EditorSignal::None => {}
                }
            }
            AppState::Overlay(ov) => match ov.run(ctx, palette) {
                OverlayOutcome::Confirmed(img) => {
                    next = Some(AppState::Editor(Box::new(EditorState::new(img))));
                }
                OverlayOutcome::Cancelled => next = Some(self.finish()),
                OverlayOutcome::Pending => {}
            },
        }

        self.state = next.unwrap_or(state);
    }

    fn handle_close_request(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.viewport().close_requested()) {
            // Closing the window ends the process (no tray in the UI process).
            self.config.save();
        }
    }
}

impl eframe::App for SkrinoApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        Palette::for_theme(self.config.theme)
            .window
            .to_normalized_gamma_f32()
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if matches!(self.state, AppState::Boot) {
            self.dispatch_launch();
        }

        if self.applied_theme != Some(self.config.theme) {
            theme::apply(ctx, self.config.theme);
            self.applied_theme = Some(self.config.theme);
        }
        let palette = Palette::for_theme(self.config.theme);

        // Reshape the root window only on state transitions.
        let wanted = self.window_mode_for_state();
        if self.applied_window.as_ref() != Some(&wanted) {
            Self::apply_window_mode(ctx, &wanted);
            self.applied_window = Some(wanted);
        }

        self.draw_main(ctx, &palette);

        if self.settings.open {
            let res = self.settings.show(ctx, &mut self.config, &palette);
            self.handle_settings_result(res);
        }
        // Settings-only launch exits when its window closes.
        if matches!(self.launch, LaunchMode::Settings) && !self.settings.open {
            self.quit();
        }

        match self.toasts.show(ctx, &palette) {
            ToastAction::OpenUrl(url) => {
                let _ = webbrowser::open(&url);
            }
            ToastAction::RevealFile(path) => reveal_in_explorer(&path),
            ToastAction::Retry => self.retry_share(),
            ToastAction::None => {}
        }

        self.poll_share(ctx);
        self.handle_close_request(ctx);
    }
}

/// Restore standard windowed chrome at `size` (decorations, normal level, not
/// fullscreen, visible, focused).
fn windowed(ctx: &egui::Context, size: Vec2) {
    ctx.send_viewport_cmd(ViewportCommand::Fullscreen(false));
    ctx.send_viewport_cmd(ViewportCommand::Decorations(true));
    ctx.send_viewport_cmd(ViewportCommand::WindowLevel(WindowLevel::Normal));
    ctx.send_viewport_cmd(ViewportCommand::Resizable(true));
    ctx.send_viewport_cmd(ViewportCommand::InnerSize(size));
    ctx.send_viewport_cmd(ViewportCommand::Visible(true));
    ctx.send_viewport_cmd(ViewportCommand::Focus);
}

/// Build the overlay for the monitor under the cursor: crop that monitor's slice
/// out of the virtual-screen capture and compute the root-window geometry.
fn build_overlay(cap: VirtualScreenCapture, smoke: bool) -> Option<OverlayState> {
    let cursor = cursor_pos();
    let monitor = pick_monitor(&cap, cursor)?.clone();

    let scale = if monitor.scale_factor > 0.0 {
        monitor.scale_factor
    } else {
        1.0
    };

    // Offset of the monitor within the stitched capture image (physical px).
    let img_w = cap.image.width();
    let img_h = cap.image.height();
    let ox = (monitor.x - cap.origin_x).max(0) as u32;
    let oy = (monitor.y - cap.origin_y).max(0) as u32;
    let w = monitor.width.min(img_w.saturating_sub(ox));
    let h = monitor.height.min(img_h.saturating_sub(oy));
    if w == 0 || h == 0 {
        return None;
    }
    let slice = image::imageops::crop_imm(&cap.image, ox, oy, w, h).to_image();

    // Root-window geometry in logical points (physical / scale). egui multiplies
    // these by the window's pixels-per-point when applying the commands.
    let pos_pt = Pos2::new(monitor.x as f32 / scale, monitor.y as f32 / scale);
    let size_pt = Vec2::new(w as f32 / scale, h as f32 / scale);

    Some(OverlayState::new(slice, scale, pos_pt, size_pt, smoke))
}

/// Choose the monitor under the cursor, else the primary, else the first.
fn pick_monitor(cap: &VirtualScreenCapture, cursor: Option<(i32, i32)>) -> Option<&MonitorInfo> {
    if let Some((cx, cy)) = cursor
        && let Some(m) = cap.monitors.iter().find(|m| {
            cx >= m.x
                && cx < m.x + m.width as i32
                && cy >= m.y
                && cy < m.y + m.height as i32
        })
    {
        return Some(m);
    }
    cap.monitors
        .iter()
        .find(|m| m.is_primary)
        .or_else(|| cap.monitors.first())
}

/// Cursor position in physical virtual-screen coordinates.
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

fn copy_image_to_clipboard(img: &RgbaImage) -> Result<(), String> {
    let (w, h) = (img.width() as usize, img.height() as usize);
    let data = arboard::ImageData {
        width: w,
        height: h,
        bytes: std::borrow::Cow::Owned(img.clone().into_raw()),
    };
    arboard::Clipboard::new()
        .and_then(|mut cb| cb.set_image(data))
        .map_err(|e| e.to_string())
}

/// Open the system file manager with `path` selected.
fn reveal_in_explorer(path: &Path) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("explorer")
            .arg(format!("/select,{}", path.display()))
            .spawn();
    }
    #[cfg(not(windows))]
    {
        if let Some(parent) = path.parent() {
            let _ = webbrowser::open(&parent.display().to_string());
        }
    }
}

// --- start window ---

enum StartSignal {
    None,
    Region,
    Full,
    Open,
    Settings,
    Background,
}

fn draw_start(ctx: &egui::Context, palette: &Palette) -> StartSignal {
    let mut sig = StartSignal::None;
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(palette.window).inner_margin(egui::Margin::same(20)))
        .show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(10.0);
                ui.label(
                    RichText::new("Skrino")
                        .font(theme::heading_font(30.0))
                        .color(palette.text),
                );
                ui.label(
                    RichText::new("Быстрые скриншоты")
                        .size(13.0)
                        .color(palette.text_secondary),
                );
                ui.add_space(16.0);

                let (rect, _) = ui.allocate_exact_size(Vec2::new(240.0, 96.0), Sense::hover());
                draw_illustration(ui.painter(), rect, palette);

                ui.add_space(18.0);
                let cards_width = 3.0 * MODE_CARD_SIZE.x + 2.0 * 8.0;
                let pad = ((ui.available_width() - cards_width) / 2.0).max(0.0);
                ui.horizontal(|ui| {
                    ui.add_space(pad);
                    ui.spacing_mut().item_spacing.x = 8.0;
                    if mode_card(ui, palette, ph::SELECTION, "Область", true).clicked() {
                        sig = StartSignal::Region;
                    }
                    if mode_card(ui, palette, ph::MONITOR, "Весь экран", false).clicked() {
                        sig = StartSignal::Full;
                    }
                    if mode_card(ui, palette, ph::FOLDER_OPEN, "Открыть файл", false).clicked() {
                        sig = StartSignal::Open;
                    }
                });
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    let row = ui.available_width();
                    let btns = 260.0;
                    ui.add_space(((row - btns) / 2.0).max(0.0));
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new(format!("{}  Настройки", ph::GEAR_SIX))
                                    .color(palette.text_secondary)
                                    .size(13.0),
                            )
                            .frame(false),
                        )
                        .clicked()
                    {
                        sig = StartSignal::Settings;
                    }
                    ui.add_space(8.0);
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new(format!("{}  Работать в фоне", ph::TRAY))
                                    .color(palette.text_secondary)
                                    .size(13.0),
                            )
                            .frame(false),
                        )
                        .on_hover_text("Свернуть в трей: горячая клавиша и меню будут доступны")
                        .clicked()
                    {
                        sig = StartSignal::Background;
                    }
                });
            });
        });
    sig
}

const MODE_CARD_SIZE: Vec2 = Vec2::new(118.0, 74.0);

/// Capture-mode card: icon on top, label below.
fn mode_card(
    ui: &mut egui::Ui,
    palette: &Palette,
    icon: &str,
    label: &str,
    primary: bool,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(MODE_CARD_SIZE, Sense::click());
    let (fill, fg, stroke) = if primary {
        (
            if resp.hovered() { palette.accent_hover } else { palette.accent },
            Color32::WHITE,
            Stroke::NONE,
        )
    } else {
        (
            palette.surface,
            palette.text,
            Stroke::new(1.0, if resp.hovered() { palette.accent } else { palette.border }),
        )
    };
    let painter = ui.painter();
    painter.rect(rect, CornerRadius::same(9), fill, stroke, egui::StrokeKind::Inside);
    painter.text(
        rect.center() - Vec2::new(0.0, 12.0),
        egui::Align2::CENTER_CENTER,
        icon,
        egui::FontId::proportional(22.0),
        fg,
    );
    painter.text(
        rect.center() + Vec2::new(0.0, 16.0),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::proportional(13.0),
        fg,
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A stylised dashed selection marquee for the launcher.
fn draw_illustration(painter: &egui::Painter, area: egui::Rect, palette: &Palette) {
    let rect = egui::Rect::from_center_size(area.center(), Vec2::new(150.0, 78.0));
    let dash = 8.0;
    let gap = 5.0;
    let stroke = Stroke::new(2.0, palette.accent);

    let dashed = |a: Pos2, b: Pos2| {
        let len = (b - a).length();
        let dir = (b - a).normalized();
        let mut t = 0.0;
        while t < len {
            let s = a + dir * t;
            let e = a + dir * (t + dash).min(len);
            painter.line_segment([s, e], stroke);
            t += dash + gap;
        }
    };
    dashed(rect.left_top(), rect.right_top());
    dashed(rect.right_top(), rect.right_bottom());
    dashed(rect.right_bottom(), rect.left_bottom());
    dashed(rect.left_bottom(), rect.left_top());

    // Corner handles.
    for c in [
        rect.left_top(),
        rect.right_top(),
        rect.left_bottom(),
        rect.right_bottom(),
    ] {
        painter.rect_filled(
            egui::Rect::from_center_size(c, Vec2::splat(7.0)),
            CornerRadius::same(2),
            palette.accent,
        );
    }

    // Faint dimensions badge inside.
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        "1280 × 720",
        FontId::proportional(13.0),
        palette.text_secondary,
    );
}

/// Encode the rendered image to PNG or JPEG bytes.
fn encode_image(img: &RgbaImage, format: ImageFormat, quality: u8) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    let mut cursor = std::io::Cursor::new(&mut buf);
    match format {
        ImageFormat::Png => img
            .write_to(&mut cursor, image::ImageFormat::Png)
            .map_err(|e| e.to_string())?,
        ImageFormat::Jpeg => {
            let rgb = image::DynamicImage::ImageRgba8(img.clone()).to_rgb8();
            let mut enc =
                image::codecs::jpeg::JpegEncoder::new_with_quality(&mut cursor, quality.clamp(1, 100));
            enc.encode_image(&rgb).map_err(|e| e.to_string())?;
        }
    }
    Ok(buf)
}
