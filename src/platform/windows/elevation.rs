//! Whether this process runs elevated, and starting programs through the UAC prompt.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr::{null, null_mut};

use windows_sys::w;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_CANCELLED, HANDLE};
use windows_sys::Win32::Security::{
    CreateRestrictedToken, CreateWellKnownSid, GetTokenInformation, ImpersonateLoggedOnUser,
    RevertToSelf, SetTokenInformation, TokenElevation, TokenOwner, TokenUser,
    WinBuiltinAdministratorsSid, DISABLE_MAX_PRIVILEGE, SECURITY_MAX_SID_SIZE,
    SID_AND_ATTRIBUTES, TOKEN_ADJUST_DEFAULT, TOKEN_DUPLICATE, TOKEN_ELEVATION, TOKEN_OWNER,
    TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::System::Com::{
    CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
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

/// Runs `f` on this thread without administrator rights: our own token with Administrators
/// deny-only, no privileges, and the user as the owner of new files. For writes under the
/// user's own folders from an elevated instance: a link planted there can't redirect them onto
/// a file only an administrator may change. Unelevated, the token loses nothing it had.
/// `None`, without running `f`, if that token can't be made.
pub fn as_user<R>(f: impl FnOnce() -> R) -> Option<R> {
    let mut token: HANDLE = null_mut();
    // SAFETY: the current-process pseudo handle needs no closing; `token` is a valid out-pointer.
    let access = TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ADJUST_DEFAULT;
    if unsafe { OpenProcessToken(GetCurrentProcess(), access, &mut token) } == 0 {
        return None;
    }
    let mut admins = [0u8; SECURITY_MAX_SID_SIZE as usize];
    let mut admins_len = SECURITY_MAX_SID_SIZE;
    // TOKEN_USER and the SID it points into; usize keeps the struct aligned.
    let mut user = [0usize; 16];
    let mut user_len = 0;
    let mut restricted: HANDLE = null_mut();
    // SAFETY: each buffer's size is passed with it, and `user` holds the TOKEN_USER (and its
    // SID) that GetTokenInformation filled when it is read. Both tokens are closed once; the
    // thread keeps its own reference to the impersonation token.
    let ok = unsafe {
        let ok = CreateWellKnownSid(
            WinBuiltinAdministratorsSid,
            null_mut(),
            admins.as_mut_ptr().cast(),
            &mut admins_len,
        ) != 0
            && GetTokenInformation(
                token,
                TokenUser,
                user.as_mut_ptr().cast(),
                size_of_val(&user) as u32,
                &mut user_len,
            ) != 0
            && {
                let disable = SID_AND_ATTRIBUTES { Sid: admins.as_mut_ptr().cast(), Attributes: 0 };
                let flags = DISABLE_MAX_PRIVILEGE;
                CreateRestrictedToken(token, flags, 1, &disable, 0, null(), 0, null(), &mut restricted)
                    != 0
            }
            && {
                // An elevated token's default owner is Administrators, now deny-only.
                let owner = TOKEN_OWNER { Owner: (*user.as_ptr().cast::<TOKEN_USER>()).User.Sid };
                SetTokenInformation(
                    restricted,
                    TokenOwner,
                    (&raw const owner).cast(),
                    size_of::<TOKEN_OWNER>() as u32,
                ) != 0
            }
            && ImpersonateLoggedOnUser(restricted) != 0;
        CloseHandle(token);
        if !restricted.is_null() {
            CloseHandle(restricted);
        }
        ok
    };
    if !ok {
        return None;
    }
    let r = f();
    // SAFETY: ends the impersonation started above. If it ever failed, this thread would only
    // keep fewer rights, never more.
    unsafe { RevertToSelf() };
    Some(r)
}

/// Starts `exe args` elevated through the UAC prompt, with the program's own folder as its
/// working directory. Returns the process handle if `mask` asks for it, else null.
/// Blocks while the prompt is open.
fn runas(exe: &Path, args: &str, mask: u32, show: i32) -> Result<HANDLE, Relaunch> {
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
        nShow: show,
        ..Default::default()
    };
    // SAFETY: ShellExecuteEx asks for COM on the calling thread. It is initialised for this
    // call only (NOASYNC: the call is done when it returns); a thread that already has COM
    // just counts one more reference. `info` is initialised with its size and the strings
    // outlive the call.
    let (ok, err) = unsafe {
        let com = CoInitializeEx(
            null(),
            (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
        ) >= 0;
        let ok = ShellExecuteExW(&mut info) != 0;
        let err = GetLastError();
        if com {
            CoUninitialize();
        }
        (ok, err)
    };
    if ok {
        return Ok(info.hProcess);
    }
    if err == ERROR_CANCELLED {
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
    match runas(&exe, args, 0, SW_SHOWNORMAL) {
        Ok(_) => Relaunch::Started,
        Err(r) => r,
    }
}

/// Runs `exe args` elevated through the UAC prompt and waits for it to exit.
/// Returns its exit code. `show` is the SW_ value for its window.
pub fn run_elevated_and_wait(exe: &Path, args: &str, show: i32) -> Result<u32, Relaunch> {
    let process = runas(exe, args, SEE_MASK_NOCLOSEPROCESS, show)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// When run elevated, it also checks that a write as the user is refused in a folder only
    /// administrators may write to.
    #[test]
    fn as_user_writes_with_the_users_rights_only() {
        let dir = std::env::temp_dir().join(format!("traytemp-as-user-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mine = dir.join("mine.txt");
        assert!(as_user(|| std::fs::write(&mine, "x")).unwrap().is_ok());
        if is_elevated() {
            let admins = dir.join("admins");
            super::super::pawnio_setup::create_private_dir(&admins).unwrap();
            let denied = as_user(|| std::fs::write(admins.join("a.txt"), "x")).unwrap();
            assert_eq!(denied.unwrap_err().kind(), std::io::ErrorKind::PermissionDenied);
            std::fs::write(admins.join("b.txt"), "x").unwrap();
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
