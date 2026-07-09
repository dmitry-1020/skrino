//! The scrollable / zoomable image canvas: draws the screenshot, blur previews,
//! and every annotation, and turns pointer gestures into document mutations.

use egui::{Color32, CornerRadius, Pos2, Sense, Stroke, StrokeKind, Vec2};
use skrino_core::{Annotation, Point, Rect as CRect, Tool};

use super::{Drag, EditorState, MoveState, TextEditor, tools};
use crate::theme::Palette;

const MIN_ZOOM: f32 = 0.1;
const MAX_ZOOM: f32 = 4.0;
/// Minimum image-space distance between successive freehand points.
const POLY_MIN_DIST: f32 = 2.0;

pub fn canvas_ui(state: &mut EditorState, ui: &mut egui::Ui, palette: &Palette) {
    let canvas = ui.max_rect();
    state.canvas_rect = canvas;
    if state.fit_pending {
        // The OS window resize that should precede this fit (opening the
        // editor, or reshaping in from the overlay) is asynchronous, so
        // `canvas` may still be the *old* window's rect for a frame or two.
        // Only fit once the size has read stable across two consecutive
        // frames, to avoid locking in a zoom computed against a stale,
        // too-small canvas (previously showed the image at ~15% instead of
        // filling the window).
        if state.fit_stable_size == Some(canvas.size()) {
            state.fit(canvas);
            state.fit_pending = false;
            state.fit_stable_size = None;
        } else {
            state.fit_stable_size = Some(canvas.size());
            ui.ctx().request_repaint();
        }
    }

    let response = ui.interact(canvas, ui.id().with("canvas"), Sense::click_and_drag());

    handle_zoom_pan(state, ui, &response, canvas);
    refresh_blur_cache(state, ui.ctx());

    let t = state.transform();
    let painter = ui.painter_at(canvas);

    // --- backdrop: soft shadow + image ---
    let img_rect = t.image_rect_to_screen(CRect::from_points(
        Point::new(0.0, 0.0),
        Point::new(state.img_w as f32, state.img_h as f32),
    ));
    painter.rect_filled(
        img_rect.expand(1.0),
        CornerRadius::same(3),
        Color32::from_black_alpha(60),
    );
    if let Some(tex) = &state.texture {
        painter.image(
            tex.id(),
            img_rect,
            egui::Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
            Color32::WHITE,
        );
    }

    // --- blur previews (below the vector layer) ---
    for (rect, _sigma, tex) in &state.blur_cache {
        let r = t.image_rect_to_screen(*rect);
        painter.image(
            tex.id(),
            r,
            egui::Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
            Color32::WHITE,
        );
    }

    // --- committed annotations ---
    let anns = state.doc.annotations();
    for ann in anns {
        tools::draw_annotation(&painter, &t, ann, palette);
    }
    if let Some(idx) = state.selected
        && let Some(ann) = anns.get(idx) {
            tools::draw_selection_outline(&painter, &t, ann, palette);
        }

    // --- live preview of the in-progress gesture ---
    draw_preview(state, &painter, &t, palette);

    // --- input dispatch ---
    dispatch_input(state, ui, &response, canvas);

    // --- crop overlay & controls (own Area for input priority) ---
    crop_overlay(state, ui, &t, canvas, palette);

    // --- inline text editor ---
    text_editor_ui(state, ui, &t, palette);

    set_cursor(state, ui, &response);
}

