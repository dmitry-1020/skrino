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

/// Subtypes of the "Фигуры" group, in contextual-row / cycling order.
/// (core tool, icon, label used both as the subtype tooltip and, for
/// `Tool::Counter`, the user-facing name — the core enum variant stays
/// `Counter`, only the UI-facing word changed to "Метка").
const SHAPE_SUBTYPES: [(Tool, &str, &str); 4] = [
    (Tool::Rect, ph::SQUARE, "Прямоугольник"),
    (Tool::Ellipse, ph::CIRCLE, "Эллипс"),
    (Tool::Line, ph::LINE_SEGMENT, "Линия"),
    (Tool::Counter, ph::NUMBER_CIRCLE_ONE, "Метка"),
];

/// Subtypes of the "Маркер" group.
const MARKER_SUBTYPES: [(Tool, &str, &str); 2] = [
    (Tool::Marker, ph::HIGHLIGHTER, "Маркер"),
    (Tool::Pen, ph::PENCIL_SIMPLE, "Карандаш"),
];

fn is_shape_tool(tool: Tool) -> bool {
    SHAPE_SUBTYPES.iter().any(|(t, _, _)| *t == tool)
}

fn is_marker_tool(tool: Tool) -> bool {
    MARKER_SUBTYPES.iter().any(|(t, _, _)| *t == tool)
}

/// Icon for the currently-selected subtype of a group (falls back to the
/// group's first subtype if the given tool somehow isn't a member).
fn subtype_icon(subtypes: &[(Tool, &'static str, &'static str)], current: Tool) -> &'static str {
    subtypes
        .iter()
        .find(|(t, _, _)| *t == current)
        .or_else(|| subtypes.first())
        .map(|(_, icon, _)| *icon)
        .unwrap_or("")
}

/// Top row: the drawing tools plus undo/redo.
///
/// The main/frequent tools get an icon + text label pill (Yandex-Screenshots
/// style); "Выделение" stays a compact icon-only square. "Фигуры" and
/// "Маркер" are *groups*: the pill activates the group's last-used subtype,
/// and the contextual row below (see [`context_row`]) offers the subtype
/// picker while a group is active. This keeps the row short enough to fit the
/// default ~1120px editor width even though it now covers all 11 core tools.
pub fn top_toolbar(state: &mut EditorState, ui: &mut egui::Ui, palette: &Palette) {
    ui.add_space(2.0);
    ui.horizontal_centered(|ui| {
        ui.add_space(4.0);

        if icon_button(ui, palette, ph::CURSOR, "Выделение", state.tool == Tool::Select, true)
            .clicked()
        {
            state.set_tool(Tool::Select);
        }

        // (tool, icon, label) — single-tool pills.
        let singles: [(Tool, &str, &str); 2] = [
            (Tool::Arrow, ph::ARROW_UP_RIGHT, "Стрелка"),
            (Tool::Text, ph::TEXT_T, "Текст"),
        ];
        for (tool, icon, label) in singles {
            if labeled_tool_button(ui, palette, icon, label, state.tool == tool).clicked() {
                state.set_tool(tool);
            }
        }

        // "Фигуры" group pill: activates the last-used subtype (default
        // Прямоугольник); the pill's own icon tracks that subtype.
        let shape_active = is_shape_tool(state.tool);
        let shape_icon = subtype_icon(&SHAPE_SUBTYPES, state.toolbox_last_shape());
        if labeled_tool_button(ui, palette, shape_icon, "Фигуры", shape_active).clicked() {
            state.set_tool(state.toolbox_last_shape());
        }

        // "Маркер" group pill: same pattern.
        let marker_active = is_marker_tool(state.tool);
        let marker_icon = subtype_icon(&MARKER_SUBTYPES, state.toolbox_last_marker());
        if labeled_tool_button(ui, palette, marker_icon, "Маркер", marker_active).clicked() {
            state.set_tool(state.toolbox_last_marker());
        }

        let trailing: [(Tool, &str, &str); 2] = [
            (Tool::Blur, ph::DROP, "Размытие"),
            (Tool::Crop, ph::CROP, "Обрезать"),
        ];
        for (tool, icon, label) in trailing {
            if labeled_tool_button(ui, palette, icon, label, state.tool == tool).clicked() {
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
///
/// While a group (Фигуры / Маркер) is active, this row leads with that
/// group's subtype picker — selecting a subtype switches the actual core
/// `Tool`, so the canvas drawing/gesture logic in `canvas.rs` needs no
/// changes at all.
pub fn context_row(state: &mut EditorState, ui: &mut egui::Ui, palette: &Palette) {
    ui.horizontal_centered(|ui| {
        ui.add_space(6.0);

        let group_subtypes = if is_shape_tool(state.tool) {
            Some(&SHAPE_SUBTYPES[..])
        } else if is_marker_tool(state.tool) {
            Some(&MARKER_SUBTYPES[..])
        } else {
            None
        };

        if let Some(subtypes) = group_subtypes {
            // Group layout: subtype pickers, then thickness, then colours.
            subtype_picker(state, ui, palette, subtypes);
            toolbar_separator(ui, palette);
            if !matches!(state.tool, Tool::Counter) {
                ui.label(RichText::new("Толщина").color(palette.text_secondary).size(12.0));
                ui.add(egui::Slider::new(&mut state.thickness, 2.0..=12.0).show_value(false));
                toolbar_separator(ui, palette);
            }
            for (name, color) in SWATCHES {
                if swatch(ui, palette, color, name, state.color == color).clicked() {
                    state.color = color;
                }
            }
            return;
        }

        // Non-group layout (Выделение/Стрелка/Текст/Размытие/Обрезать):
        // colours, then thickness, then the tool's own extra controls —
        // unchanged from before the grouping.
        for (name, color) in SWATCHES {
            if swatch(ui, palette, color, name, state.color == color).clicked() {
                state.color = color;
            }
        }

        toolbar_separator(ui, palette);

        if !matches!(state.tool, Tool::Select | Tool::Text | Tool::Crop) {
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

/// Row of icon-only subtype buttons for a group; the active subtype gets the
/// accent pill (dark foreground, per `palette.accent_fg`'s contrast rule).
fn subtype_picker(
    state: &mut EditorState,
    ui: &mut egui::Ui,
    palette: &Palette,
    subtypes: &[(Tool, &'static str, &'static str)],
) {
    for &(tool, icon, label) in subtypes {
        if icon_button(ui, palette, icon, label, state.tool == tool, true).clicked() {
            state.set_tool(tool);
        }
    }
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
