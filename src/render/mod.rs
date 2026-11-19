//! Icon renderer shared by every platform. Platform-neutral core.

use crate::sensor::Kind;

mod font;
mod glyphs;

/// Straight (non-premultiplied) ARGB, 0xAARRGGBB, row-major, top-down.
pub struct Canvas {
    pub w: u32,
    pub h: u32,
    pub px: Vec<u32>,
}

/// How text and glyphs are coloured.
#[derive(Clone, Copy, PartialEq)]
pub struct TextStyle {
    /// Fill colour, 0xRRGGBB.
    pub fill: u32,
    /// A 1-px dark outline, so the text reads on light and dark backgrounds alike.
    pub outline: bool,
}

/// White with a dark outline.
pub const OUTLINED: TextStyle = TextStyle { fill: 0xFF_FFFF, outline: true };

/// 8-bit coverage, row-major, top-down.
pub struct AlphaMask {
    pub w: u32,
    pub h: u32,
    pub a: Vec<u8>,
}

/// Draws text with an OS font.
pub trait TextRasterizer {
    /// `text` at font pixel size `px` (em height) as a coverage mask cropped to the ink;
    /// None if the OS font is unavailable.
    fn rasterize(&mut self, text: &str, px: u32) -> Option<AlphaMask>;
}

/// An OS font and the pixel size fitted per box and digit count, so the size is searched once
/// and stays the same while the number changes. Make a new one when the font changes.
pub struct OsFont {
    raster: Box<dyn TextRasterizer>,
    /// (box width, box height, digit count, px); px 0 means use the bitmap font.
    fits: Vec<(u32, u32, usize, u32)>,
}

impl OsFont {
    pub fn new(raster: Box<dyn TextRasterizer>) -> OsFont {
        OsFont { raster, fits: Vec::new() }
    }

    /// Largest px at which every `digits`-digit number fits in `max_w` x `max_h`.
    /// None if the digits would be shorter than the bitmap font's.
    fn fit(&mut self, digits: usize, max_w: u32, max_h: u32) -> Option<u32> {
        if let Some(&(.., px)) = self.fits.iter().find(|f| (f.0, f.1, f.2) == (max_w, max_h, digits)) {
            return (px > 0).then_some(px);
        }
        // UI fonts give every digit the same advance, so a number's ink runs from its first
        // digit's left edge to its last digit's right edge: trying each first/last pair, with
        // zeros between, covers every number of that length.
        let step = 10u32.pow(digits as u32 - 1);
        let firsts = if digits > 1 { 10 } else { 1 };
        // Digits are well under an em tall, so the search starts above any size that fits.
        let mut px = 2 * max_h;
        let mut ink_h = 0;
        'search: while px > 0 {
            ink_h = 0;
            for v in (0..firsts).flat_map(|a| (0..10).map(move |b| a * step + b)) {
                let m = self.raster.rasterize(&format!("{v:0digits$}"), px)?;
                if m.w > max_w || m.h > max_h {
                    px -= 1;
                    continue 'search;
                }
                ink_h = ink_h.max(m.h);
            }
            break;
        }
        // Smaller digits read worse than the bitmap ones (three digits in a 16 px icon).
        if ink_h < font::DIGITS[0].len() as u32 {
            px = 0;
        }
        self.fits.push((max_w, max_h, digits, px));
        (px > 0).then_some(px)
    }
}

/// Coverage before the outline is added.
struct Mask {
    w: u32,
    h: u32,
    a: Vec<u8>,
}

impl Mask {
    fn new(w: u32, h: u32) -> Mask {
        Mask { w, h, a: vec![0; (w * h) as usize] }
    }

