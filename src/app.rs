use crate::sensor::{valid_temp, Kind, Reading, Sensor};
use crate::settings::Settings;
use crate::stats::{Stats, Summary};
use crate::tooltip::tooltip;

const KINDS: [Kind; 2] = [Kind::Cpu, Kind::Gpu];

/// What changed during a tick, indexed by `Kind::index()`.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Changes {
    pub value: [bool; 2],
    pub tooltip: [bool; 2],
}

impl Changes {
    pub fn any(&self) -> bool {
        self.value.iter().chain(self.tooltip.iter()).any(|&b| b)
    }
}

pub struct App {
    pub settings: Settings,
    sensors: [Option<Box<dyn Sensor>>; 2],
    stats: [Stats; 2],
    shown: [Option<i32>; 2],
    summaries: [Option<Summary>; 2],
    tips: [String; 2],
}

impl App {
    pub fn new(
        settings: Settings,
        cpu: Option<Box<dyn Sensor>>,
        gpu: Option<Box<dyn Sensor>>,
    ) -> App {
        App {
            settings,
            sensors: [cpu, gpu],
            stats: [Stats::default(), Stats::default()],
            shown: [None, None],
            summaries: [None, None],
            tips: [tooltip(Kind::Cpu, None), tooltip(Kind::Gpu, None)],
        }
    }

    /// Reads each enabled sensor once and reports what changed.
    pub fn tick(&mut self) -> Changes {
        let mut changes = Changes::default();
        for k in KINDS {
            let i = k.index();
            if !self.enabled(k) {
                continue;
            }
            let Some(sensor) = self.sensors[i].as_mut() else {
                continue;
            };
            let shown = match sensor.read() {
                Reading::Temp(t) if valid_temp(t) => {
                    self.stats[i].push(t);
                    Some(t.round() as i32)
                }
                Reading::Temp(_) | Reading::Unavailable => None,
                Reading::Asleep => self.shown[i],
            };
            if shown != self.shown[i] {
                self.shown[i] = shown;
                changes.value[i] = true;
            }
            let summary = self.stats[i].summary();
            if summary != self.summaries[i] {
                self.summaries[i] = summary;
                self.tips[i] = tooltip(k, summary);
                changes.tooltip[i] = true;
            }
        }
        changes
    }

    /// Value to draw; `None` means "--".
    pub fn shown(&self, k: Kind) -> Option<i32> {
        self.shown[k.index()]
    }

    pub fn tooltip(&self, k: Kind) -> &str {
        &self.tips[k.index()]
    }

    pub fn enabled(&self, k: Kind) -> bool {
        match k {
            Kind::Cpu => self.settings.cpu_enabled,
            Kind::Gpu => self.settings.gpu_enabled,
        }
    }

    /// Replaces the sensor, e.g. after the CPU driver is installed. Stats are kept; the shown
    /// value is cleared until the new sensor reads one.
    pub fn set_sensor(&mut self, k: Kind, s: Option<Box<dyn Sensor>>) {
        self.sensors[k.index()] = s;
        self.shown[k.index()] = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    struct Fake(Arc<Mutex<VecDeque<Reading>>>);

    impl Sensor for Fake {
        fn read(&mut self) -> Reading {
            self.0.lock().unwrap().pop_front().unwrap_or(Reading::Unavailable)
        }
    }

    fn fake(r: Vec<Reading>) -> (Box<dyn Sensor>, Arc<Mutex<VecDeque<Reading>>>) {
        let q = Arc::new(Mutex::new(VecDeque::from(r)));
        (Box::new(Fake(q.clone())), q)
    }

    fn app(cpu: Vec<Reading>, gpu: Vec<Reading>) -> App {
        App::new(Settings::default(), Some(fake(cpu).0), Some(fake(gpu).0))
    }

    #[test]
    fn same_value_no_redraw() {
        let mut a = app(vec![Reading::Temp(50.2), Reading::Temp(49.9)], vec![]);
        a.tick();
        let c = a.tick();
        assert!(!c.value[0]);
        assert!(!c.tooltip[0]);
    }

    #[test]
    fn invalid_and_unavailable_show_none_keep_stats() {
        let mut a = app(
            vec![
                Reading::Temp(60.0),
                Reading::Temp(0.0),
                Reading::Unavailable,
                Reading::Temp(40.0),
            ],
            vec![],
        );
        a.tick();
        a.tick();
        assert_eq!(a.shown(Kind::Cpu), None);
        a.tick();
        assert_eq!(a.shown(Kind::Cpu), None);
        a.tick();
        assert_eq!(a.tooltip(Kind::Cpu), "CPU: 40°C\nMin: 40°C\nMax: 60°C\nAvg: 50°C");
    }

    #[test]
    fn asleep_keeps_last() {
        let mut a = app(vec![], vec![Reading::Temp(45.0), Reading::Asleep]);
        a.tick();
        let c = a.tick();
        assert_eq!(a.shown(Kind::Gpu), Some(45));
        assert!(!c.value[1]);
    }

    #[test]
    fn disabled_not_read() {
        let (g, q) = fake(vec![Reading::Temp(45.0)]);
        let s = Settings {
            gpu_enabled: false,
            ..Settings::default()
        };
        let mut a = App::new(s, None, Some(g));
        a.tick();
        assert_eq!(q.lock().unwrap().len(), 1);
    }
}
