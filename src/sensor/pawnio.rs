//! Windows CPU package temperature through the PawnIO driver's user-mode library
//! and its signed Intel/AMD modules.

use super::cpu_decode::{amd_tctl, intel_package_temp};
use super::{valid_temp, Reading, Sensor};
use libloading::Library;
use std::ffi::{c_void, CStr, OsString};
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
use windows_sys::Win32::UI::Shell::{FOLDERID_ProgramFiles, SHGetKnownFolderPath};

static INTEL_MSR: &[u8] = include_bytes!("../../res/pawnio/IntelMSR.bin");
static AMD_FAMILY17: &[u8] = include_bytes!("../../res/pawnio/AMDFamily17.bin");

const E_ACCESSDENIED: i32 = 0x8007_0005_u32 as i32;
const MSR_TEMPERATURE_TARGET: u64 = 0x1A2;
const MSR_PACKAGE_THERM_STATUS: u64 = 0x1B1;
const SMN_THM_TCON_CUR_TMP: u64 = 0x0005_9800;
const MUTEX_WAIT_MS: u32 = 50;

type OpenFn = unsafe extern "system" fn(*mut HANDLE) -> i32;
type LoadFn = unsafe extern "system" fn(HANDLE, *const u8, usize) -> i32;
type ExecuteFn = unsafe extern "system" fn(
    HANDLE,
    *const u8,
    *const u64,
    usize,
    *mut u64,
    usize,
    *mut usize,
) -> i32;
type CloseFn = unsafe extern "system" fn(HANDLE) -> i32;

#[derive(Debug, PartialEq, Eq)]
pub enum Vendor {
    Intel,
    Amd,
    Other,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PawnIoError {
    NotInstalled,
    AccessDenied,
    UnsupportedCpu,
    Load(i32),
}

/// CPU vendor from CPUID leaf 0; AMD only counts when the family (leaf 1) is >= 0x17.
pub fn cpu_vendor() -> Vendor {
    use core::arch::x86_64::__cpuid;
    let l0 = __cpuid(0);
    let mut name = [0u8; 12];
    name[0..4].copy_from_slice(&l0.ebx.to_le_bytes());
    name[4..8].copy_from_slice(&l0.edx.to_le_bytes());
    name[8..12].copy_from_slice(&l0.ecx.to_le_bytes());
    match &name {
        b"GenuineIntel" => Vendor::Intel,
        b"AuthenticAMD" => {
            let eax = __cpuid(1).eax;
            let base = (eax >> 8) & 0xF;
            let family = if base == 0xF { base + ((eax >> 20) & 0xFF) } else { base };
            if family >= 0x17 {
                Vendor::Amd
            } else {
                Vendor::Other
            }
        }
        _ => Vendor::Other,
    }
}

/// `%ProgramFiles%\PawnIO`.
pub fn install_dir() -> Option<PathBuf> {
    Some(program_files()?.join("PawnIO"))
}

/// `%ProgramFiles%`, taken from the shell, not the environment: the app may run elevated and
/// a per-user variable could redirect it.
pub fn program_files() -> Option<PathBuf> {
    let mut p: *mut u16 = null_mut();
    // SAFETY: on success `p` receives a NUL-terminated string that we free below.
    let hr = unsafe { SHGetKnownFolderPath(&FOLDERID_ProgramFiles, 0, null_mut(), &mut p) };
    if hr < 0 || p.is_null() {
        return None;
    }
    // SAFETY: `p` is a valid NUL-terminated UTF-16 string until CoTaskMemFree.
    let dir = unsafe {
        let mut n = 0;
        while *p.add(n) != 0 {
            n += 1;
        }
        let s = OsString::from_wide(std::slice::from_raw_parts(p, n));
        CoTaskMemFree(p as *const c_void);
        s
    };
    Some(PathBuf::from(dir))
}

fn lib_path() -> Option<PathBuf> {
    Some(install_dir()?.join("PawnIOLib.dll"))
}

/// True when PawnIOLib.dll exists, whether or not it loads.
pub fn lib_present() -> bool {
    lib_path().is_some_and(|p| p.is_file())
}

fn load_lib() -> Result<Library, PawnIoError> {
    let path = lib_path().ok_or(PawnIoError::NotInstalled)?;
    if !path.is_file() {
        return Err(PawnIoError::NotInstalled);
    }
    // SAFETY: loading the driver's user-mode library by absolute path from the
    // admin-only Program Files tree; its initialisers are trusted as part of PawnIO.
    unsafe { Library::new(&path) }.map_err(|_| PawnIoError::NotInstalled)
}

/// True when PawnIOLib.dll exists in its install folder and loads.
pub fn pawnio_installed() -> bool {
    load_lib().is_ok()
}

fn map_hr(hr: i32) -> PawnIoError {
    if hr == E_ACCESSDENIED {
        PawnIoError::AccessDenied
    } else {
        PawnIoError::Load(hr)
    }
}

pub struct PawnIoCpu {
    execute: ExecuteFn,
    close: CloseFn,
    handle: HANDLE,
    vendor: Vendor,
    tjmax_msr: u64,
    /// `Global\Access_PCI` mutex (AMD only; null on Intel).
    pci_mutex: HANDLE,
    // Kept last: must outlive every call through the pointers above.
    _lib: Library,
}

// SAFETY: the PawnIO executor handle and the mutex handle are only used through
// `&mut self`, and neither has thread affinity, so moving the sensor is sound.
unsafe impl Send for PawnIoCpu {}

impl Drop for PawnIoCpu {
    fn drop(&mut self) {
        // SAFETY: both handles were created in `open` and are released exactly once.
        unsafe {
            (self.close)(self.handle);
            if !self.pci_mutex.is_null() {
                CloseHandle(self.pci_mutex);
            }
        }
    }
}

impl PawnIoCpu {
    pub fn open() -> Result<PawnIoCpu, PawnIoError> {
        let vendor = cpu_vendor();
        let blob = match vendor {
            Vendor::Intel => INTEL_MSR,
            Vendor::Amd => AMD_FAMILY17,
            Vendor::Other => return Err(PawnIoError::UnsupportedCpu),
        };
        let lib = load_lib()?;
        // SAFETY: the symbol names and signatures match PawnIOLib.h (STDAPICALLTYPE).
        let (open, load, execute, close) = unsafe {
            let sym = |_| PawnIoError::NotInstalled;
            let open = *lib.get::<OpenFn>(b"pawnio_open\0").map_err(sym)?;
            let load = *lib.get::<LoadFn>(b"pawnio_load\0").map_err(sym)?;
            let execute = *lib.get::<ExecuteFn>(b"pawnio_execute\0").map_err(sym)?;
            let close = *lib.get::<CloseFn>(b"pawnio_close\0").map_err(sym)?;
            (open, load, execute, close)
        };
        let mut handle: HANDLE = null_mut();
        // SAFETY: `handle` is a valid out-pointer.
        let hr = unsafe { open(&mut handle) };
        if hr != 0 {
            return Err(map_hr(hr));
        }
        // SAFETY: `handle` came from pawnio_open; blob is a valid slice.
        let hr = unsafe { load(handle, blob.as_ptr(), blob.len()) };
        if hr != 0 {
            // SAFETY: handle is valid and not used afterwards.
            unsafe { close(handle) };
            return Err(map_hr(hr));
        }
        let mut cpu = PawnIoCpu {
            execute,
            close,
            handle,
            vendor,
            tjmax_msr: 0,
            pci_mutex: null_mut(),
            _lib: lib,
        };
        // From here, dropping `cpu` on an error closes the handles.
        if cpu.vendor == Vendor::Intel {
            cpu.tjmax_msr = cpu.call(c"ioctl_read_msr", MSR_TEMPERATURE_TARGET).map_err(map_hr)?;
        } else {
            // SAFETY: the name is a NUL-terminated UTF-16 literal.
            let m = unsafe { CreateMutexW(null(), 0, windows_sys::w!("Global\\Access_PCI")) };
            if m.is_null() {
                return Err(PawnIoError::Load(E_ACCESSDENIED));
            }
            cpu.pci_mutex = m;
        }
        Ok(cpu)
    }

