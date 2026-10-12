//! Bounded, best-effort spectrum tap. Render workers only copy into a spare
//! preallocated block; overload drops visualization blocks, never audio.
use super::OutputStats;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::{thread::JoinHandle, time::Duration};
pub struct Tap {
    free: mpsc::Receiver<Vec<f32>>,
    recycle: mpsc::SyncSender<Vec<f32>>,
    queued: mpsc::SyncSender<Vec<f32>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    pub dropped: u64,
    capacity: usize,
}
impl Tap {
    pub fn new(
        channels: usize,
        rate: u32,
        capacity: usize,
        enabled: Arc<AtomicBool>,
        stats: Arc<Mutex<OutputStats>>,
    ) -> Result<Self, String> {
        let (recycle, free) = mpsc::sync_channel(3);
        for _ in 0..3 {
            recycle
                .send(vec![0.0; capacity])
                .map_err(|e| e.to_string())?;
        }
        let (queued, packets) = mpsc::sync_channel::<Vec<f32>>(2);
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let spare = recycle.clone();
        let worker = std::thread::Builder::new()
            .name("spectrum-analysis".into())
            .spawn(move || {
                let mut analyzer = matrix_core::spectrum::Analyzer::new(channels, rate);
                while !stopped.load(Ordering::Acquire) {
                    match packets.recv_timeout(Duration::from_millis(50)) {
                        Ok(mut samples) => {
                            if enabled.load(Ordering::Acquire)
                                && analyzer.push(&samples)
                                && let Ok(mut stats) = stats.try_lock()
                            {
                                stats.spectrum.clone_from(&analyzer.device);
                                stats.channel_spectra.clone_from(&analyzer.spectra);
                            }
                            samples.resize(capacity, 0.0);
                            let _ = spare.try_send(samples);
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            if !enabled.load(Ordering::Acquire)
                                && let Ok(mut stats) = stats.try_lock()
                            {
                                stats.spectrum.clear();
                                stats.channel_spectra.clear();
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            free,
            recycle,
            queued,
            stop,
            worker: Some(worker),
            dropped: 0,
            capacity,
        })
    }
    pub fn offer(&mut self, samples: &[f32]) {
        let Ok(mut block) = self.free.try_recv() else {
            self.dropped += 1;
            return;
        };
        if samples.len() > self.capacity {
            let _ = self.recycle.try_send(block);
            self.dropped += 1;
            return;
        }
        block.truncate(samples.len());
        block.copy_from_slice(samples);
        if let Err(error) = self.queued.try_send(block) {
            let (mpsc::TrySendError::Full(block) | mpsc::TrySendError::Disconnected(block)) = error;
            let _ = self.recycle.try_send(block);
            self.dropped += 1;
        }
    }
}
impl Drop for Tap {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn analysis_is_optional_bounded_and_returns_spare_blocks() {
        let enabled = Arc::new(AtomicBool::new(true));
        let stats = Arc::new(Mutex::new(OutputStats::default()));
        let mut tap = Tap::new(2, 48000, 4800, enabled.clone(), stats.clone()).unwrap();
        for _ in 0..500 {
            tap.offer(&[0.0; 4800]);
        }
        assert!(tap.dropped > 0);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while stats.lock().unwrap().spectrum.is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            stats.lock().unwrap().spectrum.len(),
            matrix_core::spectrum::BINS
        );
        enabled.store(false, Ordering::Release);
        std::thread::sleep(Duration::from_millis(100));
        assert!(stats.lock().unwrap().spectrum.is_empty());
        tap.offer(&[0.0; 4800]);
    }
}
