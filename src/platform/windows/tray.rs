use std::ptr::null_mut;

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NIM_SETVERSION, NOTIFYICONDATAW, NOTIFYICON_VERSION_4, NOTIFY_ICON_MESSAGE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyIcon, HICON, WM_APP};

use super::icon::canvas_to_hicon;
use crate::render::Canvas;
use crate::sensor::Kind;

/// Callback message for tray icon events.
pub const WM_TRAY: u32 = WM_APP + 1;

const TIP_LEN: usize = 128;

/// One notification-area icon per kind. Keeps the current icon and tip so the icons can be
/// added again after Explorer restarts.
pub struct TrayIcons {
    hwnd: HWND,
    icons: [HICON; 2],
    tips: [[u16; TIP_LEN]; 2],
    shown: [bool; 2],
}

fn uid(k: Kind) -> u32 {
    k.index() as u32 + 1
}

/// Copies `tip` as NUL-terminated UTF-16, cut at a character boundary if it is too long.
fn copy_tip(dst: &mut [u16; TIP_LEN], tip: &str) {
    let mut n = 0;
    for ch in tip.chars() {
        let mut buf = [0u16; 2];
        let units = ch.encode_utf16(&mut buf);
        if n + units.len() >= TIP_LEN {
            break;
        }
        dst[n..n + units.len()].copy_from_slice(units);
        n += units.len();
    }
    dst[n] = 0;
}

fn destroy(icon: HICON) {
    if !icon.is_null() {
        // SAFETY: the icon was created by canvas_to_hicon and is no longer stored anywhere.
        unsafe { DestroyIcon(icon) };
    }
}

impl TrayIcons {
    pub fn new(hwnd: HWND) -> Self {
        TrayIcons {
            hwnd,
            icons: [null_mut(); 2],
            tips: [[0; TIP_LEN]; 2],
            shown: [false; 2],
        }
    }

    fn notify(&self, msg: NOTIFY_ICON_MESSAGE, k: Kind) -> bool {
        let i = k.index();
        let mut nid = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: uid(k),
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP,
            uCallbackMessage: WM_TRAY,
            hIcon: self.icons[i],
            szTip: self.tips[i],
            ..Default::default()
        };
        if msg == NIM_SETVERSION {
            nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        }
        // SAFETY: `nid` is a fully initialised NOTIFYICONDATAW with the correct cbSize.
        unsafe { Shell_NotifyIconW(msg, &nid) != 0 }
    }

    /// NIM_ADD, then opt in to version 4 callbacks (event in LOWORD(lParam), anchor in wParam).
    fn add_to_shell(&self, k: Kind) {
        if self.notify(NIM_ADD, k) {
            self.notify(NIM_SETVERSION, k);
        }
    }

    pub fn add(&mut self, k: Kind, icon: &Canvas, tip: &str) {
        let i = k.index();
        destroy(std::mem::replace(&mut self.icons[i], canvas_to_hicon(icon)));
        copy_tip(&mut self.tips[i], tip);
        self.shown[i] = true;
        self.add_to_shell(k);
    }

    pub fn update(&mut self, k: Kind, icon: Option<&Canvas>, tip: Option<&str>) {
        let i = k.index();
        if !self.shown[i] {
            return;
        }
        if let Some(t) = tip {
            copy_tip(&mut self.tips[i], t);
        }
        let mut old = null_mut();
        if let Some(c) = icon {
            let new = canvas_to_hicon(c);
            if !new.is_null() {
                old = std::mem::replace(&mut self.icons[i], new);
            }
        }
        // The shell keeps its own copy, so the old icon can go once the call returns.
        if !self.notify(NIM_MODIFY, k) {
            // Not in the tray yet, e.g. the shell was not ready at startup.
            self.add_to_shell(k);
        }
        destroy(old);
    }

    pub fn remove(&mut self, k: Kind) {
        let i = k.index();
        if self.shown[i] {
            self.notify(NIM_DELETE, k);
            self.shown[i] = false;
        }
        destroy(std::mem::replace(&mut self.icons[i], null_mut()));
    }

    /// Adds every shown icon again after the taskbar was recreated.
    pub fn readd_all(&mut self) {
        for k in [Kind::Cpu, Kind::Gpu] {
            if self.shown[k.index()] {
                self.add_to_shell(k);
            }
        }
    }
}
