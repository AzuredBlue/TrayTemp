use std::ptr::null;

use windows_sys::core::PCWSTR;
use windows_sys::w;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CheckMenuRadioItem, CreatePopupMenu, DestroyMenu, TrackPopupMenu, HMENU,
    MENU_ITEM_FLAGS, MF_BYCOMMAND, MF_CHECKED, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING,
    TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON,
};

use crate::settings::{DisplayMode, Settings};

pub const ID_MODE_ICONS: u32 = 100;
pub const ID_MODE_TEXT: u32 = 101;
pub const ID_CPU: u32 = 110;
pub const ID_GPU: u32 = 111;
/// The refresh items are `ID_INTERVAL_BASE + index into INTERVALS`.
pub const ID_INTERVAL_BASE: u32 = 120;
pub const ID_AUTOSTART: u32 = 130;
pub const ID_ALL_TASKBARS: u32 = 131;
pub const ID_RETRY_DRIVER: u32 = 140;
pub const ID_EXIT: u32 = 199;

const INTERVALS: [u32; 4] = [1, 2, 5, 10];
const INTERVAL_LABELS: [PCWSTR; 4] = [w!("1 s"), w!("2 s"), w!("5 s"), w!("10 s")];

/// The refresh interval in seconds that a menu command selects, if it is a refresh item.
pub fn interval_for(id: u32) -> Option<u32> {
    INTERVALS
        .get(id.checked_sub(ID_INTERVAL_BASE)? as usize)
        .copied()
}

fn flag(on: bool, f: MENU_ITEM_FLAGS) -> MENU_ITEM_FLAGS {
    if on {
        f
    } else {
        0
    }
}

/// SAFETY: `menu` must be a valid menu handle and `label` a NUL-terminated string.
unsafe fn item(menu: HMENU, flags: MENU_ITEM_FLAGS, id: u32, label: PCWSTR) {
    AppendMenuW(menu, MF_STRING | flags, id as usize, label);
}

/// Shows the context menu at screen point (x, y) and returns the chosen command.
pub fn show(hwnd: HWND, s: &Settings, cpu_driver_failed: bool, x: i32, y: i32) -> Option<u32> {
    // SAFETY: every menu is created here and destroyed (with its submenus) before returning;
    // all labels are static NUL-terminated strings.
    unsafe {
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return None;
        }

        // Greyed items are wired up when their features land.
        let display = CreatePopupMenu();
        item(display, 0, ID_MODE_ICONS, w!("Tray icons"));
        item(display, MF_GRAYED, ID_MODE_TEXT, w!("Taskbar text"));
        let mode = match s.display_mode {
            DisplayMode::TrayIcons => ID_MODE_ICONS,
            DisplayMode::TaskbarText => ID_MODE_TEXT,
        };
        CheckMenuRadioItem(display, ID_MODE_ICONS, ID_MODE_TEXT, mode, MF_BYCOMMAND);
        AppendMenuW(menu, MF_POPUP, display as usize, w!("Display"));
        let all = flag(s.all_taskbars, MF_CHECKED);
        item(
            menu,
            MF_GRAYED | all,
            ID_ALL_TASKBARS,
            w!("Show on all taskbars"),
        );
        AppendMenuW(menu, MF_SEPARATOR, 0, null());

        // The last enabled sensor can't be turned off.
        let only_one = s.cpu_enabled != s.gpu_enabled;
        let cpu = flag(s.cpu_enabled, MF_CHECKED) | flag(only_one && s.cpu_enabled, MF_GRAYED);
        item(menu, cpu, ID_CPU, w!("Show CPU temperature"));
        let gpu = flag(s.gpu_enabled, MF_CHECKED) | flag(only_one && s.gpu_enabled, MF_GRAYED);
        item(menu, gpu, ID_GPU, w!("Show GPU temperature"));
        AppendMenuW(menu, MF_SEPARATOR, 0, null());

        let refresh = CreatePopupMenu();
        for (i, &label) in INTERVAL_LABELS.iter().enumerate() {
            item(refresh, 0, ID_INTERVAL_BASE + i as u32, label);
        }
        if let Some(i) = INTERVALS.iter().position(|&v| v == s.interval_s) {
            let last = ID_INTERVAL_BASE + INTERVALS.len() as u32 - 1;
            CheckMenuRadioItem(
                refresh,
                ID_INTERVAL_BASE,
                last,
                ID_INTERVAL_BASE + i as u32,
                MF_BYCOMMAND,
            );
        }
        AppendMenuW(menu, MF_POPUP, refresh as usize, w!("Refresh"));
        let autostart = flag(s.autostart, MF_CHECKED);
        item(
            menu,
            MF_GRAYED | autostart,
            ID_AUTOSTART,
            w!("Start with Windows"),
        );
        if cpu_driver_failed {
            item(menu, 0, ID_RETRY_DRIVER, w!("Retry CPU driver install"));
        }
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
        item(menu, 0, ID_EXIT, w!("Exit"));

        let flags = TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY;
        let cmd = TrackPopupMenu(menu, flags, x, y, 0, hwnd, null());
        DestroyMenu(menu);
        u32::try_from(cmd).ok().filter(|&c| c != 0)
    }
}
