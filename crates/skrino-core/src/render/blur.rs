//! Fast gaussian-approximation blur for privacy regions.
//!
//! Uses three successive box blurs, which converges to a true gaussian
//! (central limit theorem). Operates in-place on a clamped sub-region of the
//! (straight-alpha) base image. Screenshots are opaque, so we blur all four
//! channels uniformly and let alpha ride along harmlessly.

use crate::annotation::Rect as ARect;
use image::RgbaImage;

/// Blur `rect` of `img` with the given `sigma` (in pixels).
///
/// The region is clamped to image bounds; empty/degenerate regions are skipped.
pub fn apply_blur(img: &mut RgbaImage, rect: &ARect, sigma: f32) {
    if !sigma.is_finite() || sigma <= 0.0 {
        return;
    }
    let (iw, ih) = (img.width() as i64, img.height() as i64);
    if iw == 0 || ih == 0 {
        return;
    }

    // Clamp rect to image bounds, rounding outward so the whole visible region
    // is covered.
    let x0 = (rect.min.x.floor() as i64).clamp(0, iw);
    let y0 = (rect.min.y.floor() as i64).clamp(0, ih);
    let x1 = (rect.max.x.ceil() as i64).clamp(0, iw);
    let y1 = (rect.max.y.ceil() as i64).clamp(0, ih);
    let rw = (x1 - x0) as usize;
    let rh = (y1 - y0) as usize;
    if rw == 0 || rh == 0 {
        return;
    }

    // Box radius for a 3-pass approximation of a gaussian of this sigma.
    // sigma^2 = n * ((2r+1)^2 - 1) / 12  with n = 3  =>  solve for r.
    let ideal = (4.0 * sigma * sigma + 1.0).sqrt();
    let radius = (((ideal - 1.0) / 2.0).round() as usize).max(1);

    // Extract the region into a scratch buffer of RGBA channels as f-free u8;
    // we run separable box passes over it with clamped edge sampling.
    let mut buf: Vec<[u16; 4]> = Vec::with_capacity(rw * rh);
    for y in 0..rh {
        let iy = y0 as u32 + y as u32;
        for x in 0..rw {
            let ix = x0 as u32 + x as u32;
            let p = img.get_pixel(ix, iy).0;
            buf.push([p[0] as u16, p[1] as u16, p[2] as u16, p[3] as u16]);
        }
    }

    let mut tmp: Vec<[u16; 4]> = vec![[0; 4]; rw * rh];
    for _ in 0..3 {
        box_blur_h(&buf, &mut tmp, rw, rh, radius);
        box_blur_v(&tmp, &mut buf, rw, rh, radius);
    }

    // Write back.
    for y in 0..rh {
        let iy = y0 as u32 + y as u32;
        for x in 0..rw {
            let ix = x0 as u32 + x as u32;
            let s = buf[y * rw + x];
            img.get_pixel_mut(ix, iy).0 = [s[0] as u8, s[1] as u8, s[2] as u8, s[3] as u8];
        }
    }
}

/// Horizontal box blur with a sliding window and clamped edges.
fn box_blur_h(src: &[[u16; 4]], dst: &mut [[u16; 4]], w: usize, h: usize, r: usize) {
    let window = (2 * r + 1) as u32;
    for y in 0..h {
        let row = y * w;
        // Initialise the running sum for x = 0 (clamped left edge).
        let mut sum = [0u32; 4];
        for k in 0..=r {
            accumulate(&mut sum, &src[row + k.min(w - 1)]);
        }
        // Left edge repeats src[row] for the r pixels before it.
        add_scaled(&mut sum, &src[row], r as u32);
        for x in 0..w {
            write_avg(&mut dst[row + x], &sum, window);
            // Slide: add the pixel entering on the right, drop the one leaving.
            let add = src[row + (x + r + 1).min(w - 1)];
            let sub = src[row + x.saturating_sub(r)];
            slide(&mut sum, &add, &sub);
        }
    }
}

/// Vertical box blur with a sliding window and clamped edges.
fn box_blur_v(src: &[[u16; 4]], dst: &mut [[u16; 4]], w: usize, h: usize, r: usize) {
    let window = (2 * r + 1) as u32;
    for x in 0..w {
        let mut sum = [0u32; 4];
        for k in 0..=r {
            accumulate(&mut sum, &src[k.min(h - 1) * w + x]);
        }
        add_scaled(&mut sum, &src[x], r as u32);
        for y in 0..h {
            write_avg(&mut dst[y * w + x], &sum, window);
            let add = src[(y + r + 1).min(h - 1) * w + x];
            let sub = src[y.saturating_sub(r) * w + x];
            slide(&mut sum, &add, &sub);
        }
    }
}

#[inline]
fn accumulate(sum: &mut [u32; 4], px: &[u16; 4]) {
    for (s, &v) in sum.iter_mut().zip(px.iter()) {
        *s += v as u32;
    }
}

/// `sum += px * scale` (used to seed the clamped edge repeat).
#[inline]
fn add_scaled(sum: &mut [u32; 4], px: &[u16; 4], scale: u32) {
    for (s, &v) in sum.iter_mut().zip(px.iter()) {
        *s += v as u32 * scale;
    }
}

/// Write the box average of `sum` (window size `window`) into `dst`.
#[inline]
fn write_avg(dst: &mut [u16; 4], sum: &[u32; 4], window: u32) {
    for (d, &s) in dst.iter_mut().zip(sum.iter()) {
        *d = ((s + window / 2) / window) as u16;
    }
}

/// Slide the running window: `sum += add - sub`.
#[inline]
fn slide(sum: &mut [u32; 4], add: &[u16; 4], sub: &[u16; 4]) {
    for ((s, &a), &b) in sum.iter_mut().zip(add.iter()).zip(sub.iter()) {
        *s += a as u32;
        *s -= b as u32;
    }
}
