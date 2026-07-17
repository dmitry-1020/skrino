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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use egui::{CornerRadius, Pos2, RichText, Sense, Stroke, Vec2, ViewportCommand, WindowLevel};
use egui_phosphor::regular as ph;
use image::RgbaImage;
use skrino_capture::{MonitorInfo, VirtualScreenCapture};
use skrino_core::render::{RenderOptions, render_document};
use skrino_record::RegionPx;

use crate::config::{AppConfig, ImageFormat, ShareDestination};
use crate::editor::{EditorSignal, EditorState};
use crate::overlay::{OverlayOutcome, OverlayPurpose, OverlayState};
use crate::record::{self, RecordSession, RecordSignal, RecordingLock};
use crate::settings_ui::{SettingsResult, SettingsWindow};
use crate::share::{ShareHandle, ShareResult};
use crate::theme::{self, Palette, Theme};
use crate::toast::{ToastAction, Toasts};

/// Start-window size (tall enough for the recording row and first-run hint).
pub const START_SIZE: Vec2 = Vec2::new(340.0, 440.0);
/// Editor window default size.
const EDITOR_SIZE: Vec2 = Vec2::new(1120.0, 780.0);
/// Settings-window host size (full-window settings content, see item 5).
const SETTINGS_SIZE: Vec2 = Vec2::new(540.0, 640.0);

/// The Start-window hero image: rhino + "SKRINO" wordmark.
static HERO_PNG_BYTES: &[u8] = include_bytes!("../assets/skrino.png");
/// Displayed width of the hero image (logical points); height follows the
/// source's aspect ratio.
const HERO_WIDTH: f32 = 100.0;
/// Hard cap on how long a window-close waits for an in-flight share to finish
/// before the process exits anyway (see [`SkrinoApp::handle_close_request`]).
const SHARE_CLOSE_TIMEOUT: Duration = Duration::from_secs(90);

/// How the UI process was launched. Each mode is one-shot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchMode {
    /// Interactive launcher window; returns here after each job.
    Start,
    /// Capture + region overlay, straight to the editor.
    CaptureRegion,
    /// Capture the full screen straight to the editor.
    CaptureFull,
    /// Region overlay that starts a screen recording of the chosen area.
    RecordRegion,
    /// Record the monitor under the cursor, no overlay.
    RecordFull,
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
    /// Active recording: the compact control bar in the (reshaped) root viewport.
    Recording(Box<RecordSession>),
    /// Settings-only launch: nothing behind the settings window.
    SettingsOnly,
}

/// What `do_share` actually did — drives whether the editor closes.
enum ShareOutcome {
    /// Saved to the local folder, image in the clipboard: job done.
    LocalDone,
    /// Server upload spawned; close the editor once the link arrives.
    ServerInFlight,
    /// Nothing happened (validation/encode error, toast already shown).
    Failed,
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
    /// Compact always-on-top recording control bar.
    ControlBar { pos: Pos2, size: Vec2 },
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
    /// «Поделиться» was clicked in the editor: close it as soon as the link
    /// lands in the clipboard (failure keeps it open so «Повторить» works).
    close_after_share: bool,
    /// Set once the window-close was deferred because a share was in flight
    /// (see [`Self::handle_close_request`]). While `true` the window stays
    /// hidden and the process waits for the share to finish (or time out)
    /// before actually exiting.
    pending_close: bool,
    /// Hard deadline for a deferred close: the process exits at this instant
    /// even if the upload never reports back.
    close_deadline: Option<Instant>,
    /// The Start-window hero image, decoded once and reused every frame.
    hero_texture: Option<egui::TextureHandle>,
    /// Single-instance recording lock, held for the recording process lifetime.
    record_lock: Option<RecordingLock>,
    /// Set by the hotkey stop watcher when a stop is requested cross-process.
    record_stop: Option<Arc<AtomicBool>>,
    /// In-flight upload of a finished recording (server share destination).
    record_share: Option<ShareHandle>,
}

