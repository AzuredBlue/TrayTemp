//! Linux CPU/GPU temperatures from sysfs hwmon. Discovery runs once; each
//! sample is a single `pread` on a cached file descriptor.

use super::{Reading, Sensor};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

pub struct HwmonSensor {
    file: File,
    /// `device/power/runtime_status`, GPUs only.
    pm: Option<File>,
}

impl Sensor for HwmonSensor {
    fn read(&mut self) -> Reading {
        let mut buf = [0u8; 16];
        if let Some(pm) = &self.pm {
            if let Ok(n) = pm.read_at(&mut buf, 0) {
                if buf[..n].starts_with(b"suspended") {
                    return Reading::Asleep;
                }
            }
        }
        let n = match self.file.read_at(&mut buf, 0) {
            Ok(n) => n,
            Err(_) => return Reading::Unavailable,
        };
        match std::str::from_utf8(&buf[..n]).ok().and_then(|s| s.trim().parse::<i64>().ok()) {
            Some(milli) => Reading::Temp(milli as f32 / 1000.0),
            None => Reading::Unavailable,
        }
    }
}

const MAX_TEMP_INDEX: u32 = 16;

fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/// All `class/hwmon/*` directories with their `name`, sorted by directory name.
fn hwmons(sys: &Path) -> Vec<(String, PathBuf)> {
    let mut dirs: Vec<PathBuf> = match std::fs::read_dir(sys.join("class/hwmon")) {
        Ok(rd) => rd.filter_map(|e| e.ok().map(|e| e.path())).collect(),
        Err(_) => return Vec::new(),
    };
    dirs.sort();
    dirs.into_iter()
        .filter_map(|d| read_trimmed(&d.join("name")).map(|n| (n, d)))
        .collect()
}

fn input_path(dir: &Path, n: u32) -> PathBuf {
    dir.join(format!("temp{n}_input"))
}

/// The `tempN_input` whose `tempN_label` equals `label`.
fn labeled(dir: &Path, label: &str) -> Option<PathBuf> {
    (1..=MAX_TEMP_INDEX)
        .find(|n| read_trimmed(&dir.join(format!("temp{n}_label"))).as_deref() == Some(label))
        .map(|n| input_path(dir, n))
        .filter(|p| p.exists())
}

fn first_input(dir: &Path) -> Option<PathBuf> {
    (1..=MAX_TEMP_INDEX).map(|n| input_path(dir, n)).find(|p| p.exists())
}

fn sensor(input: &Path, pm: Option<File>) -> Option<HwmonSensor> {
    Some(HwmonSensor { file: File::open(input).ok()?, pm })
}

fn cpu_input(sys: &Path) -> Option<PathBuf> {
    let all = hwmons(sys);
    let pick = |name: &str, f: &dyn Fn(&Path) -> Option<PathBuf>| {
        all.iter().filter(|(n, _)| n == name).find_map(|(_, d)| f(d))
    };
    pick("coretemp", &|d| labeled(d, "Package id 0"))
        .or_else(|| pick("k10temp", &|d| labeled(d, "Tctl").or_else(|| labeled(d, "Tdie"))))
        .or_else(|| pick("zenpower", &|d| labeled(d, "Tdie").or_else(|| first_input(d))))
        .or_else(|| thermal_pkg(sys))
}

fn thermal_pkg(sys: &Path) -> Option<PathBuf> {
    let mut zones: Vec<PathBuf> = std::fs::read_dir(sys.join("class/thermal"))
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("thermal_zone")))
        .collect();
    zones.sort();
    zones
        .into_iter()
        .find(|z| read_trimmed(&z.join("type")).as_deref() == Some("x86_pkg_temp"))
        .map(|z| z.join("temp"))
        .filter(|p| p.exists())
}

pub fn find_cpu(sys: &Path) -> Option<HwmonSensor> {
    sensor(&cpu_input(sys)?, None)
}

