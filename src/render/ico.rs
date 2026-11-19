//! Writes `res/traytemp.ico` from the CPU glyph. Run on demand with
//! `cargo test write_app_icon -- --ignored`; the generated file is committed.

use super::{render_glyph, Canvas};
use crate::sensor::Kind;

const SIZES: [u32; 3] = [16, 32, 48];

/// One 32-bpp BMP icon image: BITMAPINFOHEADER with doubled height, bottom-up
/// BGRA rows, then a 1-bpp AND mask (all zero, alpha does the masking) with
/// rows padded to 4 bytes.
fn bmp_entry(c: &Canvas) -> Vec<u8> {
    let mask_stride = (c.w.div_ceil(32) * 4) as usize;
    let mut out = Vec::new();
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&c.w.to_le_bytes());
    out.extend_from_slice(&(c.h * 2).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    out.extend_from_slice(&(c.w * c.h * 4 + mask_stride as u32 * c.h).to_le_bytes());
    out.extend_from_slice(&[0; 16]); // resolution and palette fields
    for y in (0..c.h).rev() {
        for x in 0..c.w {
            let p = c.px[(y * c.w + x) as usize];
            out.extend_from_slice(&[p as u8, (p >> 8) as u8, (p >> 16) as u8, (p >> 24) as u8]);
        }
    }
    out.resize(out.len() + mask_stride * c.h as usize, 0);
    out
}

fn build_ico() -> Vec<u8> {
    let images: Vec<(u32, Vec<u8>)> = SIZES
        .iter()
        .map(|&s| (s, bmp_entry(&render_glyph(Kind::Cpu, s))))
        .collect();
    let mut out = Vec::new();
    out.extend_from_slice(&[0, 0, 1, 0]);
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len() as u32;
    for (s, data) in &images {
        out.extend_from_slice(&[*s as u8, *s as u8, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += data.len() as u32;
    }
    for (_, data) in &images {
        out.extend_from_slice(data);
    }
    out
}

#[test]
#[ignore = "writes res/traytemp.ico"]
fn write_app_icon() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/res/traytemp.ico");
    std::fs::write(path, build_ico()).unwrap();
}