impl SkrinoApp {
    pub fn new(cc: &eframe::CreationContext<'_>, config: AppConfig, launch: LaunchMode) -> Self {
        cc.egui_ctx.set_fonts(theme::build_fonts());
        theme::apply(&cc.egui_ctx, config.theme);
        let applied_theme = Some(config.theme);
        let hero_texture = load_hero_texture(&cc.egui_ctx);

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
            close_after_share: false,
            pending_close: false,
            close_deadline: None,
            hero_texture,
            record_lock: None,
            record_stop: None,
            record_share: None,
        }
    }

    fn is_interactive(&self) -> bool {
        matches!(self.launch, LaunchMode::Start)
    }

    /// Save config and terminate the process. Used when a one-shot job finishes.
    fn quit(&self) -> ! {
        self.config.save();
        // Don't lose a toast fired microseconds ago (copy/share auto-close).
        crate::notify::flush();
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
    fn dispatch_launch(&mut self, ctx: &egui::Context) {
        self.state = match self.launch {
            LaunchMode::Start => AppState::Start,
            LaunchMode::CaptureRegion => self.begin_region_capture(false),
            LaunchMode::OverlaySmoke => self.begin_region_capture(true),
            LaunchMode::CaptureFull => self.begin_full_capture(),
            LaunchMode::RecordRegion => self.begin_record_region(ctx),
            LaunchMode::RecordFull => self.begin_record_full(ctx),
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
            Ok(Ok(cap)) => match build_overlay(cap, OverlayPurpose::Screenshot, smoke) {
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

    // --- recording entry points ---

    /// Fail fast if the engine is unavailable or a recording already runs;
    /// otherwise acquire the single-instance lock and start the stop watcher.
    /// Returns `Err(state)` (an exit state) when recording must not proceed.
    fn arm_recording(&mut self, ctx: &egui::Context) -> Result<(), AppState> {
        if !skrino_record::is_supported() {
            crate::notify::notify(
                "Запись экрана не поддерживается",
                "Нужна Windows 10 версии 1903 или новее",
                self.config.notifications,
            );
            return Err(self.finish());
        }
        let Some(lock) = RecordingLock::acquire() else {
            crate::notify::notify(
                "Запись уже идёт",
                "Остановите текущую запись, прежде чем начать новую",
                self.config.notifications,
            );
            return Err(self.finish());
        };
        self.record_lock = Some(lock);
        self.record_stop = Some(record::spawn_stop_watcher(ctx.clone()));
        Ok(())
    }

    /// `--record-region`: region overlay, then start recording the selection.
    fn begin_record_region(&mut self, ctx: &egui::Context) -> AppState {
        if let Err(state) = self.arm_recording(ctx) {
            return state;
        }
        match catch_unwind(AssertUnwindSafe(skrino_capture::capture_virtual_screen)) {
            Ok(Ok(cap)) => match build_overlay(cap, OverlayPurpose::Record, false) {
                Some(ov) => AppState::Overlay(Box::new(ov)),
                None => {
                    crate::notify::notify(
                        "Не удалось начать запись",
                        "Не удалось подготовить область выделения",
                        self.config.notifications,
                    );
                    self.finish()
                }
            },
            _ => {
                crate::notify::notify(
                    "Не удалось начать запись",
                    "Модуль захвата экрана недоступен",
                    self.config.notifications,
                );
                self.finish()
            }
        }
    }

    /// `--record-full`: record the monitor under the cursor, no overlay.
    fn begin_record_full(&mut self, ctx: &egui::Context) -> AppState {
        if let Err(state) = self.arm_recording(ctx) {
            return state;
        }
        let (region, scale) = record::full_monitor_region();
        AppState::Recording(Box::new(self.new_record_session(region, scale)))
    }

    /// Build a recording session from the shared config options.
    fn new_record_session(&self, region: Option<RegionPx>, scale: f32) -> RecordSession {
        let stop_flag = self
            .record_stop
            .clone()
            .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
        RecordSession::new(
            region,
            scale,
            self.config.record_fps,
            self.config.record_cursor,
            record::temp_output_path(),
            stop_flag,
        )
    }

    /// Overlay confirmed a record region: start the session on that area.
    fn begin_recording(&mut self, region: RegionPx, scale: f32) -> AppState {
        AppState::Recording(Box::new(self.new_record_session(Some(region), scale)))
    }

    /// Whether the hotkey watcher has requested a stop (non-consuming peek).
    fn record_stop_requested(&self) -> bool {
        self.record_stop
            .as_ref()
            .is_some_and(|f| f.load(Ordering::Acquire))
    }

    // --- recording result pipeline ---

    /// Stop button / stop hotkey: finalize the recording per the share
    /// destination. Local saves and upload failures exit immediately; a server
    /// upload switches the control bar to its spinner and exits when it lands.
    fn finalize_recording(&mut self, sess: &mut RecordSession, ctx: &egui::Context) {
        let Some(recorder) = sess.take_recorder() else {
            self.quit();
        };
        let path = match recorder.stop() {
            Ok(p) => p,
            Err(e) => {
                crate::notify::notify(
                    "Не удалось сохранить запись",
                    e.to_string(),
                    self.config.notifications,
                );
                self.quit();
            }
        };
        match self.config.share_dest.clone() {
            ShareDestination::Server => {
                if self.start_recording_upload(&path) {
                    sess.set_finalizing();
                    ctx.request_repaint();
                } else {
                    self.quit();
                }
            }
            ShareDestination::LocalDir { .. } => {
                self.save_recording_local(&path);
                self.quit();
            }
        }
    }

    /// Kick off the mp4 upload. Returns `false` (already notified) when upload
    /// isn't configured or the file can't be read — the caller then exits.
    fn start_recording_upload(&mut self, path: &Path) -> bool {
        let Some(config) = self.config.upload.to_upload_config() else {
            let kept = move_into_dir(path, &AppConfig::fallback_dir());
            let where_ = kept.unwrap_or_else(|| path.to_path_buf());
            crate::notify::notify(
                "Не удалось отправить запись",
                format!("Настройте загрузку в параметрах. Файл сохранён: {}", where_.display()),
                self.config.notifications,
            );
            return false;
        };
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                crate::notify::notify(
                    "Не удалось отправить запись",
                    format!("Не удалось прочитать файл: {e}"),
                    self.config.notifications,
                );
                return false;
            }
        };
        let filename = match catch_unwind(|| skrino_upload::generate_filename("mp4")) {
            Ok(name) => name,
            Err(_) => "skrino.mp4".to_string(),
        };
        self.record_share = Some(ShareHandle::spawn(
            config,
            filename,
            bytes,
            AppConfig::fallback_dir(),
        ));
        // The temp file has been read into memory; the upload worker owns a copy.
        let _ = std::fs::remove_file(path);
        true
    }

    /// Poll a finished-recording upload; on completion notify and exit.
    fn poll_record_share(&mut self, ctx: &egui::Context) {
        let Some(handle) = &mut self.record_share else {
            return;
        };
        if handle.in_flight() {
            ctx.request_repaint_after(Duration::from_millis(80));
        }
        if let Some(res) = handle.poll() {
            match res {
                ShareResult::Success(url) => {
                    let _ = arboard::Clipboard::new()
                        .and_then(|mut cb| cb.set_text(url.clone()));
                    crate::notify::notify(
                        "Ссылка скопирована в буфер",
                        url,
                        self.config.notifications,
                    );
                }
                ShareResult::Failure { error, saved_to, .. } => {
                    let extra = saved_to
                        .map(|p| format!(" Файл сохранён: {}", p.display()))
                        .unwrap_or_default();
                    crate::notify::notify(
                        "Не удалось отправить запись",
                        format!("{error}{extra}"),
                        self.config.notifications,
                    );
                }
            }
            self.record_share = None;
            self.quit();
        }
    }

    /// Save the recording into the configured save folder, honouring
    /// `ask_where_to_save` (rfd dialog with an .mp4 filter). A cancelled dialog
    /// still keeps the recording (saved silently into the default folder).
    fn save_recording_local(&mut self, path: &Path) {
        let filename = match catch_unwind(|| skrino_upload::generate_filename("mp4")) {
            Ok(name) => name,
            Err(_) => "skrino.mp4".to_string(),
        };
        let dir = self.config.default_save_dir();
        let _ = std::fs::create_dir_all(&dir);

        let dest = if self.config.ask_where_to_save {
            rfd::FileDialog::new()
                .set_title("Сохранить запись")
                .set_directory(&dir)
                .set_file_name(&filename)
                .add_filter("Видео", &["mp4"])
                .save_file()
                // Cancel: don't lose the recording, drop it into the default folder.
                .unwrap_or_else(|| dir.join(&filename))
        } else {
            dir.join(&filename)
        };

        match move_file(path, &dest) {
            Ok(()) => crate::notify::notify(
                "Запись сохранена",
                dest.display().to_string(),
                self.config.notifications,
            ),
            Err(e) => crate::notify::notify(
                "Не удалось сохранить запись",
                format!("{e} (файл: {})", path.display()),
                self.config.notifications,
            ),
        }
    }

    /// Mark first-run complete and make sure the tray daemon is running. Called
    /// on every exit path of an interactive Start-window process, so that after
    /// the first session Skrino lives in the tray. The daemon's single-instance
    /// mutex makes a redundant spawn silent.
    fn settle_into_background(&mut self) {
        self.config.configured = true;
        self.config.save();
        if let Ok(exe) = std::env::current_exe() {
            let _ = std::process::Command::new(exe).arg("--tray").spawn();
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

    /// Returns `true` on success so the caller can close the editor: once the
    /// image is in the clipboard the job is done.
    fn do_copy(&mut self, ed: &EditorState) -> bool {
        let Some(img) = self.render(ed) else {
            return false;
        };
        if let Err(e) = copy_image_to_clipboard(&img) {
            self.toasts.error(format!("Буфер обмена: {e}"));
            false
        } else {
            self.toasts.success("Скопировано");
            crate::notify::notify(
                "Скриншот скопирован в буфер",
                "",
                self.config.notifications,
            );
            true
        }
    }

    /// «Сохранить» (and Ctrl+S): either straight into the remembered folder
    /// with no dialog, or through the rfd dialog — per `ask_where_to_save`.
    fn do_save(&mut self, ed: &EditorState) {
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
        if self.config.ask_where_to_save {
            self.save_bytes_via_dialog(bytes, filename);
        } else {
            let dir = self.config.default_save_dir();
            self.save_bytes_silent(&dir, &filename, bytes);
        }
    }

    /// Write `bytes` straight into `dir` with no dialog; toast offers to
    /// reveal the folder or redo this one save through the dialog.
    fn save_bytes_silent(&mut self, dir: &Path, filename: &str, bytes: Vec<u8>) {
        if let Err(e) = std::fs::create_dir_all(dir) {
            self.toasts.error(format!("Не удалось создать папку: {e}"));
            return;
        }
        let path = dir.join(filename);
        match std::fs::write(&path, &bytes) {
            Ok(()) => {
                crate::notify::notify("Сохранено", path.display().to_string(), self.config.notifications);
                self.toasts.saved_silent(
                    format!("Сохранено: {}", path.display()),
                    path,
                    bytes,
                    filename.to_string(),
                );
            }
            Err(e) => self.toasts.error(format!("Не удалось сохранить: {e}")),
        }
    }

    /// Save `bytes` through the rfd dialog; on success, remember the folder as
    /// the new default and offer to skip the dialog entirely from now on.
    fn save_bytes_via_dialog(&mut self, bytes: Vec<u8>, filename: String) {
        let ext = self.config.format.extension();
        let dir = self.config.default_save_dir();
        let _ = std::fs::create_dir_all(&dir);

        let path = rfd::FileDialog::new()
            .set_title("Сохранить скриншот")
            .set_directory(&dir)
            .set_file_name(&filename)
            .add_filter("Изображение", &[ext])
            .save_file();
        let Some(path) = path else { return };

        match std::fs::write(&path, &bytes) {
            Ok(()) => {
                let msg = format!("Сохранено: {}", path.display());
                if let Some(parent) = path.parent() {
                    self.config.save_dir = Some(parent.to_path_buf());
                    self.config.save();
                    self.toasts.saved_ask_remember(msg, parent.to_path_buf());
                } else {
                    self.toasts.success(msg);
                }
            }
            Err(e) => self.toasts.error(format!("Не удалось сохранить: {e}")),
        }
    }

    /// «Поделиться»: local folder or server, per the configured destination.
    fn do_share(&mut self, ed: &EditorState) -> ShareOutcome {
        match self.config.share_dest.clone() {
            ShareDestination::LocalDir { path } => {
                if self.share_local(ed, &path) {
                    ShareOutcome::LocalDone
                } else {
                    ShareOutcome::Failed
                }
            }
            ShareDestination::Server => {
                if self.share_server(ed) {
                    ShareOutcome::ServerInFlight
                } else {
                    ShareOutcome::Failed
                }
            }
        }
    }

    /// Returns `true` when the file landed in the folder (and the image is in
    /// the clipboard) — the caller then closes the editor.
    fn share_local(&mut self, ed: &EditorState, dir: &Path) -> bool {
        let Some(img) = self.render(ed) else {
            return false;
        };
        let ext = self.config.format.extension();
        let filename = match catch_unwind(|| skrino_upload::generate_filename(ext)) {
            Ok(name) => name,
            Err(_) => format!("skrino.{ext}"),
        };
        let bytes = match encode_image(&img, self.config.format, self.config.jpeg_quality) {
            Ok(b) => b,
            Err(e) => {
                self.toasts.error(format!("Кодирование: {e}"));
                return false;
            }
        };
        if let Err(e) = std::fs::create_dir_all(dir) {
            self.toasts.error(format!("Не удалось создать папку: {e}"));
            return false;
        }
        let path = dir.join(&filename);
        if let Err(e) = std::fs::write(&path, &bytes) {
            self.toasts.error(format!("Не удалось сохранить: {e}"));
            return false;
        }
        // Copy the rendered image (not a link) to the clipboard.
        let _ = copy_image_to_clipboard(&img);
        crate::notify::notify(
            "Скриншот сохранён",
            path.display().to_string(),
            self.config.notifications,
        );
        self.toasts
            .saved(format!("Сохранено: {}", path.display()), path);
        true
    }

    /// Returns `true` when the upload was actually spawned — the caller then
    /// hides the window and lets the deferred-close flow finish the job.
    fn share_server(&mut self, ed: &EditorState) -> bool {
        if self.share.as_ref().is_some_and(|h| h.in_flight()) {
            return false;
        }
        let Some(config) = self.config.upload.to_upload_config() else {
            self.toasts
                .error("Настройте загрузку в параметрах, чтобы делиться ссылкой");
            return false;
        };
        let Some(img) = self.render(ed) else {
            return false;
        };
        let filename = match catch_unwind(|| skrino_upload::generate_filename("png")) {
            Ok(name) => name,
            Err(_) => "skrino.png".to_string(),
        };
        let bytes = match encode_image(&img, ImageFormat::Png, 100) {
            Ok(b) => b,
            Err(e) => {
                self.toasts.error(format!("Кодирование: {e}"));
                return false;
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
        true
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
                        Ok(()) => self.toasts.link(format!("Ссылка скопирована: {url}"), url.clone()),
                        Err(_) => self.toasts.link(format!("Готово: {url}"), url.clone()),
                    }
                    crate::notify::notify(
                        "Ссылка скопирована в буфер",
                        url,
                        self.config.notifications,
                    );
                    // Link delivered: the editor's job is done, close it
                    // (unless the window was already closed by the user,
                    // then the pending_close path exits on its own).
                    if self.close_after_share && !self.pending_close {
                        self.close_after_share = false;
                        if self.is_interactive() {
                            self.state = AppState::Start;
                        } else {
                            self.quit();
                        }
                    }
                }
                ShareResult::Failure { error, auth, saved_to } => {
                    // Failure keeps the editor open so «Повторить» works.
                    self.close_after_share = false;
                    let extra = saved_to
                        .map(|p| format!(" • сохранено локально: {}", p.display()))
                        .unwrap_or_default();
                    let msg = format!("Не удалось отправить: {error}{extra}");
                    // A visible window shows the retry toast; but if we're
                    // exiting because the window was closed mid-upload, that
                    // toast will never be seen — fall back to a system
                    // notification so the user still learns the outcome.
                    if self.pending_close {
                        crate::notify::notify(
                            "Не удалось отправить скриншот",
                            msg.clone(),
                            self.config.notifications,
                        );
                    }
                    if auth {
                        // Bad credentials: offer a jump to settings alongside retry.
                        self.toasts.error_retry_auth(msg);
                    } else {
                        self.toasts.error_retry(msg);
                    }
                }
            }
            self.share = None;
        }
    }

    // --- settings side-effects ---

    fn handle_settings_result(&mut self, res: SettingsResult) {
        // The settings window already persisted the config on «Сохранить»; here
        // we only trigger the OS-level side-effects of a successful save.
        // The daemon owns the global hotkeys; ask it to reload after we persisted.
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
            AppState::Recording(sess) => WindowMode::ControlBar {
                pos: sess.window_pos(),
                size: sess.window_size(),
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

    fn apply_window_mode(&self, ctx: &egui::Context, mode: &WindowMode) {
        let send = |c| ctx.send_viewport_cmd(c);
        match mode {
            WindowMode::Hidden => send(ViewportCommand::Visible(false)),
            WindowMode::Start => windowed(ctx, START_SIZE, "Skrino"),
            WindowMode::Editor => windowed(ctx, EDITOR_SIZE, "Skrino"),
            WindowMode::Settings => {
                // The standalone `--settings` process gets its own title; the
                // Start window keeps "Skrino" while it's showing settings
                // embedded in the same root window.
                let title = if matches!(self.launch, LaunchMode::Settings) {
                    "Skrino: Настройки"
                } else {
                    "Skrino"
                };
                windowed(ctx, SETTINGS_SIZE, title);
            }
            WindowMode::Overlay { pos, size } => {
                send(ViewportCommand::Decorations(false));
                send(ViewportCommand::Resizable(false));
                send(ViewportCommand::WindowLevel(WindowLevel::AlwaysOnTop));
                send(ViewportCommand::OuterPosition(*pos));
                send(ViewportCommand::InnerSize(*size));
                send(ViewportCommand::Visible(true));
                send(ViewportCommand::Focus);
            }
            WindowMode::ControlBar { pos, size } => {
                // Transition the root window from the fullscreen overlay (or the
                // hidden boot window) into the compact, borderless, always-on-top
                // control bar. Transition-only: sent once, on the state change.
                send(ViewportCommand::Fullscreen(false));
                send(ViewportCommand::Decorations(false));
                send(ViewportCommand::Resizable(false));
                send(ViewportCommand::WindowLevel(WindowLevel::AlwaysOnTop));
                send(ViewportCommand::InnerSize(*size));
                send(ViewportCommand::OuterPosition(*pos));
                send(ViewportCommand::Visible(true));
            }
        }
    }

    // --- main content dispatch ---

    fn draw_main(&mut self, ctx: &egui::Context, frame: &eframe::Frame, palette: &Palette) {
        let mut state = std::mem::replace(&mut self.state, AppState::Boot);
        let mut next: Option<AppState> = None;

        match &mut state {
            AppState::Boot => {}
            AppState::SettingsOnly => {
                // The standalone `--settings` process: the root window IS the
                // settings screen, no separate Start content underneath.
                let res = self.settings.show(ctx, &mut self.config, palette);
                self.handle_settings_result(res);
            }
            AppState::Start if self.settings.open => {
                // «Настройки» from the Start window swaps the root window's
                // content for the settings screen (see `settings_ui` module
                // docs) instead of opening a floating window on top of it.
                let res = self.settings.show(ctx, &mut self.config, palette);
                self.handle_settings_result(res);
            }
            AppState::Start => match draw_start(
                ctx,
                palette,
                !self.config.configured,
                &self.config.hotkey,
                self.hero_texture.as_ref(),
            ) {
                StartSignal::Region => next = Some(self.begin_region_capture(false)),
                StartSignal::Full => next = Some(self.begin_full_capture()),
                StartSignal::Open => next = Some(self.begin_open_file()),
                StartSignal::RecordRegion => next = Some(self.begin_record_region(ctx)),
                StartSignal::RecordFull => next = Some(self.begin_record_full(ctx)),
                StartSignal::Settings => self.settings.open = true,
                StartSignal::None => {}
            },
            AppState::Editor(ed) => {
                let sharing = self.share.as_ref().is_some_and(|h| h.in_flight());
                let signal = ed.ui(ctx, palette, sharing);
                match signal {
                    EditorSignal::Close => {
                        // Escape closes the editor the same way the OS window
                        // X does; a one-shot process would otherwise exit
                        // immediately here and kill an in-flight upload on
                        // its worker thread. Defer exactly like
                        // `handle_close_request` does for that case.
                        if !self.is_interactive() && sharing {
                            self.defer_close_for_share(ctx);
                        } else {
                            next = Some(self.finish());
                        }
                    }
                    EditorSignal::Copy => {
                        // The image is in the clipboard: the job is done,
                        // close the editor (per user request).
                        if self.do_copy(ed) {
                            next = Some(self.finish());
                        }
                    }
                    EditorSignal::Save => self.do_save(ed),
                    EditorSignal::Share => match self.do_share(ed) {
                        ShareOutcome::LocalDone => next = Some(self.finish()),
                        ShareOutcome::ServerInFlight => self.close_after_share = true,
                        ShareOutcome::Failed => {}
                    },
                    EditorSignal::None => {}
                }
            }
            AppState::Overlay(ov) => {
                // A stop hotkey fired during a record-region selection cancels it.
                if self.record_stop_requested() {
                    next = Some(self.finish());
                } else {
                    match ov.run(ctx, palette) {
                        OverlayOutcome::Screenshot(img) => {
                            next = Some(AppState::Editor(Box::new(EditorState::new(img))));
                        }
                        OverlayOutcome::Region(region) => {
                            next = Some(self.begin_recording(region, ov.scale()));
                        }
                        OverlayOutcome::Cancelled => next = Some(self.finish()),
                        OverlayOutcome::Pending => {}
                    }
                }
            }
            AppState::Recording(sess) => match sess.ui(ctx, frame, palette) {
                RecordSignal::None => {}
                RecordSignal::Stop => self.finalize_recording(sess, ctx),
                RecordSignal::Cancel => {
                    if let Some(rec) = sess.take_recorder() {
                        rec.cancel();
                    }
                    self.quit();
                }
                RecordSignal::Error(e) => {
                    crate::notify::notify(
                        "Ошибка записи экрана",
                        e,
                        self.config.notifications,
                    );
                    if let Some(rec) = sess.take_recorder() {
                        rec.cancel();
                    }
                    self.quit();
                }
            },
        }

        self.state = next.unwrap_or(state);
    }

    /// Hide the window and mark the process as waiting out an in-flight share
    /// instead of exiting immediately (see [`Self::handle_close_request`]).
    fn defer_close_for_share(&mut self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(ViewportCommand::CancelClose);
        ctx.send_viewport_cmd(ViewportCommand::Visible(false));
        self.pending_close = true;
        self.close_deadline = Some(Instant::now() + SHARE_CLOSE_TIMEOUT);
    }

    /// Closing the window normally ends the process (no tray in the UI
    /// process). But if a share upload is still in flight, exiting now would
    /// silently kill it on its worker thread — instead we cancel the OS
    /// close, hide the window, and keep the process alive just long enough
    /// for the `ShareResult` to arrive (bounded by [`SHARE_CLOSE_TIMEOUT`])
    /// before actually exiting.
    fn handle_close_request(&mut self, ctx: &egui::Context) {
        if !self.pending_close && ctx.input(|i| i.viewport().close_requested()) {
            if self.share.as_ref().is_some_and(|h| h.in_flight()) {
                self.defer_close_for_share(ctx);
                return;
            }
            if self.is_interactive() {
                // First-run / Start window: settle into the tray on the way out.
                self.settle_into_background();
            } else {
                self.config.save();
            }
        }

        if self.pending_close {
            let still_sharing = self.share.as_ref().is_some_and(|h| h.in_flight());
            let timed_out = self.close_deadline.is_some_and(|d| Instant::now() >= d);
            if timed_out && still_sharing {
                crate::notify::notify(
                    "Не удалось отправить скриншот",
                    "истекло время ожидания ответа сервера",
                    self.config.notifications,
                );
            }
            if !still_sharing || timed_out {
                self.quit();
            } else {
                ctx.request_repaint_after(Duration::from_millis(200));
            }
        }
    }
}

impl eframe::App for SkrinoApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        Palette::for_theme(self.config.theme)
            .window
            .to_normalized_gamma_f32()
    }

    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        if matches!(self.state, AppState::Boot) {
            self.dispatch_launch(ctx);
        }

        if self.applied_theme != Some(self.config.theme) {
            theme::apply(ctx, self.config.theme);
            self.applied_theme = Some(self.config.theme);
        }
        let palette = Palette::for_theme(self.config.theme);

        // Reshape the root window only on state transitions.
        let wanted = self.window_mode_for_state();
        if self.applied_window.as_ref() != Some(&wanted) {
            self.apply_window_mode(ctx, &wanted);
            self.applied_window = Some(wanted);
        }

        self.draw_main(ctx, frame, &palette);

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
            ToastAction::OpenSettings => {
                // Spawn a detached settings process (the tray daemon owns config).
                if let Ok(exe) = std::env::current_exe() {
                    let _ = std::process::Command::new(exe).arg("--settings").spawn();
                }
            }
            ToastAction::RememberSaveFolder(dir) => {
                self.config.save_dir = Some(dir);
                self.config.ask_where_to_save = false;
                self.config.save();
                self.toasts
                    .info("Теперь сохраняю сюда без вопросов. Изменить можно в настройках.");
            }
            ToastAction::ReSaveAs { bytes, filename } => {
                self.save_bytes_via_dialog(bytes, filename);
            }
            ToastAction::None => {}
        }

        self.poll_share(ctx);
        self.poll_record_share(ctx);
        self.handle_close_request(ctx);
    }
}

