//! Tray numbers in the system UI font, rasterised with GDI, coloured like taskbar text.

use std::ptr::{null, null_mut};

use windows_sys::w;
use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LPARAM, SIZE};
use windows_sys::Win32::Globalization::{
    GetDateFormatEx, GetTimeFormatEx, DATE_SHORTDATE, TIME_NOSECONDS,
};
use windows_sys::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontIndirectW, DeleteDC, DeleteObject,
    EnumFontFamiliesExW, GdiFlush, GetDC, GetSysColor, GetTextExtentPoint32W, ReleaseDC,
    SelectObject, SetBkMode, SetTextColor, TextOutW, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    CLEARTYPE_NATURAL_QUALITY, COLOR_WINDOWTEXT, DEFAULT_CHARSET, DIB_RGB_COLORS, FW_NORMAL,
    LOGFONTW, TEXTMETRICW, TRANSPARENT,
};
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows_sys::Win32::UI::Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW};
use windows_sys::Win32::UI::HiDpi::SystemParametersInfoForDpi;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    SystemParametersInfoW, NONCLIENTMETRICSW, SPI_GETHIGHCONTRAST, SPI_GETNONCLIENTMETRICS,
};

use crate::render::{AlphaMask, TextRasterizer, TextStyle};

/// Plain text like the taskbar's own: the high-contrast text colour when high contrast is on,
/// else black on a light taskbar and white on a dark one.
pub fn taskbar_style() -> TextStyle {
    let mut hc = HIGHCONTRASTW {
        cbSize: size_of::<HIGHCONTRASTW>() as u32,
        ..Default::default()
    };
    // SAFETY: `hc` is a HIGHCONTRASTW with its size in cbSize, as the call requires.
    let ok = unsafe {
        SystemParametersInfoW(SPI_GETHIGHCONTRAST, hc.cbSize, (&raw mut hc).cast(), 0)
    };
    if ok != 0 && hc.dwFlags & HCF_HIGHCONTRASTON != 0 {
        // SAFETY: reads a system colour; COLOR_WINDOWTEXT is a valid index.
        let c = unsafe { GetSysColor(COLOR_WINDOWTEXT) }; // 0x00BBGGRR
        return TextStyle {
            fill: (c & 0xFF) << 16 | (c & 0xFF00) | (c >> 16 & 0xFF),
            outline: false,
        };
    }
    let mut light = 0u32;
    let mut len = size_of::<u32>() as u32;
    // SAFETY: reads one DWORD into `light`, whose size is in `len`.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_REG_DWORD,
            null_mut(),
            (&raw mut light).cast(),
            &mut len,
        )
    };
    // Missing means a dark taskbar, as on Windows 10 before light mode.
    let light = status == ERROR_SUCCESS && light != 0;
    TextStyle {
        fill: if light { 0 } else { 0xFF_FFFF },
        outline: false,
    }
}

/// The message font from the non-client metrics, sized for 96 DPI: Segoe UI unless the user
/// changed it. (The plain call would size it for the system DPI, since we are DPI-aware.)
pub fn ui_font() -> Option<LOGFONTW> {
    let mut ncm = NONCLIENTMETRICSW {
        cbSize: size_of::<NONCLIENTMETRICSW>() as u32,
        ..Default::default()
    };
    // SAFETY: `ncm` is a NONCLIENTMETRICSW with its size in cbSize, as the call requires.
    let ok = unsafe {
        SystemParametersInfoForDpi(SPI_GETNONCLIENTMETRICS, ncm.cbSize, (&raw mut ncm).cast(), 0, 96)
    };
    (ok != 0).then_some(ncm.lfMessageFont)
}

/// The system icon font: Segoe Fluent Icons (Windows 11) or Segoe MDL2 Assets (Windows 10),
/// if either is installed.
pub fn icon_font() -> Option<LOGFONTW> {
    unsafe extern "system" fn found(_: *const LOGFONTW, _: *const TEXTMETRICW, _: u32, l: LPARAM) -> i32 {
        // SAFETY: `l` is the `bool` passed to EnumFontFamiliesExW below.
        unsafe { *(l as *mut bool) = true };
        0
    }
    for face in ["Segoe Fluent Icons", "Segoe MDL2 Assets"] {
        let mut lf = LOGFONTW { lfCharSet: DEFAULT_CHARSET, ..Default::default() };
        for (d, c) in lf.lfFaceName.iter_mut().zip(face.encode_utf16()) {
            *d = c;
        }
        let mut exists = false;
        // SAFETY: the screen DC is released again; the callback only writes through `exists`.
        unsafe {
            let dc = GetDC(null_mut());
            EnumFontFamiliesExW(dc, &lf, Some(found), (&raw mut exists) as LPARAM, 0);
            ReleaseDC(null_mut(), dc);
        }
        if exists {
            return Some(lf);
        }
    }
    None
}