    /// Runs a one-input, one-output module function. `Err` carries the HRESULT.
    fn call(&self, name: &CStr, input: u64) -> Result<u64, i32> {
        let inp = [input];
        let mut out = [0u64];
        let mut returned = 0usize;
        // SAFETY: valid handle; buffers hold 1 element each, as the counts say.
        let hr = unsafe {
            (self.execute)(
                self.handle,
                name.as_ptr() as *const u8,
                inp.as_ptr(),
                1,
                out.as_mut_ptr(),
                1,
                &mut returned,
            )
        };
        if hr == 0 {
            Ok(out[0])
        } else {
            Err(hr)
        }
    }

    fn read_amd(&mut self) -> Reading {
        // SAFETY: pci_mutex is a valid mutex handle created in `open`.
        match unsafe { WaitForSingleObject(self.pci_mutex, MUTEX_WAIT_MS) } {
            WAIT_OBJECT_0 | WAIT_ABANDONED => {}
            WAIT_TIMEOUT => return Reading::Asleep,
            _ => return Reading::Unavailable,
        }
        let raw = self.call(c"ioctl_read_smn", SMN_THM_TCON_CUR_TMP);
        // SAFETY: we hold the mutex acquired above.
        unsafe { ReleaseMutex(self.pci_mutex) };
        match raw {
            Ok(v) => temp(Some(amd_tctl(v as u32))),
            Err(_) => Reading::Unavailable,
        }
    }
}

fn temp(t: Option<f32>) -> Reading {
    match t {
        Some(t) if valid_temp(t) => Reading::Temp(t),
        _ => Reading::Unavailable,
    }
}

impl Sensor for PawnIoCpu {
    fn read(&mut self) -> Reading {
        match self.vendor {
            Vendor::Intel => match self.call(c"ioctl_read_msr", MSR_PACKAGE_THERM_STATUS) {
                Ok(v) => temp(intel_package_temp(self.tjmax_msr, v)),
                Err(_) => Reading::Unavailable,
            },
            Vendor::Amd => self.read_amd(),
            Vendor::Other => Reading::Unavailable,
        }
    }
}
