use std::ptr::null_mut;

use windows_sys::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    DIB_RGB_COLORS,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, HICON, ICONINFO};

use crate::render::Canvas;

/// Builds an alpha icon from the canvas. Returns null on failure. The caller owns the
/// icon and must `DestroyIcon` it.
pub fn canvas_to_hicon(c: &Canvas) -> HICON {
    let (w, h) = (c.w as i32, c.h as i32);
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
    // AND mask: 1 bit per pixel, rows padded to 16 bits, set (transparent) where alpha is 0.
    let stride = (c.w as usize).div_ceil(16) * 2;
    let mut mask_bits = vec![0u8; stride * c.h as usize];
    for (i, &p) in c.px.iter().enumerate() {
        if p >> 24 == 0 {
            let (x, y) = (i % c.w as usize, i / c.w as usize);
            mask_bits[y * stride + x / 8] |= 0x80 >> (x % 8);
        }
    }
    let mut bits = null_mut();
    // SAFETY: `bmi` describes a w x h 32-bpp DIB, so `bits` points to w * h u32 pixels and
    // at most that many are copied. Both bitmaps are deleted after CreateIconIndirect copies them.
    unsafe {
        let color = CreateDIBSection(null_mut(), &bmi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        if color.is_null() {
            return null_mut();
        }
        // 0xAARRGGBB in little-endian memory is the BGRA byte order the DIB expects.
        std::ptr::copy_nonoverlapping(
            c.px.as_ptr(),
            bits.cast::<u32>(),
            c.px.len().min((w * h) as usize),
        );
        let mask = CreateBitmap(w, h, 1, 1, mask_bits.as_ptr().cast());
        let info = ICONINFO {
            fIcon: 1,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        };
        let icon = if mask.is_null() {
            null_mut()
        } else {
            CreateIconIndirect(&info)
        };
        DeleteObject(color);
        if !mask.is_null() {
            DeleteObject(mask);
        }
        icon
    }
}
