//! Pure geometry helpers for the editor: drawing annotations with an egui
//! painter, hit-testing, and translating shapes. No UI state here.

use egui::{Color32, CornerRadius, FontId, Pos2, Shape, Stroke, StrokeKind, Vec2, epaint::PathStroke};
use skrino_core::{Annotation, ArrowHead, Color, Point, Rect as CRect};

use super::c32;
use crate::theme::Palette;
use crate::transform::CanvasTransform;

const SELECT_TOL_IMG: f32 = 6.0;

/// Draw one annotation onto the canvas. `Blur` is skipped here — the canvas
/// draws its pixelation preview separately (below the vector layer).
pub fn draw_annotation(
    painter: &egui::Painter,
    t: &CanvasTransform,
    ann: &Annotation,
    palette: &Palette,
) {
    match ann {
        Annotation::Arrow {
            from,
            to,
            head,
            style,
        } => {
            let a = t.image_to_screen(*from);
            let b = t.image_to_screen(*to);
            let w = t.len_to_screen(style.thickness).max(1.0);
            draw_arrow(painter, a, b, w, style.color, *head);
        }
        Annotation::Line { from, to, style } => {
            let a = t.image_to_screen(*from);
            let b = t.image_to_screen(*to);
            let w = t.len_to_screen(style.thickness).max(1.0);
            painter.line_segment([a, b], Stroke::new(w, c32(style.color)));
        }
        Annotation::Rect { rect, style, fill } => {
            let r = t.image_rect_to_screen(*rect);
            let w = t.len_to_screen(style.thickness).max(1.0);
            if let Some(f) = fill {
                painter.rect_filled(r, CornerRadius::same(2), c32(*f));
            }
            painter.rect_stroke(
                r,
                CornerRadius::same(2),
                Stroke::new(w, c32(style.color)),
                StrokeKind::Middle,
            );
        }
        Annotation::Ellipse { rect, style, fill } => {
            let r = t.image_rect_to_screen(*rect);
            let pts = ellipse_points(r, 48);
            let w = t.len_to_screen(style.thickness).max(1.0);
            if let Some(f) = fill {
                painter.add(Shape::convex_polygon(pts.clone(), c32(*f), Stroke::NONE));
            }
            painter.add(Shape::closed_line(pts, Stroke::new(w, c32(style.color))));
        }
        Annotation::Text {
            pos,
            content,
            size,
            color,
            background,
        } => {
            let p = t.image_to_screen(*pos);
            let font = FontId::proportional((size * t.zoom).max(6.0));
            let galley = painter.layout_no_wrap(content.clone(), font, c32(*color));
            if let Some(bg) = background {
                let pad = Vec2::new(6.0, 3.0) * t.zoom.max(0.3);
                let rect = egui::Rect::from_min_size(p - pad, galley.size() + pad * 2.0);
                painter.rect_filled(rect, CornerRadius::same(4), c32(*bg));
            }
            painter.galley(p, galley, c32(*color));
        }
        Annotation::Marker { points, style } => {
            let pts = map_points(t, points);
            let w = (style.thickness * t.zoom * 1.6).max(2.0);
            // Highlighter: translucent, rounded.
            let mut mc = style.color;
            mc.a = 110;
            if pts.len() >= 2 {
                painter.add(Shape::line(pts, PathStroke::new(w, c32(mc))));
            }
        }
        Annotation::Pen { points, style } => {
            let pts = map_points(t, points);
            let w = t.len_to_screen(style.thickness).max(1.0);
            if pts.len() >= 2 {
                painter.add(Shape::line(pts, PathStroke::new(w, c32(style.color))));
            } else if let Some(p) = pts.first() {
                painter.circle_filled(*p, w * 0.5, c32(style.color));
            }
        }
        Annotation::Blur { .. } => {
            // Preview drawn by the canvas layer.
        }
        Annotation::Counter {
            pos,
            number,
            radius,
            color,
        } => {
            let p = t.image_to_screen(*pos);
            let r = (radius * t.zoom).max(6.0);
            painter.circle_filled(p, r, c32(*color));
            painter.circle_stroke(p, r, Stroke::new((1.0 * t.zoom).max(1.0), Color32::WHITE));
            let font = FontId::new(r * 1.05, egui::FontFamily::Name(crate::theme::HEADING_FAMILY.into()));
            let _ = palette;
            painter.text(
                p,
                egui::Align2::CENTER_CENTER,
                number.to_string(),
                font,
                Color32::WHITE,
            );
        }
    }
}

