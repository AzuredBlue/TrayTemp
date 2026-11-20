//! Taskbar-text mode: a layered child window of each taskbar, left of its notification area,
//! showing `[icon] 54  [icon] 61` in the taskbar's own font and colour.
//!
//! The windows are children of Explorer's taskbar, so they share its input queue: nothing on the
//! UI thread may block while they exist.

use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Once;

use windows_sys::w;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC, SelectObject,
    AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION,
    DIB_RGB_COLORS, LOGFONTW,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows_sys::Win32::UI::Controls::{
    InitCommonControlsEx, ICC_WIN95_CLASSES, INITCOMMONCONTROLSEX, TTTOOLINFOW, TOOLTIPS_CLASSW,
    TTF_SUBCLASS, TTM_ADDTOOLW, TTM_NEWTOOLRECTW, TTM_SETMAXTIPWIDTH, TTM_UPDATETIPTEXTW,
    TTS_ALWAYSTIP, TTS_NOPREFIX, WM_MOUSELEAVE,
};
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, FindWindowExW, FindWindowW, GetCursorPos,
    GetWindowLongW, GetWindowRect, GetWindowThreadProcessId, IsWindow, IsWindowVisible,
    LoadCursorW, PostMessageW, RegisterClassW, SendMessageW, SetWindowLongW, SetWindowPos,
    UpdateLayeredWindow, CHILDID_SELF, CW_USEDEFAULT, EVENT_OBJECT_LOCATIONCHANGE, HWND_TOP,
    IDC_ARROW, MA_NOACTIVATE, OBJID_WINDOW, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, ULW_ALPHA,
    WINEVENT_OUTOFCONTEXT, WM_CONTEXTMENU, WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_RBUTTONUP,
    WNDCLASSW, WS_CHILD, WS_EX_LAYERED, WS_EX_NOPARENTNOTIFY, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_POPUP, WS_VISIBLE,
};

use super::font;
use super::uia;
use super::tray::{copy_tip, TIP_LEN, WM_TRAY};
use super::{WM_STRIP_HOVER, WM_TASKBAR_MOVED};
use crate::app::{App, Changes};
use crate::render::{render_text_strip, strip_pad, under_highlight, Canvas, OsFont, Strip, TextStyle};
use crate::sensor::Kind;

/// The window that gets the right-click, for the menu.
static MAIN: AtomicIsize = AtomicIsize::new(0);
/// A `WM_TASKBAR_MOVED` is posted and not yet handled, so a burst of events posts one message.
/// The handler clears it before it looks at the taskbar.
pub static PENDING: AtomicBool = AtomicBool::new(false);

/// Gap between the strip and the notification area, at 96 DPI.
const GAP: i32 = 8;
/// Padding on each side of the clock text in a taskbar without a Win32 notification area
/// (Windows 11's secondary taskbars), at 96 DPI.
const CLOCK_PAD: i32 = 12;
/// A clock this wide (at 96 DPI) is assumed if the font can't be read.
const CLOCK_FALLBACK: i32 = 62;
/// Room kept for the notification bell's button (at 96 DPI) when the real tray position isn't
/// known: Do Not Disturb adds it right of the clock and moves the clock left by its width.
const BELL: i32 = 28;
/// The hover highlight's inset from the strip's top and bottom, and its corner radius, at 96 DPI.
/// The radius is what Windows' own tray hover's corners measure.
const HOVER_INSET: i32 = 4;
const HOVER_RADIUS: f32 = 4.5;
/// Highlight colours (0xAARRGGBB), as Windows' own tray hover draws them: white at 15/255 on a
/// dark taskbar, black at 9/255 on a light one.
const HOVER_ON_DARK: u32 = 0x0F_FFFFFF;
const HOVER_ON_LIGHT: u32 = 0x09_000000;
/// Most windows of other programs in a taskbar that the strip steps around.
const MAX_NEIGHBOURS: usize = 8;

/// The label font and colours in use.
pub struct Look<'a> {
    pub font: Option<&'a mut OsFont>,
    pub style: TextStyle,
}

