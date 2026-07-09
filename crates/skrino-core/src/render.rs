//! Export renderer: rasterizes the document (base image + blur regions +
//! vector annotations + crop) into the final RGBA image.
//!
//! Pipeline:
//!   1. compute the drawing canvas (base rect unioned with annotation bounds),
//!   2. allocate the pixmap at canvas size and fill it opaque WHITE,
//!   3. blur regions applied to a working copy of the base pixels,
//!   4. blit the (blurred) base into the canvas at the canvas offset,
//!   5. vector annotations drawn (antialiased) in insertion order, all
//!      translated by the same canvas offset,
//!   6. crop applied last, in the same canvas coordinate space.
//!
//! `tiny-skia` works in premultiplied RGBA; `image::RgbaImage` is straight
//! alpha. Conversions in both directions are done explicitly so translucent
//! shapes keep correct colors.

mod blur;
mod text;

use ab_glyph::{FontRef, PxScale};
use image::RgbaImage;
use tiny_skia::{
    FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Rect as SkRect, Stroke, StrokeDash,
    Transform,
};

use crate::annotation::{Annotation, ArrowHead, Color, Point, Rect, Style};
use crate::document::Document;

pub struct RenderOptions<'a> {
    /// TTF/OTF bytes of the UI font — used for Text and Counter annotations.
    pub font_data: &'a [u8],
}

/// The document's drawing canvas in image-pixel coordinates: the union of the
/// base image rect (0,0..w,h) and the bounds of every annotation (inflated by
/// what the shape actually paints), so shapes drawn outside the screenshot
/// expand the canvas. The result is rounded OUTWARD to integers (floor min,
/// ceil max) so the canvas is pixel-aligned and stable. `render_document`
/// fills the area outside the base image with WHITE. Crop coordinates are
/// interpreted in this same space (min can be negative). The UI uses this to
/// draw the expanded white backdrop and to fit-to-window.
pub fn canvas_rect(doc: &Document) -> crate::Rect {
    let base = crate::Rect {
        min: crate::Point::new(0.0, 0.0),
        max: crate::Point::new(doc.base().width() as f32, doc.base().height() as f32),
    };
    // Union with per-annotation paint bounds so nothing gets clipped.
    let mut rect = base;
    for a in doc.annotations() {
        if let Some(b) = annotation_bounds(a) {
            rect.min.x = rect.min.x.min(b.min.x);
            rect.min.y = rect.min.y.min(b.min.y);
            rect.max.x = rect.max.x.max(b.max.x);
            rect.max.y = rect.max.y.max(b.max.y);
        }
    }
    // Round OUTWARD so the canvas is integer-aligned and stable frame to frame.
    crate::Rect {
        min: crate::Point::new(rect.min.x.floor(), rect.min.y.floor()),
        max: crate::Point::new(rect.max.x.ceil(), rect.max.y.ceil()),
    }
}

/// Bounds of a single annotation in image pixels, inflated by its stroke
/// thickness / head size so the shape fully fits inside. `None` for
/// annotations that never extend the canvas (Blur only re-samples base pixels,
/// so it is clamped to the base image instead) or that paint nothing
/// (e.g. an empty freehand stroke).
///
/// The UI mirrors this helper for its live white-backdrop preview, so the
/// approximations here (notably the layout-free text estimate) must match the
/// UI exactly — consistency matters more than typographic precision.
pub fn annotation_bounds(a: &Annotation) -> Option<crate::Rect> {
    match a {
        Annotation::Arrow {
            from, to, style, ..
        } => {
            let t = style.thickness.max(0.5);
            // Widest head across the three styles: the tapered head base
            // half-width is ~2*t; the filled triangle floors at a 10px head
            // (half-width 5). Add half the shaft thickness on top.
            let margin = (2.0 * t).max(5.0) + t * 0.5;
            Some(inflate(seg_bounds(*from, *to), margin))
        }
        Annotation::Line { from, to, style } => {
            Some(inflate(seg_bounds(*from, *to), style.thickness.max(0.5) * 0.5))
        }
        Annotation::Rect { rect, style, .. } | Annotation::Ellipse { rect, style, .. } => {
            // Outline is centered on the path, so it extends thickness/2 outward.
            Some(inflate(*rect, style.thickness.max(0.5) * 0.5))
        }
        Annotation::Marker { points, style } | Annotation::Pen { points, style } => {
            let b = points_bounds(points)?;
            Some(inflate(b, style.thickness.max(0.5) * 0.5))
        }
        Annotation::Text {
            pos,
            content,
            size,
            background,
            ..
        } => Some(text_bounds(*pos, content, *size, background.is_some())),
        Annotation::Counter {
            pos, radius, ..
        } => {
            // Badge fill radius plus the darker ring stroke (width 2 => +1),
            // rounded up to +2 for antialiased edge coverage.
            let r = radius.max(1.0) + 2.0;
            Some(crate::Rect {
                min: crate::Point::new(pos.x - r, pos.y - r),
                max: crate::Point::new(pos.x + r, pos.y + r),
            })
        }
        // Blur never expands the canvas — it re-samples base pixels only.
        Annotation::Blur { .. } => None,
    }
}

/// Conservative, layout-free text bounds. Height = line_count * 1.25 * size;
/// width = max_line_chars * 0.62 * size. When a background pill is present it
/// pads by 0.35 * size on all sides (matching `draw_text`). The UI must use
/// the same formula.
fn text_bounds(pos: Point, content: &str, size: f32, background: bool) -> crate::Rect {
    let size = size.max(0.0);
    let lines = content.split('\n');
    let mut line_count = 0usize;
    let mut max_chars = 0usize;
    for l in lines {
        line_count += 1;
        max_chars = max_chars.max(l.chars().count());
    }
    let line_count = line_count.max(1);
    let width = max_chars as f32 * 0.62 * size;
    let height = line_count as f32 * 1.25 * size;
    let mut r = crate::Rect {
        min: crate::Point::new(pos.x, pos.y),
        max: crate::Point::new(pos.x + width, pos.y + height),
    };
    if background {
        let pad = 0.35 * size;
        r = inflate(r, pad);
    }
    r
}

fn seg_bounds(a: Point, b: Point) -> crate::Rect {
    crate::Rect {
        min: crate::Point::new(a.x.min(b.x), a.y.min(b.y)),
        max: crate::Point::new(a.x.max(b.x), a.y.max(b.y)),
    }
}