/// Outline a selected annotation with a subtle accent rectangle.
pub fn draw_selection_outline(
    painter: &egui::Painter,
    t: &CanvasTransform,
    ann: &Annotation,
    palette: &Palette,
) {
    let Some(bb) = bounding_box(ann) else { return };
    let mut r = t.image_rect_to_screen(bb);
    r = r.expand(4.0);
    painter.rect_stroke(
        r,
        CornerRadius::same(4),
        Stroke::new(1.5, palette.accent),
        StrokeKind::Outside,
    );
}

fn map_points(t: &CanvasTransform, points: &[Point]) -> Vec<Pos2> {
    points.iter().map(|p| t.image_to_screen(*p)).collect()
}

/// Draw one arrow annotation (shaft + head) matching `head`'s style. Mirrors
/// `skrino_core::render`'s three `ArrowHead` variants so the live UI preview
/// and committed annotations look the same as the exported PNG.
fn draw_arrow(painter: &egui::Painter, from: Pos2, to: Pos2, width: f32, color: Color, head: ArrowHead) {
    let dir = (to - from).normalized();
    if !dir.is_finite() || dir == Vec2::ZERO {
        // Degenerate (zero-length) arrow: draw a dot so something is visible.
        painter.circle_filled(to, width.max(1.0) * 0.5, c32(color));
        return;
    }
    let col = c32(color);
    let len = (to - from).length();
    match head {
        ArrowHead::Filled | ArrowHead::Dashed => {
            // Head length ~3.5x thickness (min 10px), clamped to the arrow's
            // own length so it never overshoots a very short arrow. The
            // shaft endpoint sits width/2 INSIDE the head (past its base):
            // egui strokes have flat ends, so stopping at or before the base
            // leaves a visible gap; ending slightly inside makes the joint
            // seamless while the triangle, wider than the shaft near its
            // base, still covers the shaft end completely. Degenerate/short
            // arrows skip the shaft and draw the head only.
            let head_len = (width * 3.5).max(10.0).min(len);
            let shaft_len = len - head_len + width * 0.5;
            if shaft_len > 0.0 {
                let shaft_end = from + dir * shaft_len;
                if matches!(head, ArrowHead::Dashed) {
                    draw_dashed_shaft(painter, from, shaft_end, width, col);
                } else {
                    painter.line_segment([from, shaft_end], Stroke::new(width, col));
                }
            }
            draw_arrow_head_triangle(painter, dir, to, width, head_len, col);
        }
        ArrowHead::Tapered => {
            draw_tapered_arrow(painter, from, to, dir, width, col);
        }
    }
}

/// The filled triangle head shared by `Filled` and `Dashed` arrows. Mirrors
/// `skrino_core::render`; `head_len` is computed once by the caller (shared
/// with the shaft pullback) instead of being recomputed here. The head half
/// width must stay wide enough that the shaft's round cap (radius width/2)
/// never peeks past its base edges — `head_len*0.5` already clears this in
/// practice, but the explicit floor keeps the invariant true if the
/// constants above ever change.
fn draw_arrow_head_triangle(painter: &egui::Painter, dir: Vec2, to: Pos2, width: f32, head_len: f32, color: Color32) {
    let half_w = (head_len * 0.5).max(width * 0.5 + 1.0);
    let perp = Vec2::new(-dir.y, dir.x);
    let base = to - dir * head_len;
    let left = base + perp * half_w;
    let right = base - perp * half_w;
    painter.add(Shape::convex_polygon(vec![to, left, right], color, Stroke::NONE));
}

/// Dashed shaft: dash ~3x thickness, gap ~2x thickness, starting at the tail.
/// The filled head triangle is drawn separately (over the dashes near the
/// tip), matching `Filled`'s shaft-then-head layering. `to` here is already
/// the pulled-back shaft end, not the arrow's tip.
fn draw_dashed_shaft(painter: &egui::Painter, from: Pos2, to: Pos2, width: f32, color: Color32) {
    let vec = to - from;
    let len = vec.length();
    if len < 1e-3 {
        return;
    }
    let dir = vec / len;
    let dash = (width * 3.0).max(2.0);
    let gap = (width * 2.0).max(2.0);
    let stroke = Stroke::new(width, color);
    let mut d = 0.0;
    while d < len {
        let seg_end = (d + dash).min(len);
        painter.line_segment([from + dir * d, from + dir * seg_end], stroke);
        d += dash + gap;
    }
}

