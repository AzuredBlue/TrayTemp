//! Start with Windows. With CPU temperature on, TrayTemp has to run elevated, so it starts
//! from a Task Scheduler logon task that runs with the highest privileges, from a copy of the
//! exe in Program Files (an elevated task must never point at a path the user can write).
//! Otherwise it starts from a plain `Run` value for the current user.
//!
//! All the work runs on one worker thread (see `request`), never on the UI thread: it waits for
//! other programs and for the UAC prompt. Errors come back to the window as a posted message.

use std::ffi::{c_void, OsString};
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use windows_sys::core::{GUID, PCWSTR};
use windows_sys::w;
use windows_sys::Win32::Foundation::{
    SysAllocString, SysFreeString, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS, HWND,
    MAX_PATH,
};
use windows_sys::Win32::Security::Authentication::Identity::{GetUserNameExW, NameSamCompatible};
use windows_sys::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows_sys::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ,
};
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, SW_HIDE};

use super::elevation::{is_elevated, run_elevated_and_wait, wide, Relaunch};
use super::{AUTOSTART_ERROR, WM_AUTOSTART_ERROR};
use crate::sensor::pawnio::program_files;

const TASK_NAME: &str = "TrayTemp";
const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("TrayTemp");

/// `<Program Files>\TrayTemp\traytemp.exe`.
fn program_files_exe() -> Option<PathBuf> {
    Some(program_files()?.join("TrayTemp").join("traytemp.exe"))
}

/// What the worker is to make true.
struct Want {
    hwnd: isize,
    enabled: bool,
    elevated_task: bool,
}

static PENDING: Mutex<Option<Want>> = Mutex::new(None);
static RUNNING: AtomicBool = AtomicBool::new(false);

/// Asks for autostart to be in the form `elevated_task` wants, or off when `enabled` is false.
/// Returns at once; one worker applies them, and of requests that pile up the newest wins, so
/// there are never two writers. A failure is left in `AUTOSTART_ERROR` and `hwnd` gets
/// `WM_AUTOSTART_ERROR`.
pub fn request(hwnd: HWND, enabled: bool, elevated_task: bool) {
    // Setting the request and claiming the worker happen under one lock, and the worker
    // releases its claim under the same lock, so a request is never left without a worker.
    let mut pending = PENDING.lock().unwrap();
    *pending = Some(Want {
        hwnd: hwnd as isize,
        enabled,
        elevated_task,
    });
    if !RUNNING.swap(true, Ordering::AcqRel) && std::thread::Builder::new().spawn(worker).is_err() {
        RUNNING.store(false, Ordering::Release);
    }
}

fn worker() {
    // SAFETY: balanced by CoUninitialize below when it succeeded.
    let com = unsafe { CoInitializeEx(null(), COINIT_APARTMENTTHREADED as u32) } >= 0;
    loop {
        let want = {
            let mut pending = PENDING.lock().unwrap();
            let want = pending.take();
            if want.is_none() {
                RUNNING.store(false, Ordering::Release);
            }
            want
        };
        let Some(want) = want else { break };
        if let Err(e) = apply(want.enabled, want.elevated_task) {
            *AUTOSTART_ERROR.lock().unwrap() = Some(e);
            // SAFETY: plain post to the window.
            unsafe { PostMessageW(want.hwnd as HWND, WM_AUTOSTART_ERROR, 0, 0) };
        }
    }
    if com {
        // SAFETY: pairs with the successful CoInitializeEx above.
        unsafe { CoUninitialize() };
    }
}

/// Brings autostart in line with the settings at startup. The process that handled enabling
/// or disabling CPU may have been replaced by an elevated relaunch before it could, and an
/// updated TrayTemp must replace the Program Files copy before the task runs the old one.
/// Elevated, that is the whole job (it also removes a copy that is no longer wanted); not
/// elevated it only brings a stale `Run` value up to date and leaves a task form alone.
pub fn sync_at_startup(hwnd: HWND, autostart: bool, cpu_enabled: bool) {
    if is_elevated() {
        request(hwnd, autostart, cpu_enabled);
    } else if autostart {
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        if run_value().is_some_and(|v| v != run_value_text(&exe)) {
            request(hwnd, true, true);
        }
    }
}

