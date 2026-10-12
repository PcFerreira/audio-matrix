//! Bounded polyphase windowed-sinc interpolation for small asynchronous drift.
//! Not a general arbitrary-ratio sample-rate converter (Windows handles nominal rates).
pub const TAPS: usize = 32;
pub const LEFT: i32 = 15;
pub const RIGHT: usize = 16;
const PHASES: usize = 256;

pub struct SincKernel {
    weights: Vec<[f32; TAPS]>,
}
impl Default for SincKernel {
    fn default() -> Self {
        let weights = (0..=PHASES)
            .map(|phase| {
                let fraction = phase as f64 / PHASES as f64;
                let mut weights = [0.0; TAPS];
                let mut sum = 0.0;
                for (i, weight) in weights.iter_mut().enumerate() {
                    let distance = i as f64 - LEFT as f64 - fraction;
                    let normalized = distance / 16.0;
                    let window = if normalized.abs() < 1.0 {
                        0.42 + 0.5 * (std::f64::consts::PI * normalized).cos()
                            + 0.08 * (std::f64::consts::TAU * normalized).cos()
                    } else {
                        0.0
                    };
                    let x = std::f64::consts::PI * distance * 0.97;
                    let value = if x.abs() < 1e-10 {
                        0.97
                    } else {
                        x.sin() / (std::f64::consts::PI * distance)
                    } * window;
                    *weight = value as f32;
                    sum += value;
                }
                for weight in &mut weights {
                    *weight /= sum as f32;
                }
                weights
            })
            .collect();
        Self { weights }
    }
}
impl SincKernel {
    pub fn sample(&self, fraction: f64, mut source: impl FnMut(i32) -> f32) -> f32 {
        let phase = fraction.clamp(0.0, 1.0) * PHASES as f64;
        let index = (phase.floor() as usize).min(PHASES - 1);
        let mix = (phase - index as f64) as f32;
        let mut result = 0.0;
        for i in 0..TAPS {
            let a = self.weights[index][i];
            let b = self.weights[index + 1][i];
            result += source(i as i32 - LEFT) * (a + (b - a) * mix);
        }
        result
    }
    pub fn weights(&self, fraction: f64) -> [f32; TAPS] {
        let mut result = [0.0; TAPS];
        let phase = fraction.clamp(0.0, 1.0) * PHASES as f64;
        let index = (phase.floor() as usize).min(PHASES - 1);
        let mix = (phase - index as f64) as f32;
        for (i, weight) in result.iter_mut().enumerate() {
            let a = self.weights[index][i];
            *weight = a + (self.weights[index + 1][i] - a) * mix;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn normalized_kernel_preserves_dc_and_beats_linear_at_high_frequency() {
        let kernel = SincKernel::default();
        for fraction in [0.0, 0.1, 0.37, 0.9, 1.0] {
            assert!((kernel.sample(fraction, |_| 0.25) - 0.25).abs() < 1e-6);
        }
        let w = std::f64::consts::TAU * 18000.0 / 48000.0;
        let fraction = 0.37;
        let exact = (w * fraction).cos() as f32;
        let sinc = kernel.sample(fraction, |i| (w * i as f64).cos() as f32);
        let linear = 1.0 + (w.cos() as f32 - 1.0) * fraction as f32;
        assert!((sinc - exact).abs() < (linear - exact).abs() * 0.1);
        assert!((sinc - exact).abs() < 0.01);
    }
}
