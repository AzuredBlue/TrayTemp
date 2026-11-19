use std::sync::mpsc::Sender;

use ksni::menu::{CheckmarkItem, RadioGroup, RadioItem, StandardItem, SubMenu};
use ksni::{Category, Icon, MenuItem, ToolTip, Tray};

use crate::render::{render_value, Canvas, OsFont, TextStyle};
use crate::sensor::Kind;
use crate::settings::Settings;

pub const INTERVALS: [u32; 4] = [1, 2, 5, 10];

/// What a menu click asks the main loop to do.
pub enum Action {
    ToggleCpu,
    ToggleGpu,
    SetInterval(u32),
    ToggleAutostart,
    Exit,
}

/// The settings the menu needs to draw its checkmarks.
#[derive(Clone, Copy)]
pub struct Prefs {
    pub cpu: bool,
    pub gpu: bool,
    pub interval_s: u32,
    pub autostart: bool,
}

impl Prefs {
    pub fn from_settings(s: &Settings) -> Prefs {
        Prefs {
            cpu: s.cpu_enabled,
            gpu: s.gpu_enabled,
            interval_s: s.interval_s,
            autostart: s.autostart,
        }
    }
}

pub struct SensorTray {
    pub kind: Kind,
    /// The number at every size, drawn by the main loop: callbacks run on the tray's thread.
    pub icons: Vec<Icon>,
    pub tip_title: String,
    pub tip_desc: String,
    pub prefs: Prefs,
    pub tx: Sender<Action>,
}

impl SensorTray {
    pub fn set_tip(&mut self, tip: &str) {
        let (title, desc) = tip.split_once('\n').unwrap_or((tip, ""));
        self.tip_title.clear();
        self.tip_title.push_str(title);
        self.tip_desc.clear();
        self.tip_desc.push_str(desc);
    }
}

/// The number as pixmaps for the panel: 22 px is its icon size, 44 px the same at 2x scale.
pub fn icons(value: Option<i32>, mut font: Option<&mut OsFont>, style: TextStyle) -> Vec<Icon> {
    [22, 32, 44]
        .map(|size| icon(render_value(value, size, font.as_deref_mut(), style)))
        .into()
}

fn icon(canvas: Canvas) -> Icon {
    let mut data = Vec::with_capacity(canvas.px.len() * 4);
    for p in &canvas.px {
        data.extend_from_slice(&p.to_be_bytes());
    }
    Icon {
        width: canvas.w as i32,
        height: canvas.h as i32,
        data,
    }
}

fn send(tx: &Sender<Action>, action: Action) {
    // The receiver only disappears while the app is exiting.
    let _ = tx.send(action);
}

impl Tray for SensorTray {
    fn id(&self) -> String {
        match self.kind {
            Kind::Cpu => "traytemp-cpu",
            Kind::Gpu => "traytemp-gpu",
        }
        .into()
    }

    fn title(&self) -> String {
        format!("TrayTemp {}", self.kind.label())
    }

    fn category(&self) -> Category {
        Category::Hardware
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        self.icons.clone()
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: self.tip_title.clone(),
            description: self.tip_desc.clone(),
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let p = self.prefs;
        let only_one = p.cpu != p.gpu;
        let cpu_tx = self.tx.clone();
        let gpu_tx = self.tx.clone();
        let radio_tx = self.tx.clone();
        let autostart_tx = self.tx.clone();
        let exit_tx = self.tx.clone();
        vec![
            CheckmarkItem {
                label: "Show CPU temperature".into(),
                enabled: !(only_one && p.cpu),
                checked: p.cpu,
                activate: Box::new(move |_| send(&cpu_tx, Action::ToggleCpu)),
                ..Default::default()
            }
            .into(),
            CheckmarkItem {
                label: "Show GPU temperature".into(),
                enabled: !(only_one && p.gpu),
                checked: p.gpu,
                activate: Box::new(move |_| send(&gpu_tx, Action::ToggleGpu)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            SubMenu {
                label: "Refresh".into(),
                submenu: vec![RadioGroup {
                    selected: INTERVALS
                        .iter()
                        .position(|&i| i == p.interval_s)
                        .unwrap_or(1),
                    select: Box::new(move |_, i| {
                        if let Some(&s) = INTERVALS.get(i) {
                            send(&radio_tx, Action::SetInterval(s));
                        }
                    }),
                    options: INTERVALS
                        .iter()
                        .map(|s| RadioItem {
                            label: format!("{s} s"),
                            ..Default::default()
                        })
                        .collect(),
                }
                .into()],
                ..Default::default()
            }
            .into(),
            CheckmarkItem {
                label: "Start on login".into(),
                checked: p.autostart,
                activate: Box::new(move |_| send(&autostart_tx, Action::ToggleAutostart)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Exit".into(),
                activate: Box::new(move |_| send(&exit_tx, Action::Exit)),
                ..Default::default()
            }
            .into(),
        ]
    }
}
