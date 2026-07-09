//! `SkrinoApp`: the eframe application. Owns config, tray, hotkeys and the
//! current state machine, and pumps tray/hotkey events inside the update loop.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Duration;

use egui::{Color32, CornerRadius, FontId, Pos2, RichText, Sense, Stroke, Vec2, ViewportCommand};
use egui_phosphor::regular as ph;
use global_hotkey::{GlobalHotKeyEvent, HotKeyState};
use image::RgbaImage;
use skrino_core::render::{RenderOptions, render_document};
use tray_icon::menu::MenuEvent;

use crate::config::{AppConfig, ImageFormat};
use crate::editor::{EditorSignal, EditorState};
use crate::hotkey::HotkeyRegistration;
use crate::overlay::{OverlayOutcome, OverlayState};
use crate::settings_ui::{SettingsResult, SettingsWindow};
use crate::share::{ShareHandle, ShareResult};
use crate::theme::{self, Palette, Theme};
use crate::toast::{ToastAction, Toasts};
use crate::tray::{Tray, TrayCommand};

/// Editor window default size.
const EDITOR_SIZE: Vec2 = Vec2::new(1120.0, 780.0);

pub enum AppState {
    /// Small launcher window.
    Start,
    /// No visible window; driven from the tray.
    Hidden,
    /// Region-selection overlay in its own viewport.
    Overlay(Box<OverlayState>),
    /// Main editor.
    Editor(Box<EditorState>),
}

pub struct SkrinoApp {
    config: AppConfig,
    applied_theme: Option<Theme>,
    tray: Option<Tray>,
    hotkeys: Option<HotkeyRegistration>,
    toasts: Toasts,
    state: AppState,
    settings: SettingsWindow,
    share: Option<ShareHandle>,
    /// Last share payload, kept so "Повторить" can re-upload.
    pending_share: Option<(skrino_upload::UploadConfig, String, Vec<u8>)>,
    /// True when launched normally (return to Start rather than Hidden).
    home_start: bool,
}

impl SkrinoApp {
    pub fn new(cc: &eframe::CreationContext<'_>, config: AppConfig, start_hidden: bool) -> Self {
        cc.egui_ctx.set_fonts(theme::build_fonts());
        theme::apply(&cc.egui_ctx, config.theme);

        let tray = match Tray::new() {
            Ok(t) => Some(t),
            Err(e) => {
                log::error!("tray init failed: {e}");
                None
            }
        };

        let mut hotkeys = match HotkeyRegistration::new() {
            Ok(h) => Some(h),
            Err(e) => {
                log::error!("hotkey manager init failed: {e}");
                None
            }
        };
        if let Some(h) = &mut hotkeys
            && let Err(e) = h.set(&config.hotkey) {
                log::warn!("hotkey register failed: {e}");
            }

        let applied_theme = Some(config.theme);
        let state = if start_hidden {
            AppState::Hidden
        } else {
            AppState::Start
        };

        Self {
            config,
            applied_theme,
            tray,
            hotkeys,
            toasts: Toasts::default(),
            state,
            settings: SettingsWindow::default(),
            share: None,
            pending_share: None,
            home_start: !start_hidden,
        }
    }

    fn home(&self) -> AppState {
        if self.home_start {
            AppState::Start
        } else {
            AppState::Hidden
        }
    }

    // --- event pumping ---

    fn pump_events(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            if let Some(cmd) = self.tray.as_ref().and_then(|t| t.command_for(&ev.id)) {
                self.handle_tray(cmd, ctx);
            }
        }

