use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayMode {
    TrayIcons,
    TaskbarText,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub display_mode: DisplayMode,
    pub cpu_enabled: bool,
    pub gpu_enabled: bool,
    pub interval_s: u32,
    pub autostart: bool,
    pub all_taskbars: bool,
    pub pawnio_installed_by_traytemp: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            display_mode: DisplayMode::TrayIcons,
            cpu_enabled: true,
            gpu_enabled: true,
            interval_s: 2,
            autostart: false,
            all_taskbars: true,
            pawnio_installed_by_traytemp: false,
        }
    }
}

fn non_empty(v: Option<OsString>) -> Option<OsString> {
    v.filter(|s| !s.is_empty())
}

fn path_for(
    is_windows: bool,
    appdata: Option<OsString>,
    xdg_config_home: Option<OsString>,
    home: Option<OsString>,
) -> Option<PathBuf> {
    if is_windows {
        let base = non_empty(appdata)?;
        return Some(PathBuf::from(base).join("TrayTemp").join("settings.ini"));
    }
    let base = match non_empty(xdg_config_home) {
        Some(x) => PathBuf::from(x),
        None => PathBuf::from(non_empty(home)?).join(".config"),
    };
    Some(base.join("traytemp").join("settings.ini"))
}

fn parse_bool(v: &str) -> Option<bool> {
    match v {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

impl Settings {
    pub fn parse(text: &str) -> Settings {
        let mut s = Settings::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let (key, value) = (key.trim(), value.trim());
            match key {
                "display_mode" => match value {
                    "tray_icons" => s.display_mode = DisplayMode::TrayIcons,
                    "taskbar_text" => s.display_mode = DisplayMode::TaskbarText,
                    _ => s.display_mode = DisplayMode::TrayIcons,
                },
                "cpu_enabled" => s.cpu_enabled = parse_bool(value).unwrap_or(true),
                "gpu_enabled" => s.gpu_enabled = parse_bool(value).unwrap_or(true),
                "interval_s" => {
                    s.interval_s = value
                        .parse::<u32>()
                        .ok()
                        .filter(|n| matches!(n, 1 | 2 | 5 | 10))
                        .unwrap_or(2)
                }
                "autostart" => s.autostart = parse_bool(value).unwrap_or(false),
                "all_taskbars" => s.all_taskbars = parse_bool(value).unwrap_or(true),
                "pawnio_installed_by_traytemp" => {
                    s.pawnio_installed_by_traytemp = parse_bool(value).unwrap_or(false)
                }
                _ => {}
            }
        }
        if !s.cpu_enabled && !s.gpu_enabled {
            s.cpu_enabled = true;
            s.gpu_enabled = true;
        }
        s
    }

    pub fn to_ini(&self) -> String {
        let mode = match self.display_mode {
            DisplayMode::TrayIcons => "tray_icons",
            DisplayMode::TaskbarText => "taskbar_text",
        };
        format!(
            "display_mode={}
cpu_enabled={}
gpu_enabled={}
interval_s={}
autostart={}
all_taskbars={}
pawnio_installed_by_traytemp={}
",
            mode,
            self.cpu_enabled,
            self.gpu_enabled,
            self.interval_s,
            self.autostart,
            self.all_taskbars,
            self.pawnio_installed_by_traytemp
        )
    }

    pub fn path() -> Option<PathBuf> {
        path_for(
            std::env::consts::OS == "windows",
            std::env::var_os("APPDATA"),
            std::env::var_os("XDG_CONFIG_HOME"),
            std::env::var_os("HOME"),
        )
    }

    /// Missing or unreadable file yields the defaults.
    pub fn load() -> Settings {
        match Settings::path() {
            Some(p) => Settings::load_from(&p),
            None => Settings::default(),
        }
    }

    /// Creates the parent directory; a no-op when no config path is available.
    pub fn save(&self) -> std::io::Result<()> {
        match Settings::path() {
            Some(p) => self.save_to(&p),
            None => Ok(()),
        }
    }

    fn load_from(path: &Path) -> Settings {
        match std::fs::read_to_string(path) {
            Ok(text) => Settings::parse(&text),
            Err(_) => Settings::default(),
        }
    }

    fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, self.to_ini())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let s = Settings {
            display_mode: DisplayMode::TaskbarText,
            cpu_enabled: false,
            gpu_enabled: true,
            interval_s: 5,
            autostart: true,
            all_taskbars: false,
            pawnio_installed_by_traytemp: true,
        };
        assert_eq!(Settings::parse(&s.to_ini()), s);
    }

    #[test]
    fn bad_values_fall_back_per_key() {
        let s = Settings::parse("interval_s=7\ncpu_enabled=maybe\ngpu_enabled=false\ngarbage line\n=x\n");
        assert_eq!(s.interval_s, 2);
        assert!(s.cpu_enabled);
        assert!(!s.gpu_enabled);
    }

    #[test]
    fn both_disabled_reenables_cpu_and_gpu() {
        let s = Settings::parse("cpu_enabled=false\ngpu_enabled=false");
        assert!(s.cpu_enabled && s.gpu_enabled);
    }

    #[test]
    fn tolerates_whitespace_crlf_and_comments() {
        let s = Settings::parse("# c\r\n  interval_s = 10 \r\n");
        assert_eq!(s.interval_s, 10);
    }

}
