//! Taskbar-text mode: a layered child window of each taskbar, left of its notification area,
//! showing `[icon] 54°  [icon] 61°` in the taskbar's own font and colour.
//!
//! The windows are children of Explorer's taskbar, so they share its input queue: nothing on the
//! UI thread may block while they exist.

use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Once;

use windows_sys::w;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC, SelectObject,
    AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION,
    DIB_RGB_COLORS,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::{
    InitCommonControlsEx, ICC_WIN95_CLASSES, INITCOMMONCONTROLSEX, TTTOOLINFOW, TOOLTIPS_CLASSW,
    TTF_SUBCLASS, TTM_ADDTOOLW, TTM_NEWTOOLRECTW, TTM_SETMAXTIPWIDTH, TTM_UPDATETIPTEXTW,
    TTS_ALWAYSTIP, TTS_NOPREFIX,
};
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, FindWindowExW, FindWindowW, GetCursorPos,
    GetWindowRect, IsWindow, LoadCursorW, PostMessageW, RegisterClassW, SendMessageW, SetWindowPos,
    UpdateLayeredWindow, CW_USEDEFAULT, HWND_TOP, IDC_ARROW, MA_NOACTIVATE, SWP_NOACTIVATE,
    ULW_ALPHA, WM_CONTEXTMENU, WM_MOUSEACTIVATE, WM_RBUTTONUP, WNDCLASSW, WS_CHILD, WS_EX_LAYERED,
    WS_EX_NOPARENTNOTIFY, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
};

use super::font;
use super::tray::{copy_tip, TIP_LEN, WM_TRAY};
use crate::app::{App, Changes};
use crate::render::{render_text_strip, Canvas, OsFont, TextStyle};
use crate::sensor::Kind;

/// The window that gets the right-click, for the menu.
static MAIN: AtomicIsize = AtomicIsize::new(0);

/// Gap between the strip and the notification area, at 96 DPI.
const GAP: i32 = 8;
/// Padding on each side of the clock text in a taskbar without a Win32 notification area
/// (Windows 11's secondary taskbars), at 96 DPI.
const CLOCK_PAD: i32 = 12;
/// A clock this wide (at 96 DPI) is assumed if the font can't be read.
const CLOCK_FALLBACK: i32 = 62;

/// The label font and colours in use.
pub struct Look<'a> {
    pub font: Option<&'a mut OsFont>,
    pub style: TextStyle,
}

/// Position facts compared on every tick, so a moved or resized taskbar is noticed.
#[derive(Clone, Copy, PartialEq, Default)]
struct Geo {
    taskbar: [i32; 4],
    notify: [i32; 4],
    dpi: u32,
}

struct Bar {
    taskbar: HWND,
    win: HWND,
    tip: HWND,
    /// The taskbar's `TrayNotifyWnd`, if it has one.
    notify: HWND,
    geo: Geo,
}

pub struct TaskbarText {
    bars: Vec<Bar>,
    all: bool,
    icons: Option<OsFont>,
    /// Whether `icons` was made for light text; it is made again when the taskbar theme flips.
    icons_light: bool,
}

fn icon_font(light: bool) -> Option<OsFont> {
    font::icon_font().map(|lf| OsFont::new(Box::new(font::GdiText::new(lf, light))))
}

fn rect_of(h: HWND) -> [i32; 4] {
    let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: `r` is a valid out-pointer; a dead window just leaves it zero.
    unsafe { GetWindowRect(h, &mut r) };
    [r.left, r.top, r.right, r.bottom]
}

unsafe extern "system" fn strip_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_RBUTTONUP => {
            let mut pt = POINT { x: 0, y: 0 };
            // SAFETY: valid out-pointer. The menu runs from the main window, as for the tray
            // icons: this handler never touches the state.
            unsafe {
                GetCursorPos(&mut pt);
                let anchor = (pt.x as u16 as usize) | (pt.y as u16 as usize) << 16;
                PostMessageW(MAIN.load(Ordering::Relaxed) as HWND, WM_TRAY, anchor, WM_CONTEXTMENU as isize);
            }
            0
        }
        // Don't take the focus from what the user is working in.
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        // SAFETY: default handling.
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

