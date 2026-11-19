mod font;
mod icon;
mod menu;
mod power;
mod tray;

use std::cell::Cell;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU32, Ordering};

use windows_sys::w;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTOPRIMARY};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Power::HPOWERNOTIFY;
use windows_sys::Win32::UI::HiDpi::{
    GetDpiForMonitor, GetDpiForSystem, GetSystemMetricsForDpi, MDT_EFFECTIVE_DPI,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    ChangeWindowMessageFilterEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetMessageW, KillTimer, PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW,
    SetForegroundWindow, SetTimer, MSG, MSGFLT_ALLOW, PBT_POWERSETTINGCHANGE, SM_CXSMICON, WM_APP,
    WM_CONTEXTMENU, WM_DESTROY, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_NULL, WM_POWERBROADCAST,
    WM_SETTINGCHANGE, WM_TIMER, WNDCLASSW, WS_EX_TOOLWINDOW, WS_POPUP,
};

use crate::app::{App, Changes};
use crate::render::{render_value, OsFont, TextStyle};
use crate::sensor::pawnio::PawnIoCpu;
use crate::sensor::{Kind, Sensor};
use crate::settings::{DisplayMode, Settings};
use font::GdiText;
use tray::{TrayIcons, WM_TRAY};

const KINDS: [Kind; 2] = [Kind::Cpu, Kind::Gpu];
const TIMER_ID: usize = 1;
/// Posted to ourselves: wParam is 1 when the display came on, 0 when it went off.
const WM_DISPLAY_STATE: u32 = WM_APP + 2;
/// Posted to ourselves: the tray icon size, the UI font or the taskbar theme may have changed.
const WM_RECHECK: u32 = WM_APP + 3;

static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

struct State {
    hwnd: HWND,
    app: App,
    tray: TrayIcons,
    icon_size: u32,
    /// Face of the UI font in use, to notice when the user changes it.
    face: Option<[u16; 32]>,
    font: Option<OsFont>,
    style: TextStyle,
    display_on: bool,
    power: HPOWERNOTIFY,
}

thread_local! {
    // Moved out while a handler runs: a message that re-enters the window procedure
    // (Shell_NotifyIconW can dispatch sent messages) finds it empty instead of aliasing it.
    static STATE: Cell<Option<State>> = const { Cell::new(None) };
}

/// Runs `f` on the state; `None` if a handler further up the stack has it.
fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    let mut st = STATE.take()?;
    let r = f(&mut st);
    STATE.set(Some(st));
    Some(r)
}

/// Runs `f` on the state, or posts the message again for when the state is free.
fn run_or_repost(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, f: impl FnOnce(&mut State)) {
    if with_state(f).is_none() {
        // SAFETY: plain post to our own window.
        unsafe { PostMessageW(hwnd, msg, wp, lp) };
    }
}

/// Small-icon size at the primary monitor's current DPI, where the notification area is.
/// GetDpiForSystem alone would not follow scaling changes made while running.
fn icon_size() -> u32 {
    // SAFETY: plain queries; the out-pointers are valid locals.
    unsafe {
        let monitor = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        let (mut dpi, mut dpi_y) = (0, 0);
        if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi, &mut dpi_y) != 0 || dpi == 0 {
            dpi = GetDpiForSystem();
        }
        GetSystemMetricsForDpi(SM_CXSMICON, dpi).max(16) as u32
    }
}

impl State {
    fn show_icon(&mut self, k: Kind) {
        let canvas = render_value(self.app.shown(k), self.icon_size, self.font.as_mut(), self.style);
        self.tray.add(k, &canvas, self.app.tooltip(k));
    }

    /// One NIM_MODIFY per changed kind; renders only when the shown number changed.
    fn apply(&mut self, changes: Changes) {
        for k in KINDS {
            let i = k.index();
            if !self.app.enabled(k) || !(changes.value[i] || changes.tooltip[i]) {
                continue;
            }
            let canvas = changes.value[i]
                .then(|| render_value(self.app.shown(k), self.icon_size, self.font.as_mut(), self.style));
            let tip = changes.tooltip[i].then(|| self.app.tooltip(k));
            self.tray.update(k, canvas.as_ref(), tip);
        }
    }

    fn tick(&mut self) {
        let changes = self.app.tick();
        self.apply(changes);
    }