fn handle_zoom_pan(
    state: &mut EditorState,
    ui: &egui::Ui,
    response: &egui::Response,
    canvas: egui::Rect,
) {
    let (scroll, ctrl, hover) = ui.ctx().input(|i| {
        (
            i.raw_scroll_delta,
            i.modifiers.ctrl || i.modifiers.command,
            i.pointer.hover_pos(),
        )
    });

    if response.hovered() && ctrl && scroll.y != 0.0 {
        if let Some(cursor) = hover {
            let t = state.transform();
            let img_p = t.screen_to_image(cursor);
            let factor = if scroll.y > 0.0 { 1.1 } else { 1.0 / 1.1 };
            let new_zoom = (state.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
            state.zoom = new_zoom;
            state.offset = Vec2::new(
                cursor.x - img_p.x * new_zoom,
                cursor.y - img_p.y * new_zoom,
            );
        }
    } else if response.hovered() && scroll != Vec2::ZERO {
        // Plain wheel = pan.
        state.offset += scroll;
    }

    let _ = canvas;
}

fn dispatch_input(
    state: &mut EditorState,
    ui: &egui::Ui,
    response: &egui::Response,
    _canvas: egui::Rect,
) {
    let t = state.transform();
    let ptr_img = response
        .interact_pointer_pos()
        .map(|p| state.clamp_img(t.screen_to_image(p)));

    match state.tool {
        Tool::Select => select_input(state, response, &t),
        Tool::Arrow | Tool::Line | Tool::Rect | Tool::Ellipse | Tool::Blur => {
            shape_input(state, response, ptr_img)
        }
        Tool::Marker | Tool::Pen => poly_input(state, response, ptr_img),
        Tool::Text => {
            if response.clicked()
                && let Some(p) = ptr_img {
                    commit_text(state);
                    state.text_editor = Some(TextEditor {
                        pos: p,
                        buffer: String::new(),
                        size: state.text_size,
                        color: state.color,
                    });
                }
        }
        Tool::Counter => {
            if response.clicked()
                && let Some(p) = ptr_img {
                    let number = state.doc.next_counter_number();
                    state.doc.add_annotation(Annotation::Counter {
                        pos: p,
                        number,
                        radius: 14.0,
                        color: state.color,
                    });
                }
        }
        Tool::Crop => crop_input(state, response, ptr_img),
    }

    let _ = ui;
}

fn select_input(state: &mut EditorState, response: &egui::Response, t: &crate::transform::CanvasTransform) {
    if response.drag_started() {
        if let Some(pos) = response.interact_pointer_pos() {
            let img = t.screen_to_image(pos);
            match tools::hit_test(state.doc.annotations(), img) {
                Some(idx) => {
                    state.doc.push_undo();
                    state.selected = Some(idx);
                    state.moving = Some(MoveState { index: idx, last: img });
                    state.panning = false;
                }
                None => {
                    state.panning = true;
                    state.selected = None;
                }
            }
        }
    } else if response.dragged() {
        if state.panning {
            state.offset += response.drag_delta();
        } else if let Some(mv) = &mut state.moving
            && let Some(pos) = response.interact_pointer_pos() {
                let img = t.screen_to_image(pos);
                let dx = img.x - mv.last.x;
                let dy = img.y - mv.last.y;
                mv.last = img;
                let idx = mv.index;
                if let Some(ann) = state.doc.annotations_mut().get_mut(idx) {
                    tools::translate(ann, dx, dy);
                }
            }
    } else if response.drag_stopped() {
        state.moving = None;
        state.panning = false;
    } else if response.clicked()
        && let Some(pos) = response.interact_pointer_pos() {
            let img = t.screen_to_image(pos);
            state.selected = tools::hit_test(state.doc.annotations(), img);
        }
}

fn shape_input(state: &mut EditorState, response: &egui::Response, ptr_img: Option<Point>) {
    if response.drag_started() {
        if let Some(p) = ptr_img {
            state.drag = Some(Drag { start: p, cur: p });
        }
    } else if response.dragged() {
        if let (Some(d), Some(p)) = (state.drag.as_mut(), ptr_img) {
            d.cur = p;
        }
    } else if response.drag_stopped()
        && let Some(d) = state.drag.take()
            && let Some(ann) = state.build_shape(d.start, d.cur) {
                state.doc.add_annotation(ann);
            }
}

fn poly_input(state: &mut EditorState, response: &egui::Response, ptr_img: Option<Point>) {
    if response.drag_started() {
        state.poly.clear();
        state.poly_active = true;
        if let Some(p) = ptr_img {
            state.poly.push(p);
        }
    } else if response.dragged() {
        if let Some(p) = ptr_img {
            let far = state
                .poly
                .last()
                .map(|l| (l.x - p.x).hypot(l.y - p.y) >= POLY_MIN_DIST)
                .unwrap_or(true);
            if far {
                state.poly.push(p);
            }
        }
    } else if response.drag_stopped() {
        state.poly_active = false;
        if state.poly.len() >= 2 {
            let points = std::mem::take(&mut state.poly);
            let style = state.current_style();
            let ann = match state.tool {
                Tool::Marker => Annotation::Marker { points, style },
                _ => Annotation::Pen { points, style },
            };
            state.doc.add_annotation(ann);
        } else {
            state.poly.clear();
        }
    }
}

fn crop_input(state: &mut EditorState, response: &egui::Response, ptr_img: Option<Point>) {
    if response.drag_started() {
        state.crop_start = ptr_img;
        state.crop_cur = ptr_img;
        state.crop_active = true;
    } else if response.dragged()
        && ptr_img.is_some() {
            state.crop_cur = ptr_img;
        }
    // On stop we keep the rect and show Apply/Cancel controls.
}

fn draw_preview(
    state: &EditorState,
    painter: &egui::Painter,
    t: &crate::transform::CanvasTransform,
    palette: &Palette,
) {
    if let Some(d) = &state.drag
        && let Some(ann) = state.build_shape(d.start, d.cur) {
            tools::draw_annotation(painter, t, &ann, palette);
        }
    if state.poly_active && state.poly.len() >= 2 {
        let style = state.current_style();
        let ann = match state.tool {
            Tool::Marker => Annotation::Marker {
                points: state.poly.clone(),
                style,
            },
            _ => Annotation::Pen {
                points: state.poly.clone(),
                style,
            },
        };
        tools::draw_annotation(painter, t, &ann, palette);
    }
}

fn crop_overlay(
    state: &mut EditorState,
    ui: &mut egui::Ui,
    t: &crate::transform::CanvasTransform,
    canvas: egui::Rect,
    palette: &Palette,
) {
    if state.tool != Tool::Crop {
        // Still hint an applied crop with a dashed rectangle.
        if let Some(crop) = state.doc.crop() {
            let r = t.image_rect_to_screen(crop);
            ui.painter_at(canvas).rect_stroke(
                r,
                CornerRadius::ZERO,
                Stroke::new(1.5, palette.accent),
                StrokeKind::Middle,
            );
        }
        return;
    }

    let rect_img = match (state.crop_start, state.crop_cur) {
        (Some(a), Some(b)) => Some(CRect::from_points(a, b)),
        _ => state.doc.crop(),
    };
    let Some(rect_img) = rect_img else { return };
    if rect_img.width() < 2.0 || rect_img.height() < 2.0 {
        return;
    }

    let sel = t.image_rect_to_screen(rect_img);
    let painter = ui.painter_at(canvas);
    // Dim outside the crop rect (four bands).
    let dim = Color32::from_black_alpha(120);
    let full = t.image_rect_to_screen(CRect::from_points(
        Point::new(0.0, 0.0),
        Point::new(state.img_w as f32, state.img_h as f32),
    ));
    for band in [
        egui::Rect::from_min_max(full.left_top(), Pos2::new(full.right(), sel.top())),
        egui::Rect::from_min_max(Pos2::new(full.left(), sel.bottom()), full.right_bottom()),
        egui::Rect::from_min_max(Pos2::new(full.left(), sel.top()), Pos2::new(sel.left(), sel.bottom())),
        egui::Rect::from_min_max(Pos2::new(sel.right(), sel.top()), Pos2::new(full.right(), sel.bottom())),
    ] {
        if band.is_positive() {
            painter.rect_filled(band, CornerRadius::ZERO, dim);
        }
    }
    painter.rect_stroke(
        sel,
        CornerRadius::ZERO,
        Stroke::new(1.5, palette.accent),
        StrokeKind::Middle,
    );

    // Apply / Cancel controls under the selection (own Area for input priority).
    if !state.crop_active || state.crop_cur != state.crop_start {
        let anchor = Pos2::new(sel.right() - 172.0, (sel.bottom() + 8.0).min(canvas.bottom() - 40.0));
        egui::Area::new(ui.id().with("crop_ctrls"))
            .order(egui::Order::Foreground)
            .fixed_pos(anchor.max(canvas.left_top()))
            .show(ui.ctx(), |ui| {
                ui.horizontal(|ui| {
                    let apply = ui.add(
                        egui::Button::new(
                            egui::RichText::new("Применить").color(palette.accent_fg),
                        )
                        .fill(palette.accent)
                        .corner_radius(CornerRadius::same(8)),
                    );
                    let cancel = ui.add(
                        egui::Button::new("Отмена").corner_radius(CornerRadius::same(8)),
                    );
                    if apply.clicked() {
                        state.doc.set_crop(Some(rect_img));
                        state.cancel_crop();
                    }
                    if cancel.clicked() {
                        state.cancel_crop();
                    }
                });
            });
    }
}

fn text_editor_ui(
    state: &mut EditorState,
    ui: &mut egui::Ui,
    t: &crate::transform::CanvasTransform,
    palette: &Palette,
) {
    let Some(ed) = &state.text_editor else { return };
    let screen = t.image_to_screen(ed.pos);
    let color = super::c32(ed.color);
    let font_size = (ed.size * t.zoom).clamp(12.0, 64.0);

    let mut commit = false;
    let mut cancel = false;
    let mut buffer = ed.buffer.clone();

    egui::Area::new(ui.id().with("text_editor"))
        .order(egui::Order::Foreground)
        .fixed_pos(screen)
        .show(ui.ctx(), |ui| {
            egui::Frame::new()
                .fill(palette.panel)
                .stroke(Stroke::new(1.0, palette.accent))
                .corner_radius(CornerRadius::same(6))
                .inner_margin(egui::Margin::symmetric(6, 4))
                .show(ui, |ui| {
                    let edit = egui::TextEdit::singleline(&mut buffer)
                        .desired_width(220.0)
                        .font(egui::FontId::proportional(font_size.min(28.0)))
                        .text_color(color)
                        .hint_text("Текст…");
                    let resp = ui.add(edit);
                    resp.request_focus();
                    let (enter, esc) = ui.ctx().input(|i| {
                        (i.key_pressed(egui::Key::Enter), i.key_pressed(egui::Key::Escape))
                    });
                    if enter {
                        commit = true;
                    }
                    if esc {
                        cancel = true;
                    }
                    if resp.lost_focus() && !esc {
                        commit = true;
                    }
                });
        });

    if let Some(ed) = &mut state.text_editor {
        ed.buffer = buffer;
    }
    if cancel {
        state.text_editor = None;
    } else if commit {
        commit_text(state);
    }
}

fn commit_text(state: &mut EditorState) {
    state.finish_text();
}

fn set_cursor(state: &EditorState, ui: &egui::Ui, response: &egui::Response) {
    if !response.hovered() {
        return;
    }
    let icon = match state.tool {
        Tool::Select => {
            if state.panning {
                egui::CursorIcon::Grabbing
            } else {
                egui::CursorIcon::Default
            }
        }
        Tool::Text => egui::CursorIcon::Text,
        _ => egui::CursorIcon::Crosshair,
    };
    ui.ctx().set_cursor_icon(icon);
}

/// Rebuild the blur pixelation textures when the set of blur regions changes.
fn refresh_blur_cache(state: &mut EditorState, ctx: &egui::Context) {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let mut count = 0usize;
    for ann in state.doc.annotations() {
        if let Annotation::Blur { rect, sigma } = ann {
            count += 1;
            rect.min.x.to_bits().hash(&mut hasher);
            rect.min.y.to_bits().hash(&mut hasher);
            rect.max.x.to_bits().hash(&mut hasher);
            rect.max.y.to_bits().hash(&mut hasher);
            sigma.to_bits().hash(&mut hasher);
        }
    }
    let sig = hasher.finish() ^ (count as u64).wrapping_mul(0x9E3779B97F4A7C15);
    // Stale iff the signature drifted, or all blurs were removed but textures
    // still linger in the cache.
    let cache_stale = count == 0 && !state.blur_cache.is_empty();
    if sig == state.blur_sig && !cache_stale {
        return;
    }
    state.blur_sig = sig;
    state.blur_cache.clear();

    let base = state.doc.base();
    let (iw, ih) = (base.width(), base.height());
    for ann in state.doc.annotations() {
        let Annotation::Blur { rect, sigma } = ann else {
            continue;
        };
        let x = rect.min.x.max(0.0).round() as u32;
        let y = rect.min.y.max(0.0).round() as u32;
        let x2 = rect.max.x.round().clamp(0.0, iw as f32) as u32;
        let y2 = rect.max.y.round().clamp(0.0, ih as f32) as u32;
        if x2 <= x || y2 <= y {
            continue;
        }
        let (w, h) = (x2 - x, y2 - y);
        let region = image::imageops::crop_imm(base, x, y, w, h).to_image();
        let block = sigma.max(4.0);
        let dw = ((w as f32 / block).round() as u32).max(1);
        let dh = ((h as f32 / block).round() as u32).max(1);
        let small = image::imageops::resize(&region, dw, dh, image::imageops::FilterType::Triangle);
        let color = egui::ColorImage::from_rgba_unmultiplied(
            [dw as usize, dh as usize],
            small.as_raw(),
        );
        let tex = ctx.load_texture(
            format!("skrino_blur_{x}_{y}_{w}_{h}"),
            color,
            egui::TextureOptions::NEAREST,
        );
        state.blur_cache.push((*rect, *sigma, tex));
    }
}

impl EditorState {
    fn clamp_img(&self, p: Point) -> Point {
        Point::new(
            p.x.clamp(0.0, self.img_w as f32),
            p.y.clamp(0.0, self.img_h as f32),
        )
    }

    /// Build an annotation for the two-point shape tools from a drag.
    fn build_shape(&self, start: Point, cur: Point) -> Option<Annotation> {
        let dx = (cur.x - start.x).abs();
        let dy = (cur.y - start.y).abs();
        if dx < 1.0 && dy < 1.0 {
            return None;
        }
        let style = self.current_style();
        Some(match self.tool {
            Tool::Arrow => Annotation::Arrow {
                from: start,
                to: cur,
                head: self.arrow_head,
                style,
            },
            Tool::Line => Annotation::Line {
                from: start,
                to: cur,
                style,
            },
            Tool::Rect => Annotation::Rect {
                rect: CRect::from_points(start, cur),
                style,
                fill: None,
            },
            Tool::Ellipse => Annotation::Ellipse {
                rect: CRect::from_points(start, cur),
                style,
                fill: None,
            },
            Tool::Blur => Annotation::Blur {
                rect: CRect::from_points(start, cur),
                sigma: (self.thickness * 2.5).clamp(6.0, 24.0),
            },
            _ => return None,
        })
    }

    /// Zoom to fit the current canvas (used by the "По размеру окна" button).
    pub fn request_fit(&mut self) {
        self.fit_pending = true;
        self.fit_stable_size = None;
    }

    /// Read-only accessors used by the toolbar module.
    pub fn zoom(&self) -> f32 {
        self.zoom
    }

    pub fn set_zoom_centered(&mut self, zoom: f32, canvas: egui::Rect) {
        self.zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        self.center(canvas);
    }
}
