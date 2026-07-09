//! Region-selection overlay: a borderless, always-on-top viewport that shows the
//! frozen screenshot dimmed, with a live drag-selection punched through at full
//! brightness.
//!
//! COORDINATE MODEL
//! ----------------
//! `capture.image` is in physical pixels; `origin_x/origin_y` are the
//! virtual-screen physical coordinates of image pixel (0,0). The overlay
//! viewport is placed so its top-left coincides with that image origin, and its
//! logical size is `image_size / scale`. egui then reports pointer positions in
//! logical points relative to the viewport top-left, so a point maps to a
//! physical image pixel by multiplying by `scale` (see
//! [`crate::transform::OverlayTransform`]).
//!
//! v1 CAVEAT: a single `scale` is used for the whole span. On a multi-monitor
//! setup with mixed DPI the mapping is exact only on monitors sharing the chosen
//! scale (the primary's). Cross-DPI refinement is left to the integrator.

use egui::{
    Color32, CornerRadius, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, TextureHandle,
    TextureOptions, Vec2, ViewportBuilder, ViewportId,
};
use image::RgbaImage;
use skrino_capture::VirtualScreenCapture;

use crate::theme::Palette;
use crate::transform::OverlayTransform;

/// What the overlay produced this frame.
pub enum OverlayOutcome {
    /// Still selecting.
    Pending,
    /// User pressed Esc / dismissed.
    Cancelled,
    /// Confirmed; the cropped physical-pixel image is ready for the editor.
    Confirmed(RgbaImage),
}

pub struct OverlayState {
    capture: VirtualScreenCapture,
    transform: OverlayTransform,
    /// Logical position (points) of the viewport top-left.
    origin_pt: Pos2,
    /// Logical size (points) of the overlay.
    size_pt: Vec2,
    texture: Option<TextureHandle>,
    /// Selection anchor / current, in overlay-local logical points.
    drag_start: Option<Pos2>,
    drag_cur: Option<Pos2>,
    /// True while the mouse button is held.
    dragging: bool,
    /// Finalised selection awaiting confirm (Enter / ОК / double-click).
    committed: Option<Rect>,
}

impl OverlayState {
    pub fn new(capture: VirtualScreenCapture) -> Self {
        // Choose the effective scale: the primary monitor's, falling back to the
        // first monitor or 1.0.
        let scale = capture
            .monitors
            .iter()
            .find(|m| m.is_primary)
            .or_else(|| capture.monitors.first())
            .map(|m| m.scale_factor)
            .filter(|s| *s > 0.0)
            .unwrap_or(1.0);

        let (w, h) = (capture.image.width(), capture.image.height());
        let transform = OverlayTransform::new(scale, w, h);
        let origin_pt = Pos2::new(
            capture.origin_x as f32 / scale,
            capture.origin_y as f32 / scale,
        );
        let size_pt = Vec2::new(w as f32 / scale, h as f32 / scale);

        Self {
            capture,
            transform,
            origin_pt,
            size_pt,
            texture: None,
            drag_start: None,
            drag_cur: None,
            dragging: false,
            committed: None,
        }
    }

    /// Render the overlay in its own immediate viewport and return the outcome.
    pub fn run(&mut self, ctx: &egui::Context, palette: &Palette) -> OverlayOutcome {
        let builder = ViewportBuilder::default()
            .with_title("Skrino — выделение")
            .with_position(self.origin_pt)
            .with_inner_size(self.size_pt)
            .with_decorations(false)
            .with_resizable(false)
            .with_transparent(true)
            .with_taskbar(false)
            .with_always_on_top();

        let vid = ViewportId::from_hash_of("skrino_overlay");
        
        ctx.show_viewport_immediate(vid, builder, |ctx, _class| {
            self.ui(ctx, palette)
        })
    }

