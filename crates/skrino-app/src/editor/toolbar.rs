//! Editor chrome: the tool toolbar, the contextual style row, and the bottom
//! action bar.

use egui::{Color32, CornerRadius, FontId, Pos2, RichText, Sense, Stroke, Vec2};
use egui_phosphor::regular as ph;
use skrino_core::{ArrowHead, Tool};

use super::{EditorSignal, EditorState};
use crate::theme::{Palette, SWATCHES};

/// Minimum height for the bottom-bar action buttons (Копировать/Сохранить/
/// Поделиться/По размеру окна) so they read as proper action buttons rather
/// than egui's default compact size.
const ACTION_BUTTON_HEIGHT: f32 = 36.0;

/// Top row: the drawing tools plus undo/redo.
///
/// The main/frequent tools get an icon + text label pill (Yandex-Screenshots
/// style); the rarer ones stay compact icon-only squares with a tooltip, so
/// the whole row still fits the default ~1120px editor width.
pub fn top_toolbar(state: &mut EditorState, ui: &mut egui::Ui, palette: &Palette) {
    ui.add_space(2.0);
    ui.horizontal_centered(|ui| {
        ui.add_space(4.0);
        // (tool, icon, label/tooltip, is a "main" tool → labeled pill)
        let tools: [(Tool, &str, &str, bool); 11] = [
            (Tool::Select, ph::CURSOR, "Выделение", false),
            (Tool::Arrow, ph::ARROW_UP_RIGHT, "Стрелка", true),
            (Tool::Line, ph::LINE_SEGMENT, "Линия", false),
            (Tool::Rect, ph::SQUARE, "Прямоугольник", true),
            (Tool::Ellipse, ph::CIRCLE, "Эллипс", false),
            (Tool::Text, ph::TEXT_T, "Текст", true),
            (Tool::Marker, ph::HIGHLIGHTER, "Маркер", true),
            (Tool::Pen, ph::PENCIL_SIMPLE, "Карандаш", false),
            (Tool::Blur, ph::DROP, "Размытие", true),
            (Tool::Counter, ph::NUMBER_CIRCLE_ONE, "Счётчик", false),
            (Tool::Crop, ph::CROP, "Обрезать", true),
        ];
        for (tool, icon, label, main) in tools {
            let selected = state.tool == tool;
            let clicked = if main {
                labeled_tool_button(ui, palette, icon, label, selected).clicked()
            } else {
                icon_button(ui, palette, icon, label, selected, true).clicked()
            };
            if clicked {
                state.set_tool(tool);
            }
        }

        toolbar_separator(ui, palette);

        let can_undo = state.doc.can_undo();
        let can_redo = state.doc.can_redo();
        if icon_button(ui, palette, ph::ARROW_ARC_LEFT, "Отменить (Ctrl+Z)", false, can_undo).clicked()
            && can_undo
        {
            state.doc.undo();
            state.selected = None;
        }
        if icon_button(ui, palette, ph::ARROW_ARC_RIGHT, "Повторить (Ctrl+Y)", false, can_redo).clicked()
            && can_redo
        {
            state.doc.redo();
            state.selected = None;
        }
    });
}