/// Sample a quadratic Bezier (`p0`, control `c`, `p1`) at `n-1` interior
/// points (excluding both endpoints), for building sampled polygon edges out
/// of curves — `Shape::convex_polygon` only accepts a flat point list.
fn sample_quad(p0: Pos2, c: Pos2, p1: Pos2, n: usize) -> Vec<Pos2> {
    (1..n)
        .map(|i| {
            let t = i as f32 / n as f32;
            let mt = 1.0 - t;
            Pos2::new(
                mt * mt * p0.x + 2.0 * mt * t * c.x + t * t * p1.x,
                mt * mt * p0.y + 2.0 * mt * t * c.y + t * t * p1.y,
            )
        })
        .collect()
}

/// Tapered "Yandex-style" arrow. Mirrors `skrino_core::render`'s `draw_arrow`
/// Tapered branch's proportions exactly: head length ~3.5x thickness (min
/// 12px), clamped to <=60% of the arrow's length for short arrows via `k` —
/// and the whole taper (tail/neck half-widths, not just the head) scales
/// down by that same `k`, so a short arrow doesn't end up with a
/// disproportionately fat neck.
///
/// The silhouette is softened the same way as core, but via a different
/// implementation: core fills one path with fillets at every corner
/// (including the re-entrant one at the neck-to-shoulder transition, which
/// `tiny-skia`'s winding-rule rasterizer handles natively); egui's
/// `Shape::convex_polygon` fan-triangulates from vertex 0 and silently
/// mis-renders a concave outline, so here the shape is split into pieces
/// that are each convex on their own:
///   - the shaft (tail to neck), with gently-curved edges and a rounded tail
///     cap (a plain filled circle of radius `tail_half_w`, overlaid — since
///     it's the same opaque color and fully nested inside the shaft's width,
///     the union reads as one smoothly-capped shape);
///   - the head, drawn as a triangle whose *base* curves back toward the
///     tail a little (`base_bulge`), which softens the neck-to-wing jump
///     without introducing a concave vertex, plus small fillets at the two
///     wing-back corners and the tip (rounding an already-convex corner
///     keeps the whole piece convex).
fn draw_tapered_arrow(painter: &egui::Painter, from: Pos2, to: Pos2, dir: Vec2, width: f32, color: Color32) {
    let t = width;
    let len = (to - from).length();
    let perp = Vec2::new(-dir.y, dir.x);

    let base_head_len = (t * 3.5).max(12.0);
    let head_len = base_head_len.min(len * 0.6);
    let k = (head_len / base_head_len).clamp(0.0, 1.0);
    let tail_half_w = 0.25 * t * k;
    let neck_half_w = 0.8 * t * k;
    let head_half_w = 2.0 * t * k;

    let neck = to - dir * head_len;
    let tail_left = from + perp * tail_half_w;
    let tail_right = from - perp * tail_half_w;
    let neck_left = neck + perp * neck_half_w;
    let neck_right = neck - perp * neck_half_w;
    let head_left = neck + perp * head_half_w;
    let head_right = neck - perp * head_half_w;

    // --- Piece A: shaft (tail to neck). Edges ease ~12% toward the
    // centerline at their midpoint (a soft concave bow instead of a
    // razor-straight wedge side); the tail is rounded by an overlaid circle.
    let ease = |a: Pos2, b: Pos2| -> Pos2 {
        let mid = a + (b - a) * 0.5;
        let proj = from + dir * (mid - from).dot(dir);
        mid + (proj - mid) * 0.12
    };
    let ctrl_l = ease(tail_left, neck_left);
    let ctrl_r = ease(tail_right, neck_right);
    let mut shaft_pts = vec![tail_left];
    shaft_pts.extend(sample_quad(tail_left, ctrl_l, neck_left, 8));
    shaft_pts.push(neck_left);
    shaft_pts.push(neck_right);
    shaft_pts.extend(sample_quad(neck_right, ctrl_r, tail_right, 8));
    shaft_pts.push(tail_right);
    painter.add(Shape::convex_polygon(shaft_pts, color, Stroke::NONE));
    if tail_half_w > 0.3 {
        painter.circle_filled(from, tail_half_w, color);
    }

    // --- Piece B: head. The base curves back toward the tail a little
    // (softening the neck-to-wing shoulder instead of a hard right angle);
    // the wing-back corners and the tip are lightly filleted.
    let step = (head_half_w - neck_half_w).max(0.0);
    let base_bulge = (0.5 * step).min(head_half_w * 0.4);
    let control_base = neck - dir * base_bulge;
    let wing_r = (0.15 * head_len).min(head_half_w * 0.6);
    let tip_r = (0.08 * t)
        .clamp(0.3, 1.0)
        .min((to - head_left).length() * 0.3)
        .min((to - head_right).length() * 0.3);

    let dir_htl = (to - head_left).normalized();
    let dir_htr = (to - head_right).normalized();

    let head_left_in = head_left - perp * wing_r;
    let head_left_out = head_left + dir_htl * wing_r;
    let head_right_in = head_right + perp * wing_r;
    let head_right_out = head_right + dir_htr * wing_r;
    let tip_left = to - dir_htl * tip_r;
    let tip_right = to - dir_htr * tip_r;

    let mut head_pts = vec![head_left_in];
    head_pts.extend(sample_quad(head_left_in, head_left, head_left_out, 6));
    head_pts.push(head_left_out);
    head_pts.push(tip_left);
    head_pts.extend(sample_quad(tip_left, to, tip_right, 6));
    head_pts.push(tip_right);
    head_pts.push(head_right_out);
    head_pts.extend(sample_quad(head_right_out, head_right, head_right_in, 6));
    head_pts.push(head_right_in);
    head_pts.extend(sample_quad(head_right_in, control_base, head_left_in, 8));
    painter.add(Shape::convex_polygon(head_pts, color, Stroke::NONE));
}