    fn index(&self, x: i32, y: i32) -> Option<usize> {
        let inside = x >= 0 && y >= 0 && (x as u32) < self.w && (y as u32) < self.h;
        inside.then(|| (y as u32 * self.w + x as u32) as usize)
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
                        if let Some(i) = self.index(px, py) {
                            self.a[i] = 255;
                        }
                    }
                }
            }
        }
    }

    /// Draw coverage with its top-left at (x, y). Pixels outside the mask are skipped.
    fn draw(&mut self, m: &AlphaMask, x: i32, y: i32) {
        for my in 0..m.h {
            for mx in 0..m.w {
                if let Some(i) = self.index(x + mx as i32, y + my as i32) {
                    self.a[i] = self.a[i].max(m.a[(my * m.w + mx) as usize]);
                }
            }
        }
    }

    fn get(&self, x: i32, y: i32) -> u32 {
        self.index(x, y).map_or(0, |i| self.a[i] as u32)
    }

    /// The fill colour where covered, with the style's outline made of the coverage spread
    /// 1 px to its 8 neighbours; transparent elsewhere. Partial coverage stays soft.
    fn into_canvas(self, style: TextStyle) -> Canvas {
        let mut px = vec![0; self.a.len()];
        for y in 0..self.h as i32 {
            for x in 0..self.w as i32 {
                let f = self.get(x, y);
                let o = if style.outline {
                    let around = (-1..=1).flat_map(|dy| (-1..=1).map(move |dx| (dx, dy)));
                    around.map(|(dx, dy)| self.get(x + dx, y + dy)).max().unwrap_or(0)
                } else {
                    0
                };
                // Fill with alpha f over black with alpha o.
                let a = f + o - f * o / 255;
                let channel = |shift: u32| ((style.fill >> shift & 0xFF) * f + a / 2).checked_div(a);
                if let (Some(r), Some(g), Some(b)) = (channel(16), channel(8), channel(0)) {
                    px[(y as u32 * self.w + x as u32) as usize] = a << 24 | r << 16 | g << 8 | b;
                }
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

/// Square icon showing the temperature number (`--` for no value) in the given style.
/// The number uses the OS font when there is one, else the bitmap font; `--` is always bitmap.
pub fn render_value(value: Option<i32>, size: u32, font: Option<&mut OsFont>, style: TextStyle) -> Canvas {
    let mut m = Mask::new(size, size);
    // Without an outline the text may use the whole icon.
    let room = if style.outline { size.saturating_sub(2) } else { size };
    if let Some(t) = font.and_then(|f| os_number(f, value, room)) {
        m.draw(&t, (size as i32 - t.w as i32) / 2, (size as i32 - t.h as i32) / 2);
    } else {
        // Largest scale whose digit block (11 x 7 cells plus the 1-px outline) fits.
        let s = (size.saturating_sub(2) / 11).min(size.saturating_sub(2) / 7).max(1);
        let w = number_cells(value, true) * s;
        let x = (size as i32 - w as i32) / 2;
        let y = (size as i32 - (7 * s) as i32) / 2;
        draw_number(&mut m, value, x, y, s, true);
    }
    m.into_canvas(style)
}

/// The number in the OS font, at the largest size where every value with as many digits
/// fits a `room` x `room` square.
fn os_number(f: &mut OsFont, value: Option<i32>, room: u32) -> Option<AlphaMask> {
    let text = value?.clamp(0, 999).to_string();
    // One digit uses the two-digit size, so the number doesn't jump in size.
    let px = f.fit(text.len().max(2), room, room)?;
    f.raster.rasterize(&text, px)
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
    m.into_canvas(OUTLINED)
}

/// The strip's `54°` in the OS font, as tall as the strip allows.
fn os_label(f: &mut OsFont, value: Option<i32>, height: u32) -> Option<AlphaMask> {
    let text = format!("{}\u{b0}", value?.clamp(0, 999));
    let px = f.fit(1, u32::MAX, height.saturating_sub(2))?;
    f.raster.rasterize(&text, px)
}

/// Taskbar-text strip: `[glyph] number°` per enabled sensor (outer `None` = disabled), in the
/// given style; width 0 if none are enabled.
/// Numbers use the OS font when there is one, else the bitmap font; `--` is always bitmap.
pub fn render_text_strip(
    cpu: Option<Option<i32>>,
    gpu: Option<Option<i32>>,
    height: u32,
    mut font: Option<&mut OsFont>,
    style: TextStyle,
) -> Canvas {
    let groups: [(Kind, Option<Option<i32>>); 2] = [(Kind::Cpu, cpu), (Kind::Gpu, gpu)];
    let labels = groups.map(|(_, v)| os_label(font.as_deref_mut()?, v?, height));
    let s = (height.saturating_sub(2) / 7).max(1);
    let gs = glyph_scale(height);
    let glyph_w = 16 * gs;
    let gap = 2 * s;
    let group_gap = 5 * s;
    let deg_w = 3 * s;

    // First pass: total content width. The outline adds 1 px each side.
    let mut width = 0;
    let mut count = 0;
    for ((_, v), label) in groups.iter().zip(&labels) {
        if let Some(v) = v {
            if count > 0 {
                width += group_gap;
            }
            count += 1;
            width += glyph_w + gap;
            match label {
                Some(t) => width += t.w,
                None => {
                    width += number_cells(*v, false) * s;
                    if v.is_some() {
                        width += s + deg_w;
                    }
                }
            }
        }
    }
    if count == 0 {
        return Canvas { w: 0, h: height, px: Vec::new() };
    }

    let mut m = Mask::new(width + 2, height);
    let mut x = 1;
    let mut first = true;
    for ((kind, v), label) in groups.iter().zip(&labels) {
        let Some(v) = v else { continue };
        if !first {
            x += group_gap as i32;
        }
        first = false;
        let gy = (height as i32 - (16 * gs) as i32) / 2;
        m.blit(&glyph_rows(*kind), 16, x, gy, gs);
        x += (glyph_w + gap) as i32;
        if let Some(t) = label {
            m.draw(t, x, (height as i32 - t.h as i32) / 2);
            x += t.w as i32;
            continue;
        }
        let ty = (height as i32 - (7 * s) as i32) / 2;
        x = draw_number(&mut m, *v, x, ty, s, false);
        if v.is_some() {
            m.blit(&widen_deg(), 3, x + s as i32, ty, s);
            x += (s + deg_w) as i32;
        }
    }
    m.into_canvas(style)
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
        let c = render_value(Some(100), 16, None, OUTLINED);
        // no white pixel in the outermost column on either side => not clipped
        for y in 0..16 {
            assert_ne!(c.px[(y * 16) as usize], 0xFFFFFFFF);
            assert_ne!(c.px[(y * 16 + 15) as usize], 0xFFFFFFFF);
        }
    }

    /// Solid boxes, 7/10 px tall: '1' is px/5 wide, other digits 2/5 px.
    struct BoxFont;

    impl TextRasterizer for BoxFont {
        fn rasterize(&mut self, text: &str, px: u32) -> Option<AlphaMask> {
            let w = text.chars().map(|c| if c == '1' { px / 5 } else { px * 2 / 5 }).sum::<u32>();
            let h = px * 7 / 10;
            Some(AlphaMask { w, h, a: vec![255; (w * h) as usize] })
        }
    }

    #[test]
    fn os_font_numbers_fit_at_one_size_per_digit_count() {
        let mut font = OsFont::new(Box::new(BoxFont));
        // Height of the white ink.
        let mut ink_h = |v| {
            let c = render_value(Some(v), 16, Some(&mut font), OUTLINED);
            let white = |x: u32, y: u32| c.px[(y * 16 + x) as usize] & 0xFF_FFFF != 0;
            for i in 0..16 {
                let ring = [(i, 0), (i, 15), (0, i), (15, i)];
                assert!(!ring.iter().any(|&(x, y)| white(x, y)), "{v} leaves no room for the outline");
            }
            (0..16).filter(|&y| (0..16).any(|x| white(x, y))).count()
        };
        // px 19 is the largest at which "00".."99" fit in 14 x 14; its digits are 13 tall.
        assert_eq!(ink_h(54), 13);
        assert_eq!(ink_h(11), 13);
        assert_eq!(ink_h(100), 8);
    }
}
