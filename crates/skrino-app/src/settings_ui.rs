//! Settings window: upload (FTP/FTPS/SFTP) credentials and general options.
//!
//! Editing is **staged**: the window mutates a private *working copy* of
//! [`AppConfig`] plus staged secret strings. Nothing touches disk or the OS
//! keychain until the user presses «Сохранить»; «Отмена» (and the titlebar
//! close) discards the working copy wholesale. This keeps secrets out of the
//! keychain until an explicit save, and makes the daemon reload fire exactly
//! once per save instead of per keystroke.

use std::sync::mpsc::Receiver;

use egui::{Align, ComboBox, Layout, RichText};
use skrino_upload::{Protocol, UploadConfig};

use crate::config::{AppConfig, ImageFormat, ShareDestination};
use crate::theme::{Palette, Theme};

/// What the app should act on after the settings window ran this frame.
#[derive(Default)]
pub struct SettingsResult {
    /// The window closed this frame (saved or cancelled).
    pub close: bool,
    /// A hotkey (region or full) was changed and persisted — reload the daemon.
    pub hotkey_changed: bool,
    /// The autostart flag was changed and persisted.
    pub autostart_changed: bool,
}

#[derive(Default)]
pub struct SettingsWindow {
    pub open: bool,
    /// Staged edits; `Some` while the window is open. Cloned from the live
    /// config on open, written back only on save.
    working: Option<AppConfig>,
    password_input: String,
    passphrase_input: String,
    /// The user edited the password/passphrase field this session (so it should
    /// be pushed to the keychain on save — clearing it deletes the secret).
    password_touched: bool,
    passphrase_touched: bool,
    /// Validation message shown above the buttons when a save is refused.
    save_error: Option<String>,
    test_rx: Option<Receiver<Result<String, String>>>,
    test_result: Option<Result<String, String>>,
    testing: bool,
}

/// Fields that may need a side-effect after a successful save.
struct CommitChanged {
    hotkey: bool,
    autostart: bool,
}