fn ellipse_points(r: egui::Rect, segments: usize) -> Vec<Pos2> {
    let c = r.center();
    let (rx, ry) = (r.width() * 0.5, r.height() * 0.5);
    (0..segments)
        .map(|i| {
            let a = i as f32 / segments as f32 * std::f32::consts::TAU;
            Pos2::new(c.x + rx * a.cos(), c.y + ry * a.sin())
        })
        .collect()
}

// --- hit-testing & translation (image space) ---

/// Topmost annotation under `p` (image px), if any.
pub fn hit_test(anns: &[Annotation], p: Point) -> Option<usize> {
    for (i, ann) in anns.iter().enumerate().rev() {
        if hits(ann, p) {
            return Some(i);
        }
    }
    None
}

fn hits(ann: &Annotation, p: Point) -> bool {
    let tol = SELECT_TOL_IMG;
    match ann {
        Annotation::Arrow { from, to, .. } | Annotation::Line { from, to, .. } => {
            point_seg_dist(p, *from, *to) <= tol
        }
        Annotation::Rect { rect, fill, .. } => {
            if fill.is_some() && rect.contains(p) {
                true
            } else {
                near_rect_border(*rect, p, tol)
            }
        }
        Annotation::Ellipse { rect, fill, .. } => {
            if fill.is_some() && rect.contains(p) {
                return true;
            }
            // Near the border: check normalised radius close to 1.
            let cx = (rect.min.x + rect.max.x) * 0.5;
            let cy = (rect.min.y + rect.max.y) * 0.5;
            let rx = (rect.width() * 0.5).max(1.0);
            let ry = (rect.height() * 0.5).max(1.0);
            let nx = (p.x - cx) / rx;
            let ny = (p.y - cy) / ry;
            let d = (nx * nx + ny * ny).sqrt();
            (d - 1.0).abs() < 0.15
        }
        Annotation::Text { pos, content, size, .. } => {
            let w = content.chars().count() as f32 * size * 0.55;
            let r = CRect::from_points(*pos, Point::new(pos.x + w.max(*size), pos.y + size * 1.2));
            r.contains(p)
        }
        Annotation::Marker { points, .. } | Annotation::Pen { points, .. } => {
            points.windows(2).any(|w| point_seg_dist(p, w[0], w[1]) <= tol.max(4.0))
        }
        Annotation::Blur { rect, .. } => rect.contains(p),
        Annotation::Counter { pos, radius, .. } => {
            let dx = p.x - pos.x;
            let dy = p.y - pos.y;
            (dx * dx + dy * dy).sqrt() <= *radius
        }
    }
}

fn near_rect_border(r: CRect, p: Point, tol: f32) -> bool {
    let inside = r.contains(p);
    let inner = CRect {
        min: Point::new(r.min.x + tol, r.min.y + tol),
        max: Point::new(r.max.x - tol, r.max.y - tol),
    };
    let strict_inside = inner.max.x > inner.min.x
        && inner.max.y > inner.min.y
        && inner.contains(p);
    // Within tol of the outer edge too.
    let near_outer = p.x >= r.min.x - tol
        && p.x <= r.max.x + tol
        && p.y >= r.min.y - tol
        && p.y <= r.max.y + tol;
    (inside || near_outer) && !strict_inside
}

