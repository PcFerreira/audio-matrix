use crate::limiter::Limiter;
use crate::{EqBand, GlobalVolume, OutputConfig, db_to_gain};

#[derive(Clone, Default)]
struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    z: [f64; 2],
}

impl Biquad {
    fn configure(&mut self, band: EqBand, rate: u32) {
        (self.b, self.a) = crate::eq::coefficients(band, rate);
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

/// Crossfade stable filter banks instead of interpolating recursive poles.
/// Rapid edits coalesce at the end of each 20-ms transition; fader edits do not
/// rebuild EQ state. This changes response, not the sample timeline/delay.
struct EqBank {
    current: Vec<Biquad>,
    next: Option<Vec<Biquad>>,
    pending: Option<Vec<EqBand>>,
    bands: Vec<EqBand>,
    remaining: usize,
    fade: usize,
    rate: u32,
}
impl EqBank {
    fn new(rate: u32) -> Self {
        Self {
            current: Vec::new(),
            next: None,
            pending: None,
            bands: Vec::new(),
            remaining: 0,
            fade: (rate / 50).max(1) as usize,
            rate,
        }
    }
    fn configure(&mut self, bands: Vec<EqBand>) {
        if self.pending.as_ref().unwrap_or(&self.bands) == &bands {
            return;
        }
        if self.remaining > 0 {
            self.pending = Some(bands);
            return;
        }
        let mut next = self.current.clone();
        next.resize_with(bands.len(), Biquad::default);
        for (filter, band) in next.iter_mut().zip(&bands) {
            filter.configure(*band, self.rate);
        }
        if self.bands.is_empty() {
            self.current = next;
        } else {
            self.next = Some(next);
            self.remaining = self.fade;
        }
        self.bands = bands;
    }
    fn process(&mut self, input: f32) -> f32 {
        if self.remaining == 0
            && let Some(bands) = self.pending.take()
        {
            self.configure(bands);
        }
        let mut old = input;
        for filter in &mut self.current {
            old = filter.process(old);
        }
        let Some(next) = &mut self.next else {
            return old;
        };
        let mut new = input;
        for filter in next {
            new = filter.process(new);
        }
        let mix = 1.0 - self.remaining as f32 / self.fade as f32;
        self.remaining = self.remaining.saturating_sub(1);
        if self.remaining == 0 {
            self.current = self.next.take().unwrap();
        }
        old + (new - old) * mix
    }
}

struct ChannelState {
    filters: EqBank,
    gain: f32,
    target_gain: f32,
    routes: Vec<f32>,
    target_routes: Vec<f32>,
}

#[derive(Clone, Debug, Default)]
pub struct BlockMetrics {
    pub true_peak: f32,
    pub peaks: Vec<f32>,
    pub clipped_samples: u64,
    pub input_peaks: Vec<f32>,
    pub overload_samples: u64,
    pub limited_frames: u64,
    pub gain_reduction_db: f32,
}

pub struct Processor {
    inputs: usize,
    sample_rate: u32,
    smoothing: f32,
    states: Vec<ChannelState>,
    global_gain: f32,
    global_target: f32,
    limiter: Limiter,
}

impl Processor {
    pub fn new(inputs: usize, rate: u32, config: OutputConfig) -> Result<Self, String> {
        config.validate(inputs, config.channels.len(), rate)?;
        let outputs = config.channels.len();
        let mut processor = Self {
            inputs,
            sample_rate: rate,
            smoothing: 1.0 - (-1.0 / (rate as f32 * 0.010)).exp(),
            global_gain: 1.0,
            global_target: 1.0,
            limiter: Limiter::new(outputs, rate, config.limiter)?,
            states: (0..outputs)
                .map(|_| ChannelState {
                    filters: EqBank::new(rate),
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

    pub fn with_global_volume(
        inputs: usize,
        rate: u32,
        config: OutputConfig,
        volume: GlobalVolume,
    ) -> Result<Self, String> {
        volume.validate()?;
        let mut processor = Self::new(inputs, rate, config)?;
        processor.global_gain = volume.gain();
        processor.global_target = volume.gain();
        Ok(processor)
    }

    pub fn set_global_volume(&mut self, volume: GlobalVolume) -> Result<(), String> {
        volume.validate()?;
        self.global_target = volume.gain();
        Ok(())
    }

    pub fn apply_config(&mut self, config: OutputConfig) -> Result<(), String> {
        config.validate(self.inputs, self.states.len(), self.sample_rate)?;
        self.limiter.configure(config.limiter)?;
        for (state, channel) in self.states.iter_mut().zip(&config.channels) {
            state.target_gain = if config.mute || channel.mute {
                0.0
            } else {
                db_to_gain(config.gain_db + channel.gain_db)
                    * if channel.invert { -1.0 } else { 1.0 }
            };
            state.target_routes.clone_from(&channel.routes);
            state
                .filters
                .configure(channel.eq.iter().chain(&config.eq).copied().collect());
        }
        Ok(())
    }

    pub fn latency_frames(&self) -> usize {
        self.limiter.delay_frames()
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
        metrics.input_peaks.resize(self.states.len(), 0.0);
        metrics.input_peaks.fill(0.0);
        metrics.clipped_samples = 0;
        metrics.overload_samples = 0;
        metrics.limited_frames = 0;
        metrics.gain_reduction_db = 0.0;
        metrics.true_peak = 0.0;
        for (src, dst) in source
            .chunks_exact(self.inputs)
            .zip(destination.chunks_exact_mut(self.states.len()))
        {
            // Update once per frame so channels share exactly the same envelope.
            self.global_gain += (self.global_target - self.global_gain) * self.smoothing;
            for (index, (state, out)) in self.states.iter_mut().zip(dst.iter_mut()).enumerate() {
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
                mixed = state.filters.process(mixed);
                state.gain += (state.target_gain - state.gain) * self.smoothing;
                // Attenuate BEFORE meters and clamping: a global fade must not
                // merely make already-clipped audio quieter.
                mixed *= state.gain * self.global_gain;
                if !mixed.is_finite() {
                    mixed = 0.0;
                }
                metrics.input_peaks[index] = metrics.input_peaks[index].max(mixed.abs());
                metrics.overload_samples += u64::from(mixed.abs() > 1.0);
                *out = mixed;
            }
            let limiting = self.limiter.process_frame(dst);
            metrics.clipped_samples += limiting.clipped_samples;
            metrics.limited_frames += u64::from(limiting.limited);
            metrics.gain_reduction_db = metrics.gain_reduction_db.max(limiting.gain_reduction_db);
            metrics.true_peak = metrics.true_peak.max(limiting.true_peak);
            for (index, sample) in dst.iter().enumerate() {
                metrics.peaks[index] = metrics.peaks[index].max(sample.abs());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rapid_filter_edits_are_crossfaded_finite_and_do_not_change_latency() {
        let mut config = config(1, 1);
        let mut processor = Processor::new(1, 48000, config.clone()).unwrap();
        let delay = processor.latency_frames();
        let mut metrics = BlockMetrics::default();
        let mut destination = [0.0; 120];
        for i in 0..120 {
            config.eq[2].gain_db = if i % 2 == 0 { 18.0 } else { -18.0 };
            config.eq[2].frequency = if i % 3 == 0 { 200.0 } else { 16000.0 };
            config.eq[2].q = if i % 2 == 0 { 10.0 } else { 0.1 };
            config.eq[2].kind = if i % 5 == 0 {
                crate::FilterType::HighPass
            } else {
                crate::FilterType::Bell
            };
            processor.apply_config(config.clone()).unwrap();
            processor.process(&[0.1; 120], &mut destination, &mut metrics);
            assert!(destination.iter().all(|x| x.is_finite() && x.abs() <= 1.0));
            assert_eq!(processor.latency_frames(), delay);
        }
        config.eq[2].enabled = false;
        processor.apply_config(config).unwrap();
        for _ in 0..30 {
            processor.process(&[0.1; 120], &mut destination, &mut metrics);
        }
        assert!((destination[119] - 0.1).abs() < 0.002);
    }

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
                .as_chunks::<2>()
                .0
                .iter()
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
        c.limiter.enabled = false;
        c.channels[0].routes = vec![1.0, 1.0];
        let mut p = Processor::new(2, 48000, c).unwrap();
        let mut dst = vec![0.0; 12000];
        let mut metrics = BlockMetrics::default();
        p.process(&vec![0.75; 24000], &mut dst, &mut metrics);
        assert_eq!(dst[11999], 1.0);
        assert!(metrics.clipped_samples > 1000);
        assert!(metrics.input_peaks[0] > 1.49);
        assert_eq!(metrics.peaks[0], 1.0);
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
    fn global_volume_scales_all_channels_without_changing_device_balance() {
        let mut c = config(1, 2);
        c.channels[1].gain_db = -6.0;
        let original = c.clone();
        let volume = GlobalVolume {
            gain_db: -12.0,
            mute: false,
        };
        let mut p = Processor::with_global_volume(1, 48000, c, volume).unwrap();
        let mut dst = vec![0.0; 24000];
        p.process(&vec![0.5; 12000], &mut dst, &mut BlockMetrics::default());
        assert!((dst[23998] - 0.5 * db_to_gain(-12.0)).abs() < 0.0001);
        assert!((dst[23999] / dst[23998] - db_to_gain(-6.0)).abs() < 0.0001);
        p.set_global_volume(GlobalVolume::default()).unwrap();
        p.process(&vec![0.5; 12000], &mut dst, &mut BlockMetrics::default());
        assert!((dst[23998] - 0.5).abs() < 0.0001);
        assert_eq!(original.channels[1].gain_db, -6.0);
    }

    #[test]
    fn global_attenuation_prevents_clipping_and_global_mute_is_smoothed() {
        let mut c = config(2, 1);
        c.channels[0].routes = vec![1.0, 1.0];
        let mut p = Processor::with_global_volume(
            2,
            48000,
            c,
            GlobalVolume {
                gain_db: -6.0,
                mute: false,
            },
        )
        .unwrap();
        let src = vec![0.75; 24000];
        let mut dst = vec![0.0; 12000];
        let mut metrics = BlockMetrics::default();
        p.process(&src, &mut dst, &mut metrics);
        assert_eq!(metrics.clipped_samples, 0);
        assert!((dst[11999] - 1.5 * db_to_gain(-6.0)).abs() < 0.0001);
        let previous = dst[11999];
        p.set_global_volume(GlobalVolume {
            gain_db: -6.0,
            mute: true,
        })
        .unwrap();
        p.process(&src, &mut dst, &mut metrics);
        assert!(
            (dst[0] - previous).abs() < 0.00001,
            "look-ahead retains already queued audio"
        );
        let first_fade = dst[p.latency_frames()];
        assert!(first_fade < previous && first_fade > previous * 0.99);
        assert!(dst[11999].abs() < 0.000001);
    }

    #[test]
    fn hot_routes_are_limited_after_eq_and_global_gain_without_emergency_clipping() {
        let mut c = config(2, 2);
        c.channels[0].routes = vec![1.0, 1.0];
        c.channels[1].routes = vec![0.25, 0.25];
        let mut p = Processor::new(2, 48000, c).unwrap();
        let src = vec![0.75; 24000];
        let mut dst = vec![0.0; 24000];
        let mut metrics = BlockMetrics::default();
        p.process(&src, &mut dst, &mut metrics);
        assert_eq!(metrics.clipped_samples, 0);
        assert!(metrics.overload_samples > 1000 && metrics.limited_frames > 1000);
        assert!(metrics.gain_reduction_db > 4.0);
        assert!(metrics.input_peaks[0] > 1.49);
        assert!(metrics.peaks[0] <= db_to_gain(-1.0));
        assert!((dst[23999] / dst[23998] - 0.25).abs() < 0.00001);
        assert_eq!(p.latency_frames(), 240);
    }

    #[test]
    fn invalid_global_volume_does_not_replace_the_current_target() {
        let mut p = Processor::new(1, 48000, config(1, 1)).unwrap();
        for gain_db in [1.0, -61.0, f32::NAN, f32::INFINITY] {
            assert!(
                p.set_global_volume(GlobalVolume {
                    gain_db,
                    mute: false
                })
                .is_err()
            );
        }
        assert_eq!(p.global_target, 1.0);
        assert_eq!(GlobalVolume::default().gain(), 1.0);
        assert_eq!(
            GlobalVolume {
                gain_db: 0.0,
                mute: true
            }
            .gain(),
            0.0
        );
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
