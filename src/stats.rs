//! Since-start temperature statistics. Platform-neutral.

/// Rounded integer values for display.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Summary {
    pub cur: i32,
    pub min: i32,
    pub max: i32,
    pub avg: i32,
}

/// Running statistics over every sample pushed since start.
#[derive(Default)]
pub struct Stats {
    cur: f32,
    min: f32,
    max: f32,
    sum: f64,
    count: u64,
}

impl Stats {
    /// Records one sample. Does not allocate.
    pub fn push(&mut self, celsius: f32) {
        if self.count == 0 {
            self.min = celsius;
            self.max = celsius;
        } else {
            self.min = self.min.min(celsius);
            self.max = self.max.max(celsius);
        }
        self.cur = celsius;
        self.sum += f64::from(celsius);
        self.count += 1;
    }

    /// Rounded summary, or `None` before the first sample.
    pub fn summary(&self) -> Option<Summary> {
        if self.count == 0 {
            return None;
        }
        Some(Summary {
            cur: self.cur.round() as i32,
            min: self.min.round() as i32,
            max: self.max.round() as i32,
            avg: (self.sum / self.count as f64).round() as i32,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_min_max_avg_current() {
        let mut s = Stats::default();
        for t in [50.0, 40.0, 60.0] {
            s.push(t);
        }
        assert_eq!(s.summary(), Some(Summary { cur: 60, min: 40, max: 60, avg: 50 }));
    }

    #[test]
    fn rounds_for_display_only() {
        let mut s = Stats::default();
        s.push(54.4);
        s.push(54.6);
        assert_eq!(s.summary(), Some(Summary { cur: 55, min: 54, max: 55, avg: 55 }));
    }

}
