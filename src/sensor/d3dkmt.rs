//! Windows GPU temperature through D3DKMT (the source Task Manager uses), with
//! NVML as an NVIDIA-only fallback.

use super::nvml::NvmlSensor;
use super::{valid_temp, Reading, Sensor};
use windows_sys::Wdk::Graphics::Direct3D::{
    D3DKMTCloseAdapter, D3DKMTEnumAdapters2, D3DKMTQueryAdapterInfo, D3DKMT_ADAPTERINFO,
    D3DKMT_ADAPTERTYPE, D3DKMT_ADAPTER_PERFDATA, D3DKMT_CLOSEADAPTER, D3DKMT_ENUMADAPTERS2,
    D3DKMT_QUERYADAPTERINFO, D3DKMT_QUERY_DEVICE_IDS, D3DKMT_SEGMENTSIZEINFO,
    KMTQAITYPE_ADAPTERPERFDATA, KMTQAITYPE_ADAPTERTYPE, KMTQAITYPE_GETSEGMENTSIZE,
    KMTQAITYPE_PHYSICALADAPTERDEVICEIDS, KMTQUERYADAPTERINFOTYPE,
};

const VENDOR_NVIDIA: u32 = 0x10DE;
// D3DKMT_ADAPTERTYPE bitfield positions.
const TYPE_SOFTWARE_DEVICE: u32 = 1 << 2;
const TYPE_HYBRID_DISCRETE: u32 = 1 << 4;

pub struct D3dkmtSensor {
    adapter: u32,
}

impl Drop for D3dkmtSensor {
    fn drop(&mut self) {
        close(self.adapter);
    }
}

fn close(adapter: u32) {
    let c = D3DKMT_CLOSEADAPTER { hAdapter: adapter };
    // SAFETY: `c` is a valid CLOSEADAPTER for a handle returned by EnumAdapters2.
    unsafe { D3DKMTCloseAdapter(&c) };
}

/// Queries a fixed-size struct. False when the NTSTATUS is not success.
fn query<T: Default>(adapter: u32, kind: KMTQUERYADAPTERINFOTYPE, data: &mut T) -> bool {
    let mut q = D3DKMT_QUERYADAPTERINFO {
        hAdapter: adapter,
        Type: kind,
        pPrivateDriverData: (data as *mut T).cast(),
        PrivateDriverDataSize: std::mem::size_of::<T>() as u32,
    };
    // SAFETY: pPrivateDriverData points at `data`, valid for PrivateDriverDataSize bytes.
    unsafe { D3DKMTQueryAdapterInfo(&mut q) == 0 }
}

fn read_temp(adapter: u32) -> Option<f32> {
    let mut perf = D3DKMT_ADAPTER_PERFDATA { PhysicalAdapterIndex: 0, ..Default::default() };
    if !query(adapter, KMTQAITYPE_ADAPTERPERFDATA, &mut perf) {
        return None;
    }
    Some(perf.Temperature as f32 / 10.0)
}

impl Sensor for D3dkmtSensor {
    fn read(&mut self) -> Reading {
        // Unverified: whether this query wakes a sleeping hybrid-laptop dGPU. If it does,
        // a non-waking power-state check (returning `Reading::Asleep`) belongs right here.
        match read_temp(self.adapter) {
            Some(t) if valid_temp(t) => Reading::Temp(t),
            _ => Reading::Unavailable,
        }
    }
}

struct Candidate {
    adapter: u32,
    has_temp: bool,
    discrete: bool,
    dedicated: u64,
    vendor: u32,
}

impl Candidate {
    fn key(&self) -> (bool, bool, u64) {
        (self.has_temp, self.discrete, self.dedicated)
    }
}

fn enum_adapters() -> Vec<D3DKMT_ADAPTERINFO> {
    let mut e = D3DKMT_ENUMADAPTERS2 { NumAdapters: 0, pAdapters: std::ptr::null_mut() };
    // SAFETY: first call with a null array only reports the count.
    if unsafe { D3DKMTEnumAdapters2(&mut e) } != 0 || e.NumAdapters == 0 {
        return Vec::new();
    }
    let mut list = vec![D3DKMT_ADAPTERINFO::default(); e.NumAdapters as usize];
    e.pAdapters = list.as_mut_ptr();
    // SAFETY: pAdapters has room for NumAdapters entries.
    if unsafe { D3DKMTEnumAdapters2(&mut e) } != 0 {
        return Vec::new();
    }
    list.truncate(e.NumAdapters as usize);
    list
}

/// Picks the best adapter. Returns the sensor if it has a valid temperature, otherwise
/// whether the best adapter is an NVIDIA one (for the NVML fallback).
fn pick() -> Result<D3dkmtSensor, bool> {
    let mut best: Option<Candidate> = None;
    for info in enum_adapters() {
        let h = info.hAdapter;
        let mut ty = D3DKMT_ADAPTERTYPE::default();
        // SAFETY: reading the plain-integer view of the union.
        let flags = if query(h, KMTQAITYPE_ADAPTERTYPE, &mut ty) {
            unsafe { ty.Anonymous.Value }
        } else {
            0
        };
        if flags & TYPE_SOFTWARE_DEVICE != 0 {
            close(h);
            continue;
        }
        let mut seg = D3DKMT_SEGMENTSIZEINFO::default();
        let dedicated = if query(h, KMTQAITYPE_GETSEGMENTSIZE, &mut seg) {
            seg.DedicatedVideoMemorySize
        } else {
            0
        };
        let mut ids = D3DKMT_QUERY_DEVICE_IDS::default();
        let vendor = if query(h, KMTQAITYPE_PHYSICALADAPTERDEVICEIDS, &mut ids) {
            ids.DeviceIds.VendorID
        } else {
            0
        };
        let c = Candidate {
            adapter: h,
            has_temp: read_temp(h).is_some_and(valid_temp),
            discrete: flags & TYPE_HYBRID_DISCRETE != 0,
            dedicated,
            vendor,
        };
        // Strictly greater: ties keep the first adapter enumerated.
        match &best {
            Some(b) if c.key() <= b.key() => close(h),
            _ => {
                if let Some(old) = best.replace(c) {
                    close(old.adapter);
                }
            }
        }
    }
    match best {
        Some(b) if b.has_temp => Ok(D3dkmtSensor { adapter: b.adapter }),
        Some(b) => {
            close(b.adapter);
            Err(b.vendor == VENDOR_NVIDIA)
        }
        None => Err(false),
    }
}

/// D3DKMT if the chosen adapter returns a valid temperature, else NVML for NVIDIA, else None.
pub fn open_gpu_sensor() -> Option<Box<dyn Sensor>> {
    match pick() {
        Ok(s) => Some(Box::new(s)),
        Err(true) => NvmlSensor::open().map(|s| Box::new(s) as Box<dyn Sensor>),
        Err(false) => None,
    }
}
