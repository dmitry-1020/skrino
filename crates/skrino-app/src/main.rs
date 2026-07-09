//! skrino — fast screenshot tool: capture, annotate, upload, share.
//!
//! Two process roles share one executable:
//! * `--tray` runs the lightweight background daemon (tray icon + global hotkey,
//!   no GPU/egui). See [`daemon`].
//! * every other invocation is a one-shot UI process that opens straight to the
//!   point and exits when its window closes. See [`app::LaunchMode`].

// Windows: don't spawn a console window for the GUI app in release.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod autostart;
mod config;
mod daemon;
mod editor;
mod hotkey;
mod overlay;
mod settings_ui;
mod share;
mod theme;
mod toast;
mod transform;
mod tray;

use app::{LaunchMode, SkrinoApp};
use config::AppConfig;
use egui::Vec2;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // The tray daemon is a completely separate, GUI-less role.
    if std::env::args().any(|a| a == "--tray") {
        daemon::run();
        return;
    }

    let mode = parse_mode();
    let config = AppConfig::load();

    // No-arg launch is context-sensitive:
    //   * first run (configured == false) → show the Start window (below);
    //   * after setup (configured == true) → this is just a "make sure it's
    //     running" double-click: spawn the tray daemon (the single-instance
    //     mutex dedupes) and exit without any window.
    // `--start` is the escape hatch that always forces the Start window.
    let forced_start = std::env::args().any(|a| a == "--start");
    if matches!(mode, LaunchMode::Start) && config.configured && !forced_start {
        spawn_tray();
        return;
    }

    // Start-window is shown immediately; one-shot modes stay hidden until the app
    // reshapes the root window on its first frame (capture happens first).
    let start_visible = matches!(mode, LaunchMode::Start);
    let initial_size: Vec2 = match mode {
        LaunchMode::Start => Vec2::new(420.0, 392.0),
        LaunchMode::Settings => Vec2::new(540.0, 620.0),
        _ => Vec2::new(420.0, 392.0),
    };

    let viewport = egui::ViewportBuilder::default()
        .with_title("Skrino")
        .with_inner_size(initial_size)
        .with_min_inner_size([360.0, 300.0])
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
        Box::new(move |cc| Ok(Box::new(SkrinoApp::new(cc, config, mode)))),
    ) {
        log::error!("failed to start: {e}");
        std::process::exit(1);
    }
}

/// Spawn the background tray daemon and detach. The daemon's single-instance
/// mutex makes a redundant spawn silent and harmless.
fn spawn_tray() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).arg("--tray").spawn();
    }
}

/// Map CLI flags to a UI launch mode (first recognised flag wins).
fn parse_mode() -> LaunchMode {
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--capture-region" => return LaunchMode::CaptureRegion,
            "--capture-full" => return LaunchMode::CaptureFull,
            "--open-file" => return LaunchMode::OpenFile,
            "--settings" => return LaunchMode::Settings,
            "--overlay-smoke" => return LaunchMode::OverlaySmoke,
            "--start" => return LaunchMode::Start,
            _ => {}
        }
    }
    LaunchMode::Start
}

#[cfg(test)]
mod arg_tests {
    use super::*;

    fn mode_from<'a>(args: impl IntoIterator<Item = &'a str>) -> LaunchMode {
        // Mirror of `parse_mode` over an explicit arg list (env-independent).
        for arg in args {
            match arg {
                "--capture-region" => return LaunchMode::CaptureRegion,
                "--capture-full" => return LaunchMode::CaptureFull,
                "--open-file" => return LaunchMode::OpenFile,
                "--settings" => return LaunchMode::Settings,
                "--overlay-smoke" => return LaunchMode::OverlaySmoke,
                "--start" => return LaunchMode::Start,
                _ => {}
            }
        }
        LaunchMode::Start
    }

    #[test]
    fn no_args_is_start() {
        assert_eq!(mode_from([]), LaunchMode::Start);
        assert_eq!(mode_from(["skrino"]), LaunchMode::Start);
    }

    #[test]
    fn each_flag_maps_to_its_mode() {
        assert_eq!(mode_from(["--capture-region"]), LaunchMode::CaptureRegion);
        assert_eq!(mode_from(["--capture-full"]), LaunchMode::CaptureFull);
        assert_eq!(mode_from(["--open-file"]), LaunchMode::OpenFile);
        assert_eq!(mode_from(["--settings"]), LaunchMode::Settings);
        assert_eq!(mode_from(["--overlay-smoke"]), LaunchMode::OverlaySmoke);
        assert_eq!(mode_from(["--start"]), LaunchMode::Start);
    }

    #[test]
    fn start_flag_wins_over_later_flags() {
        // `--start` is an explicit escape hatch: first recognised flag wins.
        assert_eq!(mode_from(["--start", "--settings"]), LaunchMode::Start);
    }

    #[test]
    fn unknown_flags_fall_back_to_start() {
        assert_eq!(mode_from(["--nope", "-x"]), LaunchMode::Start);
    }

    #[test]
    fn first_recognised_flag_wins() {
        assert_eq!(
            mode_from(["--settings", "--capture-full"]),
            LaunchMode::Settings
        );
    }
}
