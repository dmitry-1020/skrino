//! Tray daemon (`skrino --tray`): a minimal background process that owns the
//! system-tray icon and the global hotkey. It runs a **winit** event loop with
//! NO window and NO eframe/egui/GPU, so idle memory stays tiny.
//!
//! Zero idle polling: `ControlFlow::Wait` parks the loop until something happens.
//! `tray-icon` and `global-hotkey` deliver their events through global handlers
//! that forward to an [`EventLoopProxy`], waking the loop only on real events.
//!
//! Every action just spawns a fresh UI process (`std::env::current_exe()` with a
//! flag) and detaches — the daemon never draws anything itself.
//!
//! Single instance: a named Win32 mutex (`Global\skrino-tray`). A second daemon
//! exits silently. Hotkey reload: the settings UI process signals a named event
//! (`Global\skrino-tray-reload`); a blocked helper thread wakes and asks the
//! loop to re-read the config — no process killing, no polling.

use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::event::{StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::WindowId;

use global_hotkey::{GlobalHotKeyEvent, HotKeyState};
use tray_icon::TrayIconEvent;
use tray_icon::menu::{MenuEvent, MenuId};

use crate::config::AppConfig;
use crate::hotkey::HotkeyRegistration;
use crate::record;
use crate::tray::{Tray, TrayCommand};

/// Left tray-icon clicks within this window of the previous one are ignored
/// (each spawns a one-shot process, so a double-fire would just open two
/// Start windows — cheap but pointless).
const TRAY_CLICK_DEBOUNCE: Duration = Duration::from_millis(500);

/// Wake reasons delivered to the event loop.
#[derive(Debug)]
enum UserEvent {
    Menu(MenuId),
    Hotkey(u32),
    Tray(TrayIconEvent),
    ReloadConfig,
}

struct Daemon {
    tray: Option<Tray>,
    hotkeys: Option<HotkeyRegistration>,
    region_id: Option<u32>,
    full_id: Option<u32>,
    record_id: Option<u32>,
    record_full_id: Option<u32>,
    initialized: bool,
    /// When the last left-click-triggered Start-window spawn happened (debounce).
    last_tray_open: Option<Instant>,
}

impl Daemon {
    fn spawn_ui(&self, flag: &str) {
        if let Ok(exe) = std::env::current_exe() {
            match std::process::Command::new(exe).arg(flag).spawn() {
                Ok(_child) => {} // detached; we never wait on it
                Err(e) => log::error!("failed to spawn UI ({flag}): {e}"),
            }
        }
    }

    /// Recording hotkey / menu action: if a recording is already running, stop
    /// it (stop-toggle IPC); otherwise spawn a fresh recording UI process.
    fn toggle_recording(&self, flag: &str) {
        if record::is_recording_active() {
            record::signal_stop();
        } else {
            self.spawn_ui(flag);
        }
    }

    fn init(&mut self) {
        let config = AppConfig::load();
        match Tray::new(
            &config.hotkey,
            &config.hotkey_full,
            &config.hotkey_record,
            &config.hotkey_record_full,
        ) {
            Ok(t) => self.tray = Some(t),
            Err(e) => log::error!("tray init failed: {e}"),
        }
        let mut hk = match HotkeyRegistration::new() {
            Ok(h) => Some(h),
            Err(e) => {
                log::error!("hotkey manager init failed: {e}");
                None
            }
        };
        if let Some(h) = &mut hk {
            if let Err(e) = h.set_region(&config.hotkey) {
                log::warn!("region hotkey register failed: {e}");
            }
            if let Err(e) = h.set_full(&config.hotkey_full) {
                log::warn!("full hotkey register failed: {e}");
            }
            if let Err(e) = h.set_record(&config.hotkey_record) {
                log::warn!("record hotkey register failed: {e}");
            }
            if let Err(e) = h.set_record_full(&config.hotkey_record_full) {
                log::warn!("record-full hotkey register failed: {e}");
            }
        }
        self.region_id = hk.as_ref().and_then(|h| h.region_id());
        self.full_id = hk.as_ref().and_then(|h| h.full_id());
        self.record_id = hk.as_ref().and_then(|h| h.record_id());
        self.record_full_id = hk.as_ref().and_then(|h| h.record_full_id());
        self.hotkeys = hk;
    }

    /// Re-read the config and re-register every hotkey (after a settings change).
    fn reload(&mut self) {
        let config = AppConfig::load();
        if let Some(h) = &mut self.hotkeys {
            match h.set_region(&config.hotkey) {
                Ok(()) => self.region_id = h.region_id(),
                Err(e) => log::warn!("region hotkey re-register failed: {e}"),
            }
            match h.set_full(&config.hotkey_full) {
                Ok(()) => self.full_id = h.full_id(),
                Err(e) => log::warn!("full hotkey re-register failed: {e}"),
            }
            match h.set_record(&config.hotkey_record) {
                Ok(()) => self.record_id = h.record_id(),
                Err(e) => log::warn!("record hotkey re-register failed: {e}"),
            }
            match h.set_record_full(&config.hotkey_record_full) {
                Ok(()) => self.record_full_id = h.record_full_id(),
                Err(e) => log::warn!("record-full hotkey re-register failed: {e}"),
            }
        }
        if let Some(t) = &self.tray {
            t.set_hotkeys(
                &config.hotkey,
                &config.hotkey_full,
                &config.hotkey_record,
                &config.hotkey_record_full,
            );
        }
        log::info!(
            "daemon reloaded config (region: {}, full: {}, record: {}, record-full: {})",
            config.hotkey,
            config.hotkey_full,
            config.hotkey_record,
            config.hotkey_record_full
        );
    }

    /// Left-click on the tray icon opens the Start window (same action as the
    /// "Открыть окно запуска" menu item). Debounced so a stray double-fire of
    /// the underlying Down/Up events doesn't spawn two processes.
    fn handle_tray_event(&mut self, ev: tray_icon::TrayIconEvent) {
        let tray_icon::TrayIconEvent::Click {
            button: tray_icon::MouseButton::Left,
            button_state: tray_icon::MouseButtonState::Up,
            ..
        } = ev
        else {
            return;
        };
        let now = Instant::now();
        if self
            .last_tray_open
            .is_some_and(|t| now.duration_since(t) < TRAY_CLICK_DEBOUNCE)
        {
            return;
        }
        self.last_tray_open = Some(now);
        self.spawn_ui("--start");
    }
}

impl ApplicationHandler<UserEvent> for Daemon {
    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
        if matches!(cause, StartCause::Init) && !self.initialized {
            event_loop.set_control_flow(ControlFlow::Wait);
            self.init();
            self.initialized = true;
        }
    }

    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}

    fn window_event(&mut self, _e: &ActiveEventLoop, _id: WindowId, _ev: WindowEvent) {}

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Menu(id) => {
                let cmd = self.tray.as_ref().and_then(|t| t.command_for(&id));
                match cmd {
                    Some(TrayCommand::CaptureRegion) => self.spawn_ui("--capture-region"),
                    Some(TrayCommand::CaptureFull) => self.spawn_ui("--capture-full"),
                    Some(TrayCommand::RecordRegion) => self.toggle_recording("--record-region"),
                    Some(TrayCommand::RecordFull) => self.toggle_recording("--record-full"),
                    Some(TrayCommand::OpenFile) => self.spawn_ui("--open-file"),
                    Some(TrayCommand::StartWindow) => self.spawn_ui("--start"),
                    Some(TrayCommand::Settings) => self.spawn_ui("--settings"),
                    Some(TrayCommand::Quit) => {
                        remove_pidfile();
                        event_loop.exit();
                    }
                    None => {}
                }
            }
            UserEvent::Hotkey(id) => {
                if Some(id) == self.region_id {
                    self.spawn_ui("--capture-region");
                } else if Some(id) == self.full_id {
                    self.spawn_ui("--capture-full");
                } else if Some(id) == self.record_id {
                    self.toggle_recording("--record-region");
                } else if Some(id) == self.record_full_id {
                    self.toggle_recording("--record-full");
                }
            }
            UserEvent::Tray(ev) => self.handle_tray_event(ev),
            UserEvent::ReloadConfig => self.reload(),
        }
    }
}

