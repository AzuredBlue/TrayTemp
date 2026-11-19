use windows_sys::Win32::Foundation::{HWND, LPARAM};
use windows_sys::Win32::System::Power::{
    RegisterPowerSettingNotification, UnregisterPowerSettingNotification, HPOWERNOTIFY,
    POWERBROADCAST_SETTING,
};
use windows_sys::Win32::System::SystemServices::GUID_CONSOLE_DISPLAY_STATE;
use windows_sys::Win32::UI::WindowsAndMessaging::DEVICE_NOTIFY_WINDOW_HANDLE;

/// Sends WM_POWERBROADCAST to `hwnd` when the console display turns off, on or dims.
/// Returns 0 on failure.
pub fn register_display_notifications(hwnd: HWND) -> HPOWERNOTIFY {
    // SAFETY: plain registration call with a valid window and a static GUID.
    unsafe {
        RegisterPowerSettingNotification(
            hwnd,
            &GUID_CONSOLE_DISPLAY_STATE,
            DEVICE_NOTIFY_WINDOW_HANDLE,
        )
    }
}

pub fn unregister_display_notifications(h: HPOWERNOTIFY) {
    if h != 0 {
        // SAFETY: `h` came from RegisterPowerSettingNotification and is unregistered once.
        unsafe { UnregisterPowerSettingNotification(h) };
    }
}

/// Display state from a PBT_POWERSETTINGCHANGE notification: `Some(false)` when off,
/// `Some(true)` when on or dimmed, `None` for any other power setting.
///
/// # Safety
/// `lparam` must be the `lParam` of a WM_POWERBROADCAST message whose `wParam` is
/// PBT_POWERSETTINGCHANGE, i.e. null or a valid POWERBROADCAST_SETTING pointer.
pub unsafe fn display_on(lparam: LPARAM) -> Option<bool> {
    let s = unsafe { (lparam as *const POWERBROADCAST_SETTING).as_ref() }?;
    let (g, d) = (&s.PowerSetting, &GUID_CONSOLE_DISPLAY_STATE);
    let same = g.data1 == d.data1 && g.data2 == d.data2 && g.data3 == d.data3 && g.data4 == d.data4;
    // Data is a DWORD: 0 = off, 1 = on, 2 = dimmed.
    (same && s.DataLength >= 1).then(|| s.Data[0] != 0)
}
