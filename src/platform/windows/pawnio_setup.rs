//! Installing and removing the PawnIO driver. Both run elevated, so every program started
//! here must first carry a valid Authenticode signature from PawnIO's author.

use std::ffi::{c_void, OsString};
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::time::{SystemTime, UNIX_EPOCH};

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, HANDLE, MAX_PATH, TRUST_E_NOSIGNATURE,
};
use windows_sys::Win32::Networking::WinHttp::{
    WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders,
    WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest, INTERNET_DEFAULT_HTTPS_PORT,
    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE, WINHTTP_QUERY_FLAG_NUMBER,
    WINHTTP_QUERY_STATUS_CODE,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows_sys::Win32::Security::Cryptography::{
    szOID_COMMON_NAME, CertGetNameStringW, CERT_NAME_ATTR_TYPE,
};
use windows_sys::Win32::Security::WinTrust::{
    WTHelperGetProvSignerFromChain, WTHelperProvDataFromStateData, WinVerifyTrust,
    WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_DATA_0, WINTRUST_FILE_INFO,
    WTD_CHOICE_FILE, WTD_REVOKE_NONE, WTD_STATEACTION_CLOSE, WTD_STATEACTION_VERIFY, WTD_UI_NONE,
};
use windows_sys::Win32::Storage::FileSystem::{CreateDirectoryW, GetTempPathW, FILE_SHARE_READ};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, GetExitCodeProcess, WaitForSingleObject, INFINITE, PROCESS_INFORMATION,
    STARTUPINFOW,
};
use windows_sys::{s, w};

use super::elevation::{self, is_elevated, wide, Relaunch};
use crate::sensor::pawnio::{install_dir, lib_present, pawnio_installed};

/// `https://github.com/namazso/PawnIO.Setup/releases/latest/download/PawnIO_setup.exe`
const HOST: &str = "github.com";
const URL_PATH: &str = "/namazso/PawnIO.Setup/releases/latest/download/PawnIO_setup.exe";
/// Subject CN of the certificate PawnIO's binaries are signed with.
const PUBLISHER_CN: &str = "namazso.eu";
const INSTALL_ARGS: &str = "-install -silent";
const UNINSTALL_ARGS: &str = "-uninstall -silent";

/// Downloads the PawnIO installer, checks its signature and runs it silently. Needs elevation.
/// Succeeds when the installer exits with 0 and the driver is usable afterwards.
pub fn install() -> Result<(), String> {
    let setup = fetch_verified()?;
    // `setup` keeps the file locked until the installer has exited, then deletes it.
    let code = run_and_wait(&setup.path, INSTALL_ARGS)?;
    drop(setup);
    if code != 0 {
        Err(format!(
            "The PawnIO installer failed with exit code {code}."
        ))
    } else if !pawnio_installed() {
        Err("The PawnIO installer finished, but the driver is still not available.".into())
    } else {
        Ok(())
    }
}

/// Runs PawnIO's own uninstaller silently, after checking its signature; through the UAC
/// prompt when not elevated. Succeeds when PawnIOLib.dll is gone afterwards.
pub fn uninstall() -> Result<(), String> {
    let exe = install_dir()
        .ok_or("The Program Files folder was not found.")?
        .join("uninstall.exe");
    // Unlike the downloaded installer, the file is not kept locked while it runs: only
    // administrators can change Program Files, and the uninstaller may delete itself.
    verify(&open_locked(&exe)?, &exe)?;
    let code = if is_elevated() {
        run_and_wait(&exe, UNINSTALL_ARGS)?
    } else {
        elevation::run_elevated_and_wait(&exe, UNINSTALL_ARGS).map_err(|r| match r {
            Relaunch::Cancelled => "Administrator permission was not given.".to_string(),
            _ => "The PawnIO uninstaller could not be started.".to_string(),
        })?
    };
    if !lib_present() {
        Ok(())
    } else if code != 0 {
        Err(format!(
            "The PawnIO uninstaller failed with exit code {code}."
        ))
    } else {
        Err("The PawnIO uninstaller finished, but PawnIO is still installed.".into())
    }
}

