use std::io::ErrorKind;
use std::path::{Path, PathBuf};

pub fn desktop_entry(exe: &Path) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=TrayTemp\nExec=\"{}\"\nX-GNOME-Autostart-enabled=true\n",
        exe.display()
    )
}

fn entry_path() -> std::io::Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => {
            let home = std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .ok_or_else(|| std::io::Error::new(ErrorKind::NotFound, "HOME is not set"))?;
            PathBuf::from(home).join(".config")
        }
    };
    Ok(base.join("autostart").join("traytemp.desktop"))
}

/// Writes or removes `<config dir>/autostart/traytemp.desktop`.
pub fn set_autostart(enabled: bool) -> std::io::Result<()> {
    let path = entry_path()?;
    if enabled {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, desktop_entry(&std::env::current_exe()?))
    } else {
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }
}
