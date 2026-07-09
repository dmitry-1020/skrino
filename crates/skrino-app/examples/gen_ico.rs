//! One-off generator for `assets/skrino.ico` from `assets/mini-skrino.png`.
//!
//! Run once with:
//!   cargo run -p skrino-app --example gen_ico
//!
//! Produces a multi-resolution (16/32/48/256) Windows ICO with PNG-compressed
//! frames (valid since Windows Vista), so `build.rs` (via `winres`) can embed
//! it as the exe icon. Kept as a dev example rather than deleted, in case the
//! source PNG ever needs to be regenerated.

use image::imageops::FilterType;
use std::io::Write;

const SIZES: [u32; 4] = [16, 32, 48, 256];

fn main() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let src_path = format!("{manifest_dir}/assets/mini-skrino.png");
    let out_path = format!("{manifest_dir}/assets/skrino.ico");

    let img = image::open(&src_path)
        .unwrap_or_else(|e| panic!("failed to open {src_path}: {e}"))
        .to_rgba8();

    let mut entries: Vec<(u32, Vec<u8>)> = Vec::new();
    for &size in &SIZES {
        let resized = image::imageops::resize(&img, size, size, FilterType::Lanczos3);
        let mut png_bytes = Vec::new();
        image::DynamicImage::ImageRgba8(resized)
            .write_to(&mut std::io::Cursor::new(&mut png_bytes), image::ImageFormat::Png)
            .expect("PNG encode failed");
        entries.push((size, png_bytes));
    }

    let mut out = Vec::new();
    // ICONDIR header: reserved(2)=0, type(2)=1 (icon), count(2).
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());

    let header_len = 6 + entries.len() * 16;
    let mut offset = header_len as u32;

    // ICONDIRENTRY per image (16 bytes each).
    for (size, data) in &entries {
        let dim_byte = if *size >= 256 { 0u8 } else { *size as u8 };
        out.push(dim_byte); // width
        out.push(dim_byte); // height
        out.push(0); // color count (0 = no palette, true color)
        out.push(0); // reserved
        out.extend_from_slice(&1u16.to_le_bytes()); // planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bit count
        out.extend_from_slice(&(data.len() as u32).to_le_bytes()); // bytes in resource
        out.extend_from_slice(&offset.to_le_bytes()); // offset
        offset += data.len() as u32;
    }

    // Image data blocks, in the same order.
    for (_, data) in &entries {
        out.extend_from_slice(data);
    }

    let mut f = std::fs::File::create(&out_path)
        .unwrap_or_else(|e| panic!("failed to create {out_path}: {e}"));
    f.write_all(&out).expect("write failed");

    println!("wrote {out_path} ({} bytes, {} sizes)", out.len(), entries.len());
}
