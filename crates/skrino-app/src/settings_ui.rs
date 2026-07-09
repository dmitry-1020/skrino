//! Settings window: upload (FTP/FTPS/SFTP) credentials and general options.
//! Edits the live `AppConfig`; secrets go to the OS keychain, never the JSON.

use std::sync::mpsc::Receiver;

use egui::{ComboBox, RichText};
use skrino_upload::{Protocol, UploadConfig};

use crate::config::{AppConfig, ImageFormat, ShareDestination};
use crate::theme::{Palette, Theme};

/// What the app should act on after the settings window ran this frame.
#[derive(Default)]
pub struct SettingsResult {
    pub close: bool,
    pub hotkey_changed: bool,
    pub theme_changed: bool,
    pub autostart_changed: bool,
    pub dirty: bool,
}

#[derive(Default)]
pub struct SettingsWindow {
    pub open: bool,
    password_input: String,
    passphrase_input: String,
    test_rx: Option<Receiver<Result<String, String>>>,
    test_result: Option<Result<String, String>>,
    testing: bool,
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

        // Snapshot fields that need a side-effect when changed.
        let old_hotkey = cfg.hotkey.clone();
        let old_theme = cfg.theme;
        let old_autostart = cfg.autostart;
        let before = cfg.clone();

        self.poll_test();

        let mut open = self.open;
        egui::Window::new(RichText::new("Настройки").size(18.0))
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .default_width(460.0)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().max_height(560.0).show(ui, |ui| {
                    self.share_section(ui, cfg, palette);
                    ui.add_space(10.0);
                    self.general_section(ui, cfg, palette);
                });
            });
        self.open = open;

        if !self.open {
            result.close = true;
        }
        result.hotkey_changed = cfg.hotkey != old_hotkey;
        result.theme_changed = cfg.theme != old_theme;
        result.autostart_changed = cfg.autostart != old_autostart;
        result.dirty = *cfg != before || result.close;
        result
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
                if resp.lost_focus() && !self.passphrase_input.is_empty() {
                    cfg.set_passphrase(&self.passphrase_input);
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
                if resp.lost_focus() && !self.password_input.is_empty() {
                    cfg.set_password(&self.password_input);
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
                ui.label("Горячая клавиша");
                ui.vertical(|ui| {
                    ui.text_edit_singleline(&mut cfg.hotkey);
                    if crate::hotkey::parse(&cfg.hotkey).is_err() {
                        ui.label(
                            RichText::new("Неверная комбинация")
                                .size(11.0)
                                .color(palette.danger),
                        );
                    } else {
                        ui.label(
                            RichText::new("Например: PrintScreen или Ctrl+Shift+S")
                                .size(11.0)
                                .color(palette.text_secondary),
                        );
                    }
                });
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

        ui.checkbox(
            &mut cfg.autostart,
            "Запускать в фоне при старте системы",
        );
    }

    fn start_test(&mut self, cfg: &mut AppConfig) {
        // Persist any freshly-typed secret so the worker can read it back.
        if !self.password_input.is_empty() {
            cfg.set_password(&self.password_input);
        }
        if !self.passphrase_input.is_empty() {
            cfg.set_passphrase(&self.passphrase_input);
        }
        let Some(config) = cfg.upload.to_upload_config() else {
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
