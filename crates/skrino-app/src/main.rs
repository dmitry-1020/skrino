//! skrino — fast screenshot tool: capture, annotate, upload, share.
//!
//! Launch normally to show the start window, or with `--tray` to run in the
//! background (tray icon + global hotkey only).

// Windows: don't spawn a console window for the GUI app in release.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod autostart;
mod config;
mod editor;
mod hotkey;
mod overlay;
mod settings_ui;
mod share;
mod theme;
mod toast;
mod transform;
mod tray;

use app::SkrinoApp;
use config::AppConfig;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let start_hidden = std::env::args().any(|a| a == "--tray");
    let config = AppConfig::load();
    let start_visible = !start_hidden;

    let viewport = egui::ViewportBuilder::default()
        .with_title("Skrino")
        .with_inner_size([420.0, 356.0])
        .with_min_inner_size([420.0, 356.0])
        .with_resizable(true)
        .with_visible(start_visible);

    let options = eframe::NativeOptions {
        viewport,
        // We manage our own JSON config; don't let eframe persist window state.
        persist_window: false,
        ..Default::default()
    };

    if let Err(e) = eframe::run_native(
        "Skrino",
        options,
        Box::new(move |cc| Ok(Box::new(SkrinoApp::new(cc, config, start_hidden)))),
    ) {
        log::error!("failed to start: {e}");
        std::process::exit(1);
    }
}