/// Second row: colour swatches, thickness, and tool-specific controls.
pub fn context_row(state: &mut EditorState, ui: &mut egui::Ui, palette: &Palette) {
    ui.horizontal_centered(|ui| {
        ui.add_space(6.0);
        // Colour swatches.
        for (name, color) in SWATCHES {
            if swatch(ui, palette, color, name, state.color == color).clicked() {
                state.color = color;
            }
        }

        toolbar_separator(ui, palette);

        // Thickness (hidden for tools that don't stroke).
        if !matches!(state.tool, Tool::Select | Tool::Text | Tool::Counter | Tool::Crop) {
            ui.label(RichText::new("Толщина").color(palette.text_secondary).size(12.0));
            ui.add(egui::Slider::new(&mut state.thickness, 2.0..=12.0).show_value(false));
        }

        // Text size.
        if state.tool == Tool::Text {
            ui.label(RichText::new("Размер").color(palette.text_secondary).size(12.0));
            ui.add(egui::Slider::new(&mut state.text_size, 14.0..=48.0).show_value(false));
        }

        // Arrowhead toggle.
        if state.tool == Tool::Arrow {
            toolbar_separator(ui, palette);
            let filled = state.arrow_head == ArrowHead::Filled;
            if icon_button(ui, palette, ph::ARROW_UP_RIGHT, "Заполненный наконечник", filled, true)
                .clicked()
            {
                state.arrow_head = ArrowHead::Filled;
            }
            if icon_button(ui, palette, ph::ARROW_LINE_UP_RIGHT, "Открытый наконечник", !filled, true)
                .clicked()
            {
                state.arrow_head = ArrowHead::Open;
            }
        }
    });
}

/// Bottom bar: zoom controls (left) and copy/save/share (right).
pub fn bottom_bar(
    state: &mut EditorState,
    ui: &mut egui::Ui,
    palette: &Palette,
    sharing: bool,
) -> Option<EditorSignal> {
    let mut signal = None;
    ui.horizontal_centered(|ui| {
        ui.add_space(6.0);

        // Zoom percentage + slider.
        let mut zoom = state.zoom();
        let pct = (zoom * 100.0).round() as i32;
        ui.label(RichText::new(format!("{pct}%")).color(palette.text_secondary).size(13.0));
        let canvas = state.canvas_rect;
        let resp = ui.add(egui::Slider::new(&mut zoom, 0.25..=4.0).show_value(false));
        if resp.changed() {
            state.set_zoom_centered(zoom, canvas);
        }
        if ui
            .add(
                egui::Button::new(RichText::new(format!("{}  По размеру окна", ph::ARROWS_OUT)))
                    .min_size(Vec2::new(0.0, ACTION_BUTTON_HEIGHT)),
            )
            .on_hover_text("Вписать изображение в окно")
            .clicked()
        {
            state.request_fit();
        }

        // Right-aligned actions.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(6.0);

            // Поделиться (primary accent). Never disabled: the default
            // destination is a local folder, so it always does something.
            let share_enabled = !sharing;
            let share_label = if sharing {
                format!("{}  Отправка…", ph::SPINNER)
            } else {
                format!("{}  Поделиться", ph::SHARE_NETWORK)
            };
            let share_btn = egui::Button::new(RichText::new(share_label).color(palette.accent_fg))
                .fill(if share_enabled {
                    palette.accent
                } else {
                    palette.accent.gamma_multiply(0.4)
                })
                .corner_radius(CornerRadius::same(8))
                .min_size(Vec2::new(0.0, ACTION_BUTTON_HEIGHT));
            let mut resp = ui.add_enabled(share_enabled, share_btn);
            if sharing {
                resp = resp.on_hover_text("Идёт отправка…");
            }
            if resp.clicked() {
                signal = Some(EditorSignal::Share);
            }

            // Сохранить (secondary).
            if ui
                .add(secondary_button(palette, &format!("{}  Сохранить", ph::FLOPPY_DISK)))
                .on_hover_text("Сохранить PNG (Ctrl+S)")
                .clicked()
            {
                signal = Some(EditorSignal::Save);
            }

            // Копировать (secondary).
            if ui
                .add(secondary_button(palette, &format!("{}  Копировать", ph::COPY)))
                .on_hover_text("Скопировать в буфер обмена (Ctrl+C)")
                .clicked()
            {
                signal = Some(EditorSignal::Copy);
            }
        });
    });
    signal
}

// --- widgets ---

fn secondary_button(palette: &Palette, text: &str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text.to_owned()).color(palette.text))
        .fill(palette.surface)
        .stroke(Stroke::new(1.0, palette.border))
        .corner_radius(CornerRadius::same(8))
        .min_size(Vec2::new(0.0, ACTION_BUTTON_HEIGHT))
}

