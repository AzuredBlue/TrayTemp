use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_OK};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn run() {
    let title = wide("TrayTemp");
    let text = wide("scaffold");
    // SAFETY: both buffers are NUL-terminated and outlive the call.
    unsafe {
        MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), MB_OK);
    }
}
