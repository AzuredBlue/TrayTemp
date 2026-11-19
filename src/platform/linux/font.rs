//! Tray numbers in the desktop's default font, coloured like panel text. The family comes
//! from KDE's settings, the file from fontconfig, and the glyphs from FreeType; both
//! libraries are loaded at run time.

use std::ffi::{c_char, c_int, c_long, c_short, c_uint, c_ulong, c_ushort, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::time::SystemTime;

use libloading::Library;

use crate::render::{AlphaMask, OsFont, TextRasterizer, TextStyle, OUTLINED};

/// The number font and text style from the desktop's settings, reloaded when they change.
pub struct DesktopText {
    /// Every file of both cascades with its modification time when last read: first the
    /// `kdeglobals` files, then the `plasmarc` files, each in priority order.
    files: Vec<(PathBuf, Option<SystemTime>)>,
    /// How many of `files` are `kdeglobals` files.
    globals: usize,
    /// The family `font` was built for.
    family: Option<String>,
    pub font: Option<OsFont>,
    pub style: TextStyle,
}

impl DesktopText {
    pub fn load() -> DesktopText {
        let mut files: Vec<_> = config_files("kdeglobals")
            .into_iter()
            .map(|p| (p, None))
            .collect();
        let globals = files.len();
        files.extend(config_files("plasmarc").into_iter().map(|p| (p, None)));
        let mut t = DesktopText {
            files,
            globals,
            family: None,
            font: None,
            style: OUTLINED,
        };
        t.stamp();
        t.reload();
        t
    }

    /// Rereads the settings if any file changed, appeared or disappeared since they were last
    /// read; true if so. Costs one `stat` per file and no allocation when nothing changed.
    pub fn refresh(&mut self) -> bool {
        if !self.stamp() {
            return false;
        }
        self.reload();
        true
    }

    /// Records every file's modification time; true if any differs from the last record.
    fn stamp(&mut self) -> bool {
        let mut changed = false;
        for (path, stamp) in &mut self.files {
            let now = modified(path);
            changed |= now != *stamp;
            *stamp = now;
        }
        changed
    }

    fn reload(&mut self) {
        let read = |files: &[(PathBuf, Option<SystemTime>)]| -> Vec<String> {
            files
                .iter()
                .map(|(p, _)| std::fs::read_to_string(p).unwrap_or_default())
                .collect()
        };
        let (globals, plasmarc) = self.files.split_at(self.globals);
        let (globals, plasmarc) = (read(globals), read(plasmarc));
        let globals: Vec<&str> = globals.iter().map(String::as_str).collect();
        let plasmarc: Vec<&str> = plasmarc.iter().map(String::as_str).collect();
        self.style = text_style(&globals, &plasmarc);
        let family = font_family(&globals).unwrap_or("sans-serif");
        if self.family.as_deref() != Some(family) {
            // Drop the old face before resolving the new one, so the two are never mapped
            // together.
            self.font = None;
            self.font = FtText::new(family).map(|t| OsFont::new(Box::new(t)));
            // Remembered only once it loads, so a failed lookup is tried again on the next
            // change to either file.
            self.family = self.font.as_ref().map(|_| family.to_owned());
        }
    }
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// The places KDE looks for the config file `name`, highest priority first: `$XDG_CONFIG_HOME`
/// (else `$HOME/.config`), then each of `$XDG_CONFIG_DIRS` (default `/etc/xdg`). Plasma's
/// session prepends its look-and-feel defaults to the latter.
fn config_files(name: &str) -> Vec<PathBuf> {
    let var = |k| std::env::var_os(k).filter(|v| !v.is_empty());
    let home = var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| var("HOME").map(|h| PathBuf::from(h).join(".config")));
    let dirs = var("XDG_CONFIG_DIRS").unwrap_or_else(|| "/etc/xdg".into());
    let dirs = std::env::split_paths(&dirs).filter(|d| !d.as_os_str().is_empty());
    home.into_iter().chain(dirs).map(|d| d.join(name)).collect()
}

/// The last value of `key` in `[group]` of a KDE config file, trimmed and unquoted. Group and
/// key names lose a `[$i]`-style marker; a locked system key doesn't outrank the user's file
/// (KConfig's immutability isn't implemented).
fn ini_value<'a>(text: &'a str, group: &str, key: &str) -> Option<&'a str> {
    let mut in_group = false;
    let mut value = None;
    for line in text.lines().map(str::trim) {
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_group = name.split("][$").next() == Some(group);
        } else if let Some((k, v)) = line.split_once('=').filter(|_| in_group) {
            if k.trim().split("[$").next() == Some(key) {
                value = Some(v.trim().trim_matches('"'));
            }
        }
    }
    value
}

