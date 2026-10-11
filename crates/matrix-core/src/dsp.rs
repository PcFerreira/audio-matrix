use crate::{EqBand, OutputConfig, db_to_gain};

#[derive(Clone, Default)]
struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    z: [f64; 2],
}

impl Biquad {
    fn set_peak(&mut self, band: EqBand, rate: u32) {
        let a = 10.0_f64.powf(band.gain_db as f64 / 40.0);
        let w = std::f64::consts::TAU * band.frequency as f64 / rate as f64;
        let alpha = w.sin() / (2.0 * band.q as f64);
        let a0 = 1.0 + alpha / a;
        self.b = [
            (1.0 + alpha * a) / a0,
            -2.0 * w.cos() / a0,
            (1.0 - alpha * a) / a0,
        ];
        self.a = [-2.0 * w.cos() / a0, (1.0 - alpha / a) / a0];
    }

    fn process(&mut self, input: f32) -> f32 {
        let x = input as f64;
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[1] * y;
        if !y.is_finite() {
            self.z = [0.0; 2];
            return 0.0;
        }
        y as f32
    }
}

struct ChannelState {
    filters: Vec<Biquad>,
    gain: f32,
    target_gain: f32,
    routes: Vec<f32>,
    target_routes: Vec<f32>,
}

#[derive(Clone, Debug, Default)]
pub struct BlockMetrics {
    pub peaks: Vec<f32>,
    pub clipped_samples: u64,
}

pub struct Processor {
    inputs: usize,
    sample_rate: u32,
    smoothing: f32,
    states: Vec<ChannelState>,
}

impl Processor {
    pub fn new(inputs: usize, rate: u32, config: OutputConfig) -> Result<Self, String> {
        config.validate(inputs, config.channels.len(), rate)?;
        let outputs = config.channels.len();
        let mut processor = Self {
            inputs,
            sample_rate: rate,
            smoothing: 1.0 - (-1.0 / (rate as f32 * 0.010)).exp(),
            states: (0..outputs)
                .map(|_| ChannelState {
                    filters: Vec::new(),
                    gain: 0.0,
                    target_gain: 0.0,
                    routes: vec![0.0; inputs],
                    target_routes: vec![0.0; inputs],
                })
                .collect(),
        };
        processor.apply_config(config)?;
        Ok(processor)
    }

    pub fn apply_config(&mut self, config: OutputConfig) -> Result<(), String> {
        config.validate(self.inputs, self.states.len(), self.sample_rate)?;
        for (state, channel) in self.states.iter_mut().zip(&config.channels) {
            state.target_gain = if config.mute || channel.mute {
                0.0
            } else {
                db_to_gain(config.gain_db + channel.gain_db)
                    * if channel.invert { -1.0 } else { 1.0 }
            };
            state.target_routes.clone_from(&channel.routes);
            let bands = channel.eq.iter().chain(&config.eq);
            state
                .filters
                .resize_with(channel.eq.len() + config.eq.len(), Biquad::default);
            for (filter, band) in state.filters.iter_mut().zip(bands) {
                filter.set_peak(*band, self.sample_rate);
            }
        }
        Ok(())
    }

