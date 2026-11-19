//! Start with Windows. With CPU temperature on, TrayTemp has to run elevated, so it starts
//! from a Task Scheduler logon task that runs with the highest privileges, from a copy of the
//! exe in Program Files (an elevated task must never point at a path the user can write).
//! Otherwise it starts from a plain `Run` value for the current user.
//!
//! The functions here wait for other programs (and for the UAC prompt) and show no UI: call
//! them with the window state released, and from a worker thread where the UI must not block.

use std::ffi::{c_void, OsString};
use std::fs::OpenOptions;
use std::io::Write;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::ptr::null_mut;

use windows_sys::core::PCWSTR;
use windows_sys::w;
use windows_sys::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS, MAX_PATH,
};
use windows_sys::Win32::Security::Authentication::Identity::{GetUserNameExW, NameSamCompatible};
use windows_sys::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ,
};
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

use super::elevation::{is_elevated, run_elevated_and_wait, wide, Relaunch};
use super::pawnio_setup::run_and_wait;
use crate::sensor::pawnio::install_dir;

const TASK_NAME: &str = "TrayTemp";
const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("TrayTemp");

/// `<Program Files>\TrayTemp\traytemp.exe`, with Program Files taken from the shell, not the
/// environment: the app may run elevated and a per-user variable could redirect it.
pub fn program_files_exe() -> Option<PathBuf> {
    // `install_dir` is `<Program Files>\PawnIO`.
    Some(
        install_dir()?
            .parent()?
            .join("TrayTemp")
            .join("traytemp.exe"),
    )
}

/// Makes the system start TrayTemp at logon in the form `elevated_task` asks for, or in no
/// form when `enabled` is false.
/// The elevated task needs an elevated process (it is also what writes to Program Files); when
/// this one isn't, the plain `Run` form is used instead. Taking the elevated task away from a
/// process that isn't elevated goes through the UAC prompt.
pub fn set_autostart(enabled: bool, elevated_task: bool) -> Result<(), String> {
    if !enabled {
        let run = remove_run_value();
        return run.and(remove_task());
    }
    if elevated_task && is_elevated() {
        // The task goes first, so there is never a moment with no form at all.
        install_task()?;
        return remove_run_value();
    }
    set_run_value(&std::env::current_exe().map_err(|e| format!("The exe path is unknown: {e}"))?)?;
    remove_task()
}

/// Brings autostart in line with the CPU setting at startup, for when the settings say it is
/// on: the process that handled enabling or disabling CPU may have been replaced by an elevated
/// relaunch before it could. Reads one registry value; only starts a worker when something has
/// to change.
pub fn sync_at_startup(cpu_enabled: bool) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let run = run_value();
    let ours = run_value_text(&exe);
    let elevated = is_elevated();
    // Elevated with CPU on: the task form, so a `Run` value is left over.
    // Elevated with CPU off, or not elevated: the `Run` form, current (when not elevated a
    // missing value means the task form is in use, which is left alone).
    let stale = match (elevated, cpu_enabled) {
        (true, true) => run.is_some(),
        (true, false) => run.as_deref() != Some(ours.as_str()),
        (false, _) => run.is_some_and(|v| v != ours),
    };
    if !stale {
        return;
    }
    std::thread::spawn(move || {
        let _ = if elevated {
            set_autostart(true, cpu_enabled)
        } else {
            set_run_value(&exe)
        };
    });
}

/// The `Run` value's data for `exe`: its path in quotes.
fn run_value_text(exe: &Path) -> String {
    format!("\"{}\"", exe.display())
}

fn set_run_value(exe: &Path) -> Result<(), String> {
    let data = wide(run_value_text(exe));
    // SAFETY: valid NUL-terminated names; the data is `data.len()` UTF-16 units including
    // its NUL, as REG_SZ wants.
    let err = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            RUN_VALUE,
            REG_SZ,
            data.as_ptr() as *const c_void,
            (data.len() * 2) as u32,
        )
    };
    if err == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(format!("The startup entry could not be written (error {err})."))
    }
}

/// A missing value is not an error.
fn remove_run_value() -> Result<(), String> {
    // SAFETY: valid NUL-terminated names.
    let err = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE) };
    if matches!(err, ERROR_SUCCESS | ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) {
        Ok(())
    } else {
        Err(format!("The startup entry could not be removed (error {err})."))
    }
}

/// The `Run` value's text, if there is one.
fn run_value() -> Option<String> {
    let mut buf = [0u16; MAX_PATH as usize * 2];
    let mut bytes = size_of_val(&buf) as u32;
    // SAFETY: valid names; the buffer's real size in bytes is passed and updated.
    let err = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            RUN_VALUE,
            RRF_RT_REG_SZ,
            null_mut(),
            buf.as_mut_ptr() as *mut c_void,
            &mut bytes,
        )
    };
    if err != ERROR_SUCCESS {
        return None;
    }
    // The size includes the terminating NUL.
    let units = (bytes as usize / 2).saturating_sub(1).min(buf.len());
    Some(String::from_utf16_lossy(&buf[..units]))
}