struct Bar {
    taskbar: HWND,
    win: HWND,
    tip: HWND,
    /// The taskbar's `TrayNotifyWnd`, if it has one.
    notify: HWND,
    /// What the strip was rendered for: the taskbar's width and height, and its DPI. A taskbar
    /// that only moves needs nothing, since the strip is placed in taskbar coordinates.
    size: [i32; 2],
    dpi: u32,
    /// The notification window's rectangle relative to the taskbar when last looked at.
    notify_rect: [i32; 4],
    /// The rendered strip, kept to move it and to draw the hover highlight.
    strip: Option<Strip>,
    /// Where the strip is, in taskbar coordinates.
    x: i32,
    /// Gap between the strip's padded right edge and what it sits left of.
    gap: i32,
    /// Where there is no notification window: from the clock's left edge to the taskbar's right
    /// edge, used until the tray's position is known.
    clock_off: i32,
    /// The hovered sensor's index + 1; 0 for none.
    hover: usize,
}

pub struct TaskbarText {
    bars: Vec<Bar>,
    all: bool,
    /// Reports Explorer's window moves (see `on_event`); null if it couldn't be installed.
    hook: HWINEVENTHOOK,
}

/// Where the strip's left edge goes: `w` wide with `gap` to its right, ending at `anchor`, and
/// left of every range in `others` (windows' left and right edges) that is in the way.
/// Overlapping beats covering the Start button: if the walk ends below 0, the strip stays at its
/// natural spot.
fn step_aside(anchor: i32, w: i32, gap: i32, others: &[(i32, i32)]) -> i32 {
    let natural = anchor - gap - w;
    let mut x = natural;
    for _ in 0..=others.len() {
        let hit = others.iter().filter(|&&(l, r)| l < anchor && l < x + w + gap && r > x).map(|&(l, _)| l).min();
        match hit {
            Some(l) => x = l - gap - w,
            None => break,
        }
    }
    if x < 0 {
        natural
    } else {
        x
    }
}

/// Left and right edges, relative to the taskbar, of the windows of other programs that sit in
/// it, and how many there are. Windows of the taskbar's process and of ours don't count, nor do
/// invisible ones, ones outside the bar, or ones wider than a third of it (overlays).
fn neighbours(taskbar: HWND, tb: [i32; 4]) -> ([(i32, i32); MAX_NEIGHBOURS], usize) {
    let mut out = [(0, 0); MAX_NEIGHBOURS];
    let mut n = 0;
    let (mut owner, me) = (0, std::process::id());
    // SAFETY: plain queries with valid out-pointers; none of them sends a message to the
    // taskbar's thread.
    unsafe {
        GetWindowThreadProcessId(taskbar, &mut owner);
        let mut prev = null_mut();
        while n < MAX_NEIGHBOURS {
            prev = FindWindowExW(taskbar, prev, null(), null());
            if prev.is_null() {
                break;
            }
            let mut pid = 0;
            GetWindowThreadProcessId(prev, &mut pid);
            if pid == owner || pid == me || IsWindowVisible(prev) == 0 {
                continue;
            }
            let r = rect_of(prev);
            let (l, right) = (r[0] - tb[0], r[2] - tb[0]);
            if right > l && r[3] > r[1] && r[1] < tb[3] && r[3] > tb[1] && right - l <= (tb[2] - tb[0]) / 3 {
                out[n] = (l, right);
                n += 1;
            }
        }
    }
    (out, n)
}

fn rect_of(h: HWND) -> [i32; 4] {
    let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: `r` is a valid out-pointer; a dead window just leaves it zero.
    unsafe { GetWindowRect(h, &mut r) };
    [r.left, r.top, r.right, r.bottom]
}

/// Called when Explorer moves or resizes a window. It can run inside any of our message calls, so
/// it only sets a flag and posts.
unsafe extern "system" fn on_event(
    _hook: HWINEVENTHOOK,
    _event: u32,
    _hwnd: HWND,
    object: i32,
    child: i32,
    _thread: u32,
    _time: u32,
) {
    if object != OBJID_WINDOW || child != CHILDID_SELF as i32 {
        return;
    }
    if !PENDING.swap(true, Ordering::AcqRel) {
        // SAFETY: plain post to our main window.
        if unsafe { PostMessageW(MAIN.load(Ordering::Relaxed) as HWND, WM_TASKBAR_MOVED, 0, 0) } == 0 {
            PENDING.store(false, Ordering::Release);
        }
    }
}

// The strip window's extra bytes are five i32 slots. Slots 0..=3 are the padded spans (a, z) of
// CPU and GPU, 0, 0 for a disabled one; slot 4 is the hovered sensor's index + 1, 0 for none.
const SLOT_COUNT: i32 = 5;
const HOVER_SLOT: i32 = 4;