    fn start_timer(&self) {
        // SAFETY: replaces any running timer with the same id on our window.
        unsafe {
            SetTimer(
                self.hwnd,
                TIMER_ID,
                self.app.settings.interval_s * 1000,
                None,
            )
        };
    }

    /// Pauses sampling while the display is off. Acts only on off/on transitions, so
    /// the initial notification and dimming don't restart the timer.
    fn set_display(&mut self, on: bool) {
        if on == self.display_on {
            return;
        }
        self.display_on = on;
        if on {
            self.start_timer();
            self.tick();
        } else {
            // SAFETY: stops our own timer.
            unsafe { KillTimer(self.hwnd, TIMER_ID) };
        }
    }

    /// Reads the UI font again; true if its face changed.
    fn reload_font(&mut self) -> bool {
        let lf = font::ui_font();
        let face = lf.map(|f| f.lfFaceName);
        if face == self.face {
            return false;
        }
        self.face = face;
        self.font = lf.map(|f| OsFont::new(Box::new(GdiText::new(f))));
        true
    }

    /// Redraws the icons if the icon size, the UI font or the taskbar theme changed.
    fn recheck(&mut self) {
        let size = icon_size();
        let style = font::taskbar_style();
        let font_changed = self.reload_font();
        if size == self.icon_size && style == self.style && !font_changed {
            return;
        }
        self.icon_size = size;
        self.style = style;
        for k in KINDS {
            if self.app.enabled(k) {
                let canvas = render_value(self.app.shown(k), size, self.font.as_mut(), self.style);
                self.tray.update(k, Some(&canvas), None);
            }
        }
    }

    fn toggle(&mut self, k: Kind) {
        let s = &mut self.app.settings;
        let (this, other) = match k {
            Kind::Cpu => (&mut s.cpu_enabled, s.gpu_enabled),
            Kind::Gpu => (&mut s.gpu_enabled, s.cpu_enabled),
        };
        if *this && !other {
            return;
        }
        *this = !*this;
        if *this {
            self.app.set_sensor(k, open_sensor(k));
            let mut changes = self.app.tick();
            // The new icon is drawn from the fresh values below.
            changes.value[k.index()] = false;
            changes.tooltip[k.index()] = false;
            self.show_icon(k);
            self.apply(changes);
        } else {
            self.tray.remove(k);
            self.app.set_sensor(k, None);
        }
    }

    /// Applies a menu command. Returns false to exit.
    fn command(&mut self, id: u32) -> bool {
        match id {
            menu::ID_EXIT => return false,
            menu::ID_MODE_ICONS => self.app.settings.display_mode = DisplayMode::TrayIcons,
            menu::ID_CPU => self.toggle(Kind::Cpu),
            menu::ID_GPU => self.toggle(Kind::Gpu),
            _ => match menu::interval_for(id) {
                Some(s) => {
                    self.app.settings.interval_s = s;
                    if self.display_on {
                        self.start_timer();
                    }
                }
                None => return true,
            },
        }
        // There is no console to report to; the menu keeps working without a saved file.
        let _ = self.app.settings.save();
        true
    }

    fn shutdown(&mut self) {
        // SAFETY: stops our own timer.
        unsafe { KillTimer(self.hwnd, TIMER_ID) };
        power::unregister_display_notifications(self.power);
        for k in KINDS {
            self.tray.remove(k);
        }
    }
}