/// The downloaded installer in its own fresh folder. The open file keeps it from being
/// changed, renamed or deleted; dropping this closes it and deletes the file and folder.
struct Download {
    dir: PathBuf,
    path: PathBuf,
    file: Option<File>,
}

impl Drop for Download {
    fn drop(&mut self) {
        self.file = None;
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_dir(&self.dir);
    }
}

/// Downloads the installer into a fresh folder, locks it and checks its signature.
fn fetch_verified() -> Result<Download, String> {
    let dir = fresh_dir()?;
    let mut setup = Download {
        path: dir.join("PawnIO_setup.exe"),
        dir,
        file: None,
    };
    download(&setup.path)?;
    let file = open_locked(&setup.path)?;
    verify(&file, &setup.path)?;
    setup.file = Some(file);
    Ok(setup)
}

fn last_error() -> u32 {
    // SAFETY: reads this thread's last error.
    unsafe { GetLastError() }
}

/// The temp folder from GetTempPath2W where the system has it, else GetTempPathW. Looked up
/// at run time, so the exe still starts on Windows 10 builds without it.
fn temp_dir() -> Option<PathBuf> {
    type GetTempPath2W = unsafe extern "system" fn(u32, *mut u16) -> u32;
    let mut buf = [0u16; MAX_PATH as usize + 1];
    // SAFETY: kernel32 is loaded in every process, and a GetTempPath2W export has the
    // signature above; both calls get the buffer's real length.
    let n = unsafe {
        match GetProcAddress(GetModuleHandleW(w!("kernel32.dll")), s!("GetTempPath2W")) {
            Some(f) => std::mem::transmute::<unsafe extern "system" fn() -> isize, GetTempPath2W>(
                f,
            )(buf.len() as u32, buf.as_mut_ptr()),
            None => GetTempPathW(buf.len() as u32, buf.as_mut_ptr()),
        }
    } as usize;
    (n > 0 && n < buf.len()).then(|| PathBuf::from(OsString::from_wide(&buf[..n])))
}

/// Creates a new, uniquely named folder under the temp folder. Creating fails when the name
/// exists, so a returned folder is always one made here.
fn fresh_dir() -> Result<PathBuf, String> {
    let base = temp_dir().ok_or("The temp folder was not found.")?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    for i in 0..100 {
        let dir = base.join(format!("TrayTemp-{}-{stamp:x}-{i}", std::process::id()));
        match create_private_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("Couldn't create a temp folder: {e}")),
        }
    }
    Err("Couldn't create a temp folder.".into())
}

/// Creates `dir` owned by Administrators, with access for SYSTEM and Administrators only, not
/// inherited from the user's temp folder. The elevated installer runs from it, so this user's
/// unelevated processes must not be able to place DLLs next to it, swap the downloaded file or,
/// as its owner, change its access. Needs elevation.
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut sd: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: a NUL-terminated SDDL literal; on success `sd` gets a LocalAlloc'd descriptor.
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            w!("O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"),
            SDDL_REVISION_1,
            &mut sd,
            null_mut(),
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd,
        bInheritHandle: 0,
    };
    let path = wide(dir);
    // SAFETY: NUL-terminated path; `sa` and its descriptor live through the call, and the
    // descriptor is freed once, after the error code is read.
    unsafe {
        let ok = CreateDirectoryW(path.as_ptr(), &sa);
        let result = if ok == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        };
        LocalFree(sd);
        result
    }
}

/// A WinHTTP handle, closed on drop.
struct Internet(*mut c_void);

impl Drop for Internet {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the handle came from WinHTTP and is closed once.
            unsafe { WinHttpCloseHandle(self.0) };
        }
    }
}