/// An icon + text label pill for a "main" tool (Yandex-Screenshots style);
/// `selected` draws the accent-tinted fill with contrast-correct foreground.
fn labeled_tool_button(
    ui: &mut egui::Ui,
    palette: &Palette,
    icon: &str,
    label: &str,
    selected: bool,
) -> egui::Response {
    let icon_font = FontId::proportional(17.0);
    let label_font = FontId::proportional(12.5);
    let gap = 6.0;
    let pad_x = 10.0;
    let height = 34.0;

    let icon_w = ui
        .painter()
        .layout_no_wrap(icon.to_string(), icon_font.clone(), Color32::WHITE)
        .size()
        .x;
    let label_w = ui
        .painter()
        .layout_no_wrap(label.to_string(), label_font.clone(), Color32::WHITE)
        .size()
        .x;
    let width = pad_x * 2.0 + icon_w + gap + label_w;

    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click());
    let hovered = resp.hovered();
    let (bg, fg) = if selected {
        (palette.accent, palette.accent_fg)
    } else if hovered {
        (palette.surface, palette.text)
    } else {
        (Color32::TRANSPARENT, palette.text)
    };

    let painter = ui.painter();
    if bg != Color32::TRANSPARENT {
        painter.rect_filled(rect, CornerRadius::same(8), bg);
    }
    let icon_pos = Pos2::new(rect.left() + pad_x, rect.center().y);
    painter.text(icon_pos, egui::Align2::LEFT_CENTER, icon, icon_font, fg);
    let label_pos = Pos2::new(rect.left() + pad_x + icon_w + gap, rect.center().y);
    painter.text(label_pos, egui::Align2::LEFT_CENTER, label, label_font, fg);

    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.on_hover_text(label)
}

/// A 32×32 icon button; `selected` draws an accent pill.
fn icon_button(
    ui: &mut egui::Ui,
    palette: &Palette,
    icon: &str,
    tooltip: &str,
    selected: bool,
    enabled: bool,
) -> egui::Response {
    let size = Vec2::splat(34.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click());
    if !enabled {
        resp = resp.on_disabled_hover_text(tooltip);
    }
    let hovered = resp.hovered() && enabled;

    let (bg, fg) = if selected {
        (palette.accent, palette.accent_fg)
    } else if hovered {
        (palette.surface, palette.text)
    } else {
        (Color32::TRANSPARENT, if enabled { palette.text } else { palette.text_secondary })
    };

    let painter = ui.painter();
    if bg != Color32::TRANSPARENT {
        painter.rect_filled(rect.shrink(2.0), CornerRadius::same(8), bg);
    }
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        icon,
        FontId::proportional(19.0),
        fg,
    );
    if enabled {
        resp = resp.on_hover_text(tooltip);
        if hovered {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
    }
    resp
}

/// An 18px colour circle; selected gets an accent ring.
fn swatch(
    ui: &mut egui::Ui,
    palette: &Palette,
    color: skrino_core::Color,
    name: &str,
    selected: bool,
) -> egui::Response {
    let size = Vec2::splat(26.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let center = rect.center();
    let c = super::c32(color);
    let painter = ui.painter();
    if selected {
        painter.circle_stroke(center, 11.0, Stroke::new(2.0, palette.accent));
    } else if resp.hovered() {
        painter.circle_stroke(center, 11.0, Stroke::new(1.0, palette.border));
    }
    painter.circle_filled(center, 8.0, c);
    // Thin outline so white/black swatches read against the panel.
    painter.circle_stroke(center, 8.0, Stroke::new(1.0, palette.border));
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.on_hover_text(name)
}

fn toolbar_separator(ui: &mut egui::Ui, palette: &Palette) {
    ui.add_space(4.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(1.0, 24.0), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::ZERO, palette.border);
    ui.add_space(4.0);
}
