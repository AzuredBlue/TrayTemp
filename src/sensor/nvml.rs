//! NVIDIA GPU temperature through NVML, loaded at runtime.

use super::{valid_temp, Reading, Sensor};
use libloading::Library;
use std::ffi::c_void;
use std::path::PathBuf;

type Dev = *mut c_void;
type InitFn = unsafe extern "C" fn() -> i32;
type ShutdownFn = unsafe extern "C" fn() -> i32;
type HandleFn = unsafe extern "C" fn(u32, *mut Dev) -> i32;
type TempFn = unsafe extern "C" fn(Dev, i32, *mut u32) -> i32;

const NVML_TEMPERATURE_GPU: i32 = 0;

pub struct NvmlSensor {
    shutdown: ShutdownFn,
    get_temp: TempFn,
    dev: Dev,
    // Kept last: must outlive every call through the pointers above.
    _lib: Library,
}

// SAFETY: NVML is thread-safe, and the device handle is only used through
// `&mut self`, so moving the sensor to another thread is sound.
unsafe impl Send for NvmlSensor {}

#[cfg(windows)]
fn candidates() -> Vec<PathBuf> {
    use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
    // System32 only: the app may run elevated, and `ProgramFiles` can be overridden by
    // a per-user environment variable, which would let a non-admin plant a DLL.
    // Current (DCH) NVIDIA drivers install nvml.dll in System32.
    let mut out = Vec::new();
    let mut buf = [0u16; 260];
    // SAFETY: buf is valid for buf.len() u16 writes.
    let n = unsafe { GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
    if n > 0 && n < buf.len() {
        let dir = String::from_utf16_lossy(&buf[..n]);
        out.push(PathBuf::from(dir).join("nvml.dll"));
    }
    out
}

#[cfg(not(windows))]
fn candidates() -> Vec<PathBuf> {
    vec![PathBuf::from("libnvidia-ml.so.1")]
}

impl NvmlSensor {
    /// Opens NVML and the first device. `None` if the library or device is missing.
    pub fn open() -> Option<NvmlSensor> {
        for path in candidates() {
            // SAFETY: loading the vendor NVML library by absolute path (or the
            // Linux soname); its initialisers are trusted as part of the driver.
            if let Some(s) = unsafe { Self::open_from(&path) } {
                return Some(s);
            }
        }
        None
    }

    unsafe fn open_from(path: &PathBuf) -> Option<NvmlSensor> {
        let lib = Library::new(path).ok()?;
        let init: InitFn = *lib.get::<InitFn>(b"nvmlInit_v2\0").ok()?;
        let shutdown: ShutdownFn = *lib.get::<ShutdownFn>(b"nvmlShutdown\0").ok()?;
        let get_handle: HandleFn = *lib.get::<HandleFn>(b"nvmlDeviceGetHandleByIndex_v2\0").ok()?;
        let get_temp: TempFn = *lib.get::<TempFn>(b"nvmlDeviceGetTemperature\0").ok()?;
        if init() != 0 {
            return None;
        }
        let mut dev: Dev = std::ptr::null_mut();
        if get_handle(0, &mut dev) != 0 || dev.is_null() {
            shutdown();
            return None;
        }
        Some(NvmlSensor { shutdown, get_temp, dev, _lib: lib })
    }
}

impl Sensor for NvmlSensor {
    fn read(&mut self) -> Reading {
        let mut t: u32 = 0;
        // SAFETY: dev came from NVML and the library is alive (`_lib`).
        let rc = unsafe { (self.get_temp)(self.dev, NVML_TEMPERATURE_GPU, &mut t) };
        if rc != 0 {
            return Reading::Unavailable;
        }
        let c = t as f32;
        if valid_temp(c) {
            Reading::Temp(c)
        } else {
            Reading::Unavailable
        }
    }
}

impl Drop for NvmlSensor {
    fn drop(&mut self) {
        // SAFETY: runs before `_lib` is dropped, so the pointer is still mapped.
        unsafe {
            (self.shutdown)();
        }
    }
}
