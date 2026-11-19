use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=res/");
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.contains("windows") && target.contains("msvc") {
        let manifest = Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap())
            .join("res")
            .join("traytemp.manifest");
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}", manifest.display());
    }
}
