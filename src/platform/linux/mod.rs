mod autostart;
mod font;
mod tray;

use std::path::Path;
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use ksni::blocking::{Handle, TrayMethods};

use crate::app::{App, Changes};
use crate::sensor::{hwmon, nvml::NvmlSensor, Kind, Sensor};
use crate::settings::Settings;
use font::DesktopText;
use tray::{Action, Prefs, SensorTray};

type Trays = [Option<Handle<SensorTray>>; 2];

const KINDS: [Kind; 2] = [Kind::Cpu, Kind::Gpu];

fn spawn_tray(
    app: &App,
    text: &mut DesktopText,
    k: Kind,
    tx: &Sender<Action>,
) -> Result<Handle<SensorTray>, ksni::Error> {
    let mut tray = SensorTray {
        kind: k,
        icons: tray::icons(app.shown(k), text.font.as_mut(), text.style),
        tip_title: String::new(),
        tip_desc: String::new(),
        prefs: Prefs::from_settings(&app.settings),
        tx: tx.clone(),
    };
    tray.set_tip(app.tooltip(k));
    tray.spawn()
}

fn apply_changes(app: &App, trays: &Trays, text: &mut DesktopText, changes: Changes) {
    for k in KINDS {
        let i = k.index();
        if !(changes.value[i] || changes.tooltip[i]) {
            continue;
        }
        if let Some(h) = &trays[i] {
            let icons =
                changes.value[i].then(|| tray::icons(app.shown(k), text.font.as_mut(), text.style));
            h.update(|t| {
                if let Some(icons) = icons {
                    t.icons = icons;
                }
                t.set_tip(app.tooltip(k));
            });
        }
    }
}

fn sync_prefs(app: &App, trays: &Trays) {
    let prefs = Prefs::from_settings(&app.settings);
    for h in trays.iter().flatten() {
        h.update(|t| t.prefs = prefs);
    }
}

/// Starts or stops the tray of `k` to match its enabled setting.
fn sync_tray(
    app: &mut App,
    trays: &mut Trays,
    text: &mut DesktopText,
    k: Kind,
    tx: &Sender<Action>,
) {
    let i = k.index();
    if app.enabled(k) && trays[i].is_none() {
        app.set_sensor(k, open_sensor(k));
        let changes = app.tick();
        match spawn_tray(app, text, k, tx) {
            Ok(h) => trays[i] = Some(h),
            Err(e) => {
                eprintln!("traytemp: cannot show the {} tray icon: {e}", k.label());
                match k {
                    Kind::Cpu => app.settings.cpu_enabled = false,
                    Kind::Gpu => app.settings.gpu_enabled = false,
                }
                app.set_sensor(k, None);
            }
        }
        apply_changes(app, trays, text, changes);
    } else if !app.enabled(k) {
        if let Some(h) = trays[i].take() {
            h.shutdown().wait();
        }
        app.set_sensor(k, None);
    }
}

fn save(settings: &Settings) {
    if let Err(e) = settings.save() {
        eprintln!("traytemp: cannot save settings: {e}");
    }
}

/// Applies a menu action. Returns false when the app should exit.
fn handle_action(
    app: &mut App,
    trays: &mut Trays,
    text: &mut DesktopText,
    tx: &Sender<Action>,
    action: Action,
) -> bool {
    match action {
        Action::Exit => return false,
        Action::ToggleCpu => {
            if !app.settings.cpu_enabled || app.settings.gpu_enabled {
                app.settings.cpu_enabled = !app.settings.cpu_enabled;
                sync_tray(app, trays, text, Kind::Cpu, tx);
            }
        }
        Action::ToggleGpu => {
            if !app.settings.gpu_enabled || app.settings.cpu_enabled {
                app.settings.gpu_enabled = !app.settings.gpu_enabled;
                sync_tray(app, trays, text, Kind::Gpu, tx);
            }
        }
        Action::SetInterval(s) => app.settings.interval_s = s,
        Action::ToggleAutostart => {
            let want = !app.settings.autostart;
            match autostart::set_autostart(want) {
                Ok(()) => app.settings.autostart = want,
                Err(e) => eprintln!("traytemp: cannot change autostart: {e}"),
            }
        }
    }
    save(&app.settings);
    sync_prefs(app, trays);
    true
}

/// Opens the sensor of `k`; it is dropped again when the kind is disabled.
fn open_sensor(k: Kind) -> Option<Box<dyn Sensor>> {
    let sys = Path::new("/sys");
    match k {
        Kind::Cpu => hwmon::find_cpu(sys).map(|s| Box::new(s) as Box<dyn Sensor>),
        Kind::Gpu => hwmon::find_gpu(sys)
            .map(|s| Box::new(s) as Box<dyn Sensor>)
            .or_else(|| NvmlSensor::open().map(|s| Box::new(s) as Box<dyn Sensor>)),
    }
}

pub fn run() {
    let settings = Settings::load();
    let cpu = settings.cpu_enabled.then(|| open_sensor(Kind::Cpu)).flatten();
    let gpu = settings.gpu_enabled.then(|| open_sensor(Kind::Gpu)).flatten();
    let mut app = App::new(settings, cpu, gpu);
    let mut text = DesktopText::load();
    let (tx, rx) = channel();
    let mut trays: Trays = [None, None];

    app.tick();
    for k in KINDS {
        if app.enabled(k) {
            match spawn_tray(&app, &mut text, k, &tx) {
                Ok(h) => trays[k.index()] = Some(h),
                Err(e) => {
                    eprintln!("traytemp: cannot create the {} tray icon: {e}", k.label());
                    eprintln!(
                        "traytemp: a StatusNotifierWatcher (e.g. KDE Plasma) on the session bus is required"
                    );
                    std::process::exit(1);
                }
            }
        }
    }

    let mut last_tick = Instant::now();
    loop {
        let interval = Duration::from_secs(u64::from(app.settings.interval_s));
        let wait = (last_tick + interval).saturating_duration_since(Instant::now());
        match rx.recv_timeout(wait) {
            Ok(action) => {
                if !handle_action(&mut app, &mut trays, &mut text, &tx, action) {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                last_tick = Instant::now();
                let mut changes = app.tick();
                // A new desktop font or text colour redraws both numbers.
                if text.refresh() {
                    changes.value = [true, true];
                }
                if changes.any() {
                    apply_changes(&app, &trays, &mut text, changes);
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    for h in trays.iter().flatten() {
        h.shutdown().wait();
    }
}