fn apply(enabled: bool, elevated_task: bool) -> Result<(), String> {
    let elevated = is_elevated();
    if enabled && elevated_task && elevated {
        // The task goes first, so there is never a moment with no form at all.
        install_task()?;
        return remove_run_value();
    }
    let mut run = Ok(());
    if enabled {
        set_run_value(
            &std::env::current_exe().map_err(|e| format!("The exe path is unknown: {e}"))?,
        )?;
        if elevated_task {
            // The elevated form was wanted but this process isn't elevated (UAC was declined):
            // a working task stays, with no second prompt.
            return Ok(());
        }
    } else {
        run = remove_run_value();
    }
    let task = remove_task();
    if elevated && task.is_ok() {
        remove_copy();
    }
    run.and(task)
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
        Err(format!(
            "The startup entry could not be written (error {err})."
        ))
    }
}

/// A missing value is not an error.
fn remove_run_value() -> Result<(), String> {
    // SAFETY: valid NUL-terminated names.
    let err = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE) };
    if matches!(
        err,
        ERROR_SUCCESS | ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND
    ) {
        Ok(())
    } else {
        Err(format!(
            "The startup entry could not be removed (error {err})."
        ))
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

// ---- Task Scheduler 2.0 COM interfaces (taskschd.h), only the methods used ----

/// IUnknown and IDispatch: seven slots, of which only Release is called.
#[repr(C)]
struct Base {
    _query_interface: usize,
    _add_ref: usize,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    _dispatch: [usize; 4],
}

/// An empty VARIANT (VT_EMPTY), 24 bytes on x64, passed by value like the headers do.
#[repr(C)]
struct Variant([u64; 3]);

const EMPTY: Variant = Variant([0; 3]);

// The vtable signatures pass VARIANT by value with its 64-bit layout; a 32-bit build would
// need a different one.
const _: () = assert!(size_of::<Variant>() == 24 && size_of::<usize>() == 8);

#[repr(C)]
struct ServiceVtbl {
    base: Base,
    get_folder: unsafe extern "system" fn(*mut Service, *const u16, *mut *mut Folder) -> i32,
    _get_running_tasks: usize,
    _new_task: usize,
    connect: unsafe extern "system" fn(*mut Service, Variant, Variant, Variant, Variant) -> i32,
}

#[repr(C)]
struct Service {
    vtbl: *const ServiceVtbl,
}

#[repr(C)]
struct FolderVtbl {
    base: Base,
    // get_Name, get_Path, GetFolder, GetFolders, CreateFolder, DeleteFolder
    _folder_methods: [usize; 6],
    get_task: unsafe extern "system" fn(*mut Folder, *const u16, *mut *mut Task) -> i32,
    _get_tasks: usize,
    delete_task: unsafe extern "system" fn(*mut Folder, *const u16, i32) -> i32,
    register_task: unsafe extern "system" fn(
        *mut Folder,
        *const u16,
        *const u16,
        i32,
        Variant,
        Variant,
        i32,
        Variant,
        *mut *mut Task,
    ) -> i32,
}

#[repr(C)]
struct Folder {
    vtbl: *const FolderVtbl,
}

#[repr(C)]
struct TaskVtbl {
    base: Base,
}

#[repr(C)]
struct Task {
    vtbl: *const TaskVtbl,
}

/// An owned interface pointer, released once when dropped. Every interface here starts with
/// its vtable pointer, whose first seven slots are `Base`.
struct Com<T>(*mut T);

impl<T> Drop for Com<T> {
    fn drop(&mut self) {
        // SAFETY: the pointer is a live interface owning one reference; see `Base`.
        unsafe {
            let vtbl = *(self.0 as *const *const Base);
            ((*vtbl).release)(self.0 as *mut c_void);
        }
    }
}

/// An owned BSTR.
struct Bstr(*const u16);

impl Bstr {
    fn new(s: &str) -> Bstr {
        // SAFETY: the string is NUL-terminated; SysAllocString copies it.
        Bstr(unsafe { SysAllocString(wide(s).as_ptr()) })
    }
}

impl Drop for Bstr {
    fn drop(&mut self) {
        // SAFETY: allocated by SysAllocString, freed once.
        unsafe { SysFreeString(self.0) };
    }
}

const CLSID_TASK_SCHEDULER: GUID = GUID::from_u128(0x0f87369f_a4e5_4cfc_bd3e_73e6154572dd);
const IID_ITASK_SERVICE: GUID = GUID::from_u128(0x2faba4c7_4da9_4013_9697_20cc3fd40f85);
const TASK_CREATE_OR_UPDATE: i32 = 6;
const TASK_LOGON_INTERACTIVE_TOKEN: i32 = 3;
/// HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND): no such task.
const HR_NOT_FOUND: i32 = 0x8007_0002_u32 as i32;

fn hr_error(what: &str, hr: i32) -> String {
    format!("{what} (error 0x{:08X}).", hr as u32)
}

/// The root folder of the local task store. Needs COM on the calling thread.
fn root_folder() -> Result<Com<Folder>, String> {
    let fail = |hr| hr_error("The task scheduler could not be opened", hr);
    let mut service: *mut Service = null_mut();
    // SAFETY: the out-pointer receives an ITaskService reference we own from here on.
    let hr = unsafe {
        CoCreateInstance(
            &CLSID_TASK_SCHEDULER,
            null_mut(),
            CLSCTX_INPROC_SERVER,
            &IID_ITASK_SERVICE,
            &mut service as *mut _ as *mut *mut c_void,
        )
    };
    if hr < 0 {
        return Err(fail(hr));
    }
    let service = Com(service);
    let root = Bstr::new("\\");
    let mut folder: *mut Folder = null_mut();
    // SAFETY: `service` is a live ITaskService; Connect takes four optional VARIANTs, all
    // empty for the local machine and the current user; `root` is a BSTR.
    let hr = unsafe {
        let vtbl = &*(*service.0).vtbl;
        let hr = (vtbl.connect)(service.0, EMPTY, EMPTY, EMPTY, EMPTY);
        if hr < 0 {
            hr
        } else {
            (vtbl.get_folder)(service.0, root.0, &mut folder)
        }
    };
    if hr < 0 {
        return Err(fail(hr));
    }
    Ok(Com(folder))
}

/// Deletes the logon task if there is one. Elevated, that goes through the task scheduler;
/// otherwise a task that may exist is deleted through Microsoft's own schtasks.exe, started
/// elevated, so the UAC prompt names a signed Microsoft program.
fn remove_task() -> Result<(), String> {
    let folder = root_folder()?;
    let name = Bstr::new(TASK_NAME);
    // SAFETY: `folder` is a live ITaskFolder and `name` a BSTR.
    unsafe {
        let vtbl = &*(*folder.0).vtbl;
        if is_elevated() {
            return match (vtbl.delete_task)(folder.0, name.0, 0) {
                0 | HR_NOT_FOUND => Ok(()),
                hr => Err(hr_error("The startup task could not be removed", hr)),
            };
        }
        let mut task: *mut Task = null_mut();
        match (vtbl.get_task)(folder.0, name.0, &mut task) {
            0 => drop(Com(task)),
            HR_NOT_FOUND => return Ok(()),
            // Anything else (access denied for a task made elevated, say) means the task may
            // exist and this process can't see it: delete it elevated.
            _ => {}
        }
    }
    delete_task_elevated()
}

/// `schtasks /Delete` through the UAC prompt, hidden. Absolute path: no search path decides
/// what runs.
fn delete_task_elevated() -> Result<(), String> {
    let mut buf = [0u16; MAX_PATH as usize + 1];
    // SAFETY: the buffer's real length is passed.
    let n = unsafe { GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
    if n == 0 || n >= buf.len() {
        return Err("The Windows folder was not found.".into());
    }
    let schtasks = PathBuf::from(OsString::from_wide(&buf[..n])).join("schtasks.exe");
    let args = format!("/Delete /TN {TASK_NAME} /F");
    match run_elevated_and_wait(&schtasks, &args, SW_HIDE) {
        Ok(0) => Ok(()),
        Ok(code) => Err(format!(
            "Removing the startup task failed with exit code {code}."
        )),
        Err(Relaunch::Cancelled) => Err("Administrator permission was not given.".into()),
        Err(_) => Err("The startup task could not be removed.".into()),
    }
}

/// Puts the copy in Program Files in place and registers the logon task for it. Needs elevation.
fn install_task() -> Result<(), String> {
    let dest = program_files_exe().ok_or("The Program Files folder was not found.")?;
    let dir = dest
        .parent()
        .ok_or("The Program Files folder was not found.")?;
    sync_copy(&dest, dir)?;
    let user = user_name().ok_or("The user name was not found.")?;
    let xml = Bstr::new(&task_xml(&user, &dest, dir));
    let folder = root_folder()?;
    let name = Bstr::new(TASK_NAME);
    let mut task: *mut Task = null_mut();
    // SAFETY: `folder` is a live ITaskFolder; the BSTRs outlive the call; the VARIANTs are
    // empty (the task's user comes from the XML, no password with an interactive token).
    let hr = unsafe {
        ((*(*folder.0).vtbl).register_task)(
            folder.0,
            name.0,
            xml.0,
            TASK_CREATE_OR_UPDATE,
            EMPTY,
            EMPTY,
            TASK_LOGON_INTERACTIVE_TOKEN,
            EMPTY,
            &mut task,
        )
    };
    if hr < 0 {
        return Err(hr_error("The startup task could not be created", hr));
    }
    drop(Com(task));
    Ok(())
}

/// Whether `a` and `b` are the same file, comparing names without case.
fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a.as_os_str().eq_ignore_ascii_case(b.as_os_str()),
        _ => false,
    }
}