/// Downloads the installer over HTTPS into a new file at `path`.
fn download(path: &Path) -> Result<(), String> {
    let fail = |step: &str| format!("The download failed while {step} (error {}).", last_error());
    let (host, url_path) = (wide(HOST), wide(URL_PATH));
    // SAFETY: the client name is a NUL-terminated literal; no proxy strings.
    let session = Internet(unsafe {
        WinHttpOpen(
            w!("TrayTemp"),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            null(),
            null(),
            0,
        )
    });
    if session.0.is_null() {
        return Err(fail("starting"));
    }
    // SAFETY: valid session handle; `host` is NUL-terminated and outlives the call.
    let connect = Internet(unsafe {
        WinHttpConnect(session.0, host.as_ptr(), INTERNET_DEFAULT_HTTPS_PORT, 0)
    });
    if connect.0.is_null() {
        return Err(fail("connecting"));
    }
    // WINHTTP_FLAG_SECURE: TLS only. GitHub redirects to its download host; WinHTTP follows
    // redirects under its default policy, which refuses any from HTTPS to plain HTTP.
    // SAFETY: valid connection handle; the strings are NUL-terminated and outlive the call.
    let request = Internet(unsafe {
        WinHttpOpenRequest(
            connect.0,
            w!("GET"),
            url_path.as_ptr(),
            null(),
            null(),
            null(),
            WINHTTP_FLAG_SECURE,
        )
    });
    if request.0.is_null() {
        return Err(fail("connecting"));
    }
    // SAFETY: valid request handle; no extra headers or body.
    let sent = unsafe {
        WinHttpSendRequest(request.0, null(), 0, null(), 0, 0, 0) != 0
            && WinHttpReceiveResponse(request.0, null_mut()) != 0
    };
    if !sent {
        return Err(fail("connecting"));
    }
    let mut status = 0u32;
    let mut len = size_of::<u32>() as u32;
    // SAFETY: with FLAG_NUMBER the status code is written as a u32 into `status`.
    let ok = unsafe {
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            null(),
            &mut status as *mut u32 as *mut c_void,
            &mut len,
            null_mut(),
        )
    };
    if ok == 0 {
        return Err(fail("reading the response"));
    }
    if status != 200 {
        return Err(format!(
            "The download failed: the server answered {status}."
        ));
    }
    let save_err = |e: std::io::Error| format!("Couldn't save the download: {e}");
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .share_mode(FILE_SHARE_READ)
        .open(path)
        .map_err(save_err)?;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let mut n = 0u32;
        // SAFETY: valid request handle; `buf` holds the number of bytes passed.
        let ok = unsafe {
            WinHttpReadData(
                request.0,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
                &mut n,
            )
        };
        if ok == 0 {
            return Err(fail("receiving"));
        }
        if n == 0 {
            return Ok(());
        }
        out.write_all(&buf[..n as usize]).map_err(save_err)?;
    }
}

/// Opens `path` for reading. While the handle lives, nobody can write, rename or delete the
/// file (share mode read only).
fn open_locked(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(path)
        .map_err(|e| format!("Couldn't open {}: {e}", path.display()))
}

/// Checks that `file`, opened from `path`, has a valid Authenticode signature and that the
/// signing certificate was issued to PawnIO's author.
fn verify(file: &File, path: &Path) -> Result<(), String> {
    let cn = signer(file, path)?;
    if cn == PUBLISHER_CN {
        return Ok(());
    }
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    Err(format!(
        "{name} is signed by \"{cn}\", not by PawnIO's author."
    ))
}

