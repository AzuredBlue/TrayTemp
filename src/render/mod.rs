//! Bitmap icon renderer shared by every platform. Platform-neutral core.

use crate::sensor::Kind;

mod font;
mod glyphs;

pub const WHITE: u32 = 0xFFFF_FFFF;
pub const OUTLINE: u32 = 0xFF00_0000;
pub const CLEAR: u32 = 0x0000_0000;

/// Straight (non-premultiplied) ARGB, 0xAARRGGBB, row-major, top-down.
pub struct Canvas {
    pub w: u32,
    pub h: u32,
    pub px: Vec<u32>,
}

/// Set pixels before the outline is added.
struct Mask {
    w: u32,
    h: u32,
    bits: Vec<bool>,
}

impl Mask {
    fn new(w: u32, h: u32) -> Mask {
        Mask { w, h, bits: vec![false; (w * h) as usize] }
    }

    /// Draw a 1-bit bitmap (`width` columns, leftmost pixel is the highest bit)
    /// at (x, y), scaled by an integer factor. Pixels outside the mask are skipped.
    fn blit(&mut self, rows: &[u32], width: u32, x: i32, y: i32, scale: u32) {
        for (r, &row) in rows.iter().enumerate() {
            for c in 0..width {
                if row >> (width - 1 - c) & 1 == 0 {
                    continue;
                }
                for dy in 0..scale {
                    for dx in 0..scale {
                        let px = x + (c * scale + dx) as i32;
                        let py = y + (r as u32 * scale + dy) as i32;
                        if px >= 0 && py >= 0 && (px as u32) < self.w && (py as u32) < self.h {
                            self.bits[(py as u32 * self.w + px as u32) as usize] = true;
                        }
                    }
                }
            }
        }
    }

    fn get(&self, x: i32, y: i32) -> bool {
        x >= 0 && y >= 0 && (x as u32) < self.w && (y as u32) < self.h && self.bits[(y as u32 * self.w + x as u32) as usize]
    }

    /// White where set, 1-px dark outline on the 8-neighbourhood, transparent elsewhere.
    fn into_canvas(self) -> Canvas {
        let mut px = vec![CLEAR; self.bits.len()];
        for y in 0..self.h as i32 {
            for x in 0..self.w as i32 {
                px[(y as u32 * self.w + x as u32) as usize] = if self.get(x, y) {
                    WHITE
                } else if (-1..=1).any(|dy| (-1..=1).any(|dx| self.get(x + dx, y + dy))) {
                    OUTLINE
                } else {
                    CLEAR
                };
            }
        }
        Canvas { w: self.w, h: self.h, px }
    }
}

fn widen(rows: &[u8]) -> [u32; 7] {
    let mut out = [0; 7];
    for (o, &r) in out.iter_mut().zip(rows) {
        *o = r as u32;
    }
    out
}

/// One digit-row text item.
enum Item {
    Digit(usize, bool), // digit, narrow
    Dash,
}

/// Draw digits (or dashes) left to right with a `scale`-px gap, top-left at (x, y).
/// Returns the x just past the last item.
fn draw_number(m: &mut Mask, value: Option<i32>, x: i32, y: i32, scale: u32, condense: bool) -> i32 {
    let mut items = [Item::Dash, Item::Dash, Item::Dash];
    let n = match value {
        None => 2,
        Some(v) => {
            let v = v.clamp(0, 999) as usize;
            let (len, narrow) = if v >= 100 { (3, condense) } else if v >= 10 { (2, false) } else { (1, false) };
            let mut rest = v;
            for i in (0..len).rev() {
                items[i] = Item::Digit(rest % 10, narrow);
                rest /= 10;
            }
            len
        }
    };
    let mut cx = x;
    for (i, item) in items[..n].iter().enumerate() {
        if i > 0 {
            cx += scale as i32;
        }
        let w = match item {
            Item::Digit(d, true) => {
                m.blit(&widen(&font::DIGITS_NARROW[*d]), 3, cx, y, scale);
                3
            }
            Item::Digit(d, false) => {
                m.blit(&widen(&font::DIGITS[*d]), 5, cx, y, scale);
                5
            }
            Item::Dash => {
                m.blit(&widen(&font::DASH), 5, cx, y, scale);
                5
            }
        };
        cx += (w * scale) as i32;
    }
    cx
}