fn points_bounds(points: &[Point]) -> Option<crate::Rect> {
    let first = points.first()?;
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (first.x, first.y, first.x, first.y);
    for p in &points[1..] {
        min_x = min_x.min(p.x);
        min_y = min_y.min(p.y);
        max_x = max_x.max(p.x);
        max_y = max_y.max(p.y);
    }
    Some(crate::Rect {
        min: crate::Point::new(min_x, min_y),
        max: crate::Point::new(max_x, max_y),
    })
}

fn inflate(r: crate::Rect, m: f32) -> crate::Rect {
    crate::Rect {
        min: crate::Point::new(r.min.x - m, r.min.y - m),
        max: crate::Point::new(r.max.x + m, r.max.y + m),
    }
}

/// Render the full document to the final image (what gets copied/saved/uploaded).
pub fn render_document(doc: &Document, opts: &RenderOptions) -> RgbaImage {
    let base = doc.base();
    let (w, h) = (base.width(), base.height());

    // Degenerate base — nothing sensible to draw.
    if w == 0 || h == 0 {
        return RgbaImage::new(w.max(1), h.max(1));
    }

    // Canvas = base rect unioned with annotation bounds, integer-aligned.
    let canvas = canvas_rect(doc);
    let off_x = -canvas.min.x; // integral (canvas is floor/ceil rounded)
    let off_y = -canvas.min.y;
    let cw = (canvas.max.x - canvas.min.x).max(1.0) as u32;
    let ch = (canvas.max.y - canvas.min.y).max(1.0) as u32;

    // 1) Work on a copy of the base so we can blur privacy regions in place.
    let mut work = base.clone();
    for a in doc.annotations() {
        if let Annotation::Blur { rect, sigma } = a {
            blur::apply_blur(&mut work, rect, *sigma);
        }
    }

    // 2) Allocate the canvas pixmap and fill it opaque WHITE. Premultiplied
    //    white is (255,255,255,255), so filling every byte with 255 works.
    let mut pixmap = Pixmap::new(cw, ch).expect("non-zero pixmap dimensions");
    pixmap.data_mut().fill(255);

    // 3) Blit the (blurred) base into the canvas at the canvas offset. This
    //    REPLACES the white in the base region with the premultiplied base, so
    //    when nothing extends the canvas the output is byte-identical to the
    //    old "upload base into a w×h pixmap" behavior.
    blit_base_premultiplied(&work, &mut pixmap, off_x as i64, off_y as i64);

    // Everything drawn from here on is translated by the canvas offset.
    let tf = Transform::from_translate(off_x, off_y);

    // The font is optional at the mechanical level: if it fails to parse we still
    // draw shapes, we just skip glyphs (and log).
    let font = FontRef::try_from_slice(opts.font_data).ok();
    if font.is_none() {
        log::warn!("render: font failed to parse; text/counter labels will be skipped");
    }

    // 4) Draw everything else in insertion order.
    for a in doc.annotations() {
        match a {
            Annotation::Blur { .. } => {} // already applied to base
            Annotation::Arrow {
                from,
                to,
                head,
                style,
            } => draw_arrow(&mut pixmap, *from, *to, *head, style, tf),
            Annotation::Line { from, to, style } => draw_line(&mut pixmap, *from, *to, style, tf),
            Annotation::Rect { rect, style, fill } => {
                draw_rect(&mut pixmap, rect, style, *fill, tf)
            }
            Annotation::Ellipse { rect, style, fill } => {
                draw_ellipse(&mut pixmap, rect, style, *fill, tf)
            }
            Annotation::Marker { points, style } => {
                draw_freehand(&mut pixmap, points, style, true, tf)
            }
            Annotation::Pen { points, style } => {
                draw_freehand(&mut pixmap, points, style, false, tf)
            }
            Annotation::Text {
                pos,
                content,
                size,
                color,
                background,
            } => {
                if let Some(font) = &font {
                    draw_text(
                        &mut pixmap,
                        font,
                        *pos,
                        content,
                        *size,
                        *color,
                        *background,
                        (off_x, off_y),
                    );
                }
            }
            Annotation::Counter {
                pos,
                number,
                radius,
                color,
            } => draw_counter(
                &mut pixmap,
                font.as_ref(),
                *pos,
                *number,
                *radius,
                *color,
                (off_x, off_y),
            ),
        }
    }

    // 5) Read back to straight alpha, then crop (in canvas coordinate space).
    let full = download_straight(&pixmap);
    apply_crop(full, doc.crop(), off_x, off_y)
}

// ---------------------------------------------------------------------------
// Pixmap <-> RgbaImage conversion
// ---------------------------------------------------------------------------

/// Copy the straight-alpha base image into the premultiplied canvas pixmap at
/// integer offset `(off_x, off_y)`, replacing whatever (white) was there.
fn blit_base_premultiplied(src: &RgbaImage, dst: &mut Pixmap, off_x: i64, off_y: i64) {
    let cw = dst.width() as i64;
    let ch = dst.height() as i64;
    let out = dst.data_mut();
    for (x, y, px) in src.enumerate_pixels() {
        let dx = x as i64 + off_x;
        let dy = y as i64 + off_y;
        if dx < 0 || dy < 0 || dx >= cw || dy >= ch {
            continue;
        }
        let [r, g, b, a] = px.0;
        let o = ((dy * cw + dx) * 4) as usize;
        let af = a as u32;
        out[o] = ((r as u32 * af + 127) / 255) as u8;
        out[o + 1] = ((g as u32 * af + 127) / 255) as u8;
        out[o + 2] = ((b as u32 * af + 127) / 255) as u8;
        out[o + 3] = a;
    }
}

fn download_straight(src: &Pixmap) -> RgbaImage {
    let (w, h) = (src.width(), src.height());
    let data = src.data();
    let mut out = RgbaImage::new(w, h);
    for (i, px) in out.pixels_mut().enumerate() {
        let o = i * 4;
        let a = data[o + 3];
        if a == 0 {
            px.0 = [0, 0, 0, 0];
        } else {
            let a32 = a as u32;
            let un = |c: u8| -> u8 { ((c as u32 * 255 + a32 / 2) / a32).min(255) as u8 };
            px.0 = [un(data[o]), un(data[o + 1]), un(data[o + 2]), a];
        }
    }
    out
}