    fn ensure_texture(&mut self, ctx: &egui::Context) -> egui::TextureId {
        if self.texture.is_none() {
            let size = [
                self.capture.image.width() as usize,
                self.capture.image.height() as usize,
            ];
            let color = egui::ColorImage::from_rgba_unmultiplied(size, self.capture.image.as_raw());
            self.texture = Some(ctx.load_texture("skrino_overlay_tex", color, TextureOptions::LINEAR));
        }
        // Safe: just populated above.
        self.texture.as_ref().unwrap().id()
    }

    fn ui(&mut self, ctx: &egui::Context, palette: &Palette) -> OverlayOutcome {
        let tex_id = self.ensure_texture(ctx);
        let mut result = OverlayOutcome::Pending;

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(Color32::TRANSPARENT))
            .show(ctx, |ui| {
                let full = ui.max_rect();
                let painter = ui.painter_at(full);
                let resp = ui.interact(full, ui.id().with("overlay_area"), Sense::click_and_drag());

                // --- input ---
                let (esc, enter, dbl) = ctx.input(|i| {
                    (
                        i.key_pressed(egui::Key::Escape),
                        i.key_pressed(egui::Key::Enter),
                        i.pointer.button_double_clicked(egui::PointerButton::Primary),
                    )
                });

                if resp.drag_started() {
                    self.drag_start = resp.interact_pointer_pos();
                    self.drag_cur = self.drag_start;
                    self.dragging = true;
                    self.committed = None;
                } else if resp.dragged() {
                    if let Some(p) = resp.interact_pointer_pos() {
                        self.drag_cur = Some(p);
                    }
                } else if resp.drag_stopped() {
                    self.dragging = false;
                    if let (Some(a), Some(b)) = (self.drag_start, self.drag_cur) {
                        let r = Rect::from_two_pos(a, b);
                        if r.width() >= 3.0 && r.height() >= 3.0 {
                            self.committed = Some(r);
                        }
                    }
                }

                // --- background image + dim ---
                painter.image(
                    tex_id,
                    full,
                    Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                    Color32::WHITE,
                );
                painter.rect_filled(full, CornerRadius::ZERO, Color32::from_black_alpha(115));

                // Current live/committed selection rect (local points).
                let selection = self.current_selection();

                if let Some(sel) = selection {
                    // Redraw the selected region at full brightness (punch-through).
                    let uv = Rect::from_min_max(
                        Pos2::new(
                            (sel.min.x - full.min.x) / full.width(),
                            (sel.min.y - full.min.y) / full.height(),
                        ),
                        Pos2::new(
                            (sel.max.x - full.min.x) / full.width(),
                            (sel.max.y - full.min.y) / full.height(),
                        ),
                    );
                    painter.image(tex_id, sel, uv, Color32::WHITE);

                    // Accent border + corner handles.
                    painter.rect_stroke(
                        sel,
                        CornerRadius::ZERO,
                        Stroke::new(1.5, palette.accent),
                        StrokeKind::Middle,
                    );
                    draw_handles(&painter, sel, palette.accent);

                    // Dimensions badge (physical pixels) near the selection.
                    let (_, _, w, h) = self.transform.selection_to_pixel_rect(sel);
                    let badge_pos = Pos2::new(sel.min.x, (sel.min.y - 26.0).max(full.min.y + 4.0));
                    draw_badge(&painter, badge_pos, &format!("{w} × {h}"), palette);
                } else if let Some(cursor) = ctx.pointer_hover_pos() {
                    // Crosshair before the first drag.
                    let s = Stroke::new(1.0, Color32::from_white_alpha(120));
                    painter.line_segment([Pos2::new(full.min.x, cursor.y), Pos2::new(full.max.x, cursor.y)], s);
                    painter.line_segment([Pos2::new(cursor.x, full.min.y), Pos2::new(cursor.x, full.max.y)], s);
                }

                // Hint at top-centre.
                let hint_pos = Pos2::new(full.center().x, full.min.y + 22.0);
                draw_hint(&painter, hint_pos, "Выделите область  •  Esc — отмена", palette);

                // "ОК" button once a selection is committed.
                if let Some(sel) = self.committed {
                    let btn_size = Vec2::new(66.0, 30.0);
                    let btn_pos = Pos2::new(
                        (sel.max.x - btn_size.x).max(full.min.x + 4.0),
                        (sel.max.y + 8.0).min(full.max.y - btn_size.y - 4.0),
                    );
                    let btn_rect = Rect::from_min_size(btn_pos, btn_size);
                    let ok = ui.put(
                        btn_rect,
                        egui::Button::new(
                            egui::RichText::new("ОК").color(Color32::WHITE).strong(),
                        )
                        .fill(palette.accent)
                        .corner_radius(CornerRadius::same(8)),
                    );
                    if ok.clicked() {
                        result = self.confirm();
                    }
                }

                // --- keyboard / gesture confirm & cancel ---
                if esc {
                    result = OverlayOutcome::Cancelled;
                } else if (enter || dbl) && self.current_selection().is_some() {
                    // Promote a live drag to committed if needed, then confirm.
                    if self.committed.is_none() {
                        self.committed = self.current_selection();
                    }
                    result = self.confirm();
                }
            });