impl SettingsWindow {
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        cfg: &mut AppConfig,
        palette: &Palette,
    ) -> SettingsResult {
        let mut result = SettingsResult::default();
        if !self.open {
            return result;
        }
        // Snapshot the live config into a working copy the first frame we're open.
        if self.working.is_none() {
            self.reset_working(cfg);
        }
        self.poll_test();

        // Take the working copy out so section methods can borrow `self` freely.
        let mut work = self.working.take().expect("working copy present while open");

        let mut keep_open = true; // titlebar X clears this → cancel
        let mut save_clicked = false;
        let mut cancel_clicked = false;

        egui::Window::new(RichText::new("Настройки").size(18.0))
            .open(&mut keep_open)
            .resizable(false)
            .collapsible(false)
            .default_width(460.0)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().max_height(500.0).show(ui, |ui| {
                    self.share_section(ui, &mut work, palette);
                    ui.add_space(10.0);
                    self.save_section(ui, &mut work, palette);
                    ui.add_space(10.0);
                    self.general_section(ui, &mut work, palette);
                });

                ui.add_space(8.0);
                ui.separator();
                ui.add_space(4.0);
                if let Some(err) = &self.save_error {
                    ui.label(RichText::new(err).size(12.0).color(palette.danger));
                    ui.add_space(4.0);
                }
                ui.horizontal(|ui| {
                    if ui.button("Отмена").clicked() {
                        cancel_clicked = true;
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let save = egui::Button::new(
                            RichText::new("Сохранить").color(egui::Color32::WHITE),
                        )
                        .fill(palette.accent);
                        if ui.add(save).clicked() {
                            save_clicked = true;
                        }
                    });
                });
            });

        if save_clicked {
            match validate(&work) {
                Ok(()) => {
                    let changed = self.commit(cfg, work);
                    result.close = true;
                    result.hotkey_changed = changed.hotkey;
                    result.autostart_changed = changed.autostart;
                    self.finish_close();
                }
                Err(msg) => {
                    // Keep the window open with the offending values for a fix.
                    self.save_error = Some(msg);
                    self.working = Some(work);
                }
            }
        } else if cancel_clicked || !keep_open {
            // Discard the working copy; nothing written to disk or keychain.
            result.close = true;
            self.finish_close();
        } else {
            self.working = Some(work);
        }
        result
    }

    /// Clone the live config into the working copy and reset staged inputs.
    fn reset_working(&mut self, cfg: &AppConfig) {
        self.working = Some(cfg.clone());
        self.password_input.clear();
        self.passphrase_input.clear();
        self.password_touched = false;
        self.passphrase_touched = false;
        self.save_error = None;
        self.test_result = None;
        self.test_rx = None;
        self.testing = false;
    }

    /// Close the window and drop all staged state.
    fn finish_close(&mut self) {
        self.open = false;
        self.working = None;
        self.password_input.clear();
        self.passphrase_input.clear();
        self.password_touched = false;
        self.passphrase_touched = false;
        self.save_error = None;
        self.test_result = None;
        self.test_rx = None;
        self.testing = false;
    }

    /// Write the working copy back to the live config, push staged secrets to the
    /// keychain, and persist. Returns which side-effect-worthy fields changed.
    fn commit(&mut self, cfg: &mut AppConfig, work: AppConfig) -> CommitChanged {
        let changed = CommitChanged {
            hotkey: cfg.hotkey != work.hotkey || cfg.hotkey_full != work.hotkey_full,
            autostart: cfg.autostart != work.autostart,
        };
        *cfg = work;
        // Secrets reach the keychain ONLY here, on save.
        if cfg.upload.use_key_file {
            if self.passphrase_touched {
                cfg.set_passphrase(&self.passphrase_input);
            }
        } else if self.password_touched {
            cfg.set_password(&self.password_input);
        }
        cfg.save();
        changed
    }

    /// «Поделиться»: choose a local folder or the FTP/SFTP server. When the
    /// server is chosen, the upload credentials appear below.
    fn share_section(&mut self, ui: &mut egui::Ui, cfg: &mut AppConfig, palette: &Palette) {
        section_header(ui, palette, "Поделиться");

        let is_local = matches!(cfg.share_dest, ShareDestination::LocalDir { .. });
        if ui.radio(is_local, "В локальную папку").clicked() && !is_local {
            cfg.share_dest = ShareDestination::LocalDir {
                path: AppConfig::fallback_dir(),
            };
        }
        if let ShareDestination::LocalDir { path } = &mut cfg.share_dest {
            ui.horizontal(|ui| {
                ui.add_space(22.0);
                if ui.button("Выбрать папку…").clicked()
                    && let Some(dir) = rfd::FileDialog::new()
                        .set_title("Папка для сохранения при «Поделиться»")
                        .set_directory(&*path)
                        .pick_folder()
                {
                    *path = dir;
                }
                ui.label(
                    RichText::new(path.display().to_string())
                        .size(12.0)
                        .color(palette.text_secondary),
                );
            });
        }

        let is_server = matches!(cfg.share_dest, ShareDestination::Server);
        if ui.radio(is_server, "На сервер (FTP/SFTP)").clicked() && !is_server {
            cfg.share_dest = ShareDestination::Server;
        }

        if is_server {
            ui.add_space(8.0);
            self.upload_section(ui, cfg, palette);
        }
    }

    /// «Сохранение»: the folder «Сохранить» writes into, and whether it asks
    /// every time or just saves straight into that folder.
    fn save_section(&mut self, ui: &mut egui::Ui, cfg: &mut AppConfig, palette: &Palette) {
        section_header(ui, palette, "Сохранение");

        let dir = cfg.default_save_dir();
        ui.horizontal(|ui| {
            if ui.button("Выбрать папку…").clicked()
                && let Some(picked) = rfd::FileDialog::new()
                    .set_title("Папка для сохранения скриншотов")
                    .set_directory(&dir)
                    .pick_folder()
            {
                cfg.save_dir = Some(picked);
            }
            ui.label(
                RichText::new(dir.display().to_string())
                    .size(12.0)
                    .color(palette.text_secondary),
            );
        });
        ui.checkbox(
            &mut cfg.ask_where_to_save,
            "Спрашивать место при каждом сохранении",
        );
    }

    fn upload_section(&mut self, ui: &mut egui::Ui, cfg: &mut AppConfig, palette: &Palette) {
        section_header(ui, palette, "Загрузка");

        egui::Grid::new("upload_grid")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .show(ui, |ui| {
                let u = &mut cfg.upload;
                ui.label("Протокол");
                let prev = u.protocol;
                ComboBox::from_id_salt("protocol")
                    .selected_text(protocol_name(u.protocol))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut u.protocol, Protocol::Ftp, "FTP");
                        ui.selectable_value(&mut u.protocol, Protocol::Ftps, "FTPS");
                        ui.selectable_value(&mut u.protocol, Protocol::Sftp, "SFTP");
                    });
                if u.protocol != prev {
                    // Auto-apply the default port for the new protocol.
                    u.port = UploadConfig::default_port(u.protocol);
                }
                ui.end_row();

                ui.label("Хост");
                ui.text_edit_singleline(&mut u.host);
                ui.end_row();

                ui.label("Порт");
                ui.add(egui::DragValue::new(&mut u.port).range(1..=65535));
                ui.end_row();

                ui.label("Логин");
                ui.text_edit_singleline(&mut u.username);
                ui.end_row();
            });

        // Auth: password or (SFTP) key file.
        if cfg.upload.protocol == Protocol::Sftp {
            ui.checkbox(&mut cfg.upload.use_key_file, "Использовать файл ключа (SFTP)");
        } else {
            cfg.upload.use_key_file = false;
        }

        if cfg.upload.use_key_file {
            ui.horizontal(|ui| {
                ui.label("Файл ключа");
                ui.text_edit_singleline(&mut cfg.upload.key_file);
                if ui.button("Обзор…").clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .set_title("Выберите файл приватного ключа")
                        .pick_file()
                {
                    cfg.upload.key_file = path.display().to_string();
                }
            });
            ui.horizontal(|ui| {
                ui.label("Пароль ключа");
                let has = cfg.upload.has_passphrase;
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.passphrase_input)
                        .password(true)
                        .hint_text(if has { "•••• сохранён" } else { "" }),
                );
                if resp.changed() {
                    self.passphrase_touched = true;
                }
            });
        } else {
            ui.horizontal(|ui| {
                ui.label("Пароль");
                let has = cfg.upload.has_password;
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.password_input)
                        .password(true)
                        .hint_text(if has { "•••• сохранён" } else { "" }),
                );
                if resp.changed() {
                    self.password_touched = true;
                }
            });
        }

        egui::Grid::new("upload_grid2")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .show(ui, |ui| {
                ui.label("Удалённая папка");
                ui.text_edit_singleline(&mut cfg.upload.remote_dir);
                ui.end_row();

                ui.label("Шаблон ссылки");
                ui.vertical(|ui| {
                    ui.text_edit_singleline(&mut cfg.upload.url_template);
                    ui.label(
                        RichText::new("{filename} будет заменено именем файла")
                            .size(11.0)
                            .color(palette.text_secondary),
                    );
                });
                ui.end_row();
            });

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let can_test = cfg.upload.is_configured() && !self.testing;
            if ui
                .add_enabled(can_test, egui::Button::new("Проверить соединение"))
                .clicked()
            {
                self.start_test(cfg);
            }
            if self.testing {
                ui.spinner();
                ui.label(RichText::new("Проверка…").color(palette.text_secondary));
            } else if let Some(res) = &self.test_result {
                match res {
                    Ok(msg) => {
                        ui.label(RichText::new(format!("✓ {msg}")).color(palette.success));
                    }
                    Err(e) => {
                        ui.label(RichText::new(format!("✗ {e}")).color(palette.danger));
                    }
                }
            }
        });
    }

    fn general_section(&mut self, ui: &mut egui::Ui, cfg: &mut AppConfig, palette: &Palette) {
        section_header(ui, palette, "Общие");

        egui::Grid::new("general_grid")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .show(ui, |ui| {
                ui.label("Область");
                hotkey_field(ui, palette, &mut cfg.hotkey, "Ctrl+Shift+3");
                ui.end_row();

                ui.label("Весь экран");
                hotkey_field(ui, palette, &mut cfg.hotkey_full, "Ctrl+Shift+4");
                ui.end_row();

                ui.label("Формат");
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut cfg.format, ImageFormat::Png, "PNG");
                    ui.selectable_value(&mut cfg.format, ImageFormat::Jpeg, "JPEG");
                });
                ui.end_row();

                if cfg.format == ImageFormat::Jpeg {
                    ui.label("Качество JPEG");
                    ui.add(egui::Slider::new(&mut cfg.jpeg_quality, 40..=100));
                    ui.end_row();
                }

                ui.label("Тема");
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut cfg.theme, Theme::Dark, "Тёмная");
                    ui.selectable_value(&mut cfg.theme, Theme::Light, "Светлая");
                });
                ui.end_row();
            });

        ui.checkbox(&mut cfg.autostart, "Запускать в фоне при старте системы");
        ui.checkbox(&mut cfg.notifications, "Показывать уведомления");
    }

    /// Kick off a background connection test against the *unsaved* form values,
    /// including a freshly-typed password (never round-tripped via the keychain).
    fn start_test(&mut self, cfg: &AppConfig) {
        let Some(config) = cfg
            .upload
            .to_upload_config_staged(&self.password_input, &self.passphrase_input)
        else {
            self.test_result = Some(Err("не задан пароль/ключ".into()));
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        self.test_rx = Some(rx);
        self.testing = true;
        self.test_result = None;
        std::thread::Builder::new()
            .name("skrino-test".into())
            .spawn(move || {
                let res = skrino_upload::test_connection(&config).map_err(|e| e.to_string());
                let _ = tx.send(res);
            })
            .ok();
    }

    fn poll_test(&mut self) {
        if let Some(rx) = &self.test_rx {
            match rx.try_recv() {
                Ok(res) => {
                    self.test_result = Some(res);
                    self.testing = false;
                    self.test_rx = None;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.test_result = Some(Err("проверка прервана".into()));
                    self.testing = false;
                    self.test_rx = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
    }
}

/// Validate the staged config before a save is allowed.
fn validate(work: &AppConfig) -> Result<(), String> {
    let region = crate::hotkey::parse(&work.hotkey)
        .map_err(|_| "Горячая клавиша «Область» указана неверно".to_string())?;
    let full = crate::hotkey::parse(&work.hotkey_full)
        .map_err(|_| "Горячая клавиша «Весь экран» указана неверно".to_string())?;
    if region == full {
        return Err("Горячие клавиши «Область» и «Весь экран» должны различаться".into());
    }
    if work.upload.port == 0 {
        return Err("Порт должен быть больше нуля".into());
    }
    Ok(())
}

/// A hotkey text field with a live red error when the current text won't parse.
fn hotkey_field(ui: &mut egui::Ui, palette: &Palette, value: &mut String, example: &str) {
    ui.vertical(|ui| {
        ui.text_edit_singleline(value);
        if crate::hotkey::parse(value).is_err() {
            ui.label(
                RichText::new("Неверная комбинация")
                    .size(11.0)
                    .color(palette.danger),
            );
        } else {
            ui.label(
                RichText::new(format!("Например: {example}"))
                    .size(11.0)
                    .color(palette.text_secondary),
            );
        }
    });
}

fn section_header(ui: &mut egui::Ui, palette: &Palette, text: &str) {
    ui.label(
        RichText::new(text)
            .color(palette.text)
            .font(crate::theme::heading_font(15.0)),
    );
    ui.add_space(4.0);
}

fn protocol_name(p: Protocol) -> &'static str {
    match p {
        Protocol::Ftp => "FTP",
        Protocol::Ftps => "FTPS",
        Protocol::Sftp => "SFTP",
    }
}