/// Crop the canvas-space image. `crop` is given in image coordinates; it is
/// converted into canvas pixels via `(off_x, off_y)` and clamped to the canvas.
fn apply_crop(img: RgbaImage, crop: Option<Rect>, off_x: f32, off_y: f32) -> RgbaImage {
    let Some(rect) = crop else { return img };
    let (iw, ih) = (img.width() as i64, img.height() as i64);
    let x0 = ((rect.min.x + off_x).round() as i64).clamp(0, iw);
    let y0 = ((rect.min.y + off_y).round() as i64).clamp(0, ih);
    let x1 = ((rect.max.x + off_x).round() as i64).clamp(0, iw);
    let y1 = ((rect.max.y + off_y).round() as i64).clamp(0, ih);
    let cw = (x1 - x0) as u32;
    let ch = (y1 - y0) as u32;
    if cw == 0 || ch == 0 {
        // Guard against a zero-size crop: return the uncropped image rather than
        // an empty (and useless) buffer.
        return img;
    }
    let mut out = RgbaImage::new(cw, ch);
    for y in 0..ch {
        for x in 0..cw {
            let p = *img.get_pixel(x0 as u32 + x, y0 as u32 + y);
            out.put_pixel(x, y, p);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Drawing helpers
// ---------------------------------------------------------------------------

fn solid_paint(color: Color) -> Paint<'static> {
    let mut paint = Paint {
        anti_alias: true,
        ..Default::default()
    };
    paint.set_color_rgba8(color.r, color.g, color.b, color.a);
    paint
}

fn round_stroke(width: f32) -> Stroke {
    Stroke {
        width: width.max(0.1),
        line_cap: LineCap::Round,
        line_join: LineJoin::Round,
        ..Default::default()
    }
}

fn darken(c: Color, factor: f32) -> Color {
    let f = factor.clamp(0.0, 1.0);
    Color::rgba(
        (c.r as f32 * f) as u8,
        (c.g as f32 * f) as u8,
        (c.b as f32 * f) as u8,
        c.a,
    )
}

fn draw_line(pixmap: &mut Pixmap, from: Point, to: Point, style: &Style, tf: Transform) {
    let mut pb = PathBuilder::new();
    pb.move_to(from.x, from.y);
    pb.line_to(to.x, to.y);
    if let Some(path) = pb.finish() {
        pixmap.stroke_path(
            &path,
            &solid_paint(style.color),
            &round_stroke(style.thickness),
            tf,
            None,
        );
    }
}

fn draw_arrow(
    pixmap: &mut Pixmap,
    from: Point,
    to: Point,
    head: ArrowHead,
    style: &Style,
    tf: Transform,
) {
    let dx = to.x - from.x;
    let dy = to.y - from.y;
    let len = (dx * dx + dy * dy).sqrt();
    let t = style.thickness.max(0.5);
    let paint = solid_paint(style.color);

    if len < 1e-3 {
        // Degenerate arrow: just a dot so the user sees *something*.
        if let Some(path) = PathBuilder::from_circle(to.x, to.y, t.max(1.0)) {
            pixmap.fill_path(&path, &paint, FillRule::Winding, tf, None);
        }
        return;
    }

    let (ux, uy) = (dx / len, dy / len);
    let (px, py) = (-uy, ux); // perpendicular

    match head {
        ArrowHead::Filled | ArrowHead::Dashed => {
            let head_len = (t * 3.5).max(10.0).min(len);
            // The head base must be wide enough that the shaft's round cap
            // (radius t/2) never peeks past its edges, even at small
            // thicknesses where `head_len` floors at 10px. In practice
            // `head_len*0.5` already clears this (it floors at 5px, well
            // above `t/2+1` for any thickness up to and beyond the 12px
            // maximum), but the explicit floor keeps the invariant true if
            // those constants ever change.
            let half_w = (head_len * 0.5).max(t * 0.5 + 1.0);
            let bcx = to.x - ux * head_len;
            let bcy = to.y - uy * head_len;
            // The shaft endpoint sits t/2 INSIDE the head (past its base), so
            // the round end cap tucks fully under the triangle: the cap's
            // rearmost point lands exactly on the base line (seamless joint,
            // no notch), and near the base the triangle is wider than the
            // shaft, so nothing bulges past its edges. Degenerate/short
            // arrows (where that point would land behind `from`) skip the
            // shaft and draw the head only.
            let shaft_len = len - head_len + t * 0.5;
            if shaft_len > 0.0 {
                let shaft_end_x = from.x + ux * shaft_len;
                let shaft_end_y = from.y + uy * shaft_len;
                let mut shaft = PathBuilder::new();
                shaft.move_to(from.x, from.y); // dash phase starts at the tail
                shaft.line_to(shaft_end_x, shaft_end_y);
                if let Some(path) = shaft.finish() {
                    let stroke = if matches!(head, ArrowHead::Dashed) {
                        dashed_stroke(t)
                    } else {
                        round_stroke(t)
                    };
                    pixmap.stroke_path(&path, &paint, &stroke, tf, None);
                }
            }
            let mut tri = PathBuilder::new();
            tri.move_to(to.x, to.y);
            tri.line_to(bcx + px * half_w, bcy + py * half_w);
            tri.line_to(bcx - px * half_w, bcy - py * half_w);
            tri.close();
            if let Some(path) = tri.finish() {
                pixmap.fill_path(&path, &paint, FillRule::Winding, tf, None);
            }
        }
        ArrowHead::Tapered => {
            // One closed filled shape: a rounded tail cap fading into a
            // gently-curved shaft, widening at a filleted shoulder into the
            // head, easing to a nearly-sharp tip. Base proportions (scaled
            // down for short arrows by `k`): head length ~3.5*t (min 12px),
            // head base half-width 2*t, shaft neck half-width 0.8*t, tail
            // half-width 0.25*t — unchanged from before; only the corners
            // and edges are softened, so the silhouette never grows past the
            // old sharp-cornered bounds (see `annotation_bounds`).
            let base_head_len = (t * 3.5).max(12.0);
            let head_len = base_head_len.min(len * 0.6); // never > 60% of length
            let k = (head_len / base_head_len).clamp(0.0, 1.0);
            let tail_half = 0.25 * t * k;
            let neck_half = 0.8 * t * k;
            let head_half = 2.0 * t * k;

            // Neck = where the shaft meets the head base (axial position).
            let nx = to.x - ux * head_len;
            let ny = to.y - uy * head_len;

            // Fillet reach at the shoulder (neck_l/neck_r — a re-entrant
            // corner, since the head base is wider than the shaft) and at
            // the wing-back corner (head_l/head_r — convex). Both are
            // clamped so together they never overrun the short straight
            // step between neck_half and head_half.
            let step = (head_half - neck_half).max(0.0);
            let mut r_neck = 0.35 * step;
            let mut r_wing = (0.15 * head_len).min(head_half * 0.6);
            let reach = r_neck + r_wing;
            if reach > step && reach > 1e-6 {
                let s = step / reach;
                r_neck *= s;
                r_wing *= s;
            }
            // Tiny tip ease: keeps the point reading sharp, just not a
            // razor-thin spike.
            let r_tip = (0.08 * t).clamp(0.3, 1.0);

            let tail_l = (from.x + px * tail_half, from.y + py * tail_half);
            let tail_r = (from.x - px * tail_half, from.y - py * tail_half);
            let neck_l = (nx + px * neck_half, ny + py * neck_half);
            let neck_r = (nx - px * neck_half, ny - py * neck_half);
            let head_l = (nx + px * head_half, ny + py * head_half);
            let head_r = (nx - px * head_half, ny - py * head_half);

            let dist = |a: (f32, f32), b: (f32, f32)| -> f32 {
                ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
            };
            let norm = |dx: f32, dy: f32| -> (f32, f32) {
                let l = (dx * dx + dy * dy).sqrt().max(1e-6);
                (dx / l, dy / l)
            };
            let (tnl_x, tnl_y) = norm(neck_l.0 - tail_l.0, neck_l.1 - tail_l.1);
            let (tnr_x, tnr_y) = norm(neck_r.0 - tail_r.0, neck_r.1 - tail_r.1);
            let (htl_x, htl_y) = norm(to.x - head_l.0, to.y - head_l.1);
            let (htr_x, htr_y) = norm(to.x - head_r.0, to.y - head_r.1);

            let r_neck = r_neck.min(dist(tail_l, neck_l) * 0.9).min(dist(tail_r, neck_r) * 0.9);

            let neck_in_l = (neck_l.0 - tnl_x * r_neck, neck_l.1 - tnl_y * r_neck);
            let neck_out_l = (neck_l.0 + px * r_neck, neck_l.1 + py * r_neck);
            let neck_in_r = (neck_r.0 - tnr_x * r_neck, neck_r.1 - tnr_y * r_neck);
            let neck_out_r = (neck_r.0 - px * r_neck, neck_r.1 - py * r_neck);

            let head_in_l = (head_l.0 - px * r_wing, head_l.1 - py * r_wing);
            let head_out_l = (head_l.0 + htl_x * r_wing, head_l.1 + htl_y * r_wing);
            let head_in_r = (head_r.0 + px * r_wing, head_r.1 + py * r_wing);
            let head_out_r = (head_r.0 + htr_x * r_wing, head_r.1 + htr_y * r_wing);

            let r_tip_l = r_tip.min(dist(head_l, (to.x, to.y)) * 0.3);
            let r_tip_r = r_tip.min(dist(head_r, (to.x, to.y)) * 0.3);
            let tip_in_l = (to.x - htl_x * r_tip_l, to.y - htl_y * r_tip_l);
            let tip_in_r = (to.x - htr_x * r_tip_r, to.y - htr_y * r_tip_r);

            // Shaft edges ease very slightly toward the centerline (a soft
            // concave bow rather than a razor-straight wedge side): the quad
            // control point is the straight-edge midpoint, pulled 12% of the
            // way toward its projection onto the from->to axis.
            let ease = |a: (f32, f32), b: (f32, f32)| -> (f32, f32) {
                let mid = ((a.0 + b.0) * 0.5, (a.1 + b.1) * 0.5);
                let tproj = (mid.0 - from.x) * ux + (mid.1 - from.y) * uy;
                let proj = (from.x + ux * tproj, from.y + uy * tproj);
                (mid.0 + (proj.0 - mid.0) * 0.12, mid.1 + (proj.1 - mid.1) * 0.12)
            };
            let ctrl_l = ease(tail_l, neck_in_l);
            let ctrl_r = ease(tail_r, neck_in_r);

            let mut pb = PathBuilder::new();
            pb.move_to(tail_l.0, tail_l.1);
            pb.quad_to(ctrl_l.0, ctrl_l.1, neck_in_l.0, neck_in_l.1);
            pb.quad_to(neck_l.0, neck_l.1, neck_out_l.0, neck_out_l.1);
            pb.line_to(head_in_l.0, head_in_l.1);
            pb.quad_to(head_l.0, head_l.1, head_out_l.0, head_out_l.1);
            pb.line_to(tip_in_l.0, tip_in_l.1);
            pb.quad_to(to.x, to.y, tip_in_r.0, tip_in_r.1); // sharp tip, eased
            pb.line_to(head_out_r.0, head_out_r.1);
            pb.quad_to(head_r.0, head_r.1, head_in_r.0, head_in_r.1);
            pb.line_to(neck_out_r.0, neck_out_r.1);
            pb.quad_to(neck_r.0, neck_r.1, neck_in_r.0, neck_in_r.1);
            pb.quad_to(ctrl_r.0, ctrl_r.1, tail_r.0, tail_r.1);
            pb.close();
            if let Some(path) = pb.finish() {
                pixmap.fill_path(&path, &paint, FillRule::Winding, tf, None);
            }
            // Rounded tail cap: a small circle at `from` (radius tail_half)
            // drawn as its own fill, so the outline above can stay a plain
            // straight closing edge at the tail without needing to match
            // this circle's winding direction.
            if tail_half > 0.05
                && let Some(path) = PathBuilder::from_circle(from.x, from.y, tail_half)
            {
                pixmap.fill_path(&path, &paint, FillRule::Winding, tf, None);
            }
        }
    }
}

/// Dashed variant of the round shaft stroke: dash ~3*t, gap ~2*t, scaling with
/// thickness, phase 0 so the pattern begins at the tail.
fn dashed_stroke(t: f32) -> Stroke {
    let mut stroke = round_stroke(t);
    let dash = (t * 3.0).max(1.0);
    let gap = (t * 2.0).max(1.0);
    stroke.dash = StrokeDash::new(vec![dash, gap], 0.0);
    stroke
}

/// Build a rounded-rectangle path. Falls back to a plain rect for r <= 0.
fn rounded_rect_path(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<tiny_skia::Path> {
    let r = r.max(0.0).min(w * 0.5).min(h * 0.5);
    if r <= 0.01 {
        let rect = SkRect::from_xywh(x, y, w, h)?;
        return Some(PathBuilder::from_rect(rect));
    }
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.quad_to(x + w, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.quad_to(x, y + h, x, y + h - r);
    pb.line_to(x, y + r);
    pb.quad_to(x, y, x + r, y);
    pb.close();
    pb.finish()
}

fn draw_rect(pixmap: &mut Pixmap, rect: &Rect, style: &Style, fill: Option<Color>, tf: Transform) {
    let (w, h) = (rect.width(), rect.height());
    if w < 0.5 || h < 0.5 {
        return;
    }
    let r = (2.0 * style.thickness).min(w * 0.5).min(h * 0.5);
    let Some(path) = rounded_rect_path(rect.min.x, rect.min.y, w, h, r) else {
        return;
    };
    if let Some(fc) = fill {
        pixmap.fill_path(&path, &solid_paint(fc), FillRule::Winding, tf, None);
    }
    pixmap.stroke_path(
        &path,
        &solid_paint(style.color),
        &round_stroke(style.thickness),
        tf,
        None,
    );
}

fn draw_ellipse(
    pixmap: &mut Pixmap,
    rect: &Rect,
    style: &Style,
    fill: Option<Color>,
    tf: Transform,
) {
    let (w, h) = (rect.width(), rect.height());
    if w < 0.5 || h < 0.5 {
        return;
    }
    let Some(sk) = SkRect::from_xywh(rect.min.x, rect.min.y, w, h) else {
        return;
    };
    let mut pb = PathBuilder::new();
    pb.push_oval(sk);
    let Some(path) = pb.finish() else { return };
    if let Some(fc) = fill {
        pixmap.fill_path(&path, &solid_paint(fc), FillRule::Winding, tf, None);
    }
    pixmap.stroke_path(
        &path,
        &solid_paint(style.color),
        &round_stroke(style.thickness),
        tf,
        None,
    );
}

/// Marker (highlighter, forced translucent) and Pen (opaque, smoothed) share
/// this polyline path; `highlighter` picks the translucency behavior.
fn draw_freehand(
    pixmap: &mut Pixmap,
    points: &[Point],
    style: &Style,
    highlighter: bool,
    tf: Transform,
) {
    if points.is_empty() {
        return;
    }
    let mut color = style.color;
    if highlighter && color.a == 255 {
        color.a = 110;
    }
    let paint = solid_paint(color);
    let stroke = round_stroke(style.thickness);

    if points.len() == 1 {
        let p = points[0];
        if let Some(path) = PathBuilder::from_circle(p.x, p.y, (style.thickness * 0.5).max(0.5)) {
            pixmap.fill_path(&path, &paint, FillRule::Winding, tf, None);
        }
        return;
    }

    let mut pb = PathBuilder::new();
    pb.move_to(points[0].x, points[0].y);
    if highlighter {
        // Straight segments — a highlighter reads as a flat swipe.
        for p in &points[1..] {
            pb.line_to(p.x, p.y);
        }
    } else {
        // Smooth pen: quadratic segments through midpoints, control = vertex.
        for i in 1..points.len() - 1 {
            let cur = points[i];
            let next = points[i + 1];
            let mid = Point::new((cur.x + next.x) * 0.5, (cur.y + next.y) * 0.5);
            pb.quad_to(cur.x, cur.y, mid.x, mid.y);
        }
        let last = points[points.len() - 1];
        pb.line_to(last.x, last.y);
    }
    if let Some(path) = pb.finish() {
        pixmap.stroke_path(&path, &paint, &stroke, tf, None);
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_text(
    pixmap: &mut Pixmap,
    font: &FontRef,
    pos: Point,
    content: &str,
    size: f32,
    color: Color,
    background: Option<Color>,
    off: (f32, f32),
) {
    if size <= 0.0 {
        return;
    }
    let (w, h) = (pixmap.width(), pixmap.height());
    let metrics = text::measure_block(font, size, content);
    let tf = Transform::from_translate(off.0, off.1);

    // Optional background pill drawn first.
    if let Some(bg) = background {
        let pad = 0.35 * size;
        let bx = pos.x - pad;
        let by = pos.y - pad;
        let bw = metrics.width + 2.0 * pad;
        let bh = metrics.height + 2.0 * pad;
        let radius = (pad * 1.2).min(bw * 0.5).min(bh * 0.5);
        if let Some(path) = rounded_rect_path(bx, by, bw, bh, radius) {
            pixmap.fill_path(&path, &solid_paint(bg), FillRule::Winding, tf, None);
        }
    }

    let scale = PxScale::from(size);
    let data = pixmap.data_mut();
    let mut baseline = pos.y + metrics.ascent;
    for line in content.split('\n') {
        let laid = text::layout_line(font, scale, line);
        text::draw_line(data, w, h, &laid, pos.x + off.0, baseline + off.1, color);
        baseline += metrics.line_height;
    }
}

fn draw_counter(
    pixmap: &mut Pixmap,
    font: Option<&FontRef>,
    pos: Point,
    number: u32,
    radius: f32,
    color: Color,
    off: (f32, f32),
) {
    let r = radius.max(1.0);
    let tf = Transform::from_translate(off.0, off.1);
    // Filled badge.
    if let Some(path) = PathBuilder::from_circle(pos.x, pos.y, r) {
        pixmap.fill_path(&path, &solid_paint(color), FillRule::Winding, tf, None);
    }
    // Subtle darker ring.
    if let Some(path) = PathBuilder::from_circle(pos.x, pos.y, r) {
        pixmap.stroke_path(&path, &solid_paint(darken(color, 0.6)), &round_stroke(2.0), tf, None);
    }

    // Centered white number.
    let Some(font) = font else { return };
    let label = number.to_string();
    let size = 1.2 * r;
    let scale = PxScale::from(size);
    let laid = text::layout_line(font, scale, &label);
    if laid.glyphs.is_empty() {
        return;
    }
    // Optically center the tight visual bounds of the digits on `pos`.
    let cx = (laid.min_x + laid.max_x) * 0.5;
    let cy = (laid.min_y + laid.max_y) * 0.5;
    let off_x = pos.x - cx + off.0;
    let off_y = pos.y - cy + off.1;
    let (w, h) = (pixmap.width(), pixmap.height());
    text::draw_line(
        pixmap.data_mut(),
        w,
        h,
        &laid,
        off_x,
        off_y,
        Color::rgb(255, 255, 255),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annotation::{Rect as ARect, Style};
    use image::RgbaImage;

    // Inter ships as OTF (CFF outlines). Resolved from the crate manifest dir so
    // the test is independent of the working directory.
    const FONT_PATH: &str =
        concat!(env!("CARGO_MANIFEST_DIR"), "/../skrino-app/assets/fonts/Inter-Regular.otf");

    fn font_bytes() -> Vec<u8> {
        std::fs::read(FONT_PATH).expect("read Inter-Regular.otf")
    }

    fn base(w: u32, h: u32, c: [u8; 4]) -> RgbaImage {
        RgbaImage::from_pixel(w, h, image::Rgba(c))
    }

    fn pt(x: f32, y: f32) -> Point {
        Point::new(x, y)
    }

    fn style(color: Color, thickness: f32) -> Style {
        Style { color, thickness }
    }

    /// Count pixels differing from `bg` inside `[x0,x1) x [y0,y1)`.
    fn changed_in(img: &RgbaImage, bg: [u8; 4], x0: u32, y0: u32, x1: u32, y1: u32) -> usize {
        let mut n = 0;
        for y in y0..y1.min(img.height()) {
            for x in x0..x1.min(img.width()) {
                if img.get_pixel(x, y).0 != bg {
                    n += 1;
                }
            }
        }
        n
    }

    /// Set of (x,y) pixels differing from `bg` over the whole image.
    fn changed_set(img: &RgbaImage, bg: [u8; 4]) -> std::collections::HashSet<(u32, u32)> {
        let mut s = std::collections::HashSet::new();
        for (x, y, p) in img.enumerate_pixels() {
            if p.0 != bg {
                s.insert((x, y));
            }
        }
        s
    }

    fn render_with(anns: Vec<Annotation>, base_img: RgbaImage, font: &[u8]) -> RgbaImage {
        let mut doc = Document::new(base_img);
        for a in anns {
            doc.add_annotation(a);
        }
        render_document(&doc, &RenderOptions { font_data: font })
    }

    fn arrow(head: ArrowHead) -> Annotation {
        Annotation::Arrow {
            from: pt(50.0, 150.0),
            to: pt(340.0, 150.0),
            head,
            style: style(Color::rgb(255, 0, 0), 8.0),
        }
    }

    #[test]
    fn inter_otf_parses_with_ab_glyph() {
        let bytes = font_bytes();
        assert!(
            FontRef::try_from_slice(&bytes).is_ok(),
            "ab_glyph could not parse Inter-Regular.otf (CFF outlines)"
        );
    }

    #[test]
    fn arrow_filled_renders() {
        let bg = [10, 20, 30, 255];
        let out = render_with(
            vec![Annotation::Arrow {
                from: pt(50.0, 50.0),
                to: pt(300.0, 200.0),
                head: ArrowHead::Filled,
                style: style(Color::rgb(255, 0, 0), 6.0),
            }],
            base(400, 300, bg),
            &font_bytes(),
        );
        assert!(changed_in(&out, bg, 0, 0, 400, 300) > 200, "arrow drew nothing");
        assert!(changed_in(&out, bg, 270, 170, 310, 210) > 0);
    }

    #[test]
    fn arrow_styles_all_render_and_differ() {
        let bg = [10, 20, 30, 255];
        let base_img = base(400, 300, bg);
        let filled = changed_set(&render_with(vec![arrow(ArrowHead::Filled)], base_img.clone(), &font_bytes()), bg);
        let dashed = changed_set(&render_with(vec![arrow(ArrowHead::Dashed)], base_img.clone(), &font_bytes()), bg);
        let tapered = changed_set(&render_with(vec![arrow(ArrowHead::Tapered)], base_img.clone(), &font_bytes()), bg);

        assert!(filled.len() > 200, "filled arrow drew nothing");
        assert!(dashed.len() > 100, "dashed arrow drew nothing");
        assert!(tapered.len() > 100, "tapered arrow drew nothing");

        // Dashed has gaps => fewer painted pixels than the solid filled shaft.
        assert!(dashed.len() < filled.len(), "dashed should paint fewer px than filled");
        // Each style's painted-pixel set differs from the others.
        assert_ne!(filled, dashed, "filled and dashed identical");
        assert_ne!(filled, tapered, "filled and tapered identical");
        assert_ne!(dashed, tapered, "dashed and tapered identical");
    }

    #[test]
    fn dashed_shaft_has_gaps() {
        let bg = [10, 20, 30, 255];
        let out = render_with(
            vec![Annotation::Arrow {
                from: pt(40.0, 150.0),
                to: pt(360.0, 150.0),
                head: ArrowHead::Dashed,
                style: style(Color::rgb(0, 0, 0), 6.0),
            }],
            base(400, 300, bg),
            &font_bytes(),
        );
        // Sample the shaft centerline well clear of the head (x < 250).
        let y = 150u32;
        let mut painted = 0;
        let mut background = 0;
        for x in 45u32..250 {
            if out.get_pixel(x, y).0 == bg {
                background += 1;
            } else {
                painted += 1;
            }
        }
        assert!(painted > 0, "dashed shaft painted nothing");
        assert!(background > 0, "dashed shaft has no gaps");
    }

    #[test]
    fn tapered_tip_lands_at_to() {
        let bg = [10, 20, 30, 255];
        let to = pt(300.0, 150.0);
        let out = render_with(
            vec![Annotation::Arrow {
                from: pt(50.0, 150.0),
                to,
                head: ArrowHead::Tapered,
                style: style(Color::rgb(255, 0, 0), 10.0),
            }],
            base(400, 300, bg),
            &font_bytes(),
        );
        // Some painted pixel exists within a couple px of the tip.
        let mut near_tip = false;
        for dy in -2i32..=2 {
            for dx in -2i32..=2 {
                let x = (to.x as i32 + dx) as u32;
                let y = (to.y as i32 + dy) as u32;
                if out.get_pixel(x, y).0 != bg {
                    near_tip = true;
                }
            }
        }
        assert!(near_tip, "tapered tip not present at `to`");
    }

    #[test]
    fn line_renders() {
        let bg = [255, 255, 255, 255];
        let out = render_with(
            vec![Annotation::Line {
                from: pt(10.0, 150.0),
                to: pt(390.0, 150.0),
                style: style(Color::rgb(0, 0, 0), 4.0),
            }],
            base(400, 300, bg),
            &font_bytes(),
        );
        assert!(changed_in(&out, bg, 0, 145, 400, 156) > 300);
    }

    #[test]
    fn rect_fill_and_stroke_renders() {
        let bg = [255, 255, 255, 255];
        let out = render_with(
            vec![Annotation::Rect {
                rect: ARect::from_points(pt(100.0, 80.0), pt(300.0, 220.0)),
                style: style(Color::rgb(255, 0, 0), 5.0),
                fill: Some(Color::rgba(0, 0, 255, 128)),
            }],
            base(400, 300, bg),
            &font_bytes(),
        );
        let interior = *out.get_pixel(200, 150);
        assert_ne!(interior.0, bg, "rect fill did not apply");
        assert!(interior.0[2] > interior.0[0], "interior should be blue-ish");
    }

    #[test]
    fn ellipse_renders() {
        let bg = [255, 255, 255, 255];
        let out = render_with(
            vec![Annotation::Ellipse {
                rect: ARect::from_points(pt(100.0, 80.0), pt(300.0, 220.0)),
                style: style(Color::rgb(0, 128, 255), 6.0),
                fill: None,
            }],
            base(400, 300, bg),
            &font_bytes(),
        );
        assert_eq!(out.get_pixel(200, 150).0, bg);
        assert!(changed_in(&out, bg, 95, 145, 130, 156) > 0, "ellipse rim missing");
    }

    #[test]
    fn marker_is_translucent() {
        let bg = [255, 255, 255, 255];
        let out = render_with(
            vec![Annotation::Marker {
                points: vec![pt(20.0, 150.0), pt(200.0, 150.0), pt(380.0, 150.0)],
                style: style(Color::rgb(255, 255, 0), 20.0),
            }],
            base(400, 300, bg),
            &font_bytes(),
        );
        let p = out.get_pixel(200, 150).0;
        assert_ne!(p, bg, "marker drew nothing");
        assert!(p[2] > 40, "marker should be translucent (blue channel retained)");
    }

    #[test]
    fn pen_is_opaque() {
        let bg = [255, 255, 255, 255];
        let out = render_with(
            vec![Annotation::Pen {
                points: vec![
                    pt(20.0, 40.0),
                    pt(120.0, 200.0),
                    pt(220.0, 60.0),
                    pt(360.0, 240.0),
                ],
                style: style(Color::rgb(20, 20, 20), 6.0),
            }],
            base(400, 300, bg),
            &font_bytes(),
        );
        assert!(changed_in(&out, bg, 0, 0, 400, 300) > 200, "pen drew nothing");
    }

    #[test]
    fn counter_renders_with_number() {
        let bg = [255, 255, 255, 255];
        let out = render_with(
            vec![Annotation::Counter {
                pos: pt(200.0, 150.0),
                number: 3,
                radius: 30.0,
                color: Color::rgb(220, 40, 40),
            }],
            base(400, 300, bg),
            &font_bytes(),
        );
        assert_ne!(out.get_pixel(200, 150).0, bg, "counter badge missing");
        let mut white = false;
        for y in 130..170 {
            for x in 180..220 {
                let p = out.get_pixel(x, y).0;
                if p[0] > 240 && p[1] > 240 && p[2] > 240 {
                    white = true;
                }
            }
        }
        assert!(white, "counter number glyph not rasterized");
    }

    #[test]
    fn text_renders_pixels() {
        let bg = [0, 0, 0, 255];
        let out = render_with(
            vec![Annotation::Text {
                pos: pt(20.0, 40.0),
                content: "Hello\nWorld".into(),
                size: 40.0,
                color: Color::rgb(255, 255, 255),
                background: Some(Color::rgba(0, 0, 0, 180)),
            }],
            base(400, 300, bg),
            &font_bytes(),
        );
        assert!(changed_in(&out, bg, 0, 0, 400, 300) > 100, "text drew nothing");
    }

    #[test]
    fn cyrillic_text_renders() {
        let bg = [0, 0, 0, 255];
        let out = render_with(
            vec![Annotation::Text {
                pos: pt(20.0, 60.0),
                content: "Привет".into(),
                size: 48.0,
                color: Color::rgb(255, 255, 255),
                background: None,
            }],
            base(400, 300, bg),
            &font_bytes(),
        );
        let mut white = 0;
        for p in out.pixels() {
            if p.0[0] > 200 {
                white += 1;
            }
        }
        assert!(white > 100, "Cyrillic text produced no glyph pixels");
    }

    #[test]
    fn blur_reduces_variance() {
        let mut img = RgbaImage::new(400, 300);
        for (x, _y, p) in img.enumerate_pixels_mut() {
            *p = if x < 200 {
                image::Rgba([200, 0, 0, 255])
            } else {
                image::Rgba([0, 0, 200, 255])
            };
        }
        let region = ARect::from_points(pt(120.0, 0.0), pt(280.0, 300.0));

        fn variance_r(img: &RgbaImage, r: &ARect) -> f64 {
            let (mut sum, mut sq, mut n) = (0.0f64, 0.0f64, 0.0f64);
            for y in r.min.y as u32..r.max.y as u32 {
                for x in r.min.x as u32..r.max.x as u32 {
                    let v = img.get_pixel(x, y).0[0] as f64;
                    sum += v;
                    sq += v * v;
                    n += 1.0;
                }
            }
            sq / n - (sum / n).powi(2)
        }

        let base_var = variance_r(&img, &region);
        let out = render_with(
            vec![Annotation::Blur { rect: region, sigma: 12.0 }],
            img,
            &font_bytes(),
        );
        let blurred_var = variance_r(&out, &region);
        assert!(
            blurred_var < base_var,
            "blur should reduce variance: base={base_var}, blurred={blurred_var}"
        );
    }

    #[test]
    fn crop_returns_subrect() {
        let bg = [50, 60, 70, 255];
        let mut doc = Document::new(base(400, 300, bg));
        doc.set_crop(Some(ARect::from_points(pt(50.0, 40.0), pt(250.0, 190.0))));
        let out = render_document(&doc, &RenderOptions { font_data: &font_bytes() });
        assert_eq!(out.dimensions(), (200, 150));
    }

    #[test]
    fn crop_clamps_out_of_bounds() {
        let bg = [50, 60, 70, 255];
        let mut doc = Document::new(base(400, 300, bg));
        doc.set_crop(Some(ARect::from_points(pt(-50.0, -50.0), pt(1000.0, 1000.0))));
        let out = render_document(&doc, &RenderOptions { font_data: &font_bytes() });
        assert_eq!(out.dimensions(), (400, 300));
    }

    #[test]
    fn no_outside_annotation_keeps_base_dimensions() {
        let bg = [123, 45, 67, 255];
        let out = render_with(
            vec![Annotation::Rect {
                rect: ARect::from_points(pt(100.0, 100.0), pt(300.0, 200.0)),
                style: style(Color::rgb(0, 0, 0), 4.0),
                fill: None,
            }],
            base(400, 300, bg),
            &font_bytes(),
        );
        // Everything is inside => canvas == base rect.
        assert_eq!(out.dimensions(), (400, 300));
        let doc_rect = {
            let mut doc = Document::new(base(400, 300, bg));
            doc.add_annotation(Annotation::Rect {
                rect: ARect::from_points(pt(100.0, 100.0), pt(300.0, 200.0)),
                style: style(Color::rgb(0, 0, 0), 4.0),
                fill: None,
            });
            canvas_rect(&doc)
        };
        assert_eq!((doc_rect.min.x, doc_rect.min.y), (0.0, 0.0));
        assert_eq!((doc_rect.max.x, doc_rect.max.y), (400.0, 300.0));
    }

    #[test]
    fn annotation_outside_expands_canvas_with_white_margin() {
        let bg = [10, 20, 30, 255];
        let mut doc = Document::new(base(400, 300, bg));
        // A rectangle whose outline reaches x = 500, expanding the canvas right.
        doc.add_annotation(Annotation::Rect {
            rect: ARect::from_points(pt(420.0, 100.0), pt(480.0, 200.0)),
            style: style(Color::rgb(0, 0, 0), 6.0),
            fill: None,
        });
        let canvas = canvas_rect(&doc);
        let out = render_document(&doc, &RenderOptions { font_data: &font_bytes() });

        let expected_w = (canvas.max.x - canvas.min.x) as u32;
        let expected_h = (canvas.max.y - canvas.min.y) as u32;
        assert_eq!(out.dimensions(), (expected_w, expected_h));
        assert!(expected_w > 400, "canvas should have grown horizontally");

        // The margin between base (x >= 400) and the shape is opaque white.
        let white = [255, 255, 255, 255];
        assert_eq!(out.get_pixel(408, 20).0, white, "margin must be opaque white");
        // Original base pixels are preserved at their original coordinates
        // (offset here is zero because min stays at 0,0).
        assert_eq!(out.get_pixel(10, 10).0, bg, "base pixels must be preserved");
    }

    #[test]
    fn annotation_left_of_base_offsets_and_stays_white() {
        let bg = [10, 20, 30, 255];
        let mut doc = Document::new(base(400, 300, bg));
        // Text placed at negative x expands the canvas to the left.
        doc.add_annotation(Annotation::Rect {
            rect: ARect::from_points(pt(-120.0, 100.0), pt(-40.0, 180.0)),
            style: style(Color::rgb(0, 0, 0), 6.0),
            fill: Some(Color::rgb(200, 50, 50)),
        });
        let canvas = canvas_rect(&doc);
        assert!(canvas.min.x < 0.0, "canvas should extend left of origin");
        let out = render_document(&doc, &RenderOptions { font_data: &font_bytes() });

        let expected_w = (canvas.max.x - canvas.min.x) as u32;
        let expected_h = (canvas.max.y - canvas.min.y) as u32;
        assert_eq!(out.dimensions(), (expected_w, expected_h));

        // Base image now sits at offset -canvas.min.x; its top-left is still bg.
        let off_x = (-canvas.min.x) as u32;
        let off_y = (-canvas.min.y) as u32;
        assert_eq!(out.get_pixel(off_x + 5, off_y + 5).0, bg, "base must shift by offset");
        // A pixel in the far-left margin, above the shape, is opaque white.
        assert_eq!(out.get_pixel(2, 2).0, [255, 255, 255, 255], "left margin must be white");
    }

    #[test]
    fn blur_outside_bounds_does_not_expand() {
        let bg = [10, 20, 30, 255];
        let mut doc = Document::new(base(400, 300, bg));
        doc.add_annotation(Annotation::Blur {
            rect: ARect::from_points(pt(600.0, 600.0), pt(900.0, 900.0)),
            sigma: 10.0,
        });
        let canvas = canvas_rect(&doc);
        assert_eq!((canvas.min.x, canvas.min.y), (0.0, 0.0));
        assert_eq!((canvas.max.x, canvas.max.y), (400.0, 300.0));
        let out = render_document(&doc, &RenderOptions { font_data: &font_bytes() });
        assert_eq!(out.dimensions(), (400, 300), "blur must not expand the canvas");
    }

    #[test]
    fn crop_exact_in_expanded_space() {
        let bg = [10, 20, 30, 255];
        let mut doc = Document::new(base(400, 300, bg));
        // Expand the canvas leftward.
        doc.add_annotation(Annotation::Rect {
            rect: ARect::from_points(pt(-100.0, 50.0), pt(-20.0, 150.0)),
            style: style(Color::rgb(0, 0, 0), 4.0),
            fill: None,
        });
        // Crop given in image coords; should map exactly through the offset.
        doc.set_crop(Some(ARect::from_points(pt(-100.0, 50.0), pt(100.0, 150.0))));
        let out = render_document(&doc, &RenderOptions { font_data: &font_bytes() });
        assert_eq!(out.dimensions(), (200, 100), "crop must be exact in expanded space");
    }

    #[test]
    fn annotations_outside_image_do_not_panic() {
        let bg = [255, 255, 255, 255];
        let out = render_with(
            vec![
                Annotation::Arrow {
                    from: pt(1000.0, 1000.0),
                    to: pt(2000.0, 2000.0),
                    head: ArrowHead::Filled,
                    style: style(Color::rgb(255, 0, 0), 8.0),
                },
                Annotation::Rect {
                    rect: ARect::from_points(pt(-500.0, -500.0), pt(-100.0, -100.0)),
                    style: style(Color::rgb(0, 255, 0), 4.0),
                    fill: Some(Color::rgb(0, 0, 255)),
                },
                Annotation::Text {
                    pos: pt(-300.0, -300.0),
                    content: "off".into(),
                    size: 30.0,
                    color: Color::rgb(0, 0, 0),
                    background: None,
                },
                Annotation::Blur {
                    rect: ARect::from_points(pt(600.0, 600.0), pt(900.0, 900.0)),
                    sigma: 10.0,
                },
                Annotation::Counter {
                    pos: pt(5000.0, 5000.0),
                    number: 9,
                    radius: 20.0,
                    color: Color::rgb(10, 10, 10),
                },
            ],
            base(400, 300, bg),
            &font_bytes(),
        );
        // Canvas grows to include the far-flung shapes; it must not panic and
        // the far corner must be opaque white.
        assert!(out.dimensions().0 > 400 && out.dimensions().1 > 300);
        assert_eq!(out.get_pixel(0, 0).0[3], 255, "expanded canvas must be opaque");
    }

    #[test]
    fn empty_marker_and_single_point_do_not_panic() {
        let bg = [255, 255, 255, 255];
        let out = render_with(
            vec![
                Annotation::Marker { points: vec![], style: style(Color::rgb(255, 255, 0), 10.0) },
                Annotation::Pen {
                    points: vec![pt(200.0, 150.0)],
                    style: style(Color::rgb(0, 0, 0), 8.0),
                },
            ],
            base(400, 300, bg),
            &font_bytes(),
        );
        assert!(changed_in(&out, bg, 190, 140, 210, 160) > 0);
    }
}