/// Entry point for `skrino --tray`.
pub fn run() {
    if !acquire_single_instance() {
        log::info!("another skrino tray daemon is already running; exiting");
        return;
    }
    write_pidfile();

    let event_loop = match EventLoop::<UserEvent>::with_user_event().build() {
        Ok(el) => el,
        Err(e) => {
            log::error!("failed to build event loop: {e}");
            return;
        }
    };
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();

    // Route tray-menu and global-hotkey events into the loop (wakes on demand).
    let menu_proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        let _ = menu_proxy.send_event(UserEvent::Menu(e.id));
    }));
    let hk_proxy = proxy.clone();
    GlobalHotKeyEvent::set_event_handler(Some(move |e: GlobalHotKeyEvent| {
        if e.state == HotKeyState::Pressed {
            let _ = hk_proxy.send_event(UserEvent::Hotkey(e.id));
        }
    }));
    let tray_proxy = proxy.clone();
    TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
        let _ = tray_proxy.send_event(UserEvent::Tray(e));
    }));

    // Blocked helper thread: wakes only when the settings process signals the
    // reload event, then asks the loop to re-read the config.
    spawn_reload_watcher(proxy);

    let mut app = Daemon {
        tray: None,
        hotkeys: None,
        region_id: None,
        full_id: None,
        record_id: None,
        record_full_id: None,
        initialized: false,
        last_tray_open: None,
    };
    if let Err(e) = event_loop.run_app(&mut app) {
        log::error!("tray event loop error: {e}");
    }
    remove_pidfile();
}

