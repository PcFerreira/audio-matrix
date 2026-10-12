//! Optional 1024-point Hann FFT display. UI analysis is never in the signal path.
const SIZE: usize = 1024;
pub const BINS: usize = 64;
pub struct Analyzer {
    rate: u32,
    channels: usize,
    ring: Vec<f32>,
    cursor: usize,
    accumulated: usize,
    real: Vec<f32>,
    imaginary: Vec<f32>,
    pub spectra: Vec<Vec<f32>>,
    pub device: Vec<f32>,
}
impl Analyzer {
    pub fn new(channels: usize, rate: u32) -> Self {
        Self {
            rate,
            channels,
            ring: vec![0.0; SIZE * channels],
            cursor: 0,
            accumulated: 0,
            real: vec![0.0; SIZE],
            imaginary: vec![0.0; SIZE],
            spectra: vec![vec![-120.0; BINS]; channels],
            device: vec![-120.0; BINS],
        }
    }
    /// At most 20 FFT updates per second, with one reusable scratch allocation.
    pub fn push(&mut self, samples: &[f32]) -> bool {
        for frame in samples.chunks_exact(self.channels) {
            self.ring[self.cursor * self.channels..(self.cursor + 1) * self.channels]
                .copy_from_slice(frame);
            self.cursor = (self.cursor + 1) % SIZE;
            self.accumulated += 1;
        }
        if self.accumulated < (self.rate / 20).max(SIZE as u32) as usize {
            return false;
        }
        self.accumulated = 0;
        self.device.fill(-120.0);
        for channel in 0..self.channels {
            for i in 0..SIZE {
                let x = self.ring[((self.cursor + i) % SIZE) * self.channels + channel];
                let window = 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / SIZE as f32).cos();
                self.real[i] = if x.is_finite() { x * window } else { 0.0 };
                self.imaginary[i] = 0.0;
            }
            fft(&mut self.real, &mut self.imaginary);
            let maximum = 20000.0_f32.min(self.rate as f32 * 0.45);
            for bin in 0..BINS {
                let low = 20.0 * (maximum / 20.0).powf(bin as f32 / BINS as f32);
                let high = 20.0 * (maximum / 20.0).powf((bin + 1) as f32 / BINS as f32);
                let first = (low * SIZE as f32 / self.rate as f32).round().max(1.0) as usize;
                let last = (high * SIZE as f32 / self.rate as f32)
                    .round()
                    .max(first as f32) as usize;
                let peak = (first..=last.min(SIZE / 2 - 1))
                    .map(|i| {
                        (self.real[i].powi(2) + self.imaginary[i].powi(2)).sqrt() * 4.0
                            / SIZE as f32
                    })
                    .fold(0.0_f32, f32::max);
                let db = crate::gain_to_db(peak).clamp(-120.0, 12.0);
                self.spectra[channel][bin] = db;
                // Maximum per-channel magnitude avoids anti-phase cancellation.
                self.device[bin] = self.device[bin].max(db);
            }
        }
        true
    }
}
fn fft(real: &mut [f32], imaginary: &mut [f32]) {
    for i in 0..SIZE {
        let reverse = i.reverse_bits() >> (usize::BITS - SIZE.ilog2());
        if reverse > i {
            real.swap(i, reverse);
            imaginary.swap(i, reverse);
        }
    }
    let mut length = 2;
    while length <= SIZE {
        let angle = -std::f32::consts::TAU / length as f32;
        let (step_im, step_re) = angle.sin_cos();
        for start in (0..SIZE).step_by(length) {
            let (mut wr, mut wi) = (1.0, 0.0);
            for i in 0..length / 2 {
                let even = start + i;
                let odd = even + length / 2;
                let re = real[odd] * wr - imaginary[odd] * wi;
                let im = real[odd] * wi + imaginary[odd] * wr;
                real[odd] = real[even] - re;
                imaginary[odd] = imaginary[even] - im;
                real[even] += re;
                imaginary[even] += im;
                let next = wr * step_re - wi * step_im;
                wi = wr * step_im + wi * step_re;
                wr = next;
            }
        }
        length *= 2;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fft_reports_tone_level_without_cancelling_opposite_channels() {
        let mut analyzer = Analyzer::new(2, 48000);
        let samples: Vec<_> = (0..4800)
            .flat_map(|i| {
                let x = 0.5 * (std::f32::consts::TAU * 1500.0 * i as f32 / 48000.0).sin();
                [x, -x]
            })
            .collect();
        assert!(analyzer.push(&samples));
        let level = analyzer.device.iter().copied().fold(-120.0_f32, f32::max);
        assert!((level + 6.0206).abs() < 0.05);
        assert_eq!(analyzer.spectra[0], analyzer.spectra[1]);
        assert!(analyzer.push(&vec![0.0; 9600]));
        assert!(analyzer.device.iter().all(|v| *v <= -119.0));
    }
}
