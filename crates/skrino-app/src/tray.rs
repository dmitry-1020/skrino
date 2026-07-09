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
    OpenFile,
    Settings,
    Quit,
}

pub struct Tray {
    _tray: TrayIcon,
    id_region: MenuId,
    id_full: MenuId,
    id_open: MenuId,
    id_settings: MenuId,
    id_quit: MenuId,
}

impl Tray {
    pub fn new() -> Result<Self, String> {
        let region = MenuItem::new("Скриншот области  (PrtScr)", true, None);
        let full = MenuItem::new("Скриншот всего экрана", true, None);
        let open = MenuItem::new("Открыть файл…", true, None);
        let settings = MenuItem::new("Настройки", true, None);
        let quit = MenuItem::new("Выход", true, None);

        let menu = Menu::new();
        let sep1 = PredefinedMenuItem::separator();
        let sep2 = PredefinedMenuItem::separator();
        menu.append_items(&[&region, &full, &open, &sep1, &settings, &sep2, &quit])
            .map_err(|e| e.to_string())?;

        let (rgba, w, h) = make_icon_rgba();
        let icon = Icon::from_rgba(rgba, w, h).map_err(|e| e.to_string())?;

        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("Skrino — скриншоты")
            .with_icon(icon)
            .build()
            .map_err(|e| e.to_string())?;

        Ok(Self {
            _tray: tray,
            id_region: region.id().clone(),
            id_full: full.id().clone(),
            id_open: open.id().clone(),
            id_settings: settings.id().clone(),
            id_quit: quit.id().clone(),
        })
    }

    /// Map an incoming menu-event id to a command.
    pub fn command_for(&self, id: &MenuId) -> Option<TrayCommand> {
        if id == &self.id_region {
            Some(TrayCommand::CaptureRegion)
        } else if id == &self.id_full {
            Some(TrayCommand::CaptureFull)
        } else if id == &self.id_open {
            Some(TrayCommand::OpenFile)
        } else if id == &self.id_settings {
            Some(TrayCommand::Settings)
        } else if id == &self.id_quit {
            Some(TrayCommand::Quit)
        } else {
            None
        }
    }
}

/// A 32×32 tray icon: accent rounded square with white corner brackets
/// suggesting a selection marquee. Generated so we don't ship a binary asset.
fn make_icon_rgba() -> (Vec<u8>, u32, u32) {
    const S: usize = 32;
    let accent = [0x35u8, 0x74, 0xF0, 0xFF];
    let white = [0xFFu8, 0xFF, 0xFF, 0xFF];
    let clear = [0u8, 0, 0, 0];
    let mut buf = vec![0u8; S * S * 4];

    let put = |buf: &mut [u8], x: usize, y: usize, c: [u8; 4]| {
        let i = (y * S + x) * 4;
        buf[i..i + 4].copy_from_slice(&c);
    };

    // Rounded-square background.
    let r: f32 = 7.0;
    for y in 0..S {
        for x in 0..S {
            let inside = rounded_inside(x as f32, y as f32, S as f32, r);
            put(&mut buf, x, y, if inside { accent } else { clear });
        }
    }

    // White corner brackets (marquee look).
    let lo = 9;
    let hi = 22;
    let arm = 5;
    for t in 0..=arm {
        // top-left
        put(&mut buf, lo + t, lo, white);
        put(&mut buf, lo, lo + t, white);
        // top-right
        put(&mut buf, hi - t, lo, white);
        put(&mut buf, hi, lo + t, white);
        // bottom-left
        put(&mut buf, lo + t, hi, white);
        put(&mut buf, lo, hi - t, white);
        // bottom-right
        put(&mut buf, hi - t, hi, white);
        put(&mut buf, hi, hi - t, white);
    }

    (buf, S as u32, S as u32)
}

/// Is pixel (x,y) inside a rounded square of side `s` with corner radius `r`?
fn rounded_inside(x: f32, y: f32, s: f32, r: f32) -> bool {
    let (px, py) = (x + 0.5, y + 0.5);
    let cx = px.clamp(r, s - r);
    let cy = py.clamp(r, s - r);
    let dx = px - cx;
    let dy = py - cy;
    dx * dx + dy * dy <= r * r + 0.5
}
