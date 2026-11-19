mod autostart;
mod elevation;
mod font;
mod icon;
mod menu;
mod pawnio_setup;
mod power;
mod tray;

use std::cell::Cell;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU32, Ordering};

use windows_sys::core::PCWSTR;
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
    GetMessageW, KillTimer, MessageBoxW, PostMessageW, PostQuitMessage, RegisterClassW,
    RegisterWindowMessageW, SetForegroundWindow, SetTimer, IDOK, IDYES, MB_ICONERROR,
    MB_ICONINFORMATION, MB_ICONQUESTION, MB_OKCANCEL, MB_SETFOREGROUND, MB_YESNO,
    MESSAGEBOX_RESULT, MESSAGEBOX_STYLE, MSG, MSGFLT_ALLOW, PBT_POWERSETTINGCHANGE, SM_CXSMICON,
    WM_APP, WM_CONTEXTMENU, WM_DESTROY, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_NULL,
    WM_POWERBROADCAST, WM_SETTINGCHANGE, WM_TIMER, WNDCLASSW, WS_EX_TOOLWINDOW, WS_POPUP,
};

use crate::app::{App, Changes};
use crate::render::{render_value, OsFont, TextStyle};
use crate::sensor::d3dkmt::open_gpu_sensor;
use crate::sensor::pawnio::{self, PawnIoCpu, PawnIoError};
use crate::sensor::{Kind, Sensor};
use crate::settings::{DisplayMode, Settings};
use elevation::{is_elevated, wide, Relaunch};
use font::GdiText;
use tray::{TrayIcons, WM_TRAY};

const KINDS: [Kind; 2] = [Kind::Cpu, Kind::Gpu];
const TIMER_ID: usize = 1;
/// Posted to ourselves: wParam is 1 when the display came on, 0 when it went off.
const WM_DISPLAY_STATE: u32 = WM_APP + 2;
/// Posted to ourselves: the tray icon size, the UI font or the taskbar theme may have changed.
const WM_RECHECK: u32 = WM_APP + 3;
/// Posted by the autostart worker: lParam is a `Box<String>` (from `Box::into_raw`) with what
/// went wrong; the window procedure takes it over.
const WM_AUTOSTART_ERROR: u32 = WM_APP + 4;

static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

const INSTALL_PROMPT: PCWSTR = w!("TrayTemp needs the PawnIO driver to read CPU temperature. It will be downloaded from github.com/namazso/PawnIO.Setup and installed now, which requires administrator permission.");
const UNINSTALL_PROMPT: PCWSTR =
    w!("Also uninstall the CPU temperature driver (PawnIO)? Other apps may use it.");

struct State {
    hwnd: HWND,
    app: App,
    tray: TrayIcons,
    icon_size: u32,
    /// Face of the UI font in use and whether the text is light, to notice when either changes.
    face: Option<([u16; 32], bool)>,
    font: Option<OsFont>,
    style: TextStyle,
    display_on: bool,
    power: HPOWERNOTIFY,
    /// CPU is enabled but its driver is missing or unusable, or TrayTemp isn't elevated.
    /// The menu then offers to retry.
    cpu_driver_failed: bool,
}

thread_local! {
    // Moved out while a handler runs: a message that re-enters the window procedure
    // (Shell_NotifyIconW can dispatch sent messages) finds it empty instead of aliasing it.
    static STATE: Cell<Option<State>> = const { Cell::new(None) };
    /// Set while a CPU driver step shows a dialog, the UAC prompt or waits for an installer,
    /// so the menu can't open again and start a second one.
    static BUSY: Cell<bool> = const { Cell::new(false) };
}