/// Width in digit-grid cells of the number as drawn by `draw_number` (unscaled, gaps included).
fn number_cells(value: Option<i32>, condense: bool) -> u32 {
    match value {
        None => 11,
        Some(v) if v >= 100 => if condense { 11 } else { 17 },
        Some(v) if v >= 10 => 11,
        Some(_) => 5,
    }
}

/// Square icon showing the temperature number (`--` for no value), white with a 1-px dark outline.
pub fn render_value(value: Option<i32>, size: u32) -> Canvas {
    // Largest scale whose digit block (11 x 7 cells plus the 1-px outline) fits.
    let s = (size.saturating_sub(2) / 11).min(size.saturating_sub(2) / 7).max(1);
    let mut m = Mask::new(size, size);
    let w = number_cells(value, true) * s;
    let x = (size as i32 - w as i32) / 2;
    let y = (size as i32 - (7 * s) as i32) / 2;
    draw_number(&mut m, value, x, y, s, true);
    m.into_canvas()
}

fn glyph_rows(kind: Kind) -> [u32; 16] {
    let src = match kind {
        Kind::Cpu => &glyphs::CPU,
        Kind::Gpu => &glyphs::GPU,
    };
    let mut out = [0; 16];
    for (o, &r) in out.iter_mut().zip(src) {
        *o = r as u32;
    }
    out
}

fn glyph_scale(size: u32) -> u32 {
    (size / 16).max(1)
}

/// Square CPU or GPU glyph, drawn white with a 1-px dark outline so it stays visible on light backgrounds.
pub fn render_glyph(kind: Kind, size: u32) -> Canvas {
    let s = glyph_scale(size);
    let mut m = Mask::new(size, size);
    let off = (size as i32 - (16 * s) as i32) / 2;
    m.blit(&glyph_rows(kind), 16, off, off, s);
    m.into_canvas()
}

/// Taskbar-text strip: `[glyph] number°` per enabled sensor (outer `None` = disabled); width 0 if none are enabled.
pub fn render_text_strip(cpu: Option<Option<i32>>, gpu: Option<Option<i32>>, height: u32) -> Canvas {
    let groups: [(Kind, Option<Option<i32>>); 2] = [(Kind::Cpu, cpu), (Kind::Gpu, gpu)];
    let s = (height.saturating_sub(2) / 7).max(1);
    let gs = glyph_scale(height);
    let glyph_w = 16 * gs;
    let gap = 2 * s;
    let group_gap = 5 * s;
    let deg_w = 3 * s;

    // First pass: total content width. The outline adds 1 px each side.
    let mut width = 0;
    let mut count = 0;
    for (_, v) in groups.iter() {
        if let Some(v) = v {
            if count > 0 {
                width += group_gap;
            }
            count += 1;
            width += glyph_w + gap + number_cells(*v, false) * s;
            if v.is_some() {
                width += s + deg_w;
            }
        }
    }
    if count == 0 {
        return Canvas { w: 0, h: height, px: Vec::new() };
    }

    let mut m = Mask::new(width + 2, height);
    let mut x = 1;
    let mut first = true;
    for (kind, v) in groups.iter() {
        let Some(v) = v else { continue };
        if !first {
            x += group_gap as i32;
        }
        first = false;
        let gy = (height as i32 - (16 * gs) as i32) / 2;
        m.blit(&glyph_rows(*kind), 16, x, gy, gs);
        x += (glyph_w + gap) as i32;
        let ty = (height as i32 - (7 * s) as i32) / 2;
        x = draw_number(&mut m, *v, x, ty, s, false);
        if v.is_some() {
            m.blit(&widen_deg(), 3, x + s as i32, ty, s);
            x += (s + deg_w) as i32;
        }
    }
    m.into_canvas()
}

fn widen_deg() -> [u32; 3] {
    [font::DEGREE[0] as u32, font::DEGREE[1] as u32, font::DEGREE[2] as u32]
}

#[cfg(test)]
mod ico;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_digits_fit() {
        let c = render_value(Some(100), 16);
        // no white pixel in the outermost column on either side => not clipped
        for y in 0..16 {
            assert_ne!(c.px[(y * 16) as usize], 0xFFFFFFFF);
            assert_ne!(c.px[(y * 16 + 15) as usize], 0xFFFFFFFF);
        }
    }

}
