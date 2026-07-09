//! The main editor: cropped screenshot as a `skrino_core::Document`, annotation
//! tools, zoom/pan canvas, and the copy/save/share bottom bar.
//!
//! All document mutations go through `Document` methods so undo/redo works.
//! In-progress drag shapes live outside the document and are committed on
//! release.

mod canvas;
mod toolbar;
mod tools;

use egui::{Color32, TextureHandle, Vec2};
use image::RgbaImage;
use skrino_core::{Annotation, ArrowHead, Color, Document, Point, Tool};

use crate::theme::Palette;
use crate::transform::CanvasTransform;

/// A request bubbled up to the app after handling editor input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorSignal {
    None,
    Close,
    Copy,
    Save,
    Share,
}

/// In-progress drag creating a two-point shape.
struct Drag {
    start: Point,
    cur: Point,
}

/// In-progress translate of an existing annotation (Select tool).
struct MoveState {
    index: usize,
    last: Point,
}

/// Floating inline text editor.
struct TextEditor {
    pos: Point,
    buffer: String,
    size: f32,
    color: Color,
}

pub struct EditorState {
    pub doc: Document,
    img_w: u32,
    img_h: u32,
    texture: Option<TextureHandle>,

    // Active tool & style.
    tool: Tool,
    color: Color,
    thickness: f32,
    text_size: f32,
    arrow_head: ArrowHead,

    // View transform.
    zoom: f32,
    offset: Vec2,
    fit_pending: bool,
    /// Canvas rect from the last frame (for centred zoom from the slider).
    canvas_rect: egui::Rect,

    // Interaction state.
    drag: Option<Drag>,
    poly: Vec<Point>,
    poly_active: bool,
    moving: Option<MoveState>,
    panning: bool,
    selected: Option<usize>,
    text_editor: Option<TextEditor>,

    // Crop tool: dragging rect (image px, two corners) then pending confirm.
    crop_start: Option<Point>,
    crop_cur: Option<Point>,
    crop_active: bool,

    // Blur pixelation preview cache: (rect, sigma, texture).
    blur_cache: Vec<(skrino_core::Rect, f32, TextureHandle)>,
    blur_sig: u64,
}

impl EditorState {
    pub fn new(image: RgbaImage) -> Self {
        let img_w = image.width();
        let img_h = image.height();
        Self {
            doc: Document::new(image),
            img_w,
            img_h,
            texture: None,
            tool: Tool::Select,
            color: Color::rgb(0xE8, 0x48, 0x4D), // red — most common annotation colour
            thickness: 4.0,
            text_size: 24.0,
            arrow_head: ArrowHead::Filled,
            zoom: 1.0,
            offset: Vec2::ZERO,
            fit_pending: true,
            canvas_rect: egui::Rect::ZERO,
            drag: None,
            poly: Vec::new(),
            poly_active: false,
            moving: None,
            panning: false,
            selected: None,
            text_editor: None,
            crop_start: None,
            crop_cur: None,
            crop_active: false,
            blur_cache: Vec::new(),
            blur_sig: 0,
        }
    }

    fn transform(&self) -> CanvasTransform {
        CanvasTransform::new(self.offset, self.zoom)
    }

    fn ensure_texture(&mut self, ctx: &egui::Context) {
        if self.texture.is_none() {
            let base = self.doc.base();
            let size = [base.width() as usize, base.height() as usize];
            let color = egui::ColorImage::from_rgba_unmultiplied(size, base.as_raw());
            self.texture =
                Some(ctx.load_texture("skrino_editor_tex", color, egui::TextureOptions::LINEAR));
        }
    }

    /// Fit the image inside `canvas` and centre it.
    fn fit(&mut self, canvas: egui::Rect) {
        let iw = self.img_w as f32;
        let ih = self.img_h as f32;
        if iw <= 0.0 || ih <= 0.0 {
            return;
        }
        let margin = 32.0;
        let avail_w = (canvas.width() - margin).max(16.0);
        let avail_h = (canvas.height() - margin).max(16.0);
        let zoom = (avail_w / iw).min(avail_h / ih).clamp(0.05, 1.0);
        self.zoom = zoom;
        self.center(canvas);
    }