/// `ini_value` over a cascade of files, highest priority first: the first file with the key wins.
fn cascade_value<'a>(files: &[&'a str], group: &str, key: &str) -> Option<&'a str> {
    files.iter().find_map(|text| ini_value(text, group, key))
}

/// The family of KDE's general font: the text before the first comma of a Qt font string
/// such as `Noto Sans,10,-1,5,400,0,0,0,0,0,0,0,0,0,0,1`.
fn font_family<'a>(kdeglobals: &[&'a str]) -> Option<&'a str> {
    let font = cascade_value(kdeglobals, "General", "font")?;
    let family = font.split(',').next()?.trim().trim_matches('"').trim();
    (!family.is_empty()).then_some(family)
}

/// Plain text in the colour scheme's window text colour when the panel uses Plasma's default
/// theme (Breeze), which follows the colour scheme; otherwise white with an outline, which
/// reads on any panel. With no colour set anywhere it is Breeze's own default, 35,38,41, which
/// is what Plasma draws in that case.
fn text_style(kdeglobals: &[&str], plasmarc: &[&str]) -> TextStyle {
    match cascade_value(plasmarc, "Theme", "name") {
        None | Some("default") => TextStyle {
            fill: cascade_value(kdeglobals, "Colors:Window", "ForegroundNormal")
                .and_then(rgb)
                .unwrap_or(0x23_2629),
            outline: false,
        },
        Some(_) => OUTLINED,
    }
}

/// A KDE colour `r,g,b` (or `r,g,b,a`, alpha ignored) as 0xRRGGBB.
fn rgb(v: &str) -> Option<u32> {
    let mut parts = v.split(',').map(|p| p.trim().parse::<u8>());
    let mut c = 0;
    for _ in 0..3 {
        c = c << 8 | u32::from(parts.next()?.ok()?);
    }
    Some(c)
}

// FreeType 2 structures, read through the pointers FreeType returns. Each mirrors the C
// declaration field by field, in order, so the offsets match the library's ABI; structures
// that are only read up to a field leave out the fields after it.

/// `FT_Generic`, freetype/fttypes.h.
#[repr(C)]
#[allow(dead_code)]
struct Generic {
    data: *mut c_void,
    finalizer: *mut c_void,
}

/// `FT_Vector`, freetype/ftimage.h. 26.6 fixed point.
#[repr(C)]
#[allow(dead_code)]
struct Vector {
    x: c_long,
    y: c_long,
}

/// `FT_Bitmap`, freetype/ftimage.h.
#[repr(C)]
#[allow(dead_code)]
struct Bitmap {
    rows: c_uint,
    width: c_uint,
    pitch: c_int,
    buffer: *mut u8,
    num_grays: c_ushort,
    pixel_mode: u8,
    palette_mode: u8,
    palette: *mut c_void,
}

/// `FT_FaceRec`, freetype/freetype.h, up to `glyph`.
#[repr(C)]
#[allow(dead_code)]
struct FaceRec {
    num_faces: c_long,
    face_index: c_long,
    face_flags: c_long,
    style_flags: c_long,
    num_glyphs: c_long,
    family_name: *mut c_char,
    style_name: *mut c_char,
    num_fixed_sizes: c_int,
    available_sizes: *mut c_void,
    num_charmaps: c_int,
    charmaps: *mut c_void,
    generic: Generic,
    /// `FT_BBox`: xMin, yMin, xMax, yMax.
    bbox: [c_long; 4],
    units_per_em: c_ushort,
    ascender: c_short,
    descender: c_short,
    height: c_short,
    max_advance_width: c_short,
    max_advance_height: c_short,
    underline_position: c_short,
    underline_thickness: c_short,
    glyph: *mut GlyphSlotRec,
}

