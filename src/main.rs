#![cfg_attr(windows, windows_subsystem = "windows")]

#[allow(dead_code)]
mod app;
mod platform;
#[allow(dead_code)]
mod render;
#[allow(dead_code)]
mod sensor;
#[allow(dead_code)]
mod settings;
#[allow(dead_code)]
mod stats;
#[allow(dead_code)]
mod tooltip;

fn main() {
    #[cfg(windows)]
    restrict_dll_search();
    platform::run()
}

/// Limit every later `LoadLibrary` to System32, dropping the exe folder and the current directory.
#[cfg(windows)]
fn restrict_dll_search() {
    use windows_sys::Win32::System::LibraryLoader::{SetDefaultDllDirectories, LOAD_LIBRARY_SEARCH_SYSTEM32};
    // SAFETY: plain Win32 call with a valid flag; a failure just keeps the default search order.
    unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32) };
}