    /// Process interleaved frames. Destination length must match exactly.
    pub fn process(&mut self, source: &[f32], destination: &mut [f32], metrics: &mut BlockMetrics) {
        assert_eq!(source.len() % self.inputs, 0);
        assert_eq!(
            destination.len(),
            source.len() / self.inputs * self.states.len()
        );
        metrics.peaks.resize(self.states.len(), 0.0);
        metrics.peaks.fill(0.0);
        metrics.clipped_samples = 0;
        for (src, dst) in source
            .chunks_exact(self.inputs)
            .zip(destination.chunks_exact_mut(self.states.len()))
        {
            for (index, (state, out)) in self.states.iter_mut().zip(dst).enumerate() {
                let mut mixed = 0.0;
                for ((route, target), sample) in
                    state.routes.iter_mut().zip(&state.target_routes).zip(src)
                {
                    *route += (*target - *route) * self.smoothing;
                    mixed += if sample.is_finite() {
                        sample * *route
                    } else {
                        0.0
                    };
                }
                for filter in &mut state.filters {
                    mixed = filter.process(mixed);
                }
                state.gain += (state.target_gain - state.gain) * self.smoothing;
                mixed *= state.gain;
                if !mixed.is_finite() {
                    mixed = 0.0;
                }
                metrics.peaks[index] = metrics.peaks[index].max(mixed.abs());
                metrics.clipped_samples += u64::from(mixed.abs() > 1.0);
                *out = mixed.clamp(-1.0, 1.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(inputs: usize, outputs: usize) -> OutputConfig {
        let mut result = OutputConfig::identity(inputs, outputs);
        result.gain_db = 0.0;
        result
    }

    #[test]
    fn swaps_channels_without_crosstalk() {
        let mut c = config(2, 2);
        c.channels[0].routes = vec![0.0, 1.0];
        c.channels[1].routes = vec![1.0, 0.0];
        let mut p = Processor::new(2, 48000, c).unwrap();
        let src: Vec<_> = (0..10000).flat_map(|_| [0.25, -0.5]).collect();
        let mut dst = vec![0.0; src.len()];
        p.process(&src, &mut dst, &mut BlockMetrics::default());
        assert!((dst[dst.len() - 2] + 0.5).abs() < 0.0001);
        assert!((dst[dst.len() - 1] - 0.25).abs() < 0.0001);
    }

    #[test]
    fn applies_device_and_channel_gain_and_mute_live() {
        let mut c = config(1, 2);
        c.gain_db = -6.0;
        c.channels[1].gain_db = -6.0;
        let mut p = Processor::new(1, 48000, c.clone()).unwrap();
        let src = vec![0.5; 12000];
        let mut dst = vec![0.0; 24000];
        let mut metrics = BlockMetrics::default();
        p.process(&src, &mut dst, &mut metrics);
        assert!((dst[23998] - 0.5 * db_to_gain(-6.0)).abs() < 0.0001);
        assert!((dst[23999] - 0.5 * db_to_gain(-12.0)).abs() < 0.0001);
        c.mute = true;
        p.apply_config(c).unwrap();
        p.process(&src, &mut dst, &mut metrics);
        assert!(dst[23998].abs() < 0.0001);
        assert!(dst[23999].abs() < 0.0001);
    }

    #[test]
    fn eq_boosts_its_center_frequency_and_is_channel_local() {
        let mut c = config(1, 2);
        c.channels[0].eq[2].gain_db = 6.0;
        let mut p = Processor::new(1, 48000, c).unwrap();
        let src: Vec<_> = (0..48000)
            .map(|n| 0.1 * (std::f32::consts::TAU * 1000.0 * n as f32 / 48000.0).sin())
            .collect();
        let mut dst = vec![0.0; 96000];
        p.process(&src, &mut dst, &mut BlockMetrics::default());
        let rms = |ch: usize| {
            (dst[48000..]
                .chunks_exact(2)
                .map(|f| f[ch].powi(2))
                .sum::<f32>()
                / 24000.0)
                .sqrt()
        };
        assert!((rms(0) / rms(1) - db_to_gain(6.0)).abs() < 0.01);
    }

    #[test]
    fn sums_routes_clamps_and_counts_clipping() {
        let mut c = config(2, 1);
        c.channels[0].routes = vec![1.0, 1.0];
        let mut p = Processor::new(2, 48000, c).unwrap();
        let mut dst = vec![0.0; 12000];
        let mut metrics = BlockMetrics::default();
        p.process(&vec![0.75; 24000], &mut dst, &mut metrics);
        assert_eq!(dst[11999], 1.0);
        assert!(metrics.clipped_samples > 1000);
        assert!(metrics.peaks[0] > 1.49);
    }

    #[test]
    fn rejects_invalid_config_and_sanitizes_nonfinite_input() {
        let mut c = config(1, 1);
        c.channels[0].eq[0].q = f32::NAN;
        assert!(Processor::new(1, 48000, c).is_err());
        let mut p = Processor::new(1, 48000, config(1, 1)).unwrap();
        let mut dst = [1.0; 3];
        p.process(
            &[f32::NAN, f32::INFINITY, f32::NEG_INFINITY],
            &mut dst,
            &mut BlockMetrics::default(),
        );
        assert_eq!(dst, [0.0; 3]);
    }

    #[test]
    fn devices_have_independent_state_and_polarity() {
        let src = vec![0.25; 16000];
        let mut a_config = config(1, 1);
        a_config.channels[0].invert = true;
        let mut b_config = config(1, 1);
        b_config.gain_db = -20.0;
        let mut a = Processor::new(1, 48000, a_config).unwrap();
        let mut b = Processor::new(1, 48000, b_config).unwrap();
        let mut a_out = vec![0.0; src.len()];
        let mut b_out = vec![0.0; src.len()];
        a.process(&src, &mut a_out, &mut BlockMetrics::default());
        b.process(&src, &mut b_out, &mut BlockMetrics::default());
        assert!((a_out[15999] + 0.25).abs() < 0.0001);
        assert!((b_out[15999] - 0.025).abs() < 0.0001);
    }

    #[test]
    fn device_eq_and_channel_eq_cascade() {
        let mut c = config(1, 1);
        c.eq[2].gain_db = 6.0;
        c.channels[0].eq[2].gain_db = 6.0;
        let mut p = Processor::new(1, 48000, c).unwrap();
        let src: Vec<_> = (0..48000)
            .map(|n| 0.1 * (std::f32::consts::TAU * 1000.0 * n as f32 / 48000.0).sin())
            .collect();
        let mut dst = vec![0.0; src.len()];
        p.process(&src, &mut dst, &mut BlockMetrics::default());
        let rms = (dst[24000..].iter().map(|v| v * v).sum::<f32>() / 24000.0).sqrt();
        assert!((rms / (0.1 / 2.0_f32.sqrt()) - db_to_gain(12.0)).abs() < 0.02);
    }
}
