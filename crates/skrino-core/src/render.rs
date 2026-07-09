//! Export renderer: rasterizes the document (base image + blur regions +
//! vector annotations + crop) into the final RGBA image.
//!
//! Pipeline:
//!   1. blur regions applied to a working copy of the base pixels,
//!   2. vector annotations drawn (antialiased) in insertion order,
//!   3. crop applied last.
//!
//! `tiny-skia` works in premultiplied RGBA; `image::RgbaImage` is straight
//! alpha. Conversions in both directions are done explicitly so translucent
//! shapes keep correct colors.

mod blur;
mod text;

use ab_glyph::{FontRef, PxScale};
use image::RgbaImage;
use tiny_skia::{
    FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Rect as SkRect, Stroke, Transform,
};

use crate::annotation::{Annotation, ArrowHead, Color, Point, Rect, Style};
use crate::document::Document;

pub struct RenderOptions<'a> {
    /// TTF/OTF bytes of the UI font — used for Text and Counter annotations.
    pub font_data: &'a [u8],
}

/// Render the full document to the final image (what gets copied/saved/uploaded).
pub fn render_document(doc: &Document, opts: &RenderOptions) -> RgbaImage {
    let base = doc.base();
    let (w, h) = (base.width(), base.height());

    // Degenerate base — nothing sensible to draw.
    if w == 0 || h == 0 {
        return RgbaImage::new(w.max(1), h.max(1));
    }

    // 1) Work on a copy of the base so we can blur privacy regions in place.
    let mut work = base.clone();
    for a in doc.annotations() {
        if let Annotation::Blur { rect, sigma } = a {
            blur::apply_blur(&mut work, rect, *sigma);
        }
    }

    // Upload straight-alpha base into a premultiplied pixmap.
    let mut pixmap = Pixmap::new(w, h).expect("non-zero pixmap dimensions");
    upload_premultiplied(&work, &mut pixmap);

    // The font is optional at the mechanical level: if it fails to parse we still
    // draw shapes, we just skip glyphs (and log).
    let font = FontRef::try_from_slice(opts.font_data).ok();
    if font.is_none() {
        log::warn!("render: font failed to parse; text/counter labels will be skipped");
    }

    // 2) Draw everything else in insertion order.
    for a in doc.annotations() {
        match a {
            Annotation::Blur { .. } => {} // already applied to base
            Annotation::Arrow {
                from,
                to,
                head,
                style,
            } => draw_arrow(&mut pixmap, *from, *to, *head, style),
            Annotation::Line { from, to, style } => draw_line(&mut pixmap, *from, *to, style),
            Annotation::Rect { rect, style, fill } => {
                draw_rect(&mut pixmap, rect, style, *fill)
            }
            Annotation::Ellipse { rect, style, fill } => {
                draw_ellipse(&mut pixmap, rect, style, *fill)
            }
            Annotation::Marker { points, style } => {
                draw_freehand(&mut pixmap, points, style, true)
            }
            Annotation::Pen { points, style } => {
                draw_freehand(&mut pixmap, points, style, false)
            }
            Annotation::Text {
                pos,
                content,
                size,
                color,
                background,
            } => {
                if let Some(font) = &font {
                    draw_text(&mut pixmap, font, *pos, content, *size, *color, *background);
                }
            }
            Annotation::Counter {
                pos,
                number,
                radius,
                color,
            } => draw_counter(&mut pixmap, font.as_ref(), *pos, *number, *radius, *color),
        }
    }

    // 3) Read back to straight alpha, then crop.
    let full = download_straight(&pixmap);
    apply_crop(full, doc.crop())
}

// ---------------------------------------------------------------------------
// Pixmap <-> RgbaImage conversion
// ---------------------------------------------------------------------------

