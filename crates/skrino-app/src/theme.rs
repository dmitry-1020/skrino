//! Design system: fonts, colour palette and a thoroughly customised egui Style.
//! The goal is a calm, modern look (reference: CleanShot / Yandex Screenshots),
//! not egui's default grey.

use egui::{
    Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Stroke, Style, Visuals,
};
use serde::{Deserialize, Serialize};

/// Bytes of the UI font — also handed to `skrino_core::render` so the exported
/// PNG matches the on-screen text exactly.
pub static INTER_REGULAR_BYTES: &[u8] = include_bytes!("../assets/fonts/Inter-Regular.otf");
static INTER_MEDIUM_BYTES: &[u8] = include_bytes!("../assets/fonts/Inter-Medium.otf");
static INTER_SEMIBOLD_BYTES: &[u8] = include_bytes!("../assets/fonts/Inter-SemiBold.otf");

/// Named font family for headings / emphasised labels (Inter SemiBold).
pub const HEADING_FAMILY: &str = "inter-semibold";
/// Named font family for medium-weight labels.
pub const MEDIUM_FAMILY: &str = "inter-medium";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Theme {
    #[default]
    Dark,
    Light,
}

impl Theme {
    pub fn is_dark(self) -> bool {
        matches!(self, Theme::Dark)
    }
}

/// The full resolved palette for one theme.
#[derive(Clone, Copy)]
pub struct Palette {
    pub window: Color32,
    pub panel: Color32,
    /// Slightly raised surface (button rest, toolbar wells).
    pub surface: Color32,
    pub border: Color32,
    pub text: Color32,
    pub text_secondary: Color32,
    pub accent: Color32,
    pub accent_hover: Color32,
    /// Neutral canvas backdrop behind the screenshot.
    pub canvas_bg: Color32,
    pub danger: Color32,
    pub success: Color32,
}

impl Palette {
    pub fn for_theme(theme: Theme) -> Self {
        match theme {
            Theme::Dark => Palette {
                window: Color32::from_rgb(0x1E, 0x1F, 0x24),
                panel: Color32::from_rgb(0x26, 0x27, 0x2E),
                surface: Color32::from_rgb(0x2E, 0x30, 0x38),
                border: Color32::from_rgb(0x35, 0x36, 0x3E),
                text: Color32::from_rgb(0xE8, 0xE9, 0xED),
                text_secondary: Color32::from_rgb(0x9A, 0x9C, 0xA6),
                accent: Color32::from_rgb(0x35, 0x74, 0xF0),
                accent_hover: Color32::from_rgb(0x4A, 0x86, 0xF5),
                canvas_bg: Color32::from_rgb(0x17, 0x18, 0x1C),
                danger: Color32::from_rgb(0xE8, 0x48, 0x4D),
                success: Color32::from_rgb(0x2E, 0xAE, 0x5E),
            },
            Theme::Light => Palette {
                window: Color32::from_rgb(0xF5, 0xF6, 0xF8),
                panel: Color32::from_rgb(0xFF, 0xFF, 0xFF),
                surface: Color32::from_rgb(0xEE, 0xF0, 0xF3),
                border: Color32::from_rgb(0xE2, 0xE3, 0xE8),
                text: Color32::from_rgb(0x1A, 0x1B, 0x1F),
                text_secondary: Color32::from_rgb(0x6A, 0x6C, 0x74),
                accent: Color32::from_rgb(0x35, 0x74, 0xF0),
                accent_hover: Color32::from_rgb(0x2A, 0x63, 0xD8),
                canvas_bg: Color32::from_rgb(0xDD, 0xDF, 0xE4),
                danger: Color32::from_rgb(0xE8, 0x48, 0x4D),
                success: Color32::from_rgb(0x1F, 0x9B, 0x50),
            },
        }
    }
}

/// The eight editor swatch colours (label, colour).
pub const SWATCHES: [(&str, skrino_core::Color); 8] = [
    ("Чёрный", skrino_core::Color::rgb(0x1A, 0x1A, 0x1A)),
    ("Белый", skrino_core::Color::rgb(0xFF, 0xFF, 0xFF)),
    ("Синий", skrino_core::Color::rgb(0x35, 0x74, 0xF0)),
    ("Жёлтый", skrino_core::Color::rgb(0xFF, 0xC4, 0x02)),
    ("Красный", skrino_core::Color::rgb(0xE8, 0x48, 0x4D)),
    ("Зелёный", skrino_core::Color::rgb(0x2E, 0xAE, 0x5E)),
    ("Фиолетовый", skrino_core::Color::rgb(0x8E, 0x5B, 0xD8)),
    ("Серый", skrino_core::Color::rgb(0x8C, 0x8C, 0x8C)),
];