/// Where the running exe is now. `current_exe()` is where it was at start, and a running exe can
/// be renamed and another file put in its place.
fn running_exe() -> Result<PathBuf, String> {
    let mut buf = [0u16; 1024];
    let mut n = buf.len() as u32;
    // SAFETY: `n` is the buffer's length in units; on success it is the length written.
    let ok = unsafe {
        QueryFullProcessImageNameW(GetCurrentProcess(), PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut n)
    };
    if ok == 0 {
        return Err("The exe path is unknown.".into());
    }
    Ok(PathBuf::from(OsString::from_wide(&buf[..n as usize])))
}

/// Opens the running exe with renames and writes blocked, so its bytes are what this process
/// runs. Once the opened file can't be renamed, the running exe can only still have its name if
/// it is that file.
fn open_running_exe() -> Result<(PathBuf, File), String> {
    let path = running_exe()?;
    let file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(&path)
        .map_err(|e| format!("TrayTemp could not read its own exe: {e}"))?;
    if !running_exe()?.as_os_str().eq_ignore_ascii_case(path.as_os_str()) {
        return Err("TrayTemp's exe was moved while it ran. Start it again.".into());
    }
    Ok((path, file))
}

/// Makes `dest` a copy of the running exe: nothing happens when that is where it runs from or
/// the contents already match, so an updated TrayTemp replaces an old copy and an identical one
/// is not written again. The bytes come from the open running exe, never from a path that
/// another program could have swapped (the copy runs elevated at every logon).
fn sync_copy(dest: &Path, dir: &Path) -> Result<(), String> {
    let (exe, mut file) = open_running_exe()?;
    if same_file(&exe, dest) {
        return Ok(());
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|e| format!("TrayTemp could not read its own exe: {e}"))?;
    if std::fs::read(dest).is_ok_and(|old| old == bytes) {
        return Ok(());
    }
    // Written under a temporary name and then renamed over the old copy, so the task never
    // points at a half-written exe, even if TrayTemp exits during the copy.
    let part = dest.with_extension("exe.part");
    std::fs::create_dir_all(dir)
        .and_then(|()| std::fs::write(&part, &bytes))
        .and_then(|()| std::fs::rename(&part, dest))
        .map_err(|e| {
            let _ = std::fs::remove_file(&part);
            format!("TrayTemp could not be copied to Program Files: {e}")
        })
}

/// Removes the Program Files copy and its folder, for an elevated process once the task that
/// pointed at it is gone. Best effort: a copy that is in use stays and the next elevated start
/// tries again. When this process runs from the copy, the copy stays: it may be the only
/// TrayTemp the user has.
fn remove_copy() {
    let Some(dest) = program_files_exe() else {
        return;
    };
    let Some(dir) = dest.parent() else {
        return;
    };
    if !dest.exists() || std::env::current_exe().is_ok_and(|exe| same_file(&exe, &dest)) {
        return;
    }
    if std::fs::remove_file(&dest).is_ok() {
        let _ = std::fs::remove_dir(dir);
    }
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
/// of built through the API's objects: the defaults there stop a task after three days and
/// keep it from starting on battery power.
fn task_xml(user: &str, exe: &Path, dir: &Path) -> String {
    let user = xml_escape(user);
    let exe = xml_escape(&exe.to_string_lossy());
    let dir = xml_escape(&dir.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Author>TrayTemp</Author>
    <Description>Starts TrayTemp when you sign in, with administrator rights, so it can read the CPU temperature. Turning off "Start with Windows" in TrayTemp's menu removes this task.</Description>
  </RegistrationInfo>
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