/// Subject CN of the certificate `file` (opened from `path`) is signed with, once its
/// Authenticode signature checks out (revocation as the system's default policy checks it).
fn signer(file: &File, path: &Path) -> Result<String, String> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let wpath = wide(path);
    let mut info = WINTRUST_FILE_INFO {
        cbStruct: size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: wpath.as_ptr(),
        // The open file is what gets checked, not whatever the path names by now.
        hFile: file.as_raw_handle() as HANDLE,
        pgKnownSubject: null_mut(),
    };
    let mut data = WINTRUST_DATA {
        cbStruct: size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        // No revocation checks beyond those of the default policy.
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 { pFile: &mut info },
        dwStateAction: WTD_STATEACTION_VERIFY,
        ..Default::default()
    };
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    // SAFETY: `data` and `info` are initialised with their sizes and outlive the call; the
    // file handle stays open.
    let status = unsafe {
        WinVerifyTrust(
            null_mut(),
            &mut action,
            &mut data as *mut WINTRUST_DATA as *mut c_void,
        )
    };
    let result = if status == TRUST_E_NOSIGNATURE {
        Err(format!("{name} is not signed."))
    } else if status != 0 {
        Err(format!(
            "The signature of {name} is not valid (0x{:08X}).",
            status as u32
        ))
    } else {
        // SAFETY: the verify call succeeded and its state is closed only below.
        Ok(unsafe { signer_cn(data.hWVTStateData) }.unwrap_or_default())
    };
    data.dwStateAction = WTD_STATEACTION_CLOSE;
    // SAFETY: releases the state the verify call allocated; `data` is otherwise unchanged.
    unsafe {
        WinVerifyTrust(
            null_mut(),
            &mut action,
            &mut data as *mut WINTRUST_DATA as *mut c_void,
        )
    };
    result
}

/// Subject CN of the first signer's own certificate (the leaf of its chain).
/// SAFETY: `state` must be the open state of a successful WinVerifyTrust call.
unsafe fn signer_cn(state: HANDLE) -> Option<String> {
    let prov = WTHelperProvDataFromStateData(state);
    if prov.is_null() {
        return None;
    }
    let signer = WTHelperGetProvSignerFromChain(prov, 0, 0, 0);
    if signer.is_null() || (*signer).csCertChain == 0 || (*signer).pasCertChain.is_null() {
        return None;
    }
    let cert = (*(*signer).pasCertChain).pCert;
    if cert.is_null() {
        return None;
    }
    let mut buf = [0u16; 256];
    let n = CertGetNameStringW(
        cert,
        CERT_NAME_ATTR_TYPE,
        0,
        szOID_COMMON_NAME as *const c_void,
        buf.as_mut_ptr(),
        buf.len() as u32,
    ) as usize;
    // The count includes the terminating NUL; 1 means there is no CN.
    (n > 1 && n <= buf.len()).then(|| String::from_utf16_lossy(&buf[..n - 1]))
}

/// Runs `exe args` and waits for it to exit; returns its exit code. `exe` is absolute and
/// quoted in the command line; the program's own folder is its working directory.
fn run_and_wait(exe: &Path, args: &str) -> Result<u32, String> {
    let app = wide(exe);
    let mut line = OsString::from("\"");
    line.push(exe);
    line.push("\" ");
    line.push(args);
    let mut line = wide(line);
    let dir = exe.parent().map(wide);
    let si = STARTUPINFOW {
        cb: size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut pi = PROCESS_INFORMATION::default();
    // SAFETY: absolute application path and a writable, NUL-terminated command line, both
    // alive through the call; no handles are inherited.
    let ok = unsafe {
        CreateProcessW(
            app.as_ptr(),
            line.as_mut_ptr(),
            null(),
            null(),
            0,
            0,
            null(),
            dir.as_ref().map_or(null(), |d| d.as_ptr()),
            &si,
            &mut pi,
        )
    };
    if ok == 0 {
        return Err(format!(
            "Couldn't start {} (error {}).",
            exe.display(),
            last_error()
        ));
    }
    let mut code = 0;
    // SAFETY: both handles came from CreateProcessW and are closed once.
    let ok = unsafe {
        CloseHandle(pi.hThread);
        WaitForSingleObject(pi.hProcess, INFINITE);
        let ok = GetExitCodeProcess(pi.hProcess, &mut code);
        CloseHandle(pi.hProcess);
        ok
    };
    if ok == 0 {
        return Err(format!("Couldn't read the exit code of {}.", exe.display()));
    }
    Ok(code)
}
