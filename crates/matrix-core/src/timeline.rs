//! One bounded history of input audio, with independent parallel playback cursors.
use std::collections::VecDeque;

pub struct Timeline {
    kernel: crate::resample::SincKernel,
    samples: VecDeque<f32>,
    channels: usize,
    capacity: usize,
    first: u64,
    end: u64,
}

impl Timeline {
    pub fn new(channels: usize, rate: u32) -> Self {
        assert!(channels > 0 && rate > 0);
        Self {
            kernel: crate::resample::SincKernel::default(),
            samples: VecDeque::with_capacity(rate as usize * channels * 2),
            channels,
            capacity: rate as usize * 2,
            first: 0,
            end: 0,
        }
    }
    pub fn push(&mut self, samples: &[f32]) {
        for frame in samples.chunks_exact(self.channels) {
            if (self.end - self.first) as usize == self.capacity {
                self.samples.drain(..self.channels);
                self.first += 1;
            }
            self.samples
                .extend(frame.iter().map(|s| if s.is_finite() { *s } else { 0.0 }));
            self.end += 1;
        }
    }
    pub fn end(&self) -> u64 {
        self.end
    }
    fn contains(&self, position: f64, frames: usize, step: f64) -> bool {
        position >= self.first as f64 + crate::resample::LEFT as f64
            && position + frames as f64 * step + (crate::resample::RIGHT as f64) < self.end as f64
    }
    fn sample(&self, position: f64, channel: usize) -> f32 {
        let center = position.floor() as u64;
        self.kernel.sample(position.fract(), |offset| {
            let frame = center.wrapping_add_signed(offset as i64);
            self.samples[(frame - self.first) as usize * self.channels + channel]
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ReadMapping {
    pub source_start: f64,
    pub step: f64,
    pub delay_ms: f64,
    pub valid: bool,
}

pub struct Reader {
    rate: f64,
    base: f64,
    position: Option<f64>,
    delay_ms: f64,
    old_position: Option<f64>,
    fade_remaining: usize,
    correction: f64,
    pub underruns: u64,
    pub dropped_frames: u64,
    pub buffered_ms: f32,
}

impl Reader {
    pub fn new(rate: u32, latency_ms: u32) -> Self {
        Self {
            rate: rate as f64,
            base: rate as f64 * latency_ms as f64 / 1000.0,
            position: None,
            delay_ms: 0.0,
            old_position: None,
            fade_remaining: 0,
            correction: 0.0,
            underruns: 0,
            dropped_frames: 0,
            buffered_ms: 0.0,
        }
    }
    /// Delay changes seek within retained history, crossfading over 10 ms.
    /// A clock reference is source frames per renderer frame, not serial audio.
    pub fn pull(
        &mut self,
        timeline: &Timeline,
        output: &mut [f32],
        delay_ms: f64,
        reference_step: Option<f64>,
    ) -> ReadMapping {
        output.fill(0.0);
        let frames = output.len() / timeline.channels;
        let delay_ms = if delay_ms.is_finite() {
            delay_ms.clamp(0.0, 1000.0)
        } else {
            0.0
        };
        if (delay_ms - self.delay_ms).abs() > 0.05 && self.fade_remaining == 0 {
            if let Some(position) = self.position {
                self.old_position = Some(position);
                self.position = Some(position - (delay_ms - self.delay_ms) * self.rate / 1000.0);
                self.fade_remaining = (self.rate / 100.0) as usize;
            }
            self.delay_ms = delay_ms;
        }
        let desired = self.base + self.delay_ms * self.rate / 1000.0 + frames as f64 / 2.0;
        let mut mapping = ReadMapping {
            source_start: 0.0,
            step: 1.0,
            delay_ms: self.delay_ms,
            valid: false,
        };
        if self.position.is_none() {
            let position = timeline.end as f64 - desired.max(frames as f64 + 3.0);
            if !timeline.contains(position, frames, 1.002) {
                return mapping;
            }
            self.position = Some(position);
        }
        let position = self.position.unwrap();
        let occupancy = timeline.end as f64 - position;
        // Smooth scheduler jitter before making tiny resampling corrections.
        let wanted = ((occupancy - desired) / self.base.max(1.0) * 0.002).clamp(-0.002, 0.002);
        self.correction += 0.02 * (wanted - self.correction);
        let reference = reference_step.filter(|v| v.is_finite() && (0.99..=1.01).contains(v));
        let step = reference.map_or(1.0 + self.correction, |v| {
            v + (self.correction * 0.25).clamp(-0.0005, 0.0005)
        });
        mapping.step = step;
        if !timeline.contains(position, frames, step) {
            self.underruns += 1;
            if position < timeline.first as f64 {
                self.dropped_frames += (timeline.first as f64 - position).ceil() as u64;
            }
            self.position = None;
            self.old_position = None;
            self.fade_remaining = 0;
            return mapping;
        }
        mapping.source_start = position;
        mapping.valid = self.fade_remaining == 0;
        let fade_length = (self.rate / 100.0).max(1.0);
        for (index, frame) in output.chunks_exact_mut(timeline.channels).enumerate() {
            let p = position + index as f64 * step;
            for (channel, sample) in frame.iter_mut().enumerate() {
                let next = timeline.sample(p, channel);
                *sample = if self.fade_remaining > 0 {
                    let old = self.old_position.unwrap() + index as f64 * step;
                    let previous = if timeline.contains(old, 1, step) {
                        timeline.sample(old, channel)
                    } else {
                        0.0
                    };
                    let mix = 1.0 - self.fade_remaining as f64 / fade_length;
                    previous + (next - previous) * mix as f32
                } else {
                    next
                };
            }
            self.fade_remaining = self.fade_remaining.saturating_sub(1);
        }
        if let Some(old) = self.old_position.as_mut() {
            *old += frames as f64 * step;
        }
        if self.fade_remaining == 0 {
            self.old_position = None;
        }
        self.position = Some(position + frames as f64 * step);
        self.buffered_ms =
            ((timeline.end as f64 - self.position.unwrap()) * 1000.0 / self.rate) as f32;
        mapping
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parallel_readers_get_identical_audio_without_consuming_shared_history() {
        let mut timeline = Timeline::new(2, 48000);
        let samples: Vec<_> = (0..4800).flat_map(|i| [i as f32, -(i as f32)]).collect();
        timeline.push(&samples);
        let mut a = Reader::new(48000, 30);
        let mut b = Reader::new(48000, 30);
        let mut x = [0.0; 960];
        let mut y = x;
        let first = a.pull(&timeline, &mut x, 0.0, None);
        let second = b.pull(&timeline, &mut y, 0.0, None);
        assert!(first.valid && second.valid);
        assert_eq!(first.source_start, second.source_start);
        assert_eq!(x, y);
        assert_eq!(timeline.end(), 4800);
        assert!(x.as_chunks::<2>().0.iter().all(|f| f[0] == -f[1]));
    }
    #[test]
    fn manual_delay_changes_live_and_crossfades_then_reports_valid_mapping() {
        let mut timeline = Timeline::new(1, 48000);
        timeline.push(&vec![0.25; 48000]);
        let mut reader = Reader::new(48000, 30);
        let mut output = [0.0; 480];
        let before = reader.pull(&timeline, &mut output, 0.0, None);
        assert!(before.valid);
        let after = reader.pull(&timeline, &mut output, 20.0, None);
        assert!(!after.valid);
        assert!((after.source_start - before.source_start + 480.0).abs() < 1.0);
        let settled = reader.pull(&timeline, &mut output, 20.0, None);
        assert!(settled.valid);
        assert_eq!(settled.delay_ms, 20.0);
        assert!(output.iter().all(|v| (*v - 0.25).abs() < 0.001));
        assert_eq!(reader.underruns, 0);
    }
    #[test]
    fn history_is_bounded_and_stalled_reader_reprimes() {
        let mut timeline = Timeline::new(1, 8000);
        let mut reader = Reader::new(8000, 30);
        timeline.push(&vec![0.5; 8000]);
        let mut output = [0.0; 80];
        reader.pull(&timeline, &mut output, 0.0, None);
        timeline.push(&vec![0.5; 32000]);
        assert_eq!(timeline.samples.len(), 16000);
        assert!(!reader.pull(&timeline, &mut output, 0.0, None).valid);
        assert_eq!(reader.underruns, 1);
        assert!(reader.dropped_frames > 0);
        assert!(reader.pull(&timeline, &mut output, 0.0, None).valid);
    }
    #[test]
    fn invalid_reference_and_delay_never_produce_non_finite_samples() {
        let mut timeline = Timeline::new(1, 48000);
        timeline.push(&vec![f32::NAN; 4800]);
        let mut reader = Reader::new(48000, 30);
        let mut output = [0.0; 480];
        reader.pull(&timeline, &mut output, f64::NAN, Some(f64::NAN));
        assert_eq!(output, [0.0; 480]);
    }

    #[test]
    fn parallel_readers_follow_independent_clocks_for_two_minutes() {
        for drift in [-0.0004, 0.0004] {
            let mut timeline = Timeline::new(1, 8000);
            let mut main = Reader::new(8000, 30);
            let mut follower = Reader::new(8000, 30);
            let mut main_fraction: f64 = 0.0;
            let mut follower_fraction: f64 = 0.0;
            let mut main_output = [0.0; 82];
            let mut follower_output = [0.0; 82];
            for _ in 0..12000 {
                timeline.push(&[0.25; 80]);
                main_fraction += 80.0 * (1.0 + drift);
                follower_fraction += 80.0 * (1.0 - drift);
                let main_frames = main_fraction.floor() as usize;
                let follower_frames = follower_fraction.floor() as usize;
                main_fraction -= main_frames as f64;
                follower_fraction -= follower_frames as f64;
                let mapping = main.pull(&timeline, &mut main_output[..main_frames], 0.0, None);
                let reference = mapping
                    .valid
                    .then_some(mapping.step * (1.0 + drift) / (1.0 - drift));
                follower.pull(
                    &timeline,
                    &mut follower_output[..follower_frames],
                    20.0,
                    reference,
                );
            }
            assert_eq!(main.underruns, 0);
            assert_eq!(follower.underruns, 0);
            assert!((10.0..50.0).contains(&main.buffered_ms));
            assert!((30.0..70.0).contains(&follower.buffered_ms));
            assert_eq!(timeline.samples.len(), 16000);
        }
    }
}