/// `FT_GlyphSlotRec`, freetype/freetype.h, up to `bitmap_top`.
#[repr(C)]
#[allow(dead_code)]
struct GlyphSlotRec {
    library: *mut c_void,
    face: *mut c_void,
    next: *mut c_void,
    glyph_index: c_uint,
    generic: Generic,
    /// `FT_Glyph_Metrics`: eight `FT_Pos`.
    metrics: [c_long; 8],
    linear_hori_advance: c_long,
    linear_vert_advance: c_long,
    advance: Vector,
    /// `FT_Glyph_Format`, an enum.
    format: c_uint,
    bitmap: Bitmap,
    bitmap_left: c_int,
    bitmap_top: c_int,
}

type FtLibrary = *mut c_void;
type FtFace = *mut FaceRec;
type SetPixelSizesFn = unsafe extern "C" fn(FtFace, c_uint, c_uint) -> c_int;
type LoadCharFn = unsafe extern "C" fn(FtFace, c_ulong, i32) -> c_int;
type DoneFaceFn = unsafe extern "C" fn(FtFace) -> c_int;
type DoneFreeTypeFn = unsafe extern "C" fn(FtLibrary) -> c_int;

/// `FT_LOAD_RENDER | FT_LOAD_NO_BITMAP`: outlines rendered with grayscale anti-aliasing,
/// never a font's embedded 1-bit bitmaps.
const LOAD_FLAGS: i32 = 1 << 2 | 1 << 3;
const PIXEL_MODE_GRAY: u8 = 2;

/// The function `name` (NUL-terminated) from `lib`.
///
/// # Safety
/// `T` must be the function's C signature, and `lib` must stay loaded while it is called.
unsafe fn sym<T: Copy>(lib: &Library, name: &[u8]) -> Option<T> {
    lib.get::<T>(name).ok().map(|s| *s)
}

/// Draws text in one font face with FreeType. Lives on the thread that made it.
pub struct FtText {
    set_pixel_sizes: SetPixelSizesFn,
    load_char: LoadCharFn,
    done_face: DoneFaceFn,
    done_freetype: DoneFreeTypeFn,
    library: FtLibrary,
    face: FtFace,
    /// Pixel size last set on the face; 0 before the first.
    px: u32,
    // Kept last: must outlive every call through the pointers above.
    _ft: Library,
}

impl FtText {
    /// The face fontconfig picks for `family`; None if FreeType, fontconfig or the font is
    /// unavailable.
    pub fn new(family: &str) -> Option<FtText> {
        let family = CString::new(family).ok()?;
        // SAFETY: loads the system's FreeType by soname; its initialisers are trusted as part
        // of the OS. Loaded first, so it stays loaded while fontconfig, which uses it, comes
        // and goes.
        let ft = unsafe { Library::new("libfreetype.so.6").ok()? };
        let (file, index) = font_file(&family)?;
        // SAFETY: each signature matches freetype.h. A face that fails to open leaves nothing
        // behind but the library, which is released.
        unsafe {
            let init: unsafe extern "C" fn(*mut FtLibrary) -> c_int =
                sym(&ft, b"FT_Init_FreeType\0")?;
            let new_face: unsafe extern "C" fn(
                FtLibrary,
                *const c_char,
                c_long,
                *mut FtFace,
            ) -> c_int = sym(&ft, b"FT_New_Face\0")?;
            let set_pixel_sizes: SetPixelSizesFn = sym(&ft, b"FT_Set_Pixel_Sizes\0")?;
            let load_char: LoadCharFn = sym(&ft, b"FT_Load_Char\0")?;
            let done_face: DoneFaceFn = sym(&ft, b"FT_Done_Face\0")?;
            let done_freetype: DoneFreeTypeFn = sym(&ft, b"FT_Done_FreeType\0")?;
            let mut library = null_mut();
            if init(&mut library) != 0 {
                return None;
            }
            let mut face = null_mut();
            if new_face(library, file.as_ptr(), index, &mut face) != 0 || face.is_null() {
                done_freetype(library);
                return None;
            }
            Some(FtText {
                set_pixel_sizes,
                load_char,
                done_face,
                done_freetype,
                library,
                face,
                px: 0,
                _ft: ft,
            })
        }
    }
}