/// Decode the Start-window hero PNG into a texture, once.
fn load_hero_texture(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let img = image::load_from_memory(HERO_PNG_BYTES).ok()?.to_rgba8();
    let size = [img.width() as usize, img.height() as usize];
    let color = egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw());
    Some(ctx.load_texture("skrino_hero", color, egui::TextureOptions::LINEAR))
}

/// Restore standard windowed chrome at `size` with `title` (decorations,
/// normal level, not fullscreen, visible, focused).
fn windowed(ctx: &egui::Context, size: Vec2, title: &str) {
    ctx.send_viewport_cmd(ViewportCommand::Fullscreen(false));
    ctx.send_viewport_cmd(ViewportCommand::Decorations(true));
    ctx.send_viewport_cmd(ViewportCommand::WindowLevel(WindowLevel::Normal));
    ctx.send_viewport_cmd(ViewportCommand::Resizable(true));
    ctx.send_viewport_cmd(ViewportCommand::InnerSize(size));
    ctx.send_viewport_cmd(ViewportCommand::Title(title.to_owned()));
    ctx.send_viewport_cmd(ViewportCommand::Visible(true));
    ctx.send_viewport_cmd(ViewportCommand::Focus);
}

/// Build the overlay for the monitor under the cursor: crop that monitor's slice
/// out of the virtual-screen capture and compute the root-window geometry. The
/// same overlay serves screenshots and recordings; `purpose` decides what a
/// confirmed selection becomes.
fn build_overlay(
    cap: VirtualScreenCapture,
    purpose: OverlayPurpose,
    smoke: bool,
) -> Option<OverlayState> {
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

    Some(OverlayState::new(
        slice, scale, pos_pt, size_pt, monitor.x, monitor.y, purpose, smoke,
    ))
}