    fn center(&mut self, canvas: egui::Rect) {
        let img_center = Vec2::new(self.img_w as f32, self.img_h as f32) * 0.5 * self.zoom;
        let c = canvas.center();
        self.offset = Vec2::new(c.x - img_center.x, c.y - img_center.y);
    }

    /// Main entry: draw the whole editor window and return a signal.
    pub fn ui(
        &mut self,
        ctx: &egui::Context,
        palette: &Palette,
        sharing: bool,
    ) -> EditorSignal {
        self.ensure_texture(ctx);
        let mut signal = EditorSignal::None;

        egui::TopBottomPanel::top("skrino_toolbar")
            .exact_height(52.0)
            .show(ctx, |ui| toolbar::top_toolbar(self, ui, palette));

        egui::TopBottomPanel::top("skrino_context")
            .exact_height(44.0)
            .show(ctx, |ui| toolbar::context_row(self, ui, palette));

        egui::TopBottomPanel::bottom("skrino_bottom")
            .exact_height(52.0)
            .show(ctx, |ui| {
                if let Some(s) = toolbar::bottom_bar(self, ui, palette, sharing) {
                    signal = s;
                }
            });

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(palette.canvas_bg))
            .show(ctx, |ui| {
                canvas::canvas_ui(self, ui, palette);
            });

        self.handle_keys(ctx, &mut signal);
        signal
    }

    fn handle_keys(&mut self, ctx: &egui::Context, signal: &mut EditorSignal) {
        // Don't steal keys while typing into the inline text editor.
        if self.text_editor.is_some() {
            return;
        }
        let (
            cmd_undo,
            cmd_redo,
            cmd_copy,
            cmd_save,
            cmd_share,
            del,
            esc,
        ) = ctx.input(|i| {
            let ctrl = i.modifiers.ctrl || i.modifiers.command;
            let shift = i.modifiers.shift;
            (
                ctrl && i.key_pressed(egui::Key::Z) && !shift,
                (ctrl && i.key_pressed(egui::Key::Y)) || (ctrl && shift && i.key_pressed(egui::Key::Z)),
                ctrl && i.key_pressed(egui::Key::C) && !shift,
                ctrl && i.key_pressed(egui::Key::S),
                (ctrl && shift && i.key_pressed(egui::Key::C))
                    || (ctrl && i.key_pressed(egui::Key::Enter)),
                i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace),
                i.key_pressed(egui::Key::Escape),
            )
        });

        if cmd_undo {
            self.doc.undo();
            self.selected = None;
        }
        if cmd_redo {
            self.doc.redo();
            self.selected = None;
        }
        if cmd_copy {
            *signal = EditorSignal::Copy;
        }
        if cmd_save {
            *signal = EditorSignal::Save;
        }
        if cmd_share {
            *signal = EditorSignal::Share;
        }
        if del
            && let Some(idx) = self.selected.take() {
                self.doc.remove_annotation(idx);
            }
        if esc {
            // Cancel an in-progress crop first; otherwise close.
            if self.crop_active || self.crop_start.is_some() {
                self.cancel_crop();
            } else {
                *signal = EditorSignal::Close;
            }
        }
    }

    fn cancel_crop(&mut self) {
        self.crop_start = None;
        self.crop_cur = None;
        self.crop_active = false;
    }

    fn current_style(&self) -> skrino_core::Style {
        skrino_core::Style {
            color: self.color,
            thickness: self.thickness,
        }
    }

    /// Switch the active tool, finalising any in-progress interaction first.
    fn set_tool(&mut self, tool: Tool) {
        if self.tool == tool {
            return;
        }
        self.finish_text();
        self.poly.clear();
        self.poly_active = false;
        self.drag = None;
        self.moving = None;
        self.panning = false;
        self.cancel_crop();
        self.selected = None;
        self.tool = tool;
    }

    /// Commit the inline text editor's contents as a Text annotation, if any.
    fn finish_text(&mut self) {
        if let Some(ed) = self.text_editor.take() {
            let content = ed.buffer.trim().to_string();
            if !content.is_empty() {
                self.doc.add_annotation(Annotation::Text {
                    pos: ed.pos,
                    content,
                    size: ed.size,
                    color: ed.color,
                    background: None,
                });
            }
        }
    }
}

/// Convert a core colour into an egui colour.
pub fn c32(c: Color) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r, c.g, c.b, c.a)
}