/// Bounding box of an annotation in image space.
pub fn bounding_box(ann: &Annotation) -> Option<CRect> {
    match ann {
        Annotation::Arrow { from, to, .. } | Annotation::Line { from, to, .. } => {
            Some(CRect::from_points(*from, *to))
        }
        Annotation::Rect { rect, .. }
        | Annotation::Ellipse { rect, .. }
        | Annotation::Blur { rect, .. } => Some(*rect),
        Annotation::Text { pos, content, size, .. } => {
            let w = (content.chars().count() as f32 * size * 0.55).max(*size);
            Some(CRect::from_points(
                *pos,
                Point::new(pos.x + w, pos.y + size * 1.2),
            ))
        }
        Annotation::Marker { points, .. } | Annotation::Pen { points, .. } => {
            bbox_of_points(points)
        }
        Annotation::Counter { pos, radius, .. } => Some(CRect {
            min: Point::new(pos.x - radius, pos.y - radius),
            max: Point::new(pos.x + radius, pos.y + radius),
        }),
    }
}

fn bbox_of_points(points: &[Point]) -> Option<CRect> {
    let first = points.first()?;
    let mut min = *first;
    let mut max = *first;
    for p in points {
        min.x = min.x.min(p.x);
        min.y = min.y.min(p.y);
        max.x = max.x.max(p.x);
        max.y = max.y.max(p.y);
    }
    Some(CRect { min, max })
}

/// Translate every point of an annotation by `(dx, dy)` image pixels.
pub fn translate(ann: &mut Annotation, dx: f32, dy: f32) {
    let shift = |p: &mut Point| {
        p.x += dx;
        p.y += dy;
    };
    let shift_rect = |r: &mut CRect| {
        r.min.x += dx;
        r.min.y += dy;
        r.max.x += dx;
        r.max.y += dy;
    };
    match ann {
        Annotation::Arrow { from, to, .. } | Annotation::Line { from, to, .. } => {
            shift(from);
            shift(to);
        }
        Annotation::Rect { rect, .. }
        | Annotation::Ellipse { rect, .. }
        | Annotation::Blur { rect, .. } => shift_rect(rect),
        Annotation::Text { pos, .. } | Annotation::Counter { pos, .. } => shift(pos),
        Annotation::Marker { points, .. } | Annotation::Pen { points, .. } => {
            for p in points {
                shift(p);
            }
        }
    }
}

fn point_seg_dist(p: Point, a: Point, b: Point) -> f32 {
    let (abx, aby) = (b.x - a.x, b.y - a.y);
    let (apx, apy) = (p.x - a.x, p.y - a.y);
    let len2 = abx * abx + aby * aby;
    if len2 <= f32::EPSILON {
        return (apx * apx + apy * apy).sqrt();
    }
    let t = ((apx * abx + apy * aby) / len2).clamp(0.0, 1.0);
    let cx = a.x + t * abx;
    let cy = a.y + t * aby;
    ((p.x - cx).powi(2) + (p.y - cy).powi(2)).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use skrino_core::Style;

    #[test]
    fn hit_test_picks_topmost_line() {
        let style = Style {
            color: Color::rgb(0, 0, 0),
            thickness: 2.0,
        };
        let anns = vec![
            Annotation::Line {
                from: Point::new(0.0, 0.0),
                to: Point::new(100.0, 0.0),
                style,
            },
            Annotation::Line {
                from: Point::new(0.0, 0.0),
                to: Point::new(0.0, 100.0),
                style,
            },
        ];
        // Near the vertical (index 1, drawn last => topmost).
        assert_eq!(hit_test(&anns, Point::new(1.0, 50.0)), Some(1));
        // Off both lines.
        assert_eq!(hit_test(&anns, Point::new(80.0, 80.0)), None);
    }

    #[test]
    fn translate_moves_counter() {
        let mut a = Annotation::Counter {
            pos: Point::new(10.0, 20.0),
            number: 1,
            radius: 12.0,
            color: Color::rgb(1, 2, 3),
        };
        translate(&mut a, 5.0, -4.0);
        if let Annotation::Counter { pos, .. } = a {
            assert_eq!((pos.x, pos.y), (15.0, 16.0));
        } else {
            panic!("wrong variant");
        }
    }
}