fn tool(win: HWND, id: usize, r: [i32; 4], text: *mut u16) -> TTTOOLINFOW {
    TTTOOLINFOW {
        cbSize: size_of::<TTTOOLINFOW>() as u32,
        uFlags: TTF_SUBCLASS,
        hwnd: win,
        uId: id,
        rect: RECT { left: r[0], top: r[1], right: r[2], bottom: r[3] },
        hinst: null_mut(),
        lpszText: text,
        lParam: 0,
        lpReserved: null_mut(),
    }
}

/// Makes the strip window and its tooltip control for one taskbar.
fn create_bar(taskbar: HWND) -> Option<Bar> {
    let r = rect_of(taskbar);
    // Side-docked taskbars have no room for a horizontal strip.
    if r[3] - r[1] > r[2] - r[0] {
        return None;
    }
    // SAFETY: plain window creation; the class is registered and the strings are static.
    unsafe {
        let hinstance = GetModuleHandleW(null());
        // NOPARENTNOTIFY: creating and destroying the strip then sends nothing to Explorer's
        // thread, so a hung Explorer can't block ours.
        let win = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOPARENTNOTIFY,
            w!("TrayTempStrip"),
            null(),
            WS_CHILD | WS_VISIBLE,
            0,
            0,
            1,
            1,
            taskbar,
            null_mut(),
            hinstance,
            null(),
        );
        if win.is_null() {
            return None;
        }
        let tip = CreateWindowExW(
            WS_EX_TOPMOST,
            TOOLTIPS_CLASSW,
            null(),
            WS_POPUP | TTS_NOPREFIX | TTS_ALWAYSTIP,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            win,
            null_mut(),
            hinstance,
            null(),
        );
        if tip.is_null() {
            DestroyWindow(win);
            return None;
        }
        // Wide enough that the tooltip's lines are never wrapped; its newlines still apply.
        SendMessageW(tip, TTM_SETMAXTIPWIDTH, 0, 400);
        for id in 0..2 {
            let ti = tool(win, id, [0; 4], w!("") as *mut u16);
            SendMessageW(tip, TTM_ADDTOOLW, 0, &ti as *const TTTOOLINFOW as LPARAM);
        }
        let notify = FindWindowExW(taskbar, null_mut(), w!("TrayNotifyWnd"), null());
        Some(Bar { taskbar, win, tip, notify, geo: Geo::default() })
    }
}

fn destroy_bar(b: &Bar) {
    // SAFETY: windows of ours, or already gone (Explorer destroys them with its taskbar), in
    // which case the checks fail and nothing happens.
    unsafe {
        if IsWindow(b.tip) != 0 {
            DestroyWindow(b.tip);
        }
        if IsWindow(b.win) != 0 {
            DestroyWindow(b.win);
        }
    }
}

fn set_tip(b: &Bar, k: Kind, text: &str) {
    let mut buf = [0u16; TIP_LEN];
    copy_tip(&mut buf, text);
    let ti = tool(b.win, k.index(), [0; 4], buf.as_mut_ptr());
    // SAFETY: the control copies the text during the call.
    unsafe { SendMessageW(b.tip, TTM_UPDATETIPTEXTW, 0, &ti as *const TTTOOLINFOW as LPARAM) };
}

