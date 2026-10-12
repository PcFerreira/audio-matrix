//! Linked look-ahead limiting with optional conservative 4x inter-sample protection.
//! A monotonic envelope queue makes detection amortized O(1) per audio frame.
use crate::db_to_gain;
use std::collections::VecDeque;

pub const LOOKAHEAD_MS: u32 = 5;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LimiterConfig {
    pub enabled: bool,
    pub ceiling_db: f32,
    pub release_ms: f32,
    pub true_peak: bool,
}

impl Default for LimiterConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            ceiling_db: -1.0,
            release_ms: 120.0,
            true_peak: false,
        }
    }
}

impl LimiterConfig {
    pub fn validate(self) -> Result<(), String> {
        if !self.ceiling_db.is_finite()
            || !(-12.0..=0.0).contains(&self.ceiling_db)
            || !self.release_ms.is_finite()
            || !(20.0..=1000.0).contains(&self.release_ms)
        {
            return Err("Limiter ceiling must be -12 to 0 dBFS; release must be 20–1000 ms".into());
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct FrameMetrics {
    pub gain_reduction_db: f32,
    pub limited: bool,
    pub clipped_samples: u64,
    pub true_peak: f32,
}

struct Constraint {
    frame: u64,
    value: f64,
}

pub struct Limiter {
    tp_weights: [[f32; crate::resample::TAPS]; 4],
    tp_bounds: Vec<f64>,
    tp_output: Vec<f32>,
    tp_cursor: usize,
    tp_recent_peaks: [f64; crate::resample::TAPS],
    tp_weight_sum: f64,
    tp_until: u64,
    channels: usize,
    rate: u32,
    delay: usize,
    audio: Vec<f32>,
    cursor: usize,
    frame: u64,
    constraints: VecDeque<Constraint>,
    config: LimiterConfig,
    ceiling: f64,
    release: f64,
    gain: f64,
}

impl Limiter {
    pub fn new(channels: usize, rate: u32, config: LimiterConfig) -> Result<Self, String> {
        config.validate()?;
        if !(1..=32).contains(&channels) || rate < 8000 {
            return Err("Invalid limiter format".into());
        }
        let delay = (rate as usize * LOOKAHEAD_MS as usize / 1000).max(1);
        let mut limiter = Self {
            tp_weights: {
                let kernel = crate::resample::SincKernel::default();
                std::array::from_fn(|phase| kernel.weights(phase as f64 / 4.0))
            },
            tp_bounds: vec![1.0; delay + 1],
            tp_output: vec![0.0; crate::resample::TAPS * channels],
            tp_cursor: 0,
            tp_recent_peaks: [0.0; crate::resample::TAPS],
            tp_weight_sum: 0.0,
            tp_until: 0,
            channels,
            rate,
            delay,
            audio: vec![0.0; (delay + 1) * channels],
            cursor: 0,
            frame: 0,
            constraints: VecDeque::with_capacity(delay + 2),
            config,
            ceiling: db_to_gain(config.ceiling_db) as f64,
            release: 0.0,
            gain: 1.0,
        };
        limiter.tp_weight_sum = limiter
            .tp_weights
            .iter()
            .map(|weights| weights.iter().map(|w| (*w as f64).abs()).sum::<f64>())
            .fold(0.0, f64::max);
        limiter.configure(config)?;
        Ok(limiter)
    }
    pub fn delay_frames(&self) -> usize {
        self.delay
    }
    pub fn configure(&mut self, config: LimiterConfig) -> Result<(), String> {
        config.validate()?;
        let ceiling = db_to_gain(config.ceiling_db) as f64;
        if ceiling < self.ceiling {
            for bound in &mut self.tp_bounds {
                *bound *= ceiling / self.ceiling;
            }
        }
        self.config = config;
        self.ceiling = ceiling;
        self.release = 1.0 - (-1.0 / (self.rate as f64 * config.release_ms as f64 / 1000.0)).exp();
        Ok(())
    }
    pub fn process_frame(&mut self, frame: &mut [f32]) -> FrameMetrics {
        assert_eq!(frame.len(), self.channels);
        let mut peak: f64 = 0.0;
        for (channel, sample) in frame.iter().enumerate() {
            let sample = if sample.is_finite() { *sample } else { 0.0 };
            self.audio[self.cursor * self.channels + channel] = sample;
            peak = peak.max(sample.abs() as f64);
        }
        let required = (self.ceiling / peak.max(self.ceiling)).min(1.0);
        self.tp_bounds[self.cursor] = 1.0;
        self.tp_recent_peaks[self.frame as usize % crate::resample::TAPS] = peak;
        let needs_detection = self.config.true_peak
            && self.tp_recent_peaks.iter().copied().fold(0.0, f64::max) * self.tp_weight_sum
                > self.ceiling;
        if needs_detection {
            // A signed peak detector alone can miss overshoot caused by the
            // subsequently changing envelope. Triangle bounds constrain EVERY
            // contributing sample, so 4x reconstructed output stays under the
            // ceiling for this finite FIR. This conservative opt-in can reduce
            // more gain than sample-peak mode; it is not a certified BS.1770 meter.
            let mut bound = 0.0_f64;
            for channel in 0..self.channels {
                for weights in &self.tp_weights {
                    let mut value = 0.0;
                    for (i, weight) in weights.iter().enumerate() {
                        let age = crate::resample::TAPS - 1 - i;
                        if self.frame >= age as u64 {
                            let slot = (self.cursor + self.delay + 1 - age) % (self.delay + 1);
                            value += (*weight as f64
                                * self.audio[slot * self.channels + channel] as f64)
                                .abs();
                        }
                    }
                    bound = bound.max(value);
                }
            }
            let limit = (self.ceiling / bound.max(self.ceiling)).min(1.0);
            if limit < 1.0 {
                self.tp_until = self.frame + self.delay as u64 + 1;
                for age in 0..crate::resample::TAPS {
                    if self.frame >= age as u64 {
                        let slot = (self.cursor + self.delay + 1 - age) % (self.delay + 1);
                        self.tp_bounds[slot] = self.tp_bounds[slot].min(limit);
                    }
                }
            }
        }
        let delay = self.delay as f64;
        // A future peak requires gain <= r at its own playback frame. Backward
        // projection r + distance/L ramps attenuation in before the peak. All
        // constraints share a slope, so their minimum is a monotonic queue.
        if required < 1.0 {
            let value = required + self.frame as f64 / delay;
            while self.constraints.back().is_some_and(|c| c.value >= value) {
                self.constraints.pop_back();
            }
            self.constraints.push_back(Constraint {
                frame: self.frame,
                value,
            });
        }
        let output_slot = (self.cursor + 1) % (self.delay + 1);
        let playable = self.frame.checked_sub(self.delay as u64);
        if let Some(output_frame) = playable {
            while self
                .constraints
                .front()
                .is_some_and(|c| c.frame < output_frame)
            {
                self.constraints.pop_front();
            }
            let bound = self.constraints.front().map_or(1.0, |constraint| {
                (constraint.value - output_frame as f64 / delay).clamp(0.0, 1.0)
            });
            self.gain = (self.gain + (1.0 - self.gain) * self.release).min(bound);
        }
        let oldest_peak = self.audio
            [output_slot * self.channels..(output_slot + 1) * self.channels]
            .iter()
            .fold(0.0_f64, |peak, sample| peak.max(sample.abs() as f64));
        // Guard floating-point roundoff and live ceiling decreases. Gain is
        // shared by every channel; we never distort channel balance separately.
        if self.config.true_peak && self.frame <= self.tp_until {
            // Project all pending FIR constraints backwards over the same fixed
            // look-ahead; there is no additional audio buffer or phase-filtering
            // of the emitted signal, and gain is still shared across channels.
            let mut bound = 1.0_f64;
            for distance in 0..=self.delay {
                let slot = (output_slot + distance) % (self.delay + 1);
                bound = bound.min(self.tp_bounds[slot] + distance as f64 / delay);
            }
            self.gain = self.gain.min(bound);
        }
        let safe_gain = self.gain.min(self.ceiling / oldest_peak.max(self.ceiling));
        let applied = if self.config.enabled { safe_gain } else { 1.0 };
        let mut metrics = FrameMetrics {
            gain_reduction_db: if self.config.enabled && applied < 1.0 {
                (-20.0 * applied.max(1e-30).log10()) as f32
            } else {
                0.0
            },
            limited: self.config.enabled && applied < 0.999999,
            clipped_samples: 0,
            true_peak: 0.0,
        };
        for (channel, sample) in frame.iter_mut().enumerate() {
            let scaled = self.audio[output_slot * self.channels + channel] as f64 * applied;
            metrics.clipped_samples += u64::from(scaled.abs() > 1.0);
            // The normal limiter is a gain envelope, not this emergency guard.
            // Bypass deliberately retains the guard and the same delay.
            *sample = scaled.clamp(-1.0, 1.0) as f32;
            self.tp_output[self.tp_cursor * self.channels + channel] = *sample;
        }
        if self.config.true_peak {
            for channel in 0..self.channels {
                for weights in &self.tp_weights {
                    let value: f64 = weights
                        .iter()
                        .enumerate()
                        .map(|(i, weight)| {
                            let age = crate::resample::TAPS - 1 - i;
                            let slot = (self.tp_cursor + crate::resample::TAPS - age)
                                % crate::resample::TAPS;
                            *weight as f64 * self.tp_output[slot * self.channels + channel] as f64
                        })
                        .sum();
                    metrics.true_peak = metrics.true_peak.max(value.abs() as f32);
                }
            }
        }
        self.tp_cursor = (self.tp_cursor + 1) % crate::resample::TAPS;
        self.cursor = output_slot;
        self.frame += 1;
        metrics
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn four_phase_protection_catches_intersample_overshoot_and_links_channels() {
        let config = LimiterConfig {
            true_peak: true,
            ..Default::default()
        };
        let mut protected = Limiter::new(2, 48000, config).unwrap();
        let mut sample_only = Limiter::new(2, 48000, LimiterConfig::default()).unwrap();
        let kernel = crate::resample::SincKernel::default();
        let reconstruction = kernel
            .sample(0.5, |i| if i.rem_euclid(4) < 2 { 0.8 } else { -0.8 })
            .abs();
        assert!(reconstruction > db_to_gain(-1.0));
        let mut peak = 0.0_f32;
        for i in 0..6000 {
            let x = if i % 4 < 2 { 0.8 } else { -0.8 };
            let mut frame = [x, -x * 0.25];
            let metrics = protected.process_frame(&mut frame);
            let mut original = [x, -x * 0.25];
            sample_only.process_frame(&mut original);
            assert!(metrics.true_peak <= db_to_gain(-1.0) + 1e-5);
            assert_eq!(metrics.clipped_samples, 0);
            assert!((frame[1] + frame[0] * 0.25).abs() < 1e-7);
            if i > 1000 {
                peak = peak.max(frame[0].abs());
                assert_eq!(original[0].abs(), 0.8);
            }
        }
        assert!(peak < 0.8);
        assert_eq!(protected.delay_frames(), sample_only.delay_frames());
    }
    #[test]
    fn intersample_protection_is_exact_for_quiet_audio_and_keeps_toggle_latency() {
        let mut config = LimiterConfig {
            true_peak: true,
            ..Default::default()
        };
        let mut limiter = Limiter::new(1, 8000, config).unwrap();
        let delay = limiter.delay_frames();
        for i in 0..1000 {
            let mut frame = [0.05];
            let metrics = limiter.process_frame(&mut frame);
            assert_eq!(metrics.gain_reduction_db, 0.0);
            assert_eq!(frame[0], if i < delay { 0.0 } else { 0.05 });
        }
        config.true_peak = false;
        limiter.configure(config).unwrap();
        config.enabled = false;
        limiter.configure(config).unwrap();
        config.true_peak = true;
        limiter.configure(config).unwrap();
        assert_eq!(limiter.delay_frames(), delay);
    }
    #[test]
    fn below_ceiling_is_bit_identical_after_exact_fixed_delay() {
        let mut limiter = Limiter::new(2, 48000, LimiterConfig::default()).unwrap();
        let delay = limiter.delay_frames();
        let input: Vec<[f32; 2]> = (0..2000)
            .map(|i| [i as f32 * 0.0001, -(i as f32) * 0.0002])
            .collect();
        for i in 0..input.len() + delay {
            if i % 17 == 0 {
                limiter.configure(LimiterConfig::default()).unwrap();
            }
            let mut frame = input.get(i).copied().unwrap_or([0.0; 2]);
            let metrics = limiter.process_frame(&mut frame);
            assert_eq!(metrics.gain_reduction_db, 0.0);
            assert_eq!(
                frame,
                if i < delay {
                    [0.0; 2]
                } else {
                    input[i - delay]
                }
            );
        }
    }
    #[test]
    fn isolated_transient_is_pre_attenuated_and_channels_remain_linked() {
        let mut limiter = Limiter::new(2, 48000, LimiterConfig::default()).unwrap();
        let delay = limiter.delay_frames();
        let ceiling = db_to_gain(-1.0);
        let mut previous_gain = 1.0;
        let mut reduced_before_peak = false;
        for i in 0..2000 {
            let peak = if i == 1000 { 4.0 } else { 0.1 };
            let mut frame = [peak, -peak * 0.25];
            let metrics = limiter.process_frame(&mut frame);
            assert!(frame.iter().all(|s| s.abs() <= ceiling));
            assert_eq!(metrics.clipped_samples, 0);
            if i >= delay {
                assert!((frame[1] + frame[0] * 0.25).abs() < 1e-7);
                let gain = if i == 1000 + delay {
                    frame[0] / 4.0
                } else {
                    frame[0] / 0.1
                };
                if (1001..1000 + delay).contains(&i) && gain < 0.99 {
                    reduced_before_peak = true;
                }
                assert!(previous_gain - gain <= 1.0 / delay as f32 + 0.00001);
                previous_gain = gain;
            }
        }
        assert!(reduced_before_peak);
    }
    #[test]
    fn bypass_has_the_same_latency_and_counts_only_actual_emergency_clipping() {
        let mut config = LimiterConfig {
            enabled: false,
            ..LimiterConfig::default()
        };
        let mut limiter = Limiter::new(1, 8000, config).unwrap();
        let delay = limiter.delay_frames();
        for i in 0..delay + 10 {
            let mut frame = [2.0];
            let metrics = limiter.process_frame(&mut frame);
            assert_eq!(frame[0], if i < delay { 0.0 } else { 1.0 });
            assert_eq!(metrics.clipped_samples, u64::from(i >= delay));
        }
        config.enabled = true;
        limiter.configure(config).unwrap();
        let mut frame = [2.0];
        let metrics = limiter.process_frame(&mut frame);
        assert!(frame[0] <= db_to_gain(-1.0));
        assert_eq!(metrics.clipped_samples, 0);
        assert_eq!(limiter.delay_frames(), delay);
    }
    #[test]
    fn multichannel_extreme_peaks_and_invalid_samples_remain_finite_and_bounded() {
        for rate in [8000, 44100, 48000, 96000, 192000] {
            let mut limiter = Limiter::new(32, rate, LimiterConfig::default()).unwrap();
            for i in 0..rate / 10 {
                let mut frame = [0.2; 32];
                frame[0] = if i % 11 == 0 { f32::MAX } else { 5.0 };
                frame[1] = if i % 7 == 0 { f32::NAN } else { -3.0 };
                let metrics = limiter.process_frame(&mut frame);
                assert!(
                    frame
                        .iter()
                        .all(|v| v.is_finite() && v.abs() <= db_to_gain(-1.0))
                );
                assert_eq!(metrics.clipped_samples, 0);
                assert!(limiter.constraints.len() <= limiter.delay_frames() + 1);
            }
        }
    }
    #[test]
    fn live_ceiling_and_release_changes_keep_history_and_channel_balance() {
        let mut limiter = Limiter::new(2, 48000, LimiterConfig::default()).unwrap();
        for _ in 0..1000 {
            limiter.process_frame(&mut [2.0, -0.5]);
        }
        for ceiling_db in [-12.0, -6.0, -1.0, 0.0] {
            limiter
                .configure(LimiterConfig {
                    true_peak: false,
                    ceiling_db,
                    release_ms: 20.0,
                    enabled: true,
                })
                .unwrap();
            for _ in 0..1000 {
                let mut frame = [2.0, -0.5];
                limiter.process_frame(&mut frame);
                assert!(frame[0] > 0.0, "configuration must not reset delay history");
                assert!(frame[0] <= db_to_gain(ceiling_db));
                assert!((frame[1] + frame[0] * 0.25).abs() < 1e-7);
            }
            assert_eq!(limiter.delay_frames(), 240);
        }
    }

    #[test]
    fn steady_sine_distortion_is_much_lower_than_hard_clipping_at_the_same_ceiling() {
        let mut limiter = Limiter::new(1, 48000, LimiterConfig::default()).unwrap();
        let ceiling = db_to_gain(-1.0);
        let mut reference = Vec::new();
        let mut limited = Vec::new();
        let mut clipped = Vec::new();
        for i in 0..48000 {
            let sample = 2.0 * (std::f32::consts::TAU * 1000.0 * i as f32 / 48000.0).sin();
            let mut frame = [sample];
            limiter.process_frame(&mut frame);
            if i >= 24000 {
                let original = 2.0
                    * (std::f32::consts::TAU * 1000.0 * (i - limiter.delay_frames()) as f32
                        / 48000.0)
                        .sin();
                reference.push(original as f64);
                limited.push(frame[0] as f64);
                clipped.push(original.clamp(-ceiling, ceiling) as f64);
            }
        }
        let residual = |output: &[f64]| {
            let gain = reference
                .iter()
                .zip(output)
                .map(|(x, y)| x * y)
                .sum::<f64>()
                / reference.iter().map(|x| x * x).sum::<f64>();
            (reference
                .iter()
                .zip(output)
                .map(|(x, y)| (y - gain * x).powi(2))
                .sum::<f64>()
                / output.iter().map(|y| y * y).sum::<f64>())
            .sqrt()
        };
        assert!(
            residual(&limited) < residual(&clipped) / 5.0,
            "normalized waveform error: limiter {}, hard clip {}",
            residual(&limited),
            residual(&clipped)
        );
    }

    #[test]
    fn release_recovers_without_boost_and_configuration_is_validated_transactionally() {
        let mut limiter = Limiter::new(1, 8000, LimiterConfig::default()).unwrap();
        for _ in 0..800 {
            limiter.process_frame(&mut [4.0]);
        }
        let mut frame = [0.1];
        let initial = limiter.process_frame(&mut frame).gain_reduction_db;
        let mut final_reduction = initial;
        for _ in 0..8000 {
            final_reduction = limiter.process_frame(&mut frame).gain_reduction_db;
            frame[0] = 0.1;
            assert!(limiter.gain <= 1.0);
        }
        assert!(initial > 10.0 && final_reduction < 0.02);
        let old = limiter.config;
        for config in [
            LimiterConfig {
                ceiling_db: 1.0,
                ..old
            },
            LimiterConfig {
                release_ms: f32::NAN,
                ..old
            },
            LimiterConfig {
                release_ms: 1.0,
                ..old
            },
        ] {
            assert!(limiter.configure(config).is_err());
            assert_eq!(limiter.config, old);
        }
    }
}