        result
    }

    /// The selection rect in overlay-local points (committed or live drag).
    fn current_selection(&self) -> Option<Rect> {
        if let Some(c) = self.committed {
            return Some(c);
        }
        if self.dragging
            && let (Some(a), Some(b)) = (self.drag_start, self.drag_cur) {
                let r = Rect::from_two_pos(a, b);
                if r.width() >= 1.0 && r.height() >= 1.0 {
                    return Some(r);
                }
            }
        None
    }

    fn confirm(&self) -> OverlayOutcome {
        let Some(sel) = self.current_selection() else {
            return OverlayOutcome::Cancelled;
        };
        let (x, y, w, h) = self.transform.selection_to_pixel_rect(sel);
        if w == 0 || h == 0 {
            return OverlayOutcome::Cancelled;
        }
        let cropped = image::imageops::crop_imm(&self.capture.image, x, y, w, h).to_image();
        OverlayOutcome::Confirmed(cropped)
    }
}

fn draw_handles(painter: &egui::Painter, r: Rect, accent: Color32) {
    let hs = 4.0;
    for c in [r.left_top(), r.right_top(), r.left_bottom(), r.right_bottom()] {
        let hr = Rect::from_center_size(c, Vec2::splat(hs * 2.0));
        painter.rect_filled(hr, CornerRadius::same(2), accent);
        painter.rect_stroke(hr, CornerRadius::same(2), Stroke::new(1.0, Color32::WHITE), StrokeKind::Middle);
    }
}

fn draw_badge(painter: &egui::Painter, pos: Pos2, text: &str, palette: &Palette) {
    let font = FontId::proportional(13.0);
    let galley = painter.layout_no_wrap(text.to_string(), font, Color32::WHITE);
    let pad = Vec2::new(8.0, 4.0);
    let rect = Rect::from_min_size(pos, galley.size() + pad * 2.0);
    painter.rect_filled(rect, CornerRadius::same(6), palette.accent);
    painter.galley(pos + pad, galley, Color32::WHITE);
}

fn draw_hint(painter: &egui::Painter, center: Pos2, text: &str, palette: &Palette) {
    let font = FontId::proportional(13.0);
    let galley = painter.layout_no_wrap(text.to_string(), font, palette.text);
    let pad = Vec2::new(12.0, 6.0);
    let size = galley.size() + pad * 2.0;
    let rect = Rect::from_center_size(center + Vec2::new(0.0, size.y * 0.5), size);
    painter.rect_filled(rect, CornerRadius::same(8), Color32::from_black_alpha(160));
    painter.galley(rect.min + pad, galley, palette.text);
}