/// Runs `f` on the state; `None` if a handler further up the stack has it.
/// Never show a dialog or the UAC prompt, or wait for a process, inside `f`: their modal loops
/// dispatch messages, which would find the state taken and repost themselves at 100 % CPU.
/// Return what is left to do and run it after, as `on_tray` does with `FollowUp`.
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

    /// Reads the UI font again; true if its face changed, or the text in `style` turned from
    /// light to dark or back, which changes how it is drawn.
    fn reload_font(&mut self, style: TextStyle) -> bool {
        let lf = font::ui_font();
        let light = font::is_light(style.fill);
        let face = lf.map(|f| (f.lfFaceName, light));
        if face == self.face {
            return false;
        }
        self.face = face;
        self.font = lf.map(|f| OsFont::new(Box::new(GdiText::new(f, light))));
        true
    }

    /// Redraws the icons if the icon size, the UI font or the taskbar theme changed.
    fn recheck(&mut self) {
        let size = icon_size();
        let style = font::taskbar_style();
        let font_changed = self.reload_font(style);
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

    fn toggle(&mut self, k: Kind) -> FollowUp {
        let s = &mut self.app.settings;
        let (this, other) = match k {
            Kind::Cpu => (&mut s.cpu_enabled, s.gpu_enabled),
            Kind::Gpu => (&mut s.gpu_enabled, s.cpu_enabled),
        };
        if *this && !other {
            return FollowUp::None;
        }
        *this = !*this;
        if !*this {
            self.tray.remove(k);
            self.app.set_sensor(k, None);
            if k == Kind::Cpu {
                self.cpu_driver_failed = false;
                return FollowUp::CpuDisabled;
            }
        } else if k == Kind::Cpu {
            // Shows "--" until `start_cpu`, which may need dialogs or a relaunch, has run.
            self.show_icon(k);
            return FollowUp::StartCpu;
        } else {
            self.app.set_sensor(k, open_gpu_sensor());
            let mut changes = self.app.tick();
            // The new icon is drawn from the fresh values below.
            changes.value[k.index()] = false;
            changes.tooltip[k.index()] = false;
            self.show_icon(k);
            self.apply(changes);
        }
        FollowUp::None
    }

    /// Takes the outcome of `start_cpu` run for a menu command; the CPU icon is shown.
    fn cpu_started(&mut self, start: CpuStart) {
        if start.installed {
            self.app.settings.pawnio_installed_by_traytemp = true;
            let _ = self.app.settings.save();
        }
        self.cpu_driver_failed = start.failed;
        self.app.set_sensor(Kind::Cpu, start.sensor);
        self.tick();
    }

    /// Applies a menu command; returns what is left to do once the state is released.
    fn command(&mut self, id: u32) -> FollowUp {
        let follow = match id {
            menu::ID_EXIT => return FollowUp::Exit,
            menu::ID_MODE_ICONS => {
                self.app.settings.display_mode = DisplayMode::TrayIcons;
                FollowUp::None
            }
            menu::ID_CPU => self.toggle(Kind::Cpu),
            menu::ID_GPU => self.toggle(Kind::Gpu),
            menu::ID_AUTOSTART => {
                self.app.settings.autostart = !self.app.settings.autostart;
                FollowUp::Autostart
            }
            menu::ID_RETRY_DRIVER if self.cpu_driver_failed => FollowUp::StartCpu,
            _ => match menu::interval_for(id) {
                Some(s) => {
                    self.app.settings.interval_s = s;
                    if self.display_on {
                        self.start_timer();
                    }
                    FollowUp::None
                }
                None => return FollowUp::None,
            },
        };
        // There is no console to report to; the menu keeps working without a saved file.
        // Saved before any relaunch, so the elevated instance starts from these settings.
        let _ = self.app.settings.save();
        follow
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
    if (lp & 0xFFFF) as u32 != WM_CONTEXTMENU || BUSY.get() {
        return;
    }
    // Copy what the menu needs and release the state: the menu's modal loop dispatches
    // messages (timer ticks keep running while it is open).
    let Some((settings, cpu_driver_failed)) =
        with_state(|st| (st.app.settings.clone(), st.cpu_driver_failed))
    else {
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
        let cmd = menu::show(hwnd, &settings, cpu_driver_failed, x, y);
        PostMessageW(hwnd, WM_NULL, 0, 0);
        cmd
    };
    if let Some(follow) = cmd.and_then(|id| with_state(|st| st.command(id))) {
        follow_up(hwnd, follow);
    }
}

/// What a menu command leaves to do once the state is released.
enum FollowUp {
    None,
    Exit,
    /// CPU was enabled, or the driver install is retried.
    StartCpu,
    CpuDisabled,
    /// "Start with Windows" was toggled.
    Autostart,
}

/// Runs what a menu command left to do, with the state released: the dialogs, the UAC
/// prompt and the waits for PawnIO's installer and uninstaller all happen here.
fn follow_up(hwnd: HWND, follow: FollowUp) {
    BUSY.set(true);
    let exit = match follow {
        FollowUp::None => false,
        FollowUp::Exit => true,
        FollowUp::StartCpu => match start_cpu(false) {
            None => true,
            Some(start) => {
                if let Some(s) = with_state(|st| {
                    st.cpu_started(start);
                    st.app.settings.clone()
                }) {
                    elevation_need_changed(hwnd, &s);
                }
                false
            }
        },
        FollowUp::CpuDisabled => {
            offer_uninstall();
            if let Some(s) = with_state(|st| st.app.settings.clone()) {
                elevation_need_changed(hwnd, &s);
            }
            false
        }
        FollowUp::Autostart => {
            // The checkmark is the user's choice and stays when applying it fails; the error
            // is shown and the next start tries again.
            if let Some((on, cpu)) =
                with_state(|st| (st.app.settings.autostart, st.app.settings.cpu_enabled))
            {
                autostart::request(hwnd, on, cpu);
            }
            false
        }
    };
    BUSY.set(false);
    if exit {
        // SAFETY: the state is back in place, so WM_DESTROY can clean it up.
        unsafe { DestroyWindow(hwnd) };
    }
}

/// After CPU was disabled: if TrayTemp installed PawnIO, offers to uninstall it.
fn offer_uninstall() {
    let ours = with_state(|st| st.app.settings.pawnio_installed_by_traytemp) == Some(true);
    if !ours || message_box(UNINSTALL_PROMPT, MB_YESNO | MB_ICONQUESTION) != IDYES {
        return;
    }
    match pawnio_setup::uninstall() {
        Ok(()) => {
            with_state(|st| {
                st.app.settings.pawnio_installed_by_traytemp = false;
                let _ = st.app.settings.save();
            });
        }
        Err(e) => error_box("The CPU temperature driver could not be uninstalled.", &e),
    }
}

/// Hook for when enabling or disabling CPU changed whether TrayTemp needs to run elevated:
/// autostart switches between its elevated and its plain form here. Called with the state
/// released; the work itself runs on the autostart worker.
fn elevation_need_changed(hwnd: HWND, settings: &Settings) {
    if settings.autostart {
        autostart::request(hwnd, true, settings.cpu_enabled);
    }
}

/// A TrayTemp message box, brought to the foreground. Runs a modal loop: never call it inside
/// `with_state`.
fn message_box(text: PCWSTR, style: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
    // SAFETY: both strings are NUL-terminated and outlive the call; no owner window, so the
    // box gets its own taskbar button.
    unsafe { MessageBoxW(null_mut(), text, w!("TrayTemp"), style | MB_SETFOREGROUND) }
}

fn error_box(what: &str, why: &str) {
    let text = wide(format!("{what}\n\n{why}"));
    message_box(text.as_ptr(), MB_ICONERROR);
}

/// What starting CPU temperature came to.
struct CpuStart {
    sensor: Option<Box<dyn Sensor>>,
    /// The driver is missing or unusable, or TrayTemp isn't elevated: CPU shows "--" and
    /// the menu offers to retry.
    failed: bool,
    /// TrayTemp installed PawnIO just now.
    installed: bool,
}

impl CpuStart {
    fn failed() -> Option<CpuStart> {
        Some(CpuStart {
            sensor: None,
            failed: true,
            installed: false,
        })
    }
}

fn has_arg(name: &str) -> bool {
    std::env::args_os().skip(1).any(|a| a == name)
}

/// The one place the CPU sensor is opened (startup, enabling CPU, retrying the driver).
/// Relaunches elevated or installs the PawnIO driver when that is what's missing.
/// `None`: an elevated instance was started and this one should exit.
/// `install_agreed`: the user already agreed to the install, in the instance that asked.
/// Shows dialogs and the UAC prompt: never call it inside `with_state`.
fn start_cpu(install_agreed: bool) -> Option<CpuStart> {
    // Passed to the elevated instance: it never relaunches again, even if elevation
    // didn't stick.
    let relaunched = has_arg("--elevated");
    let failed = match PawnIoCpu::open() {
        Ok(cpu) => {
            return Some(CpuStart {
                sensor: Some(Box::new(cpu)),
                failed: false,
                installed: false,
            })
        }
        // No PawnIO module for this CPU: the driver can't help.
        Err(PawnIoError::UnsupportedCpu) => false,
        Err(PawnIoError::AccessDenied) if !relaunched && !is_elevated() => {
            return relaunch("--elevated")
        }
        // Only a missing PawnIOLib.dll means "not installed"; one that is there but
        // doesn't load or lacks an export is broken, and installing won't be offered.
        Err(PawnIoError::NotInstalled) if !pawnio::lib_present() => {
            return install_driver(install_agreed, relaunched)
        }
        // Also Load(E_ACCESSDENIED) from creating the PCI mutex, which elevating won't fix.
        Err(_) => true,
    };
    Some(CpuStart {
        sensor: None,
        failed,
        installed: false,
    })
}

fn relaunch(args: &str) -> Option<CpuStart> {
    match elevation::relaunch_elevated(args) {
        Relaunch::Started => None,
        Relaunch::Cancelled | Relaunch::Failed => CpuStart::failed(),
    }
}

/// Asks first unless `agreed`, then installs PawnIO here when elevated, else in an elevated
/// relaunch.
fn install_driver(agreed: bool, relaunched: bool) -> Option<CpuStart> {
    if !agreed && message_box(INSTALL_PROMPT, MB_OKCANCEL | MB_ICONINFORMATION) != IDOK {
        return CpuStart::failed();
    }
    if !is_elevated() {
        if relaunched {
            return CpuStart::failed();
        }
        return relaunch("--elevated --install-driver");
    }
    if let Err(e) = pawnio_setup::install() {
        error_box("The CPU temperature driver could not be installed.", &e);
        return CpuStart::failed();
    }
    let sensor = PawnIoCpu::open()
        .ok()
        .map(|c| Box::new(c) as Box<dyn Sensor>);
    Some(CpuStart {
        failed: sensor.is_none(),
        sensor,
        installed: true,
    })
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
        WM_AUTOSTART_ERROR => {
            // SAFETY: posted by `autostart::request`'s worker with a `Box<String>` we now own.
            let why = unsafe { Box::from_raw(lp as *mut String) };
            error_box("Start with Windows could not be changed.", &why);
        }
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

pub fn run() {
    let mut settings = Settings::load();
    // Sensors are opened only while their kind is enabled, and dropped when it is disabled.
    let mut cpu = None;
    let mut cpu_driver_failed = false;
    if settings.cpu_enabled {
        // No window or state exists yet, so dialogs are safe here.
        let Some(start) = start_cpu(has_arg("--install-driver")) else {
            // The elevated instance takes over.
            return;
        };
        if start.installed {
            settings.pawnio_installed_by_traytemp = true;
            let _ = settings.save();
        }
        cpu = start.sensor;
        cpu_driver_failed = start.failed;
    }
    let gpu = settings.gpu_enabled.then(open_gpu_sensor).flatten();
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
        cpu_driver_failed,
    };
    st.reload_font(st.style);
    for k in KINDS {
        if st.app.enabled(k) {
            st.show_icon(k);
        }
    }
    st.start_timer();
    // The system answers the registration with the current state, which is "on" here.
    st.power = power::register_display_notifications(hwnd);
    autostart::sync_at_startup(hwnd, st.app.settings.autostart, st.app.settings.cpu_enabled);
    STATE.set(Some(st));

    let mut msg = MSG::default();
    // SAFETY: standard message loop on this thread.
    unsafe {
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            DispatchMessageW(&msg);
        }
    }
}
