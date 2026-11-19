//! Tooltip text for a sensor. Platform-neutral core.

use crate::sensor::Kind;
use crate::stats::Summary;

pub fn tooltip(kind: Kind, s: Option<Summary>) -> String {
    let label = kind.label();
    match s {
        Some(s) => format!(
            "{label}: {}°C\nMin: {}°C\nMax: {}°C\nAvg: {}°C",
            s.cur, s.min, s.max, s.avg
        ),
        None => format!("{label}: N/A"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full() {
        let s = Summary { cur: 54, min: 41, max: 78, avg: 52 };
        assert_eq!(tooltip(Kind::Cpu, Some(s)), "CPU: 54°C\nMin: 41°C\nMax: 78°C\nAvg: 52°C");
    }

    #[test]
    fn none() {
        assert_eq!(tooltip(Kind::Gpu, None), "GPU: N/A");
    }

}
