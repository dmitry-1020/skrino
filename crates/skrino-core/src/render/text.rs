//! Glyph rasterization + alpha blending onto the premultiplied pixmap.
//!
//! `tiny-skia` has no text support, so we lay out and rasterize glyphs with
//! `ab_glyph` and blend the resulting coverage directly onto the pixmap bytes
//! (RGBA, premultiplied) using source-over.

use ab_glyph::{Font, FontRef, Glyph, OutlinedGlyph, PxScale, ScaleFont, point};

use crate::annotation::Color;

/// A laid-out line of text: the outlined glyphs plus the tight visual bounds
/// (union of glyph pixel bounds) and the advance width.
pub struct LaidOutLine {
    pub glyphs: Vec<OutlinedGlyph>,
    pub advance: f32,
    /// Union of glyph pixel bounds, or None if the line has no visible glyphs.
    pub min_x: f32,
    pub min_y: f32,
    pub max_x: f32,
    pub max_y: f32,
}

/// Lay out a single line with the pen origin at `(0, 0)` on the baseline.
pub fn layout_line(font: &FontRef, scale: PxScale, text: &str) -> LaidOutLine {
    let scaled = font.as_scaled(scale);
    let mut glyphs = Vec::new();
    let mut caret = 0.0f32;
    let mut prev: Option<ab_glyph::GlyphId> = None;
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);

    for ch in text.chars() {
        let id = font.glyph_id(ch);
        if let Some(p) = prev {
            caret += scaled.kern(p, id);
        }
        let glyph: Glyph = id.with_scale_and_position(scale, point(caret, 0.0));
        caret += scaled.h_advance(id);
        prev = Some(id);
        if let Some(outline) = font.outline_glyph(glyph) {
            let b = outline.px_bounds();
            min_x = min_x.min(b.min.x);
            min_y = min_y.min(b.min.y);
            max_x = max_x.max(b.max.x);
            max_y = max_y.max(b.max.y);
            glyphs.push(outline);
        }
    }

    if glyphs.is_empty() {
        min_x = 0.0;
        min_y = 0.0;
        max_x = 0.0;
        max_y = 0.0;
    }

    LaidOutLine {
        glyphs,
        advance: caret,
        min_x,
        min_y,
        max_x,
        max_y,
    }
}

/// Draw a laid-out line, translating every glyph by `(off_x, off_y)`.
pub fn draw_line(
    data: &mut [u8],
    w: u32,
    h: u32,
    line: &LaidOutLine,
    off_x: f32,
    off_y: f32,
    color: Color,
) {
    for outline in &line.glyphs {
        let b = outline.px_bounds();
        let gx0 = b.min.x + off_x;
        let gy0 = b.min.y + off_y;
        outline.draw(|gx, gy, coverage| {
            let px = gx0 + gx as f32;
            let py = gy0 + gy as f32;
            if px < 0.0 || py < 0.0 {
                return;
            }
            let (px, py) = (px.floor() as i64, py.floor() as i64);
            if px >= w as i64 || py >= h as i64 {
                return;
            }
            blend_premult(
                data,
                w,
                px as u32,
                py as u32,
                color.r,
                color.g,
                color.b,
                (color.a as f32 / 255.0) * coverage,
            );
        });
    }
}

/// Metrics used to position multi-line blocks.
pub struct BlockMetrics {
    pub width: f32,
    pub height: f32,
    pub ascent: f32,
    pub line_height: f32,
}

/// Measure a (possibly multi-line) block for background-pill sizing.
pub fn measure_block(font: &FontRef, size: f32, text: &str) -> BlockMetrics {
    let scale = PxScale::from(size);
    let scaled = font.as_scaled(scale);
    let line_height = size * 1.25;
    let mut width = 0.0f32;
    let mut lines = 0usize;
    for l in text.split('\n') {
        let lo = layout_line(font, scale, l);
        width = width.max(lo.advance);
        lines += 1;
    }
    let lines = lines.max(1);
    BlockMetrics {
        width,
        height: line_height * lines as f32,
        ascent: scaled.ascent(),
        line_height,
    }
}

/// Blend a single straight-alpha color with coverage `alpha_f` (0..=1) onto the
/// premultiplied RGBA pixel at `(x, y)` using source-over.
#[inline]
pub fn blend_premult(
    data: &mut [u8],
    w: u32,
    x: u32,
    y: u32,
    sr: u8,
    sg: u8,
    sb: u8,
    alpha_f: f32,
) {
    let a = alpha_f.clamp(0.0, 1.0);
    if a <= 0.0 {
        return;
    }
    let sa = (a * 255.0 + 0.5) as u32; // source alpha 0..=255
    let idx = ((y * w + x) * 4) as usize;
    let inv = 255 - sa;
    // Premultiplied source channels.
    let psr = (sr as u32 * sa + 127) / 255;
    let psg = (sg as u32 * sa + 127) / 255;
    let psb = (sb as u32 * sa + 127) / 255;
    let dr = data[idx] as u32;
    let dg = data[idx + 1] as u32;
    let db = data[idx + 2] as u32;
    let da = data[idx + 3] as u32;
    data[idx] = (psr + (dr * inv + 127) / 255) as u8;
    data[idx + 1] = (psg + (dg * inv + 127) / 255) as u8;
    data[idx + 2] = (psb + (db * inv + 127) / 255) as u8;
    data[idx + 3] = (sa + (da * inv + 127) / 255) as u8;
}