/// Move a file to `dest` (rename, falling back to copy+delete across volumes).
fn move_file(from: &Path, dest: &Path) -> std::io::Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::rename(from, dest) {
        Ok(()) => Ok(()),
        Err(_) => {
            std::fs::copy(from, dest)?;
            let _ = std::fs::remove_file(from);
            Ok(())
        }
    }
}

/// Move `from` into `dir` keeping its file name. Returns the destination path.
fn move_into_dir(from: &Path, dir: &Path) -> Option<std::path::PathBuf> {
    let name = from.file_name()?;
    let dest = dir.join(name);
    move_file(from, &dest).ok().map(|()| dest)
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
    RecordRegion,
    RecordFull,
    Settings,
}

fn draw_start(
    ctx: &egui::Context,
    palette: &Palette,
    first_run: bool,
    hotkey: &str,
    hero: Option<&egui::TextureHandle>,
) -> StartSignal {
    let mut sig = StartSignal::None;
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(palette.window).inner_margin(egui::Margin::same(20)))
        .show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(16.0);
                if let Some(tex) = hero {
                    let size = tex.size_vec2();
                    let aspect = if size.x > 0.0 { size.y / size.x } else { 1.0 };
                    let draw_size = Vec2::new(HERO_WIDTH, HERO_WIDTH * aspect);
                    ui.add(egui::Image::new((tex.id(), draw_size)));
                }
                ui.add_space(4.0);
                ui.label(
                    RichText::new("Быстрые скриншоты")
                        .size(13.0)
                        .color(palette.text_secondary),
                );
                ui.add_space(18.0);
                // Both card rows hold two entries each, so they share one width.
                let cards_width = 2.0 * MODE_CARD_SIZE.x + 8.0;
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
                });
                ui.add_space(8.0);
                // Second row: screen recording actions.
                ui.horizontal(|ui| {
                    ui.add_space(pad);
                    ui.spacing_mut().item_spacing.x = 8.0;
                    if mode_card(ui, palette, ph::VIDEO_CAMERA, "Записать область", false).clicked() {
                        sig = StartSignal::RecordRegion;
                    }
                    if mode_card(ui, palette, ph::MONITOR_PLAY, "Записать экран", false).clicked() {
                        sig = StartSignal::RecordFull;
                    }
                });
                ui.add_space(10.0);
                // Secondary actions: opening a file and settings share the same
                // low-emphasis link style, side by side.
                ui.horizontal(|ui| {
                    let row = ui.available_width();
                    let btns = 250.0;
                    ui.add_space(((row - btns) / 2.0).max(0.0));
                    ui.spacing_mut().item_spacing.x = 16.0;
                    if secondary_button(ui, palette, ph::FOLDER_OPEN, "Открыть файл").clicked() {
                        sig = StartSignal::Open;
                    }
                    if secondary_button(ui, palette, ph::GEAR_SIX, "Настройки").clicked() {
                        sig = StartSignal::Settings;
                    }
                });

                // First-run hint: explain that Skrino keeps living in the tray.
                if first_run {
                    ui.add_space(10.0);
                    let hk = if hotkey.trim().is_empty() {
                        "горячей клавише".to_string()
                    } else {
                        hotkey.to_string()
                    };
                    ui.add(
                        egui::Label::new(
                            RichText::new(format!(
                                "Skrino будет работать в фоне: иконка в трее, скриншот по {hk}"
                            ))
                            .size(12.0)
                            .color(palette.text_secondary),
                        )
                        .wrap(),
                    );
                }
            });
        });
    sig
}

const MODE_CARD_SIZE: Vec2 = Vec2::new(96.0, 60.0);

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
            palette.accent_fg,
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
    painter.rect(rect, CornerRadius::same(8), fill, stroke, egui::StrokeKind::Inside);
    painter.text(
        rect.center() - Vec2::new(0.0, 10.0),
        egui::Align2::CENTER_CENTER,
        icon,
        egui::FontId::proportional(18.0),
        fg,
    );
    painter.text(
        rect.center() + Vec2::new(0.0, 13.0),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::proportional(12.0),
        fg,
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Low-emphasis link-style action, used for "Открыть файл" and "Настройки".
fn secondary_button(ui: &mut egui::Ui, palette: &Palette, icon: &str, label: &str) -> egui::Response {
    ui.add(
        egui::Button::new(
            RichText::new(format!("{icon}  {label}"))
                .color(palette.text_secondary)
                .size(13.0),
        )
        .frame(false),
    )
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
