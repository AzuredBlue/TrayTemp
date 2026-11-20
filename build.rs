use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=res/");
    println!("cargo:rerun-if-env-changed=RC");
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.contains("windows") && target.contains("msvc") {
        let manifest = Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap())
            .join("res")
            .join("traytemp.manifest");
        // Resolve the exe's own static imports from System32 only, never from its folder.
        println!("cargo:rustc-link-arg-bins=/DEPENDENTLOADFLAG:0x800");
        // Load these on first use instead of at every start: the driver install and Start with
        // Windows need the first four; COM, the strip's tooltips and the tray icons the others,
        // each only in some modes. The delay-load helper loads with the search order `main`
        // limits to System32.
        for dll in [
            "winhttp.dll", "wintrust.dll", "crypt32.dll", "secur32.dll",
            "ole32.dll", "oleaut32.dll", "comctl32.dll", "shell32.dll",
        ] {
            println!("cargo:rustc-link-arg-bins=/DELAYLOAD:{dll}");
        }
        println!("cargo:rustc-link-arg-bins=delayimp.lib");
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}", manifest.display());
        embed_icon();
    }
}

/// Compile `res/traytemp.rc` with rc.exe and link the result. Skips with a warning if rc.exe is missing.
fn embed_icon() {
    let Some(rc) = find_rc() else {
        println!("cargo:warning=rc.exe not found; the exe will have no icon (set RC or use a VS developer shell)");
        return;
    };
    let res_dir = Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("res");
    let out = Path::new(&std::env::var("OUT_DIR").unwrap()).join("traytemp.res");
    let status = Command::new(&rc)
        .arg("/nologo")
        .arg("/i")
        .arg(&res_dir)
        .arg("/fo")
        .arg(&out)
        .arg(res_dir.join("traytemp.rc"))
        .status();
    match status {
        Ok(s) if s.success() => println!("cargo:rustc-link-arg-bins={}", out.display()),
        _ => println!("cargo:warning=rc.exe failed on res/traytemp.rc; the exe will have no icon"),
    }
}

/// `RC` env var, then PATH, then the newest Windows SDK under Program Files (x86).
fn find_rc() -> Option<PathBuf> {
    if let Some(rc) = std::env::var_os("RC") {
        return Some(PathBuf::from(rc));
    }
    if Command::new("rc.exe").arg("/?").output().is_ok() {
        return Some(PathBuf::from("rc.exe"));
    }
    let bin = Path::new(&std::env::var_os("ProgramFiles(x86)")?)
        .join("Windows Kits")
        .join("10")
        .join("bin");
    let mut versions: Vec<PathBuf> = std::fs::read_dir(bin)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("10.")))
        .collect();
    // Version directories have the same shape (10.0.NNNNN.N), so a numeric compare is needed.
    versions.sort_by_key(|p| version_key(p));
    versions
        .into_iter()
        .rev()
        .map(|p| p.join("x64").join("rc.exe"))
        .find(|p| p.is_file())
}

fn version_key(p: &Path) -> Vec<u32> {
    p.file_name()
        .map(|n| n.to_string_lossy().split('.').filter_map(|s| s.parse().ok()).collect())
        .unwrap_or_default()
}