fn on_tray(hwnd: HWND, wp: WPARAM, lp: LPARAM) {
    // Version 4: LOWORD(lParam) is the event, wParam holds the anchor point.
    if (lp & 0xFFFF) as u32 != WM_CONTEXTMENU {
        return;
    }
    // Copy what the menu needs and release the state: the menu's modal loop dispatches
    // messages (timer ticks keep running while it is open).
    let Some(settings) = with_state(|st| st.app.settings.clone()) else {
        // SAFETY: plain post to our own window.
        unsafe { PostMessageW(hwnd, WM_TRAY, wp, lp) };
        return;
    };
    let x = (wp & 0xFFFF) as u16 as i16 as i32;
    let y = ((wp >> 16) & 0xFFFF) as u16 as i16 as i32;
    // SAFETY: plain calls on our own window. Foreground first and WM_NULL after, so the
    // menu closes when the user clicks elsewhere.
    let cmd = unsafe {
        SetForegroundWindow(hwnd);
        let cmd = menu::show(hwnd, &settings, false, x, y);
        PostMessageW(hwnd, WM_NULL, 0, 0);
        cmd
    };
    if let Some(id) = cmd {
        if with_state(|st| st.command(id)) == Some(false) {
            // SAFETY: the state is back in place, so WM_DESTROY can clean it up.
            unsafe { DestroyWindow(hwnd) };
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        // Posted, never sent, so it can't re-enter a handler; nothing to do if it ever did.
        WM_TIMER => {
            with_state(State::tick);
        }
        WM_TRAY => on_tray(hwnd, wp, lp),
        WM_POWERBROADCAST => {
            if wp as u32 == PBT_POWERSETTINGCHANGE {
                // SAFETY: lParam of PBT_POWERSETTINGCHANGE is a POWERBROADCAST_SETTING.
                if let Some(on) = unsafe { power::display_on(lp) } {
                    // SAFETY: plain post to our own window.
                    unsafe { PostMessageW(hwnd, WM_DISPLAY_STATE, on as usize, 0) };
                }
            }
            return 1;
        }
        WM_DISPLAY_STATE => run_or_repost(hwnd, msg, wp, lp, |st| st.set_display(wp != 0)),
        WM_DPICHANGED | WM_DISPLAYCHANGE | WM_SETTINGCHANGE => {
            // SAFETY: plain post to our own window.
            unsafe { PostMessageW(hwnd, WM_RECHECK, 0, 0) };
        }
        WM_RECHECK => run_or_repost(hwnd, msg, wp, lp, State::recheck),
        WM_DESTROY => {
            if let Some(mut st) = STATE.take() {
                st.shutdown();
            }
            // SAFETY: ends our message loop.
            unsafe { PostQuitMessage(0) };
        }
        m if m != 0 && m == TASKBAR_CREATED.load(Ordering::Relaxed) => {
            run_or_repost(hwnd, msg, wp, lp, |st| {
                st.tray.readd_all();
                st.recheck();
            });
        }
        // SAFETY: default handling for everything else.
        _ => return unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
    0
}

/// Opens the sensor of `k`; it is dropped again when the kind is disabled.
fn open_sensor(k: Kind) -> Option<Box<dyn Sensor>> {
    match k {
        Kind::Gpu => crate::sensor::d3dkmt::open_gpu_sensor(),
        Kind::Cpu => PawnIoCpu::open().ok().map(|c| Box::new(c) as Box<dyn Sensor>),
    }
}

pub fn run() {
    let settings = Settings::load();
    let cpu = settings.cpu_enabled.then(|| open_sensor(Kind::Cpu)).flatten();
    let gpu = settings.gpu_enabled.then(|| open_sensor(Kind::Gpu)).flatten();
    let mut app = App::new(settings, cpu, gpu);

    // SAFETY: standard window class and window creation; the class name and title are
    // static NUL-terminated strings.
    let hwnd = unsafe {
        let hinstance = GetModuleHandleW(null());
        let class = w!("TrayTempWindow");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);
        // A hidden top-level window (not message-only) so the TaskbarCreated broadcast reaches it.
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class,
            w!("TrayTemp"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            hinstance,
            null(),
        )
    };
    if hwnd.is_null() {
        return;
    }
    // SAFETY: registers a named message and lets it through UIPI when running elevated.
    unsafe {
        let taskbar_created = RegisterWindowMessageW(w!("TaskbarCreated"));
        TASKBAR_CREATED.store(taskbar_created, Ordering::Relaxed);
        ChangeWindowMessageFilterEx(hwnd, taskbar_created, MSGFLT_ALLOW, null_mut());
    }

    app.tick();
    let mut st = State {
        hwnd,
        app,
        tray: TrayIcons::new(hwnd),
        icon_size: icon_size(),
        face: None,
        font: None,
        style: font::taskbar_style(),
        display_on: true,
        power: 0,
    };
    st.reload_font();
    for k in KINDS {
        if st.app.enabled(k) {
            st.show_icon(k);
        }
    }
    st.start_timer();
    // The system answers the registration with the current state, which is "on" here.
    st.power = power::register_display_notifications(hwnd);
    STATE.set(Some(st));

    let mut msg = MSG::default();
    // SAFETY: standard message loop on this thread.
    unsafe {
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            DispatchMessageW(&msg);
        }
    }
}
