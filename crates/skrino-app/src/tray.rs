//! System-tray icon and its context menu. Built on `tray-icon` (muda menus).
//! The `TrayIcon` must be kept alive for the icon to remain visible, so it is
//! owned by [`Tray`].

use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder,
    menu::{Menu, MenuId, MenuItem, PredefinedMenuItem},
};

/// A tray-menu selection mapped to an app action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCommand {
    CaptureRegion,
    CaptureFull,
    RecordRegion,
    RecordFull,
    OpenFile,
    StartWindow,
    Settings,
    Quit,
}

pub struct Tray {
    _tray: TrayIcon,
    region: MenuItem,
    full: MenuItem,
    record: MenuItem,
    record_full: MenuItem,
    id_region: MenuId,
    id_full: MenuId,
    id_record: MenuId,
    id_record_full: MenuId,
    id_open: MenuId,
    id_start: MenuId,
    id_settings: MenuId,
    id_quit: MenuId,
}

impl Tray {
    pub fn new(
        hotkey: &str,
        hotkey_full: &str,
        hotkey_record: &str,
        hotkey_record_full: &str,
    ) -> Result<Self, String> {
        let region = MenuItem::new(region_label(hotkey), true, None);
        let full = MenuItem::new(full_label(hotkey_full), true, None);
        let record = MenuItem::new(record_label(hotkey_record), true, None);
        let record_full = MenuItem::new(record_full_label(hotkey_record_full), true, None);
        let open = MenuItem::new("Открыть файл…", true, None);
        let start = MenuItem::new("Открыть окно запуска", true, None);
        let settings = MenuItem::new("Настройки", true, None);
        let quit = MenuItem::new("Выход", true, None);

        let menu = Menu::new();
        let sep1 = PredefinedMenuItem::separator();
        let sep2 = PredefinedMenuItem::separator();
        let sep3 = PredefinedMenuItem::separator();
        menu.append_items(&[
            &region,
            &full,
            &sep1,
            &record,
            &record_full,
            &open,
            &sep2,
            &start,
            &settings,
            &sep3,
            &quit,
        ])
        .map_err(|e| e.to_string())?;

        let (rgba, w, h) = load_icon_rgba();
        let icon = Icon::from_rgba(rgba, w, h).map_err(|e| e.to_string())?;

        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("Skrino: скриншоты")
            .with_icon(icon)
            .build()
            .map_err(|e| e.to_string())?;

        Ok(Self {
            id_region: region.id().clone(),
            id_full: full.id().clone(),
            id_record: record.id().clone(),
            id_record_full: record_full.id().clone(),
            id_open: open.id().clone(),
            id_start: start.id().clone(),
            id_settings: settings.id().clone(),
            id_quit: quit.id().clone(),
            region,
            full,
            record,
            record_full,
            _tray: tray,
        })
    }

    /// Update the capture and recording menu items to show the current hotkeys.
    pub fn set_hotkeys(
        &self,
        hotkey: &str,
        hotkey_full: &str,
        hotkey_record: &str,
        hotkey_record_full: &str,
    ) {
        self.region.set_text(region_label(hotkey));
        self.full.set_text(full_label(hotkey_full));
        self.record.set_text(record_label(hotkey_record));
        self.record_full.set_text(record_full_label(hotkey_record_full));
    }

    /// Map an incoming menu-event id to a command.
    pub fn command_for(&self, id: &MenuId) -> Option<TrayCommand> {
        if id == &self.id_region {
            Some(TrayCommand::CaptureRegion)
        } else if id == &self.id_full {
            Some(TrayCommand::CaptureFull)
        } else if id == &self.id_record {
            Some(TrayCommand::RecordRegion)
        } else if id == &self.id_record_full {
            Some(TrayCommand::RecordFull)
        } else if id == &self.id_open {
            Some(TrayCommand::OpenFile)
        } else if id == &self.id_start {
            Some(TrayCommand::StartWindow)
        } else if id == &self.id_settings {
            Some(TrayCommand::Settings)
        } else if id == &self.id_quit {
            Some(TrayCommand::Quit)
        } else {
            None
        }
    }
}

fn region_label(hotkey: &str) -> String {
    if hotkey.trim().is_empty() {
        "Скриншот области".to_string()
    } else {
        format!("Скриншот области   {hotkey}")
    }
}

fn full_label(hotkey: &str) -> String {
    if hotkey.trim().is_empty() {
        "Скриншот всего экрана".to_string()
    } else {
        format!("Скриншот всего экрана   {hotkey}")
    }
}

fn record_label(hotkey: &str) -> String {
    if hotkey.trim().is_empty() {
        "Записать область".to_string()
    } else {
        format!("Записать область   {hotkey}")
    }
}

fn record_full_label(hotkey: &str) -> String {
    if hotkey.trim().is_empty() {
        "Записать весь экран".to_string()
    } else {
        format!("Записать весь экран   {hotkey}")
    }
}

/// The brand tray icon: `mini-skrino.png` (golden low-poly rhino head,
/// transparent background), decoded and resized to a crisp tray size.
static TRAY_ICON_PNG_BYTES: &[u8] = include_bytes!("../assets/mini-skrino.png");
const TRAY_ICON_SIZE: u32 = 32;

/// Decode and resize the brand icon for the system tray, preserving alpha.
fn load_icon_rgba() -> (Vec<u8>, u32, u32) {
    let img = image::load_from_memory(TRAY_ICON_PNG_BYTES)
        .expect("bundled tray icon PNG must decode")
        .to_rgba8();
    let resized = image::imageops::resize(
        &img,
        TRAY_ICON_SIZE,
        TRAY_ICON_SIZE,
        image::imageops::FilterType::Lanczos3,
    );
    (resized.into_raw(), TRAY_ICON_SIZE, TRAY_ICON_SIZE)
}
