//! Whether this process runs elevated, and starting programs through the UAC prompt.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr::{null, null_mut};

use windows_sys::w;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_CANCELLED, HANDLE};
use windows_sys::Win32::Security::{
    GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
};
use windows_sys::Win32::System::Com::{
    CoInitializeEx, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, WaitForSingleObject, INFINITE,
};
use windows_sys::Win32::UI::Shell::{
    ShellExecuteExW, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS,
    SHELLEXECUTEINFOW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

pub enum Relaunch {
    Started,
    /// The user declined the UAC prompt.
    Cancelled,
    Failed,
}

/// `s` as NUL-terminated UTF-16.
pub fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(Some(0)).collect()
}

pub fn is_elevated() -> bool {
    let mut token: HANDLE = null_mut();
    // SAFETY: the current-process pseudo handle needs no closing; `token` is a valid out-pointer.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return false;
    }
    let mut elevation = TOKEN_ELEVATION::default();
    let mut len = 0;
    // SAFETY: the buffer is a TOKEN_ELEVATION of the size passed; the token is closed once.
    let ok = unsafe {
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut TOKEN_ELEVATION as *mut _,
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        );
        CloseHandle(token);
        ok
    };
    ok != 0 && elevation.TokenIsElevated != 0
}

/// Starts `exe args` elevated through the UAC prompt, with the program's own folder as its
/// working directory. Returns the process handle if `mask` asks for it, else null.
/// Blocks while the prompt is open.
fn runas(exe: &Path, args: &str, mask: u32) -> Result<HANDLE, Relaunch> {
    let file = wide(exe);
    let params = wide(args);
    let dir = exe.parent().map(wide);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        // NOASYNC: the caller may exit right after a relaunch.
        fMask: mask | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
        lpVerb: w!("runas"),
        lpFile: file.as_ptr(),
        lpParameters: params.as_ptr(),
        lpDirectory: dir.as_ref().map_or(null(), |d| d.as_ptr()),
        nShow: SW_SHOWNORMAL,
        ..Default::default()
    };
    // SAFETY: ShellExecuteEx asks for COM on the calling thread. A repeated call only adds a
    // reference, and this thread keeps COM until the process ends.
    unsafe {
        CoInitializeEx(
            null(),
            (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
        )
    };
    // SAFETY: `info` is initialised with its size; the strings outlive the call.
    if unsafe { ShellExecuteExW(&mut info) } != 0 {
        return Ok(info.hProcess);
    }
    // SAFETY: reads this thread's last error, set by the failed call above.
    if unsafe { GetLastError() } == ERROR_CANCELLED {
        Err(Relaunch::Cancelled)
    } else {
        Err(Relaunch::Failed)
    }
}

/// Starts this exe again, elevated, with `args`. On `Started` the caller should exit.
pub fn relaunch_elevated(args: &str) -> Relaunch {
    let Ok(exe) = std::env::current_exe() else {
        return Relaunch::Failed;
    };
    match runas(&exe, args, 0) {
        Ok(_) => Relaunch::Started,
        Err(r) => r,
    }
}

/// Runs `exe args` elevated through the UAC prompt and waits for it to exit.
/// Returns its exit code.
pub fn run_elevated_and_wait(exe: &Path, args: &str) -> Result<u32, Relaunch> {
    let process = runas(exe, args, SEE_MASK_NOCLOSEPROCESS)?;
    if process.is_null() {
        return Err(Relaunch::Failed);
    }
    let mut code = 0;
    // SAFETY: `process` came from ShellExecuteExW with NOCLOSEPROCESS and is closed once.
    let ok = unsafe {
        WaitForSingleObject(process, INFINITE);
        let ok = GetExitCodeProcess(process, &mut code);
        CloseHandle(process);
        ok
    };
    if ok == 0 {
        return Err(Relaunch::Failed);
    }
    Ok(code)
}