pub fn find_gpu(sys: &Path) -> Option<HwmonSensor> {
    let all = hwmons(sys);
    // amdgpu: prefer a discrete card (boot_vga == 0) over the boot/integrated one.
    let amd: Vec<&PathBuf> = all.iter().filter(|(n, _)| n == "amdgpu").map(|(_, d)| d).collect();
    let discrete = |d: &&&PathBuf| read_trimmed(&d.join("device/boot_vga")).as_deref() == Some("0");
    let (dir, input) = amd
        .iter()
        .filter(|d| discrete(d))
        .chain(amd.iter().filter(|d| !discrete(d)))
        .find_map(|d| labeled(d, "edge").map(|i| ((**d).clone(), i)))
        .or_else(|| {
            all.iter()
                .filter(|(n, _)| n == "xe" || n == "i915")
                .find_map(|(_, d)| first_input(d).map(|i| (d.clone(), i)))
        })
        .or_else(|| {
            all.iter().filter(|(n, _)| n == "nouveau").find_map(|(_, d)| {
                Some(input_path(d, 1)).filter(|p| p.exists()).map(|i| (d.clone(), i))
            })
        })?;
    let pm = File::open(dir.join("device/power/runtime_status")).ok();
    sensor(&input, pm)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static NEXT: AtomicU32 = AtomicU32::new(0);

    struct Tmp(PathBuf);
    impl Tmp {
        fn new() -> Tmp {
            let p = std::env::temp_dir().join(format!(
                "traytemp-hwmon-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Tmp(p)
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn put(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn fake(root: &Path, hwmon: &str, name: &str, files: &[(&str, &str)]) {
        let dir = root.join("class/hwmon").join(hwmon);
        put(&dir.join("name"), &format!("{name}\n"));
        for (f, c) in files {
            put(&dir.join(f), c);
        }
    }

    #[test]
    fn cpu_prefers_coretemp_package() {
        let t = Tmp::new();
        fake(&t.0, "hwmon0", "coretemp", &[
            ("temp1_label", "Core 0\n"), ("temp1_input", "40000\n"),
            ("temp2_label", "Package id 0\n"), ("temp2_input", "55000\n"),
        ]);
        assert_eq!(find_cpu(&t.0).unwrap().read(), Reading::Temp(55.0));
    }

    #[test]
    fn cpu_k10temp_tctl() {
        let t = Tmp::new();
        fake(&t.0, "hwmon0", "k10temp", &[("temp1_label", "Tctl\n"), ("temp1_input", "61250\n")]);
        assert_eq!(find_cpu(&t.0).unwrap().read(), Reading::Temp(61.25));
    }

    #[test]
    fn gpu_amdgpu_prefers_discrete() {
        let t = Tmp::new();
        fake(&t.0, "hwmon1", "amdgpu", &[
            ("temp1_label", "edge\n"), ("temp1_input", "30000\n"), ("device/boot_vga", "1\n"),
        ]);
        fake(&t.0, "hwmon2", "amdgpu", &[
            ("temp1_label", "edge\n"), ("temp1_input", "66000\n"), ("device/boot_vga", "0\n"),
        ]);
        assert_eq!(find_gpu(&t.0).unwrap().read(), Reading::Temp(66.0));
    }

    #[test]
    fn gpu_asleep_skips_read() {
        let t = Tmp::new();
        fake(&t.0, "hwmon1", "amdgpu", &[
            ("temp1_label", "edge\n"), ("temp1_input", "48000\n"),
            ("device/power/runtime_status", "suspended\n"),
        ]);
        let mut s = find_gpu(&t.0).unwrap();
        assert_eq!(s.read(), Reading::Asleep);
        let status = t.0.join("class/hwmon/hwmon1/device/power/runtime_status");
        std::fs::write(&status, "active\n").unwrap();
        assert_eq!(s.read(), Reading::Temp(48.0));
    }

}
