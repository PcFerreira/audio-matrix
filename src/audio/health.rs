//! Fixed-size timing histogram: no sorting/allocating in a render callback.
pub struct Timing {
    pub mean_ms: f32,
    pub peak_ms: f32,
    pub budget_ms: f32,
    pub misses: u64,
    histogram: [u64; 16],
    samples: u64,
}
const BOUNDS: [f32; 16] = [
    0.01, 0.02, 0.04, 0.06, 0.08, 0.1, 0.15, 0.2, 0.3, 0.4, 0.5, 0.65, 0.8, 1.0, 1.5, 4.0,
];
impl Default for Timing {
    fn default() -> Self {
        Self {
            mean_ms: 0.0,
            peak_ms: 0.0,
            budget_ms: 0.0,
            misses: 0,
            histogram: [0; 16],
            samples: 0,
        }
    }
}
impl Timing {
    pub fn record(&mut self, elapsed_ms: f32, budget_ms: f32) {
        if !elapsed_ms.is_finite() || !budget_ms.is_finite() || budget_ms <= 0.0 {
            return;
        }
        self.mean_ms += (elapsed_ms - self.mean_ms) * 0.05;
        self.peak_ms = self.peak_ms.max(elapsed_ms);
        self.budget_ms = budget_ms;
        self.misses += u64::from(elapsed_ms > budget_ms);
        let fraction = elapsed_ms / budget_ms;
        let bucket = BOUNDS.iter().position(|v| fraction <= *v).unwrap_or(15);
        self.histogram[bucket] += 1;
        self.samples += 1;
        // A bounded recent histogram; retained lifetime misses/peak are separate.
        if self.samples >= 4096 {
            for count in &mut self.histogram {
                *count /= 2;
            }
            self.samples = self.histogram.iter().sum();
        }
    }
    pub fn p95_percent(&self) -> f32 {
        if self.samples == 0 {
            return 0.0;
        }
        let target = (self.samples as f64 * 0.95).ceil() as u64;
        let mut count = 0;
        for (index, value) in self.histogram.iter().enumerate() {
            count += value;
            if count >= target {
                return BOUNDS[index] * 100.0;
            }
        }
        400.0
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn timing_tracks_budget_without_unbounded_history() {
        let mut timing = Timing::default();
        for _ in 0..10000 {
            timing.record(0.5, 10.0);
        }
        assert!(timing.samples < 4096);
        assert_eq!(timing.p95_percent(), 6.0);
        timing.record(12.0, 10.0);
        assert_eq!(timing.misses, 1);
        assert_eq!(timing.peak_ms, 12.0);
        timing.record(f32::NAN, 0.0);
        assert!(timing.mean_ms.is_finite());
    }
}