/// Shows `c` in the layered window `win` at `x` (in its parent), full height.
fn paint(win: HWND, x: i32, c: &Canvas) {
    // SAFETY: every GDI object is checked, selected out and deleted before returning; `bits` is
    // the w x h 32-bit pixel array of the DIB section, which lives until it is deleted.
    unsafe {
        let screen = GetDC(null_mut());
        let dc = CreateCompatibleDC(screen);
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: c.w as i32,
                biHeight: -(c.h as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = null_mut();
        let bmp = CreateDIBSection(dc, &bmi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        if !bmp.is_null() && !dc.is_null() {
            let out = std::slice::from_raw_parts_mut(bits.cast::<u32>(), c.px.len());
            for (o, &p) in out.iter_mut().zip(&c.px) {
                let a = p >> 24;
                let pre = |shift: u32| ((p >> shift & 0xFF) * a + 127) / 255;
                // Alpha 0 would let clicks and the mouse pass through to the taskbar, so the
                // background gets 1/255 black: invisible, but it keeps the whole strip hot.
                *o = if a == 0 { 0x0100_0000 } else { a << 24 | pre(16) << 16 | pre(8) << 8 | pre(0) };
            }
            let old = SelectObject(dc, bmp);
            SetWindowPos(win, HWND_TOP, x, 0, c.w as i32, c.h as i32, SWP_NOACTIVATE);
            let size = SIZE { cx: c.w as i32, cy: c.h as i32 };
            let origin = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            UpdateLayeredWindow(win, screen, null(), &size, dc, &origin, 0, &blend, ULW_ALPHA);
            SelectObject(dc, old);
        }
        if !bmp.is_null() {
            DeleteObject(bmp);
        }
        if !dc.is_null() {
            DeleteDC(dc);
        }
        ReleaseDC(null_mut(), screen);
    }
}

fn scale(v: i32, dpi: u32) -> i32 {
    (v * dpi as i32 + 48) / 96
}

impl TaskbarText {
    /// Puts the strip on the main taskbar, and on the others if `all`. `None` if the main
    /// taskbar can't take it (not running yet, docked at a side, window creation failed): the
    /// caller then shows tray icons instead.
    pub fn create(main: HWND, all: bool, app: &App, look: &mut Look) -> Option<TaskbarText> {
        MAIN.store(main as isize, Ordering::Relaxed);
        static CLASSES: Once = Once::new();
        // SAFETY: registers our strip class and loads the tooltip class, once per process.
        CLASSES.call_once(|| unsafe {
            let icc = INITCOMMONCONTROLSEX {
                dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
                dwICC: ICC_WIN95_CLASSES,
            };
            InitCommonControlsEx(&icc);
            let wc = WNDCLASSW {
                lpfnWndProc: Some(strip_proc),
                hInstance: GetModuleHandleW(null()),
                hCursor: LoadCursorW(null_mut(), IDC_ARROW),
                lpszClassName: w!("TrayTempStrip"),
                ..Default::default()
            };
            RegisterClassW(&wc);
        });
        let light = font::is_light(look.style.fill);
        let mut t = TaskbarText { bars: Vec::new(), all, icons: icon_font(light), icons_light: light };
        t.relayout(all, app, look);
        // SAFETY: plain query.
        let main_taskbar = unsafe { FindWindowW(w!("Shell_TrayWnd"), null()) };
        if t.bars.first().is_some_and(|b| b.taskbar == main_taskbar) {
            Some(t)
        } else {
            t.destroy();
            None
        }
    }

    /// Finds the taskbars again and rebuilds every strip: after Explorer restarts, or the
    /// monitors, scaling or the "all taskbars" choice changed.
    pub fn relayout(&mut self, all: bool, app: &App, look: &mut Look) {
        self.all = all;
        for b in &self.bars {
            destroy_bar(b);
        }
        self.bars.clear();
        // SAFETY: plain window lookups.
        unsafe {
            let main = FindWindowW(w!("Shell_TrayWnd"), null());
            if main.is_null() {
                return;
            }
            self.bars.extend(create_bar(main));
            if self.bars.is_empty() {
                return;
            }
            let mut prev = null_mut();
            loop {
                prev = FindWindowExW(null_mut(), prev, w!("Shell_SecondaryTrayWnd"), null());
                if !all || prev.is_null() {
                    break;
                }
                self.bars.extend(create_bar(prev));
            }
        }
        self.set_tips(app);
        self.redraw(app, look);
    }

    /// Sets every enabled sensor's tooltip text from the app.
    pub fn set_tips(&self, app: &App) {
        for b in &self.bars {
            for k in [Kind::Cpu, Kind::Gpu] {
                if app.enabled(k) {
                    set_tip(b, k, app.tooltip(k));
                }
            }
        }
    }

    /// Draws every strip again from the app's values.
    pub fn redraw(&mut self, app: &App, look: &mut Look) {
        let value = |k| app.enabled(k).then(|| app.shown(k));
        let light = font::is_light(look.style.fill);
        if light != self.icons_light {
            self.icons = icon_font(light);
            self.icons_light = light;
        }
        let (em, ui) = match font::ui_font() {
            Some(lf) => (lf.lfHeight.unsigned_abs().max(8), Some(lf)),
            None => (12, None),
        };
        for b in &mut self.bars {
            let geo = Geo {
                taskbar: rect_of(b.taskbar),
                notify: if b.notify.is_null() { [0; 4] } else { rect_of(b.notify) },
                // SAFETY: plain query.
                dpi: unsafe { GetDpiForWindow(b.taskbar) }.max(96),
            };
            b.geo = geo;
            let (tw, th) = (geo.taskbar[2] - geo.taskbar[0], (geo.taskbar[3] - geo.taskbar[1]) as u32);
            let px = scale(em as i32, geo.dpi) as u32;
            let strip = render_text_strip(
                value(Kind::Cpu),
                value(Kind::Gpu),
                px,
                th,
                look.font.as_deref_mut(),
                self.icons.as_mut(),
                look.style,
            );
            // The strip's right edge stays put as its width changes. It sits left of the
            // notification area; where there is no such window, left of the clock.
            let anchor = if b.notify.is_null() {
                let text = ui.map_or(scale(CLOCK_FALLBACK, geo.dpi), |lf| font::clock_text_width(lf, px));
                tw - text - 2 * scale(CLOCK_PAD, geo.dpi)
            } else {
                geo.notify[0] - geo.taskbar[0]
            };
            let x = (anchor - scale(GAP, geo.dpi) - strip.canvas.w as i32).max(0);
            paint(b.win, x, &strip.canvas);
            for k in [Kind::Cpu, Kind::Gpu] {
                let r = match strip.spans[k.index()] {
                    Some((a, z)) => [a as i32, 0, z as i32, th as i32],
                    None => [0; 4],
                };
                let ti = tool(b.win, k.index(), r, null_mut());
                // SAFETY: the control copies the rectangle during the call.
                unsafe { SendMessageW(b.tip, TTM_NEWTOOLRECTW, 0, &ti as *const TTTOOLINFOW as LPARAM) };
            }
        }
    }

    /// Per tick: new tooltip texts, a redraw when a shown value changed, and a check that the
    /// taskbars are still where the strips were laid out for. No lookups unless one is gone.
    pub fn tick(&mut self, app: &App, changes: Changes, look: &mut Look) {
        for k in [Kind::Cpu, Kind::Gpu] {
            if changes.tooltip[k.index()] && app.enabled(k) {
                for b in &self.bars {
                    set_tip(b, k, app.tooltip(k));
                }
            }
        }
        // SAFETY: plain queries.
        let gone = self.bars.is_empty()
            || self.bars.iter().any(|b| unsafe { IsWindow(b.taskbar) == 0 || IsWindow(b.win) == 0 });
        if gone {
            self.relayout(self.all, app, look);
            return;
        }
        // A taskbar that was still being built when the strip was laid out gets its
        // notification area found now.
        for b in self.bars.iter_mut().filter(|b| b.notify.is_null()) {
            // SAFETY: plain lookup.
            b.notify = unsafe { FindWindowExW(b.taskbar, null_mut(), w!("TrayNotifyWnd"), null()) };
        }
        let moved = self.bars.iter().any(|b| {
            let now = Geo {
                taskbar: rect_of(b.taskbar),
                notify: if b.notify.is_null() { [0; 4] } else { rect_of(b.notify) },
                // SAFETY: plain query.
                dpi: unsafe { GetDpiForWindow(b.taskbar) }.max(96),
            };
            now != b.geo
        });
        if moved || changes.value.iter().any(|&v| v) {
            self.redraw(app, look);
        }
    }

    pub fn destroy(self) {
        for b in &self.bars {
            destroy_bar(b);
        }
    }
}