/// `<System32>\schtasks.exe`, absolute, so no search path decides what runs.
fn schtasks() -> Result<PathBuf, String> {
    let mut buf = [0u16; MAX_PATH as usize + 1];
    // SAFETY: the buffer's real length is passed.
    let n = unsafe { GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
    if n == 0 || n >= buf.len() {
        return Err("The Windows folder was not found.".into());
    }
    Ok(PathBuf::from(OsString::from_wide(&buf[..n])).join("schtasks.exe"))
}

fn task_exists(schtasks: &Path) -> Result<bool, String> {
    let args = format!("/Query /TN {TASK_NAME}");
    Ok(run_and_wait(schtasks, &args, CREATE_NO_WINDOW)? == 0)
}

/// Deletes the logon task if there is one. Elevated, that is a plain schtasks run; otherwise
/// the delete runs elevated through the UAC prompt.
fn remove_task() -> Result<(), String> {
    let schtasks = schtasks()?;
    if !task_exists(&schtasks)? {
        return Ok(());
    }
    let args = format!("/Delete /TN {TASK_NAME} /F");
    let code = if is_elevated() {
        run_and_wait(&schtasks, &args, CREATE_NO_WINDOW)?
    } else {
        run_elevated_and_wait(&schtasks, &args, SW_HIDE).map_err(|r| match r {
            Relaunch::Cancelled => "Administrator permission was not given.".to_string(),
            _ => "The startup task could not be removed.".to_string(),
        })?
    };
    if code == 0 {
        Ok(())
    } else {
        Err(format!("Removing the startup task failed with exit code {code}."))
    }
}

/// Copies the exe to Program Files and registers the logon task for it. Needs elevation.
fn install_task() -> Result<(), String> {
    let dest = program_files_exe().ok_or("The Program Files folder was not found.")?;
    let dir = dest.parent().ok_or("The Program Files folder was not found.")?;
    copy_exe(&dest, dir)?;
    let user = user_name().ok_or("The user name was not found.")?;
    let xml = task_xml(&user, &dest, dir);
    let schtasks = schtasks()?;

    // The definition goes next to the copied exe, which only administrators can change, not
    // into the user's temp folder, where another process could swap it before schtasks reads it.
    let file = dir.join(format!("task-{}.xml", std::process::id()));
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(xml.encode_utf16().flat_map(u16::to_le_bytes));
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&file)
        .and_then(|mut f| f.write_all(&bytes))
        .map_err(|e| {
            let _ = std::fs::remove_file(&file);
            format!("The startup task could not be prepared: {e}")
        })?;
    let args = format!("/Create /TN {TASK_NAME} /XML \"{}\" /F", file.display());
    let code = run_and_wait(&schtasks, &args, CREATE_NO_WINDOW);
    let _ = std::fs::remove_file(&file);
    match code? {
        0 => Ok(()),
        c => Err(format!("Creating the startup task failed with exit code {c}.")),
    }
}

/// Puts the running exe at `dest`, unless that is where it already runs from.
fn copy_exe(dest: &Path, dir: &Path) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("The exe path is unknown: {e}"))?;
    let same = match (exe.canonicalize(), dest.canonicalize()) {
        (Ok(a), Ok(b)) => a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase(),
        _ => false,
    };
    if same {
        return Ok(());
    }
    std::fs::create_dir_all(dir)
        .and_then(|()| std::fs::copy(&exe, dest))
        .map(drop)
        .map_err(|e| format!("TrayTemp could not be copied to Program Files: {e}"))
}

/// `DOMAIN\user` of the current user.
fn user_name() -> Option<String> {
    let mut buf = [0u16; 257 + 1 + 256 + 1];
    let mut n = buf.len() as u32;
    // SAFETY: `n` is the buffer's length in units; on success it is set to the length written,
    // without the NUL.
    let ok = unsafe { GetUserNameExW(NameSamCompatible, buf.as_mut_ptr(), &mut n) };
    (ok && n > 0).then(|| String::from_utf16_lossy(&buf[..n as usize]))
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// A logon task for `user` that runs `exe` with the highest privileges. Written out instead
/// of using schtasks switches, whose defaults stop a task after three days and keep it from
/// starting on battery power.
fn task_xml(user: &str, exe: &Path, dir: &Path) -> String {
    let user = xml_escape(user);
    let exe = xml_escape(&exe.to_string_lossy());
    let dir = xml_escape(&dir.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe}</Command>
      <WorkingDirectory>{dir}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#
    )
}