fn slot(win: HWND, i: i32) -> i32 {
    // SAFETY: `i` is below `SLOT_COUNT`, which the class reserves; a dead window gives 0.
    unsafe { GetWindowLongW(win, i * 4) }
}

fn set_slot(win: HWND, i: i32, v: i32) {
    // SAFETY: as in `slot`.
    unsafe { SetWindowLongW(win, i * 4, v) };
}

/// Tells the main window the hovered sensor's index + 1 (0: none) changed.
fn post_hover(win: HWND, index: i32) {
    // SAFETY: plain post. The handler checks the window and the index, since any process can
    // post this message.
    unsafe { PostMessageW(MAIN.load(Ordering::Relaxed) as HWND, WM_STRIP_HOVER, win as usize, index as isize) };
}

unsafe extern "system" fn strip_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEMOVE => {
            let mut track = TRACKMOUSEEVENT {
                cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            // SAFETY: a valid struct; asks for one WM_MOUSELEAVE.
            unsafe { TrackMouseEvent(&mut track) };
            let x = (lp & 0xFFFF) as u16 as i16 as i32;
            let now = (0..2).find(|&k| (slot(hwnd, 2 * k)..slot(hwnd, 2 * k + 1)).contains(&x)).map_or(0, |k| k + 1);
            if now != slot(hwnd, HOVER_SLOT) {
                set_slot(hwnd, HOVER_SLOT, now);
                post_hover(hwnd, now);
            }
            0
        }
        WM_MOUSELEAVE => {
            if slot(hwnd, HOVER_SLOT) != 0 {
                set_slot(hwnd, HOVER_SLOT, 0);
                post_hover(hwnd, 0);
            }
            0
        }
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
        Some(Bar {
            taskbar,
            win,
            tip,
            notify,
            size: [0; 2],
            dpi: 0,
            notify_rect: [0; 4],
            strip: None,
            x: 0,
            gap: 0,
            clock_off: 0,
            hover: 0,
        })
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

fn dpi_of(taskbar: HWND) -> u32 {
    // SAFETY: plain query.
    unsafe { GetDpiForWindow(taskbar) }.max(96)
}

/// The UI font's em height and the font itself, if it can be read.
fn ui_metrics() -> (u32, Option<LOGFONTW>) {
    match font::ui_font() {
        Some(lf) => (lf.lfHeight.unsigned_abs().max(8), Some(lf)),
        None => (12, None),
    }
}

/// The taskbar's rectangle and the notification window's, relative to it (zero without one).
fn rects_of(b: &Bar) -> ([i32; 4], [i32; 4]) {
    let tb = rect_of(b.taskbar);
    let notify = if b.notify.is_null() {
        [0; 4]
    } else {
        let n = rect_of(b.notify);
        [n[0] - tb[0], n[1] - tb[1], n[2] - tb[0], n[3] - tb[1]]
    };
    (tb, notify)
}

/// Where `b`'s strip goes, in taskbar coordinates. The strip's right edge stays put as its width
/// changes. It sits left of the notification area. Where there is no such window, it is the
/// tray's left edge from UI Automation, or, until that is known, a guess from the clock's width
/// that keeps room for the notification bell. Windows of other programs in the way are stepped
/// around. No allocation: this runs on every taskbar move.
fn place(b: &Bar, tb: [i32; 4], notify: [i32; 4], w: i32) -> i32 {
    let anchor = if !b.notify.is_null() {
        notify[0]
    } else if let Some(x) = uia::tray_left(b.taskbar) {
        x - tb[0]
    } else {
        tb[2] - tb[0] - b.clock_off
    };
    let (others, n) = neighbours(b.taskbar, tb);
    step_aside(anchor, w, b.gap, &others[..n]).max(0)
}

/// Draws `b`'s retained strip at its position, with the hovered sensor's highlight.
fn show(b: &Bar, style: TextStyle) {
    let Some(strip) = &b.strip else { return };
    let span = b.hover.checked_sub(1).and_then(|i| strip.spans[i]);
    match span {
        Some(span) => {
            let argb = if font::is_light(style.fill) { HOVER_ON_DARK } else { HOVER_ON_LIGHT };
            let (inset, radius) = (scale(HOVER_INSET, b.dpi) as u32, HOVER_RADIUS * b.dpi as f32 / 96.0);
            paint(b.win, b.x, &under_highlight(&strip.canvas, span, inset, radius, argb));
        }
        None => paint(b.win, b.x, &strip.canvas),
    }
}

/// Renders `b`'s strip from the app's values and shows it, placed and with its tooltip areas.
fn render_bar(b: &mut Bar, app: &App, look: &mut Look, metrics: &(u32, Option<LOGFONTW>)) {
    let value = |k| app.enabled(k).then(|| app.shown(k));
    let (tb, notify) = rects_of(b);
    let dpi = dpi_of(b.taskbar);
    let th = (tb[3] - tb[1]) as u32;
    let px = scale(metrics.0 as i32, dpi) as u32;
    let strip = render_text_strip(value(Kind::Cpu), value(Kind::Gpu), px, th, look.font.as_deref_mut(), look.style);
    // The strip carries its own padding, so the gap to the neighbour shrinks by it.
    b.gap = (scale(GAP, dpi) - strip_pad(px) as i32).max(0);
    b.clock_off = if b.notify.is_null() {
        let text = metrics.1.map_or(scale(CLOCK_FALLBACK, dpi), |lf| font::clock_text_width(lf, px));
        text + 2 * scale(CLOCK_PAD, dpi) + scale(BELL, dpi)
    } else {
        0
    };
    (b.size, b.dpi, b.notify_rect) = ([tb[2] - tb[0], tb[3] - tb[1]], dpi, notify);
    b.x = place(b, tb, notify, strip.canvas.w as i32);
    for k in [Kind::Cpu, Kind::Gpu] {
        let r = match strip.spans[k.index()] {
            Some((a, z)) => [a as i32, 0, z as i32, th as i32],
            None => [0; 4],
        };
        set_slot(b.win, 2 * k.index() as i32, r[0]);
        set_slot(b.win, 2 * k.index() as i32 + 1, r[2]);
        let ti = tool(b.win, k.index(), r, null_mut());
        // SAFETY: the control copies the rectangle during the call.
        unsafe { SendMessageW(b.tip, TTM_NEWTOOLRECTW, 0, &ti as *const TTTOOLINFOW as LPARAM) };
    }
    b.strip = Some(strip);
    show(b, look.style);
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
                cbWndExtra: SLOT_COUNT * 4,
                hInstance: GetModuleHandleW(null()),
                hCursor: LoadCursorW(null_mut(), IDC_ARROW),
                lpszClassName: w!("TrayTempStrip"),
                ..Default::default()
            };
            RegisterClassW(&wc);
        });
        let mut t = TaskbarText { bars: Vec::new(), all, hook: null_mut() };
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

    /// Makes a strip for the main taskbar, and for the others if `all`.
    fn find_bars(&mut self, all: bool) {
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
    }

    /// Has the tray-position worker follow the taskbars that have no notification window.
    fn follow_trays(&self) {
        let bars: Vec<HWND> = self.bars.iter().filter(|b| b.notify.is_null()).map(|b| b.taskbar).collect();
        uia::set_targets(MAIN.load(Ordering::Relaxed) as HWND, &bars);
    }

    fn unhook(&mut self) {
        if !self.hook.is_null() {
            // SAFETY: a hook we installed and have not removed.
            unsafe { UnhookWinEvent(self.hook) };
            self.hook = null_mut();
        }
    }

    /// Has Explorer's taskbar thread report its windows' moves and resizes: the notification
    /// area changing size (Do Not Disturb adds or removes the bell) shows up there at once.
    /// Explorer's process and thread change when it restarts, so this runs on every `relayout`.
    fn hook_explorer(&mut self) {
        self.unhook();
        let Some(b) = self.bars.first() else { return };
        let mut pid = 0;
        // SAFETY: a valid out-pointer. The hook is out of context (read-only, delivered to our
        // message loop), limited to Explorer's process and its taskbar thread.
        unsafe {
            let tid = GetWindowThreadProcessId(b.taskbar, &mut pid);
            if tid != 0 {
                self.hook = SetWinEventHook(
                    EVENT_OBJECT_LOCATIONCHANGE,
                    EVENT_OBJECT_LOCATIONCHANGE,
                    null_mut(),
                    Some(on_event),
                    pid,
                    tid,
                    WINEVENT_OUTOFCONTEXT,
                );
            }
        }
    }

    /// Finds the taskbars again and rebuilds every strip: after Explorer restarts, or the
    /// monitors, scaling or the "all taskbars" choice changed.
    pub fn relayout(&mut self, all: bool, app: &App, look: &mut Look) {
        self.all = all;
        self.unhook();
        for b in &self.bars {
            destroy_bar(b);
        }
        self.bars.clear();
        self.find_bars(all);
        self.follow_trays();
        self.hook_explorer();
        if self.bars.is_empty() {
            return;
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
        let metrics = ui_metrics();
        for b in &mut self.bars {
            render_bar(b, app, look, &metrics);
        }
    }

    /// Looks at where the taskbars and their neighbours are, and renders a strip again only if
    /// `force` (a shown value changed) or its taskbar's size or DPI changed; otherwise just moves
    /// it if its place changed. Cheap and allocation-free when nothing changed, as it runs on
    /// every tick and on every taskbar move. Taskbars that are gone are looked up again.
    pub fn check(&mut self, app: &App, look: &mut Look, force: bool) {
        // SAFETY: plain queries.
        let gone = self.bars.is_empty()
            || self.bars.iter().any(|b| unsafe { IsWindow(b.taskbar) == 0 || IsWindow(b.win) == 0 });
        if gone {
            self.relayout(self.all, app, look);
            return;
        }
        let mut metrics = None;
        let mut tray_moved = false;
        for b in &mut self.bars {
            let (tb, notify) = rects_of(b);
            tray_moved |= notify != b.notify_rect;
            if force || [tb[2] - tb[0], tb[3] - tb[1]] != b.size || dpi_of(b.taskbar) != b.dpi {
                let m = metrics.get_or_insert_with(ui_metrics);
                render_bar(b, app, look, m);
                continue;
            }
            b.notify_rect = notify;
            let w = b.strip.as_ref().map_or(0, |s| s.canvas.w as i32);
            let x = place(b, tb, notify, w);
            if x != b.x {
                b.x = x;
                // SAFETY: moves our own window only; the layered surface keeps its pixels.
                unsafe { SetWindowPos(b.win, null_mut(), x, 0, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE) };
            }
        }
        // The other taskbars' trays follow the main one's, but UI Automation only sees that on
        // its own poll: have it look now.
        if tray_moved {
            uia::poke();
        }
    }

    /// Per tick: new tooltip texts, and a check of the taskbars (see `check`), which redraws
    /// when a shown value changed.
    pub fn tick(&mut self, app: &App, changes: Changes, look: &mut Look) {
        for k in [Kind::Cpu, Kind::Gpu] {
            if changes.tooltip[k.index()] && app.enabled(k) {
                for b in &self.bars {
                    set_tip(b, k, app.tooltip(k));
                }
            }
        }
        // A taskbar that was still being built when the strip was laid out gets its
        // notification area found now.
        let mut found = false;
        for b in self.bars.iter_mut().filter(|b| b.notify.is_null()) {
            // SAFETY: plain lookup.
            b.notify = unsafe { FindWindowExW(b.taskbar, null_mut(), w!("TrayNotifyWnd"), null()) };
            found |= !b.notify.is_null();
        }
        if found {
            self.follow_trays();
        }
        self.check(app, look, changes.value.iter().any(|&v| v));
    }

    /// The mouse is over sensor `index - 1` of strip `win` (0: over none). Ignores a window that
    /// isn't one of ours and an index that isn't a shown sensor.
    pub fn hover(&mut self, win: HWND, index: usize, style: TextStyle) {
        let Some(b) = self.bars.iter_mut().find(|b| b.win == win) else { return };
        let valid = index == 0 || b.strip.as_ref().is_some_and(|s| s.spans.get(index - 1).is_some_and(|s| s.is_some()));
        if valid && index != b.hover {
            b.hover = index;
            show(b, style);
        }
    }

    pub fn destroy(mut self) {
        self.unhook();
        uia::set_targets(null_mut(), &[]);
        for b in &self.bars {
            destroy_bar(b);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::step_aside;

    #[test]
    fn strip_steps_left_of_windows_in_its_way() {
        // Anchor 1000, strip 100 wide, gap 8: the natural spot is 892.
        assert_eq!(step_aside(1000, 100, 8, &[]), 892);
        // One in the way: the strip ends a gap left of it.
        assert_eq!(step_aside(1000, 100, 8, &[(850, 950)]), 742);
        // Chained: it is stepped around again.
        assert_eq!(step_aside(1000, 100, 8, &[(700, 745), (850, 950)]), 592);
        // Far left of us, and at or right of the anchor: ignored.
        assert_eq!(step_aside(1000, 100, 8, &[(100, 200), (1000, 1050), (1100, 1200)]), 892);
        // Walking off the bar's left end keeps the natural spot.
        assert_eq!(step_aside(1000, 100, 8, &[(50, 950)]), 892);
    }
}