/// The file and face index fontconfig picks for `family`, as it would for any app asking for
/// that family at regular weight. Fontconfig is released entirely afterwards (`FcFini`, then
/// unloaded), which is safe only because nothing else in this process uses it (neither ksni
/// nor FreeType), no other thread calls it meanwhile (only the main loop does), and every
/// pattern and configuration is destroyed first. Otherwise the font caches it maps would stay
/// resident.
fn font_file(family: &CStr) -> Option<(CString, c_long)> {
    const MATCH_PATTERN: c_int = 0; // FcMatchPattern
    const RESULT_MATCH: c_int = 0; // FcResultMatch

    // SAFETY: loads the system's fontconfig by soname; its initialisers are trusted as part
    // of the OS. Each signature matches fontconfig.h. Every pattern and the configuration
    // made here are destroyed before `FcFini`, and the file name is copied out of the matched
    // pattern, which owns it, before that pattern is destroyed. No fontconfig object
    // outlives this function, which unloads the library on return.
    unsafe {
        let fc = &Library::new("libfontconfig.so.1").ok()?;
        let init: unsafe extern "C" fn() -> *mut c_void = sym(fc, b"FcInitLoadConfigAndFonts\0")?;
        let config_destroy: unsafe extern "C" fn(*mut c_void) = sym(fc, b"FcConfigDestroy\0")?;
        let fini: unsafe extern "C" fn() = sym(fc, b"FcFini\0")?;
        let create: unsafe extern "C" fn() -> *mut c_void = sym(fc, b"FcPatternCreate\0")?;
        let destroy: unsafe extern "C" fn(*mut c_void) = sym(fc, b"FcPatternDestroy\0")?;
        let add_string: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int =
            sym(fc, b"FcPatternAddString\0")?;
        let substitute: unsafe extern "C" fn(*mut c_void, *mut c_void, c_int) -> c_int =
            sym(fc, b"FcConfigSubstitute\0")?;
        let default_substitute: unsafe extern "C" fn(*mut c_void) =
            sym(fc, b"FcDefaultSubstitute\0")?;
        let font_match: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_int) -> *mut c_void =
            sym(fc, b"FcFontMatch\0")?;
        let get_string: unsafe extern "C" fn(
            *mut c_void,
            *const c_char,
            c_int,
            *mut *const c_char,
        ) -> c_int = sym(fc, b"FcPatternGetString\0")?;
        let get_integer: unsafe extern "C" fn(
            *mut c_void,
            *const c_char,
            c_int,
            *mut c_int,
        ) -> c_int = sym(fc, b"FcPatternGetInteger\0")?;

        let config = init();
        if config.is_null() {
            fini();
            return None;
        }
        let mut found = None;
        let pattern = create();
        if !pattern.is_null() {
            if add_string(pattern, c"family".as_ptr(), family.as_ptr()) != 0 {
                substitute(config, pattern, MATCH_PATTERN);
                default_substitute(pattern);
                let mut result = 0;
                let font = font_match(config, pattern, &mut result);
                if !font.is_null() {
                    let mut file = std::ptr::null();
                    let mut index = 0;
                    if get_string(font, c"file".as_ptr(), 0, &mut file) == RESULT_MATCH
                        && !file.is_null()
                    {
                        if get_integer(font, c"index".as_ptr(), 0, &mut index) != RESULT_MATCH {
                            index = 0;
                        }
                        found = Some((CStr::from_ptr(file).to_owned(), c_long::from(index)));
                    }
                    destroy(font);
                }
            }
            destroy(pattern);
        }
        config_destroy(config);
        // Safe only because nothing else in this process uses fontconfig (ksni doesn't, and
        // FreeType's harfbuzz doesn't), and fontconfig registers no destructors that would run
        // after it is unloaded.
        fini();
        found
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
        return AlphaMask {
            w: 0,
            h: 0,
            a: Vec::new(),
        };
    }
    let mut out = Vec::with_capacity(((x1 - x0) * (y1 - y0)) as usize);
    for y in y0..y1 {
        out.extend_from_slice(&a[(y * w + x0) as usize..(y * w + x1) as usize]);
    }
    AlphaMask {
        w: x1 - x0,
        h: y1 - y0,
        a: out,
    }
}