/// Called by the settings UI process after saving a hotkey change: signal a
/// running daemon (if any) to reload. No-op when no daemon is running.
pub fn reload_if_running() {
    signal_reload();
}

// ============================ Windows plumbing ============================

#[cfg(windows)]
const MUTEX_NAME: &str = "Global\\skrino-tray";
#[cfg(windows)]
const RELOAD_EVENT_NAME: &str = "Global\\skrino-tray-reload";

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Acquire the single-instance mutex. Returns false if another daemon owns it.
#[cfg(windows)]
fn acquire_single_instance() -> bool {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use winapi::shared::winerror::ERROR_ALREADY_EXISTS;
    use winapi::um::errhandlingapi::GetLastError;
    use winapi::um::synchapi::CreateMutexW;

    // Held for the whole process lifetime (released automatically on exit).
    static MUTEX_HANDLE: AtomicUsize = AtomicUsize::new(0);

    let name = wide(MUTEX_NAME);
    unsafe {
        let handle = CreateMutexW(std::ptr::null_mut(), 1, name.as_ptr());
        if handle.is_null() {
            // Could not create the mutex; err on the side of running.
            return true;
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return false;
        }
        MUTEX_HANDLE.store(handle as usize, Ordering::SeqCst);
    }
    true
}

/// Spawn a thread that blocks on the reload event and forwards a wake.
#[cfg(windows)]
fn spawn_reload_watcher(proxy: winit::event_loop::EventLoopProxy<UserEvent>) {
    use winapi::um::synchapi::{CreateEventW, WaitForSingleObject};
    use winapi::um::winbase::WAIT_OBJECT_0;

    let name = wide(RELOAD_EVENT_NAME);
    // Auto-reset, initially non-signaled.
    let handle = unsafe { CreateEventW(std::ptr::null_mut(), 0, 0, name.as_ptr()) };
    if handle.is_null() {
        log::warn!("could not create reload event; hotkey live-reload disabled");
        return;
    }
    let handle_usize = handle as usize;
    std::thread::Builder::new()
        .name("skrino-reload-watch".into())
        .spawn(move || {
            let handle = handle_usize as winapi::um::winnt::HANDLE;
            loop {
                let r = unsafe { WaitForSingleObject(handle, winapi::um::winbase::INFINITE) };
                if r != WAIT_OBJECT_0 {
                    break;
                }
                if proxy.send_event(UserEvent::ReloadConfig).is_err() {
                    break; // loop is gone
                }
            }
        })
        .ok();
}

/// Signal a running daemon to reload (open the named event and set it).
#[cfg(windows)]
fn signal_reload() {
    use winapi::um::handleapi::CloseHandle;
    use winapi::um::synchapi::{OpenEventW, SetEvent};
    use winapi::um::winnt::EVENT_MODIFY_STATE;

    let name = wide(RELOAD_EVENT_NAME);
    unsafe {
        let handle = OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr());
        if handle.is_null() {
            return; // no daemon running
        }
        SetEvent(handle);
        CloseHandle(handle);
    }
}

#[cfg(windows)]
fn pidfile_path() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|d| d.join("skrino").join("tray.pid"))
}

#[cfg(windows)]
fn write_pidfile() {
    if let Some(path) = pidfile_path() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&path, std::process::id().to_string());
    }
}

#[cfg(windows)]
fn remove_pidfile() {
    if let Some(path) = pidfile_path() {
        let _ = std::fs::remove_file(path);
    }
}

// --- non-Windows stubs (the app targets Windows; keep it compiling elsewhere) ---

#[cfg(not(windows))]
fn acquire_single_instance() -> bool {
    true
}
#[cfg(not(windows))]
fn spawn_reload_watcher(_proxy: winit::event_loop::EventLoopProxy<UserEvent>) {}
#[cfg(not(windows))]
fn signal_reload() {}
#[cfg(not(windows))]
fn write_pidfile() {}
#[cfg(not(windows))]
fn remove_pidfile() {}
