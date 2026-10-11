//! Bounded frame FIFO and small rate adjustments for independent device clocks.
//! WASAPI handles nominal rate conversion; interpolation here only corrects drift.
use std::collections::VecDeque;

pub struct AudioBuffer {
    samples: VecDeque<f32>,
    channels: usize,
    capacity_frames: usize,
    target_frames: usize,
    position: f64,
    started: bool,
    correction: f64,
    pub underruns: u64,
    pub dropped_frames: u64,
}

impl AudioBuffer {
    pub fn new(channels: usize, rate: u32, latency_ms: u32) -> Self {
        assert!(channels > 0 && rate > 0 && latency_ms > 0);
        let target_frames = rate as usize * latency_ms as usize / 1000;
        let capacity_frames = (target_frames * 8).max(rate as usize / 2);
        Self {
            samples: VecDeque::with_capacity(capacity_frames * channels),
            channels,
            capacity_frames,
            target_frames,
            position: 0.0,
            started: false,
            correction: 0.0,
            underruns: 0,
            dropped_frames: 0,
        }
    }

    pub fn push(&mut self, samples: &[f32]) {
        assert_eq!(samples.len() % self.channels, 0);
        let frames = samples.len() / self.channels;
        if frames >= self.capacity_frames {
            self.dropped_frames += self.frames() as u64 + (frames - self.capacity_frames) as u64;
            self.samples.clear();
            self.samples
                .extend(&samples[(frames - self.capacity_frames) * self.channels..]);
            self.position = 0.0;
            return;
        }
        let excess = (self.frames() + frames).saturating_sub(self.capacity_frames);
        self.discard(excess);
        self.dropped_frames += excess as u64;
        if excess > 0 {
            self.position = 0.0;
        }
        self.samples.extend(samples.iter().copied());
    }

    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels
    }
    pub fn correction_ppm(&self) -> f64 {
        self.correction * 1_000_000.0
    }

    fn discard(&mut self, frames: usize) {
        self.samples
            .drain(..frames.min(self.frames()) * self.channels);
    }

    /// Always fills a complete output block. Re-primes on underrun.
    pub fn pull(&mut self, destination: &mut [f32]) {
        assert_eq!(destination.len() % self.channels, 0);
        destination.fill(0.0);
        let requested = destination.len() / self.channels;
        if !self.started {
            if self.frames() < self.target_frames.max(requested + 2) {
                return;
            }
            self.started = true;
        }
        // Compare occupancy at the midpoint of this read with our target. Slow
        // adaptation prevents capture packet timing jitter modulating the rate.
        let error = (self.frames() as f64 - requested as f64 / 2.0 - self.target_frames as f64)
            / self.target_frames.max(1) as f64;
        let wanted = (error * 0.002).clamp(-0.002, 0.002);
        self.correction += (wanted - self.correction) * 0.02;
        let step = 1.0 + self.correction;
        for frame in destination.chunks_exact_mut(self.channels) {
            let index = self.position as usize;
            if index + 1 >= self.frames() {
                self.underruns += 1;
                self.started = false;
                self.position = 0.0;
                self.correction = 0.0;
                // Do not replay the remaining old frame on restart.
                self.samples.clear();
                break;
            }
            let fraction = (self.position - index as f64) as f32;
            for (channel, out) in frame.iter_mut().enumerate() {
                let a = self.samples[index * self.channels + channel];
                let b = self.samples[(index + 1) * self.channels + channel];
                *out = a + (b - a) * fraction;
            }
            self.position += step;
        }
        let consumed = (self.position as usize).min(self.frames());
        self.discard(consumed);
        self.position -= consumed as f64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_is_silent_and_frames_preserve_channel_order() {
        let mut b = AudioBuffer::new(2, 48000, 20);
        let mut out = vec![1.0; 480];
        b.pull(&mut out);
        assert!(out.iter().all(|v| *v == 0.0));
        b.push(&(0..1200).flat_map(|_| [0.2, -0.3]).collect::<Vec<_>>());
        b.pull(&mut out);
        assert!(
            out.chunks_exact(2)
                .all(|f| (f[0] - 0.2).abs() < 0.001 && (f[1] + 0.3).abs() < 0.001)
        );
    }

    #[test]
    fn overflow_is_bounded_and_counted_in_frames() {
        let mut b = AudioBuffer::new(2, 48000, 20);
        b.push(&vec![0.1; 60000]);
        assert_eq!(b.frames(), 24000);
        assert_eq!(b.dropped_frames, 6000);
        b.push(&vec![0.5; 2000]);
        assert_eq!(b.frames(), 24000);
        assert_eq!(b.dropped_frames, 7000);
    }

    #[test]
    fn underrun_zero_fills_and_recovers() {
        let mut b = AudioBuffer::new(1, 48000, 5);
        b.push(&vec![0.1; 300]);
        let mut out = vec![0.0; 240];
        b.pull(&mut out);
        b.pull(&mut out);
        assert_eq!(b.underruns, 1);
        assert_eq!(*out.last().unwrap(), 0.0);
        b.push(&vec![0.5; 400]);
        b.pull(&mut out);
        assert!((out[100] - 0.5).abs() < 0.001);
    }

    #[test]
    fn follows_independent_device_clocks_for_ten_minutes() {
        for ppm in [-400.0, 400.0] {
            let mut b = AudioBuffer::new(2, 48000, 30);
            b.push(&vec![0.1; 1440 * 2]);
            let mut out = vec![0.0; 480 * 2];
            let mut incoming = 0.0_f64;
            let mut input = vec![0.1; 482 * 2];
            for _ in 0..60000 {
                incoming += 480.0 * (1.0 + ppm / 1_000_000.0);
                let frames = incoming.floor() as usize;
                incoming -= frames as f64;
                input.resize(frames * 2, 0.1);
                b.push(&input);
                b.pull(&mut out);
            }
            assert_eq!(b.underruns, 0, "clock drift {ppm} ppm");
            assert_eq!(b.dropped_frames, 0, "clock drift {ppm} ppm");
            assert!(
                (500..3000).contains(&b.frames()),
                "occupancy {}",
                b.frames()
            );
            assert!((b.correction_ppm() - ppm).abs() < 50.0);
        }
    }
}
