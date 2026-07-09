//! Coordinate transforms. There are two independent mappings, each with a unit
//! test, because an off-by-one here shows up as a visibly wrong crop on HiDPI
//! displays (the #1 bug class for this kind of tool).
//!
//! NOTE: the spec sketch listed this as `editor/transform.rs`, but the overlay
//! needs the selection->pixel mapping too, so it lives at the crate root and is
//! shared by both `overlay.rs` and the editor canvas.

use egui::{Pos2, Rect as ERect, Vec2};
use skrino_core::{Point, Rect as CRect};

/// Editor canvas transform: image-pixel space <-> on-screen points.
///
/// `offset` is the screen position (points) of image pixel `(0, 0)`.
/// `zoom` is screen-points per image-pixel (1.0 = 100%).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CanvasTransform {
    pub offset: Vec2,
    pub zoom: f32,
}

impl CanvasTransform {
    pub fn new(offset: Vec2, zoom: f32) -> Self {
        Self { offset, zoom }
    }

    #[inline]
    pub fn image_to_screen(&self, p: Point) -> Pos2 {
        Pos2::new(self.offset.x + p.x * self.zoom, self.offset.y + p.y * self.zoom)
    }

    #[inline]
    pub fn screen_to_image(&self, s: Pos2) -> Point {
        Point::new(
            (s.x - self.offset.x) / self.zoom,
            (s.y - self.offset.y) / self.zoom,
        )
    }

    pub fn image_rect_to_screen(&self, r: CRect) -> ERect {
        ERect::from_min_max(self.image_to_screen(r.min), self.image_to_screen(r.max))
    }

    /// Scale an image-space length into screen points.
    #[inline]
    pub fn len_to_screen(&self, image_len: f32) -> f32 {
        image_len * self.zoom
    }
}

/// Overlay transform: selection made in overlay-local logical POINTS mapped to
/// physical image PIXELS of the captured screenshot.
///
/// The overlay viewport's top-left is positioned at the captured image's
/// top-left (virtual-screen origin). egui reports pointer coordinates in logical
/// points relative to that top-left, so `physical_pixel = point * scale`. A
/// single effective scale factor is used — see the multi-DPI caveat in
/// `overlay.rs`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OverlayTransform {
    pub scale: f32,
    pub img_w: u32,
    pub img_h: u32,
}

impl OverlayTransform {
    pub fn new(scale: f32, img_w: u32, img_h: u32) -> Self {
        Self { scale, img_w, img_h }
    }

    /// Map a local point (logical points) to a physical image pixel (f32, unclamped).
    #[inline]
    pub fn point_to_pixel(&self, p: Pos2) -> (f32, f32) {
        (p.x * self.scale, p.y * self.scale)
    }

    /// Convert an on-screen selection rect (logical points) into an integer
    /// pixel crop rect clamped to image bounds. Returns `(x, y, w, h)`.
    ///
    /// Kept as f32 math until the final `round()` so a 125%/150% scale factor
    /// lands on the intended pixel instead of drifting by a few px.
    pub fn selection_to_pixel_rect(&self, r: ERect) -> (u32, u32, u32, u32) {
        let (ax, ay) = self.point_to_pixel(r.min);
        let (bx, by) = self.point_to_pixel(r.max);
        let img_w = self.img_w as f32;
        let img_h = self.img_h as f32;
        let min_x = ax.min(bx).round().clamp(0.0, img_w);
        let min_y = ay.min(by).round().clamp(0.0, img_h);
        let max_x = ax.max(bx).round().clamp(0.0, img_w);
        let max_y = ay.max(by).round().clamp(0.0, img_h);
        (
            min_x as u32,
            min_y as u32,
            (max_x - min_x) as u32,
            (max_y - min_y) as u32,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canvas_round_trips() {
        let t = CanvasTransform::new(Vec2::new(10.0, 20.0), 2.5);
        let p = Point::new(37.0, 91.0);
        let s = t.image_to_screen(p);
        let back = t.screen_to_image(s);
        assert!((back.x - p.x).abs() < 1e-3, "x {} != {}", back.x, p.x);
        assert!((back.y - p.y).abs() < 1e-3, "y {} != {}", back.y, p.y);
    }

    #[test]
    fn canvas_maps_origin_to_offset() {
        let t = CanvasTransform::new(Vec2::new(5.0, 7.0), 3.0);
        let s = t.image_to_screen(Point::new(0.0, 0.0));
        assert_eq!(s, Pos2::new(5.0, 7.0));
        assert_eq!(t.len_to_screen(4.0), 12.0);
    }

    #[test]
    fn overlay_scales_selection_to_pixels() {
        // 150% DPI: 1 logical point = 1.5 physical pixels.
        let t = OverlayTransform::new(1.5, 3000, 2000);
        let sel = ERect::from_min_max(Pos2::new(100.0, 100.0), Pos2::new(200.0, 300.0));
        let (x, y, w, h) = t.selection_to_pixel_rect(sel);
        assert_eq!((x, y, w, h), (150, 150, 150, 300));
    }

    #[test]
    fn overlay_clamps_to_image_bounds() {
        let t = OverlayTransform::new(2.0, 1000, 800);
        // Selection runs off the right/bottom edge; must clamp, never overflow.
        let sel = ERect::from_min_max(Pos2::new(400.0, 300.0), Pos2::new(900.0, 700.0));
        let (x, y, w, h) = t.selection_to_pixel_rect(sel);
        assert_eq!(x, 800);
        assert_eq!(y, 600);
        assert_eq!(x + w, 1000, "must clamp width to image edge");
        assert_eq!(y + h, 800, "must clamp height to image edge");
    }
}