fn upload_premultiplied(src: &RgbaImage, dst: &mut Pixmap) {
    let out = dst.data_mut();
    for (i, px) in src.pixels().enumerate() {
        let [r, g, b, a] = px.0;
        let o = i * 4;
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

fn apply_crop(img: RgbaImage, crop: Option<Rect>) -> RgbaImage {
    let Some(rect) = crop else { return img };
    let (iw, ih) = (img.width() as i64, img.height() as i64);
    let x0 = (rect.min.x.round() as i64).clamp(0, iw);
    let y0 = (rect.min.y.round() as i64).clamp(0, ih);
    let x1 = (rect.max.x.round() as i64).clamp(0, iw);
    let y1 = (rect.max.y.round() as i64).clamp(0, ih);
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
    let mut paint = Paint::default();
    paint.anti_alias = true;
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

fn draw_line(pixmap: &mut Pixmap, from: Point, to: Point, style: &Style) {
    let mut pb = PathBuilder::new();
    pb.move_to(from.x, from.y);
    pb.line_to(to.x, to.y);
    if let Some(path) = pb.finish() {
        pixmap.stroke_path(
            &path,
            &solid_paint(style.color),
            &round_stroke(style.thickness),
            Transform::identity(),
            None,
        );
    }
}

fn draw_arrow(pixmap: &mut Pixmap, from: Point, to: Point, head: ArrowHead, style: &Style) {
    let dx = to.x - from.x;
    let dy = to.y - from.y;
    let len = (dx * dx + dy * dy).sqrt();
    let t = style.thickness.max(0.5);
    let paint = solid_paint(style.color);

    if len < 1e-3 {
        // Degenerate arrow: just a dot so the user sees *something*.
        if let Some(path) = PathBuilder::from_circle(to.x, to.y, t.max(1.0)) {
            pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
        }
        return;
    }

    let (ux, uy) = (dx / len, dy / len);
    let (px, py) = (-uy, ux); // perpendicular
    let head_len = (t * 3.5).max(10.0).min(len);

    match head {
        ArrowHead::Filled => {
            let half_w = head_len * 0.5;
            let bcx = to.x - ux * head_len;
            let bcy = to.y - uy * head_len;
            // Shaft stops slightly inside the head base so the two don't seam.
            let shaft_end_x = to.x - ux * (head_len * 0.9);
            let shaft_end_y = to.y - uy * (head_len * 0.9);
            let mut shaft = PathBuilder::new();
            shaft.move_to(from.x, from.y);
            shaft.line_to(shaft_end_x, shaft_end_y);
            if let Some(path) = shaft.finish() {
                pixmap.stroke_path(&path, &paint, &round_stroke(t), Transform::identity(), None);
            }
            let mut tri = PathBuilder::new();
            tri.move_to(to.x, to.y);
            tri.line_to(bcx + px * half_w, bcy + py * half_w);
            tri.line_to(bcx - px * half_w, bcy - py * half_w);
            tri.close();
            if let Some(path) = tri.finish() {
                pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
            }
        }
        ArrowHead::Open => {
            // Shaft runs all the way to the tip.
            let mut shaft = PathBuilder::new();
            shaft.move_to(from.x, from.y);
            shaft.line_to(to.x, to.y);
            if let Some(path) = shaft.finish() {
                pixmap.stroke_path(&path, &paint, &round_stroke(t), Transform::identity(), None);
            }
            // Two barbs at ~28° off the reversed shaft direction.
            let ang = 28f32.to_radians();
            let (c, s) = (ang.cos(), ang.sin());
            let (rx, ry) = (-ux, -uy);
            let b1 = (rx * c - ry * s, rx * s + ry * c);
            let b2 = (rx * c + ry * s, -rx * s + ry * c);
            let mut barbs = PathBuilder::new();
            barbs.move_to(to.x + b1.0 * head_len, to.y + b1.1 * head_len);
            barbs.line_to(to.x, to.y);
            barbs.line_to(to.x + b2.0 * head_len, to.y + b2.1 * head_len);
            if let Some(path) = barbs.finish() {
                pixmap.stroke_path(&path, &paint, &round_stroke(t), Transform::identity(), None);
            }
        }
    }
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

fn draw_rect(pixmap: &mut Pixmap, rect: &Rect, style: &Style, fill: Option<Color>) {
    let (w, h) = (rect.width(), rect.height());
    if w < 0.5 || h < 0.5 {
        return;
    }
    let r = (2.0 * style.thickness).min(w * 0.5).min(h * 0.5);
    let Some(path) = rounded_rect_path(rect.min.x, rect.min.y, w, h, r) else {
        return;
    };
    if let Some(fc) = fill {
        pixmap.fill_path(
            &path,
            &solid_paint(fc),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
    pixmap.stroke_path(
        &path,
        &solid_paint(style.color),
        &round_stroke(style.thickness),
        Transform::identity(),
        None,
    );
}

fn draw_ellipse(pixmap: &mut Pixmap, rect: &Rect, style: &Style, fill: Option<Color>) {
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
        pixmap.fill_path(
            &path,
            &solid_paint(fc),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
    pixmap.stroke_path(
        &path,
        &solid_paint(style.color),
        &round_stroke(style.thickness),
        Transform::identity(),
        None,
    );
}

/// Marker (highlighter, forced translucent) and Pen (opaque, smoothed) share
/// this polyline path; `highlighter` picks the translucency behavior.
fn draw_freehand(pixmap: &mut Pixmap, points: &[Point], style: &Style, highlighter: bool) {
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
            pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
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
        pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }
}

fn draw_text(
    pixmap: &mut Pixmap,
    font: &FontRef,
    pos: Point,
    content: &str,
    size: f32,
    color: Color,
    background: Option<Color>,
) {
    if size <= 0.0 {
        return;
    }
    let (w, h) = (pixmap.width(), pixmap.height());
    let metrics = text::measure_block(font, size, content);

    // Optional background pill drawn first.
    if let Some(bg) = background {
        let pad = 0.35 * size;
        let bx = pos.x - pad;
        let by = pos.y - pad;
        let bw = metrics.width + 2.0 * pad;
        let bh = metrics.height + 2.0 * pad;
        let radius = (pad * 1.2).min(bw * 0.5).min(bh * 0.5);
        if let Some(path) = rounded_rect_path(bx, by, bw, bh, radius) {
            pixmap.fill_path(
                &path,
                &solid_paint(bg),
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
    }

    let scale = PxScale::from(size);
    let data = pixmap.data_mut();
    let mut baseline = pos.y + metrics.ascent;
    for line in content.split('\n') {
        let laid = text::layout_line(font, scale, line);
        text::draw_line(data, w, h, &laid, pos.x, baseline, color);
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
) {
    let r = radius.max(1.0);
    // Filled badge.
    if let Some(path) = PathBuilder::from_circle(pos.x, pos.y, r) {
        pixmap.fill_path(
            &path,
            &solid_paint(color),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
    // Subtle darker ring.
    if let Some(path) = PathBuilder::from_circle(pos.x, pos.y, r) {
        pixmap.stroke_path(
            &path,
            &solid_paint(darken(color, 0.6)),
            &round_stroke(2.0),
            Transform::identity(),
            None,
        );
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
    let off_x = pos.x - cx;
    let off_y = pos.y - cy;
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

    fn render_with(anns: Vec<Annotation>, base_img: RgbaImage, font: &[u8]) -> RgbaImage {
        let mut doc = Document::new(base_img);
        for a in anns {
            doc.add_annotation(a);
        }
        render_document(&doc, &RenderOptions { font_data: font })
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
    fn arrow_open_renders() {
        let bg = [10, 20, 30, 255];
        let out = render_with(
            vec![Annotation::Arrow {
                from: pt(50.0, 250.0),
                to: pt(350.0, 50.0),
                head: ArrowHead::Open,
                style: style(Color::rgb(0, 200, 0), 5.0),
            }],
            base(400, 300, bg),
            &font_bytes(),
        );
        assert!(changed_in(&out, bg, 0, 0, 400, 300) > 200);
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
                content: "РџСЂРёРІРµС‚".into(),
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
        assert_eq!(out.dimensions(), (400, 300));
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

