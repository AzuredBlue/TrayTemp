mod autostart;
mod tray;

use std::path::Path;
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use ksni::blocking::{Handle, TrayMethods};

use crate::app::{App, Changes};
use crate::sensor::{hwmon, nvml::NvmlSensor, Kind, Sensor};
use crate::settings::Settings;
use tray::{Action, Prefs, SensorTray};

type Trays = [Option<Handle<SensorTray>>; 2];

const KINDS: [Kind; 2] = [Kind::Cpu, Kind::Gpu];

fn spawn_tray(app: &App, k: Kind, tx: &Sender<Action>) -> Result<Handle<SensorTray>, ksni::Error> {
    let mut tray = SensorTray {
        kind: k,
        value: app.shown(k),
        tip_title: String::new(),
        tip_desc: String::new(),
        prefs: Prefs::from_settings(&app.settings),
        tx: tx.clone(),
    };
    tray.set_tip(app.tooltip(k));
    tray.spawn()
}

fn apply_changes(app: &App, trays: &Trays, changes: Changes) {
    for k in KINDS {
        let i = k.index();
        if !(changes.value[i] || changes.tooltip[i]) {
            continue;
        }
        if let Some(h) = &trays[i] {
            h.update(|t| {
                t.value = app.shown(k);
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
fn sync_tray(app: &mut App, trays: &mut Trays, k: Kind, tx: &Sender<Action>) {
    let i = k.index();
    if app.enabled(k) && trays[i].is_none() {
        let changes = app.tick();
        match spawn_tray(app, k, tx) {
            Ok(h) => trays[i] = Some(h),
            Err(e) => {
                eprintln!("traytemp: cannot show the {} tray icon: {e}", k.label());
                match k {
                    Kind::Cpu => app.settings.cpu_enabled = false,
                    Kind::Gpu => app.settings.gpu_enabled = false,
                }
            }
        }
        apply_changes(app, trays, changes);
    } else if !app.enabled(k) {
        if let Some(h) = trays[i].take() {
            h.shutdown().wait();
        }
    }
}

fn save(settings: &Settings) {
    if let Err(e) = settings.save() {
        eprintln!("traytemp: cannot save settings: {e}");
    }
}

/// Applies a menu action. Returns false when the app should exit.
fn handle_action(app: &mut App, trays: &mut Trays, tx: &Sender<Action>, action: Action) -> bool {
    match action {
        Action::Exit => return false,
        Action::ToggleCpu => {
            if !app.settings.cpu_enabled || app.settings.gpu_enabled {
                app.settings.cpu_enabled = !app.settings.cpu_enabled;
                sync_tray(app, trays, Kind::Cpu, tx);
            }
        }
        Action::ToggleGpu => {
            if !app.settings.gpu_enabled || app.settings.cpu_enabled {
                app.settings.gpu_enabled = !app.settings.gpu_enabled;
                sync_tray(app, trays, Kind::Gpu, tx);
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

pub fn run() {
    let settings = Settings::load();
    let cpu = hwmon::find_cpu(Path::new("/sys")).map(|s| Box::new(s) as Box<dyn Sensor>);
    let gpu = hwmon::find_gpu(Path::new("/sys"))
        .map(|s| Box::new(s) as Box<dyn Sensor>)
        .or_else(|| NvmlSensor::open().map(|s| Box::new(s) as Box<dyn Sensor>));
    let mut app = App::new(settings, cpu, gpu);
    let (tx, rx) = channel();
    let mut trays: Trays = [None, None];

    app.tick();
    for k in KINDS {
        if app.enabled(k) {
            match spawn_tray(&app, k, &tx) {
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
                if !handle_action(&mut app, &mut trays, &tx, action) {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                last_tick = Instant::now();
                let changes = app.tick();
                if changes.any() {
                    apply_changes(&app, &trays, changes);
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    for h in trays.iter().flatten() {
        h.shutdown().wait();
    }
}