/// Build the font set: Inter (with full Cyrillic) as the proportional face plus
/// Phosphor icon glyphs, and named families for the heavier weights.
pub fn build_fonts() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();

    fonts
        .font_data
        .insert("inter".into(), FontData::from_static(INTER_REGULAR_BYTES).into());
    fonts
        .font_data
        .insert(MEDIUM_FAMILY.into(), FontData::from_static(INTER_MEDIUM_BYTES).into());
    fonts
        .font_data
        .insert(HEADING_FAMILY.into(), FontData::from_static(INTER_SEMIBOLD_BYTES).into());

    // Inter is the primary proportional face; keep egui's default fallbacks
    // (for any glyph Inter lacks) behind it.
    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, "inter".into());

    fonts
        .families
        .insert(FontFamily::Name(HEADING_FAMILY.into()), vec![HEADING_FAMILY.into(), "inter".into()]);
    fonts
        .families
        .insert(FontFamily::Name(MEDIUM_FAMILY.into()), vec![MEDIUM_FAMILY.into(), "inter".into()]);

    // Phosphor icon glyphs. Inter maps a few Private Use Area codepoints
    // itself and would shadow those icons (e.g. FOLDER_OPEN, GEAR_SIX), so
    // the icon font goes FIRST in the proportional family: it contains no
    // text glyphs, so regular text still falls through to Inter.
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    if let Some(prop) = fonts.families.get_mut(&FontFamily::Proportional)
        && let Some(pos) = prop
            .iter()
            .position(|n| n.to_lowercase().contains("phosphor"))
    {
        let name = prop.remove(pos);
        prop.insert(0, name);
    }

    fonts
}

/// A `FontId` in the SemiBold heading family.
pub fn heading_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(HEADING_FAMILY.into()))
}

/// Apply the full custom style for `theme` to the context.
pub fn apply(ctx: &egui::Context, theme: Theme) {
    let p = Palette::for_theme(theme);
    let mut style = Style::default();

    let mut v = if theme.is_dark() {
        Visuals::dark()
    } else {
        Visuals::light()
    };

    v.override_text_color = Some(p.text);
    v.panel_fill = p.panel;
    v.window_fill = p.window;
    v.extreme_bg_color = if theme.is_dark() {
        Color32::from_rgb(0x18, 0x19, 0x1D)
    } else {
        Color32::from_rgb(0xEC, 0xED, 0xF0)
    };
    v.faint_bg_color = p.surface;
    v.window_stroke = Stroke::new(1.0, p.border);
    v.window_corner_radius = CornerRadius::same(10);
    v.menu_corner_radius = CornerRadius::same(10);
    v.window_shadow.color = Color32::from_black_alpha(if theme.is_dark() { 90 } else { 40 });
    v.popup_shadow.color = Color32::from_black_alpha(if theme.is_dark() { 90 } else { 40 });
    v.hyperlink_color = p.accent;

    let radius = CornerRadius::same(8);

    v.widgets.noninteractive.bg_fill = p.panel;
    v.widgets.noninteractive.weak_bg_fill = p.panel;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.noninteractive.corner_radius = radius;

    v.widgets.inactive.bg_fill = p.surface;
    v.widgets.inactive.weak_bg_fill = p.surface;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, p.border);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.inactive.corner_radius = radius;
    v.widgets.inactive.expansion = 0.0;

    v.widgets.hovered.bg_fill = p.surface;
    v.widgets.hovered.weak_bg_fill = if theme.is_dark() {
        Color32::from_rgb(0x37, 0x39, 0x43)
    } else {
        Color32::from_rgb(0xE4, 0xE6, 0xEB)
    };
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, p.accent_hover.gamma_multiply(0.5));
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.hovered.corner_radius = radius;
    v.widgets.hovered.expansion = 0.0;

    v.widgets.active.bg_fill = p.accent;
    v.widgets.active.weak_bg_fill = p.accent;
    v.widgets.active.bg_stroke = Stroke::new(1.0, p.accent);
    v.widgets.active.fg_stroke = Stroke::new(1.5, Color32::WHITE);
    v.widgets.active.corner_radius = radius;
    v.widgets.active.expansion = 0.0;

    v.widgets.open.bg_fill = p.surface;
    v.widgets.open.weak_bg_fill = p.surface;
    v.widgets.open.bg_stroke = Stroke::new(1.0, p.border);
    v.widgets.open.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.open.corner_radius = radius;

    v.selection.bg_fill = p.accent.gamma_multiply(0.35);
    v.selection.stroke = Stroke::new(1.0, p.accent);

    style.visuals = v;

    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(12.0, 6.0);
    style.spacing.window_margin = egui::Margin::same(14);
    style.spacing.menu_margin = egui::Margin::same(8);
    style.spacing.interact_size = egui::vec2(24.0, 28.0);

    // Comfortable default text sizes.
    use egui::TextStyle;
    style.text_styles = [
        (TextStyle::Small, FontId::proportional(11.0)),
        (TextStyle::Body, FontId::proportional(14.0)),
        (TextStyle::Button, FontId::proportional(14.0)),
        (TextStyle::Heading, heading_font(20.0)),
        (TextStyle::Monospace, FontId::monospace(13.0)),
    ]
    .into();

    ctx.set_style(style);
}
