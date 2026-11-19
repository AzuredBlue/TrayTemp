//! Temperature sources. Platform-neutral core.

pub mod cpu_decode;
pub mod nvml;

#[cfg(target_os = "linux")]
pub mod hwmon;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Cpu,
    Gpu,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Cpu => "CPU",
            Kind::Gpu => "GPU",
        }
    }

    pub fn index(self) -> usize {
        match self {
            Kind::Cpu => 0,
            Kind::Gpu => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Reading {
    Temp(f32),
    Asleep,
    Unavailable,
}

pub trait Sensor: Send {
    fn read(&mut self) -> Reading;
}

/// True for a finite reading inside the plausible range.
pub fn valid_temp(c: f32) -> bool {
    c.is_finite() && (1.0..=150.0).contains(&c)
}