        let current_id = self.hotkeys.as_ref().and_then(|h| h.current_id());
        while let Ok(ev) = GlobalHotKeyEvent::receiver().try_recv() {
            if ev.state == HotKeyState::Pressed && Some(ev.id) == current_id && self.is_idle() {
                self.state = self.begin_region_capture(ctx, self.home());
            }
        }
    }

    /// Only start a new capture when not already capturing/editing.
    fn is_idle(&self) -> bool {
        matches!(self.state, AppState::Start | AppState::Hidden)
    }

    fn handle_tray(&mut self, cmd: TrayCommand, ctx: &egui::Context) {
        match cmd {
            TrayCommand::CaptureRegion => {
                if self.is_idle() {
                    self.state = self.begin_region_capture(ctx, self.home());
                }
            }
            TrayCommand::CaptureFull => {
                if self.is_idle() {
                    self.state = self.begin_full_capture(ctx, self.home());
                }
            }
            TrayCommand::OpenFile => {
                if self.is_idle() {
                    self.state = self.begin_open_file(ctx, self.home());
                }
            }
            TrayCommand::Settings => self.settings.open = true,
            TrayCommand::Quit => {
                self.config.save();
                std::process::exit(0);
            }
        }
    }

    // --- capture / open entry points ---

    fn begin_region_capture(&mut self, _ctx: &egui::Context, fallback: AppState) -> AppState {
        match catch_unwind(AssertUnwindSafe(skrino_capture::capture_virtual_screen)) {
            Ok(Ok(cap)) => AppState::Overlay(Box::new(OverlayState::new(cap))),
            Ok(Err(e)) => {
                self.toasts.error(format!("Не удалось сделать снимок: {e}"));
                fallback
            }
            Err(_) => {
                self.toasts.error("Модуль захвата ещё не готов");
                fallback
            }
        }
    }

    fn begin_full_capture(&mut self, ctx: &egui::Context, fallback: AppState) -> AppState {
        match catch_unwind(AssertUnwindSafe(skrino_capture::capture_virtual_screen)) {
            Ok(Ok(cap)) => {
                open_editor_window(ctx);
                AppState::Editor(Box::new(EditorState::new(cap.image)))
            }
            Ok(Err(e)) => {
                self.toasts.error(format!("Не удалось сделать снимок: {e}"));
                fallback
            }
            Err(_) => {
                self.toasts.error("Модуль захвата ещё не готов");
                fallback
            }
        }
    }

    fn begin_open_file(&mut self, ctx: &egui::Context, fallback: AppState) -> AppState {
        let file = rfd::FileDialog::new()
            .set_title("Открыть изображение")
            .add_filter("Изображения", &["png", "jpg", "jpeg", "bmp", "gif", "webp"])
            .pick_file();
        let Some(path) = file else {
            return fallback;
        };
        match image::open(&path) {
            Ok(img) => {
                open_editor_window(ctx);
                AppState::Editor(Box::new(EditorState::new(img.to_rgba8())))
            }
            Err(e) => {
                self.toasts.error(format!("Не удалось открыть файл: {e}"));
                fallback
            }
        }
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
        let (w, h) = (img.width() as usize, img.height() as usize);
        let data = arboard::ImageData {
            width: w,
            height: h,
            bytes: std::borrow::Cow::Owned(img.into_raw()),
        };
        match arboard::Clipboard::new().and_then(|mut cb| cb.set_image(data)) {
            Ok(()) => self.toasts.success("Скопировано"),
            Err(e) => self.toasts.error(format!("Буфер обмена: {e}")),
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

    fn do_share(&mut self, ed: &EditorState) {
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
        if res.hotkey_changed
            && let Some(h) = &mut self.hotkeys
                && let Err(e) = h.set(&self.config.hotkey) {
                    self.toasts.error(format!("Горячая клавиша: {e}"));
                }
        if res.autostart_changed
            && let Err(e) = crate::autostart::set_autostart(self.config.autostart) {
                self.toasts.error(format!("Автозапуск: {e}"));
            }
        if res.dirty {
            self.config.configured = true;
            self.config.save();
        }
    }

    // --- main content dispatch ---

    fn draw_main(&mut self, ctx: &egui::Context, palette: &Palette) {
        let mut state = std::mem::replace(&mut self.state, AppState::Hidden);
        let mut next: Option<AppState> = None;

        match &mut state {
            AppState::Hidden => {
                if self.settings.open {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        ui.centered_and_justified(|ui| {
                            ui.label(RichText::new("Skrino").font(theme::heading_font(22.0)));
                        });
                    });
                }
            }
            AppState::Start => match draw_start(ctx, palette) {
                StartSignal::Region => next = Some(self.begin_region_capture(ctx, AppState::Start)),
                StartSignal::Full => next = Some(self.begin_full_capture(ctx, AppState::Start)),
                StartSignal::Open => next = Some(self.begin_open_file(ctx, AppState::Start)),
                StartSignal::Settings => self.settings.open = true,
                StartSignal::None => {}
            },
            AppState::Editor(ed) => {
                let sharing = self.share.as_ref().is_some_and(|h| h.in_flight());
                let configured = self.config.upload.is_configured();
                let signal = ed.ui(ctx, palette, configured, sharing);
                match signal {
                    EditorSignal::Close => next = Some(self.home()),
                    EditorSignal::Copy => self.do_copy(ed),
                    EditorSignal::Save => self.do_save(ed),
                    EditorSignal::Share => self.do_share(ed),
                    EditorSignal::None => {}
                }
            }
            AppState::Overlay(ov) => match ov.run(ctx, palette) {
                OverlayOutcome::Confirmed(img) => {
                    open_editor_window(ctx);
                    next = Some(AppState::Editor(Box::new(EditorState::new(img))));
                }
                OverlayOutcome::Cancelled => next = Some(self.home()),
                OverlayOutcome::Pending => {}
            },
        }

        self.state = next.unwrap_or(state);
    }

    fn handle_close_request(&mut self, ctx: &egui::Context) {
        if !ctx.input(|i| i.viewport().close_requested()) {
            return;
        }
        // A tray app hides instead of quitting when its window is closed.
        if self.tray.is_some() {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            self.settings.open = false;
            self.state = AppState::Hidden;
        } else {
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
        self.pump_events(ctx);

        if self.applied_theme != Some(self.config.theme) {
            theme::apply(ctx, self.config.theme);
            self.applied_theme = Some(self.config.theme);
        }
        let palette = Palette::for_theme(self.config.theme);

        // Root-window visibility follows the state (overlay lives in its own
        // viewport, so the root hides during selection).
        let want_visible = self.settings.open
            || matches!(self.state, AppState::Start | AppState::Editor(_));
        ctx.send_viewport_cmd(ViewportCommand::Visible(want_visible));

        self.draw_main(ctx, &palette);

        if self.settings.open {
            let res = self.settings.show(ctx, &mut self.config, &palette);
            self.handle_settings_result(res);
        }

        match self.toasts.show(ctx, &palette) {
            ToastAction::OpenUrl(url) => {
                let _ = webbrowser::open(&url);
            }
            ToastAction::Retry => self.retry_share(),
            ToastAction::None => {}
        }

        self.poll_share(ctx);
        self.handle_close_request(ctx);

        // Heartbeat: keep pumping tray/hotkey events even when idle.
        ctx.request_repaint_after(Duration::from_millis(100));
    }
}

// --- start window ---

enum StartSignal {
    None,
    Region,
    Full,
    Open,
    Settings,
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

fn open_editor_window(ctx: &egui::Context) {
    ctx.send_viewport_cmd(ViewportCommand::InnerSize(EDITOR_SIZE));
    ctx.send_viewport_cmd(ViewportCommand::Visible(true));
    ctx.send_viewport_cmd(ViewportCommand::Focus);
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
