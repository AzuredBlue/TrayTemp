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
fn query<T>(adapter: u32, kind: KMTQUERYADAPTERINFOTYPE, data: &mut T) -> bool {
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
        // A successful query reporting 0 means the GPU is in a low-power state: Asleep.
        match read_temp(self.adapter) {
            Some(t) if valid_temp(t) => Reading::Temp(t),
            Some(0.0) => Reading::Asleep,
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
    fn key(&self) -> (bool, u64, bool) {
        (self.discrete, self.dedicated, self.has_temp)
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

/// Picks the best non-software adapter: discrete first (HybridDiscrete, then larger
/// dedicated VRAM), then one with a valid temperature. Returns it with its vendor ID and
/// whether it had a valid temperature at startup.
fn pick() -> Option<(D3dkmtSensor, u32, bool)> {
    let mut best: Option<Candidate> = None;
    for info in enum_adapters() {
        let h = info.hAdapter;
        let mut ty = D3DKMT_ADAPTERTYPE::default();
        let flags = if query(h, KMTQAITYPE_ADAPTERTYPE, &mut ty) {
            // SAFETY: reading the plain-integer view of the union.
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
    best.map(|b| (D3dkmtSensor { adapter: b.adapter }, b.vendor, b.has_temp))
}

/// D3DKMT on the preferred adapter. If it has no valid temperature at startup and is
/// NVIDIA, NVML when available; otherwise D3DKMT is kept so it recovers when the GPU wakes.
pub fn open_gpu_sensor() -> Option<Box<dyn Sensor>> {
    let (sensor, vendor, has_temp) = pick()?;
    if !has_temp && vendor == VENDOR_NVIDIA {
        if let Some(n) = NvmlSensor::open() {
            return Some(Box::new(n));
        }
    }
    Some(Box::new(sensor))
}
