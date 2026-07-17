//! Region-selection overlay. The frozen screenshot is drawn dimmed across the
//! whole ROOT window, with a live drag-selection punched through at full
//! brightness.
//!
//! ARCHITECTURE
//! ------------
//! The overlay is drawn into the **root** viewport's `CentralPanel` — it is NOT a
//! child/immediate viewport. The app transforms the root window into a
//! borderless, always-on-top surface covering the target monitor (see
//! `app.rs`), then this module renders the selection UI into it. Rendering in the
//! root avoids the previous freeze, where an immediate child viewport never
//! painted because the (hidden) root stopped receiving redraws.
//!
//! COORDINATE MODEL
//! ----------------
//! `image` is the target monitor's slice of the virtual-screen capture, in
//! physical pixels. The overlay window covers exactly that monitor, so egui
//! reports pointer positions in logical points relative to the window's
//! top-left; a point maps to a physical image pixel by multiplying by `scale`
//! (see [`crate::transform::OverlayTransform`]). The monitor's offset within the
//! virtual screen is already baked into `image` by cropping in `app.rs`, so a
//! single scale suffices here and the transform tests are unchanged.
//!
//! FAILSAFES (never wedge the screen again)
//! ----------------------------------------
//! * Esc cancels.
//! * Right-click cancels.
//! * Auto-cancel after [`MAX_OVERLAY_SECS`] regardless of input.
//! * `--overlay-smoke` mode auto-cancels after [`SMOKE_SECS`] for safe testing.

use std::time::Instant;

use egui::{
    Color32, CornerRadius, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, TextureHandle,
    TextureOptions, Vec2,
};
use image::RgbaImage;
use skrino_record::RegionPx;

use crate::theme::Palette;
use crate::transform::OverlayTransform;

/// What the overlay's confirmed selection feeds into. The same overlay code and
/// window handling serve both; only the hint text and the confirmed outcome
/// differ (a cropped image for a screenshot, a virtual-screen region for a
/// recording).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OverlayPurpose {
    /// Crop the selection out of the frozen capture and hand it to the editor.
    Screenshot,
    /// Report the selection as a physical-pixel region to start recording.
    Record,
}

/// Hard safety cap: an overlay left up this long auto-cancels so a stuck
/// fullscreen surface can never require a reboot.
const MAX_OVERLAY_SECS: f32 = 120.0;
/// Smoke-test auto-cancel delay (`--overlay-smoke`).
const SMOKE_SECS: f32 = 3.0;

/// What the overlay produced this frame.
pub enum OverlayOutcome {
    /// Still selecting.
    Pending,
    /// User pressed Esc / right-clicked / timed out.
    Cancelled,
    /// Screenshot purpose confirmed: the cropped physical-pixel image is ready
    /// for the editor.
    Screenshot(RgbaImage),
    /// Record purpose confirmed: the selected region in virtual-screen physical
    /// pixels, ready to start recording.
    Region(RegionPx),
}

pub struct OverlayState {
    /// Target monitor's portion of the capture, physical pixels.
    image: RgbaImage,
    transform: OverlayTransform,
    /// What a confirmed selection feeds into (screenshot vs recording).
    purpose: OverlayPurpose,
    /// Target monitor's top-left in virtual-screen physical pixels. Pixel (0, 0)
    /// of `image` maps to this point, so a selection's image-pixel rect plus this
    /// offset gives the virtual-screen [`RegionPx`] the recorder captures.
    mon_x: i32,
    mon_y: i32,
    /// Monitor DPI scale (physical / logical), stored for the control-bar layout
    /// after a record selection is confirmed.
    scale: f32,
    /// Desired root-window outer position (logical points).
    pos_pt: Pos2,
    /// Desired root-window inner size (logical points).
    size_pt: Vec2,
    texture: Option<TextureHandle>,
    /// Selection anchor / current, in overlay-local logical points.
    drag_start: Option<Pos2>,
    drag_cur: Option<Pos2>,
    /// True while the mouse button is held.
    dragging: bool,
    /// Finalised selection awaiting confirm (Enter / ОК / double-click).
    committed: Option<Rect>,
    /// When the overlay became active (for time-based failsafes).
    started: Instant,
    /// Smoke-test mode: auto-cancel after [`SMOKE_SECS`].
    smoke: bool,
}

