//! Row-order conversion for the encoder, pure and unit-tested.
//!
//! WGC frames are top-down (row 0 = top of the screen), but
//! `VideoEncoder::send_frame_buffer` hands the bytes to Media Foundation as an
//! uncompressed BGRA sample, which follows the DIB convention: bottom-up (row 0
//! = bottom of the screen). Feeding top-down rows produces a vertically
//! flipped video, so the rows must be reversed. Byte order *within* a row must
//! be preserved exactly (pixels are 4-byte BGRA groups); reversing bytes or
//! pixels would mirror the image horizontally.
//!
//! The same pass strips D3D row padding (`row_pitch` >= visible `row_bytes`),
//! replacing the crate's `as_nopadding_buffer`.

/// Copy `rows` rows of `row_bytes` visible bytes each from `src` (top-down,
/// rows `row_pitch` bytes apart) into `dst` in reverse row order (bottom-up,
/// tightly packed). Returns `false` without touching `dst` when the source is
/// too short or the geometry is degenerate (caller drops the frame).
pub(crate) fn pack_rows_bottom_up(
    src: &[u8],
    row_bytes: usize,
    row_pitch: usize,
    rows: usize,
    dst: &mut Vec<u8>,
) -> bool {
    if rows == 0 || row_bytes == 0 || row_pitch < row_bytes {
        return false;
    }
    // The last row does not need to be padded out to full pitch.
    let needed = (rows - 1) * row_pitch + row_bytes;
    if src.len() < needed {
        return false;
    }

    dst.clear();
    dst.reserve(rows * row_bytes);
    for y in (0..rows).rev() {
        let start = y * row_pitch;
        dst.extend_from_slice(&src[start..start + row_bytes]);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reverses_row_order_and_strips_padding() {
        // 3 rows, 8 visible bytes (2 BGRA pixels), pitch 12 (4 bytes padding).
        #[rustfmt::skip]
        let src = [
            10, 11, 12, 13, 14, 15, 16, 17, 0, 0, 0, 0, // row 0 (top)
            20, 21, 22, 23, 24, 25, 26, 27, 0, 0, 0, 0, // row 1
            30, 31, 32, 33, 34, 35, 36, 37,             // row 2 (bottom, no pad)
        ];
        let mut dst = Vec::new();
        assert!(pack_rows_bottom_up(&src, 8, 12, 3, &mut dst));
        #[rustfmt::skip]
        let expected = [
            30, 31, 32, 33, 34, 35, 36, 37, // bottom row first
            20, 21, 22, 23, 24, 25, 26, 27,
            10, 11, 12, 13, 14, 15, 16, 17, // top row last
        ];
        assert_eq!(dst, expected);
    }

    #[test]
    fn reverses_rows_even_without_padding() {
        let src = [1u8, 2, 3, 4, 5, 6, 7, 8]; // 2 rows of 4, pitch == row_bytes
        let mut dst = Vec::new();
        assert!(pack_rows_bottom_up(&src, 4, 4, 2, &mut dst));
        assert_eq!(dst, [5, 6, 7, 8, 1, 2, 3, 4]);
    }

    #[test]
    fn byte_order_within_a_row_is_untouched() {
        // One row: output must be byte-identical to input (any reversal here
        // would horizontally mirror the video or scramble BGRA channels).
        let src = [9u8, 8, 7, 6, 5, 4, 3, 2];
        let mut dst = Vec::new();
        assert!(pack_rows_bottom_up(&src, 8, 8, 1, &mut dst));
        assert_eq!(dst, src);
    }

    #[test]
    fn pixel_groups_stay_intact_across_the_flip() {
        // 2x2 image of distinct BGRA pixels; after the flip, rows swap but
        // pixels keep their horizontal position and channel order.
        let px = |n: u8| [n, n + 1, n + 2, n + 3];
        let (a, b, c, d) = (px(0), px(10), px(20), px(30));
        let src: Vec<u8> = [a, b, c, d].concat(); // rows: [a b], [c d]
        let mut dst = Vec::new();
        assert!(pack_rows_bottom_up(&src, 8, 8, 2, &mut dst));
        assert_eq!(dst, [c, d, a, b].concat()); // rows: [c d], [a b]
    }

    #[test]
    fn short_source_is_rejected() {
        let src = [0u8; 10];
        let mut dst = vec![42u8];
        assert!(!pack_rows_bottom_up(&src, 8, 12, 3, &mut dst));
        assert_eq!(dst, [42], "dst must be untouched on failure");
    }

    #[test]
    fn degenerate_geometry_is_rejected() {
        let src = [0u8; 64];
        let mut dst = Vec::new();
        assert!(!pack_rows_bottom_up(&src, 8, 8, 0, &mut dst)); // no rows
        assert!(!pack_rows_bottom_up(&src, 0, 8, 2, &mut dst)); // empty rows
        assert!(!pack_rows_bottom_up(&src, 8, 4, 2, &mut dst)); // pitch < row
    }

    #[test]
    fn double_flip_restores_the_original() {
        let src: Vec<u8> = (0u8..24).collect(); // 3 rows of 8
        let mut once = Vec::new();
        let mut twice = Vec::new();
        assert!(pack_rows_bottom_up(&src, 8, 8, 3, &mut once));
        assert!(pack_rows_bottom_up(&once, 8, 8, 3, &mut twice));
        assert_eq!(twice, src);
    }
}