/// Width in pixels of the clock's text (the widest of the short time and the short date, in
/// the user's formats) in `font` at `px`: what the taskbar's own clock takes next to its padding.
pub fn clock_text_width(font: LOGFONTW, px: u32) -> i32 {
    let (mut time, mut date) = ([0u16; 64], [0u16; 64]);
    // SAFETY: both buffers are 64 UTF-16 units, as the lengths say; null names mean the
    // user's locale, and null format strings the locale's short formats.
    let (nt, nd) = unsafe {
        (
            GetTimeFormatEx(null(), TIME_NOSECONDS, null(), null(), time.as_mut_ptr(), 64),
            GetDateFormatEx(null(), DATE_SHORTDATE, null(), null(), date.as_mut_ptr(), 64, null()),
        )
    };
    let mut lf = font;
    lf.lfHeight = -(px as i32);
    lf.lfWidth = 0;
    // SAFETY: every handle is checked and released before returning.
    unsafe {
        let dc = CreateCompatibleDC(null_mut());
        let f = CreateFontIndirectW(&lf);
        let mut widest = 0;
        if !dc.is_null() && !f.is_null() {
            let old = SelectObject(dc, f);
            for (text, n) in [(&time, nt), (&date, nd)] {
                let mut ext = SIZE::default();
                if n > 1 {
                    GetTextExtentPoint32W(dc, text.as_ptr(), n - 1, &mut ext);
                }
                widest = widest.max(ext.cx);
            }
            SelectObject(dc, old);
        }
        if !f.is_null() {
            DeleteObject(f);
        }
        if !dc.is_null() {
            DeleteDC(dc);
        }
        widest
    }
}

/// True if text in this 0xRRGGBB colour is light, by its luma.
pub fn is_light(fill: u32) -> bool {
    let (r, g, b) = (fill >> 16 & 0xFF, fill >> 8 & 0xFF, fill & 0xFF);
    r * 299 + g * 587 + b * 114 >= 128_000
}

/// Draws text in a font face as a gray coverage mask. Every GDI object lives only for one
/// `rasterize` call.
pub struct GdiText {
    font: LOGFONTW,
    /// Raise the coverage, for light text on a dark taskbar.
    boost: bool,
}

impl GdiText {
    /// `light`: the mask will be filled with a light colour.
    pub fn new(font: LOGFONTW, light: bool) -> GdiText {
        GdiText { font, boost: light }
    }
}

/// Crops a w x h coverage buffer to its ink.
fn crop(a: &[u8], w: u32, h: u32) -> AlphaMask {
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0, 0);
    for y in 0..h {
        for x in 0..w {
            if a[(y * w + x) as usize] != 0 {
                (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x + 1), y1.max(y + 1));
            }
        }
    }
    if x0 >= x1 {
        return AlphaMask { w: 0, h: 0, a: Vec::new() };
    }
    let mut out = Vec::with_capacity(((x1 - x0) * (y1 - y0)) as usize);
    for y in y0..y1 {
        out.extend_from_slice(&a[(y * w + x0) as usize..(y * w + x1) as usize]);
    }
    AlphaMask { w: x1 - x0, h: y1 - y0, a: out }
}

impl TextRasterizer for GdiText {
    fn rasterize(&mut self, text: &str, px: u32) -> Option<AlphaMask> {
        let text: Vec<u16> = text.encode_utf16().collect();
        let mut lf = self.font;
        lf.lfHeight = -(px as i32);
        lf.lfWidth = 0;
        lf.lfEscapement = 0;
        lf.lfOrientation = 0;
        lf.lfWeight = FW_NORMAL as i32;
        lf.lfItalic = 0;
        lf.lfUnderline = 0;
        lf.lfStrikeOut = 0;
        // ClearType's natural widths and soft stems, averaged to gray below. Plain grayscale
        // anti-aliasing snaps stems to whole pixels and looks bolder than the taskbar's text.
        lf.lfQuality = CLEARTYPE_NATURAL_QUALITY as u8;
        // SAFETY: every handle is checked before use, selected out again before it is deleted,
        // and deleted before returning. `bits` points to the w x h 32-bpp pixels of `bmp`,
        // which stays alive while they are read.
        unsafe {
            let dc = CreateCompatibleDC(null_mut());
            if dc.is_null() {
                return None;
            }
            let font = CreateFontIndirectW(&lf);
            let mut mask = None;
            if !font.is_null() {
                let old_font = SelectObject(dc, font);
                let mut ext = SIZE::default();
                GetTextExtentPoint32W(dc, text.as_ptr(), text.len() as i32, &mut ext);
                // Room around the text cell for ink that overhangs it.
                let pad = px as i32 / 2 + 1;
                let (w, h) = (ext.cx + 2 * pad, ext.cy + 2 * pad);
                let bmi = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: w,
                        biHeight: -h, // top-down
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let mut bits = null_mut();
                let bmp = CreateDIBSection(dc, &bmi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
                if !bmp.is_null() {
                    let old_bmp = SelectObject(dc, bmp);
                    let n = (w * h) as usize;
                    bits.cast::<u32>().write_bytes(0, n);
                    SetBkMode(dc, TRANSPARENT as i32);
                    SetTextColor(dc, 0x00FF_FFFF);
                    TextOutW(dc, pad, pad, text.as_ptr(), text.len() as i32);
                    GdiFlush();
                    // White on black: the mean of the subpixel channels is the coverage. For
                    // light text it is raised by gamma 2 so strokes are as solid as the dark
                    // taskbar's own text. Dark text uses it unchanged, which matches GDI's own
                    // black text on a light background.
                    let pixels = std::slice::from_raw_parts(bits.cast::<u32>(), n);
                    let boost = self.boost;
                    let coverage = |p: &u32| {
                        let c = ((p >> 16 & 0xFF) + (p >> 8 & 0xFF) + (p & 0xFF)) / 3;
                        if boost { (c * 255).isqrt() as u8 } else { c as u8 }
                    };
                    let a: Vec<u8> = pixels.iter().map(coverage).collect();
                    mask = Some(crop(&a, w as u32, h as u32));
                    SelectObject(dc, old_bmp);
                    DeleteObject(bmp);
                }
                SelectObject(dc, old_font);
                DeleteObject(font);
            }
            DeleteDC(dc);
            mask
        }
    }
}