impl OverlayState {
    /// `image` is the monitor slice; `scale` its DPI scale; `pos_pt`/`size_pt`
    /// the root-window geometry (logical points) the app should apply;
    /// `mon_x`/`mon_y` the monitor's top-left in virtual-screen physical pixels;
    /// `purpose` whether a confirmed selection becomes a screenshot or a
    /// recording region.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        image: RgbaImage,
        scale: f32,
        pos_pt: Pos2,
        size_pt: Vec2,
        mon_x: i32,
        mon_y: i32,
        purpose: OverlayPurpose,
        smoke: bool,
    ) -> Self {
        let scale = if scale > 0.0 { scale } else { 1.0 };
        let (w, h) = (image.width(), image.height());
        Self {
            transform: OverlayTransform::new(scale, w, h),
            image,
            purpose,
            mon_x,
            mon_y,
            scale,
            pos_pt,
            size_pt,
            texture: None,
            drag_start: None,
            drag_cur: None,
            dragging: false,
            committed: None,
            started: Instant::now(),
            smoke,
        }
    }

    /// Monitor DPI scale (physical / logical) of the overlaid monitor.
    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// Desired root-window outer position (logical points).
    pub fn window_pos(&self) -> Pos2 {
        self.pos_pt
    }

    /// Desired root-window inner size (logical points).
    pub fn window_size(&self) -> Vec2 {
        self.size_pt
    }

    /// Draw the overlay into the root viewport and return the outcome. Requests a
    /// repaint every frame so input keeps flowing without any external heartbeat.
    pub fn run(&mut self, ctx: &egui::Context, palette: &Palette) -> OverlayOutcome {
        ctx.request_repaint();
        self.ui(ctx, palette)
    }

    fn ensure_texture(&mut self, ctx: &egui::Context) -> egui::TextureId {
        if self.texture.is_none() {
            let size = [self.image.width() as usize, self.image.height() as usize];
            let color = egui::ColorImage::from_rgba_unmultiplied(size, self.image.as_raw());
            self.texture =
                Some(ctx.load_texture("skrino_overlay_tex", color, TextureOptions::LINEAR));
        }
        self.texture.as_ref().unwrap().id()
    }

    fn ui(&mut self, ctx: &egui::Context, palette: &Palette) -> OverlayOutcome {
        // Time-based failsafes first: these fire even if input is somehow lost.
        let elapsed = self.started.elapsed().as_secs_f32();
        if self.smoke && elapsed >= SMOKE_SECS {
            return OverlayOutcome::Cancelled;
        }
        if elapsed >= MAX_OVERLAY_SECS {
            log::warn!("overlay auto-cancelled after {MAX_OVERLAY_SECS}s failsafe");
            return OverlayOutcome::Cancelled;
        }

        let tex_id = self.ensure_texture(ctx);
        let mut result = OverlayOutcome::Pending;

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(Color32::BLACK))
            .show(ctx, |ui| {
                let full = ui.max_rect();
                let painter = ui.painter_at(full);
                let resp = ui.interact(full, ui.id().with("overlay_area"), Sense::click_and_drag());

                // --- input ---
                let (esc, enter, dbl, secondary) = ctx.input(|i| {
                    (
                        i.key_pressed(egui::Key::Escape),
                        i.key_pressed(egui::Key::Enter),
                        i.pointer.button_double_clicked(egui::PointerButton::Primary),
                        i.pointer.button_clicked(egui::PointerButton::Secondary),
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

                // Hint at top-centre (wording depends on the overlay purpose).
                let hint_pos = Pos2::new(full.center().x, full.min.y + 22.0);
                let hint = match self.purpose {
                    OverlayPurpose::Screenshot => {
                        "Выделите область  •  Esc или правый клик: отмена"
                    }
                    OverlayPurpose::Record => {
                        "Выделите область для записи  •  Esc или правый клик: отмена"
                    }
                };
                draw_hint(&painter, hint_pos, hint, palette);

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
                            egui::RichText::new("ОК").color(palette.accent_fg).strong(),
                        )
                        .fill(palette.accent)
                        .corner_radius(CornerRadius::same(8)),
                    );
                    if ok.clicked() {
                        result = self.confirm();
                    }
                }

                // --- keyboard / gesture confirm & cancel ---
                if esc || secondary {
                    result = OverlayOutcome::Cancelled;
                } else if (enter || dbl) && self.current_selection().is_some() {
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
            && let (Some(a), Some(b)) = (self.drag_start, self.drag_cur)
        {
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
        match self.purpose {
            OverlayPurpose::Screenshot => {
                let cropped = image::imageops::crop_imm(&self.image, x, y, w, h).to_image();
                OverlayOutcome::Screenshot(cropped)
            }
            OverlayPurpose::Record => OverlayOutcome::Region(RegionPx {
                // `x`/`y` are image-pixel offsets into the monitor slice, whose
                // origin is the monitor's virtual-screen top-left.
                x: self.mon_x + x as i32,
                y: self.mon_y + y as i32,
                width: w,
                height: h,
            }),
        }
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
    let galley = painter.layout_no_wrap(text.to_string(), font, palette.accent_fg);
    let pad = Vec2::new(8.0, 4.0);
    let rect = Rect::from_min_size(pos, galley.size() + pad * 2.0);
    painter.rect_filled(rect, CornerRadius::same(6), palette.accent);
    painter.galley(pos + pad, galley, palette.accent_fg);
}

fn draw_hint(painter: &egui::Painter, center: Pos2, text: &str, _palette: &Palette) {
    // The pill sits on top of an arbitrary screenshot, so its colors are
    // theme-independent: always white text on a dark translucent backdrop
    // (palette.text is near-black in the light theme and would be unreadable).
    let font = FontId::proportional(13.0);
    let galley = painter.layout_no_wrap(text.to_string(), font, Color32::WHITE);
    let pad = Vec2::new(12.0, 6.0);
    let size = galley.size() + pad * 2.0;
    let rect = Rect::from_center_size(center + Vec2::new(0.0, size.y * 0.5), size);
    painter.rect_filled(rect, CornerRadius::same(8), Color32::from_black_alpha(170));
    painter.galley(rect.min + pad, galley, Color32::WHITE);
}