impl TextRasterizer for FtText {
    fn rasterize(&mut self, text: &str, px: u32) -> Option<AlphaMask> {
        if px != self.px {
            // SAFETY: `face` is a live face of `library`.
            if unsafe { (self.set_pixel_sizes)(self.face, 0, px) } != 0 {
                return None;
            }
            self.px = px;
        }
        // An em of room around the pen line on every side, for ink beyond the advances.
        let n = text.chars().count() as u32;
        let (w, h) = ((n + 2) * px, 3 * px);
        let mut a = vec![0u8; (w * h) as usize];
        let baseline = 2 * px as i32;
        let mut pen = (px as c_long) << 6; // 26.6 fixed point
        for c in text.chars() {
            // SAFETY: `face` is live, and after a successful load its glyph slot holds the
            // rendered glyph until the next load.
            let slot = unsafe {
                if (self.load_char)(self.face, c_ulong::from(c), LOAD_FLAGS) != 0 {
                    return None;
                }
                &*(*self.face).glyph
            };
            let b = &slot.bitmap;
            if b.rows > 0 && b.width > 0 {
                if b.pixel_mode != PIXEL_MODE_GRAY
                    || b.buffer.is_null()
                    || b.pitch < b.width as c_int
                {
                    return None;
                }
                let x0 = (pen >> 6) as i32 + slot.bitmap_left;
                let y0 = baseline - slot.bitmap_top;
                for r in 0..b.rows {
                    // SAFETY: a gray bitmap is `rows` rows of `width` bytes, each `pitch`
                    // (checked positive) bytes after the one above it.
                    let row = unsafe {
                        std::slice::from_raw_parts(
                            b.buffer.add(r as usize * b.pitch as usize),
                            b.width as usize,
                        )
                    };
                    let y = y0 + r as i32;
                    for (i, &v) in row.iter().enumerate() {
                        let x = x0 + i as i32;
                        if x >= 0 && y >= 0 && (x as u32) < w && (y as u32) < h {
                            let p = &mut a[(y as u32 * w + x as u32) as usize];
                            *p = (*p).max(v);
                        }
                    }
                }
            }
            pen += slot.advance.x;
        }
        Some(crop(&a, w, h))
    }
}

impl Drop for FtText {
    fn drop(&mut self) {
        // SAFETY: the face and the library came from FreeType and are released once, face
        // first, while the library that holds these functions is still loaded.
        unsafe {
            (self.done_face)(self.face);
            (self.done_freetype)(self.library);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_font_family_and_text_colour() {
        let normal = "[General]\nfont=Noto Sans,10,-1,5,400,0,0,0,0,0,0,0,0,0,0,1\n\n\
                      [Colors:Window]\nForegroundNormal=35,38,41\n";
        let plain = |fill| TextStyle {
            fill,
            outline: false,
        };
        assert_eq!(font_family(&[normal]), Some("Noto Sans"));
        assert!(text_style(&[normal], &[]) == plain(0x23_2629));
        // A Plasma theme other than the default doesn't follow the colour scheme.
        assert!(text_style(&[normal], &["[Theme]\nname=breeze-twilight\n"]) == OUTLINED);
        assert!(text_style(&[normal], &["[Theme]\nname=default\n"]) == plain(0x23_2629));

        // The keys only appear elsewhere: the font defaults, and the colour is Breeze's own.
        let missing = "[General]\nfixed=Hack,10\n[Colors:View]\nForegroundNormal=1,2,3\n";
        assert_eq!(font_family(&[missing]), None);
        assert!(text_style(&[missing], &[]) == plain(0x23_2629));

        let odd = "[General]\r\n  font = \"DejaVu Serif , 11\"\r\n\
                   [Colors:Window]\r\nForegroundNormal = 252, 252, 252\r\n";
        assert_eq!(font_family(&[odd]), Some("DejaVu Serif"));
        assert!(text_style(&[odd], &[]) == plain(0xFC_FCFC));

        // Cascade: the first file with the key wins; a file lacking it defers to the next.
        let system =
            "[General][$i]\nfont[$i]=Cantarell,11\n[Colors:Window]\nForegroundNormal=1,2,3\n";
        assert_eq!(font_family(&[missing, system]), Some("Cantarell"));
        assert_eq!(font_family(&[normal, system]), Some("Noto Sans"));
        assert!(text_style(&[missing, system], &[]) == plain(0x01_0203));
    }
}
