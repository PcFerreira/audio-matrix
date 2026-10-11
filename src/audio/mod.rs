use matrix_core::OutputConfig;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread::JoinHandle;

#[cfg(windows)]
mod windows;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceKind {
    Playback,
    Capture,
}

#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub kind: DeviceKind,
    pub channels: usize,
    pub sample_rate: u32,
    pub channel_mask: u32,
}

impl DeviceInfo {
    pub fn channel_name(&self, index: usize) -> String {
        let names = [
            "Front L",
            "Front R",
            "Center",
            "LFE",
            "Back L",
            "Back R",
            "Front center L",
            "Front center R",
            "Back center",
            "Side L",
            "Side R",
            "Top center",
            "Top front L",
            "Top front center",
            "Top front R",
            "Top back L",
            "Top back center",
            "Top back R",
        ];
        let bits: Vec<_> = (0..32)
            .filter(|bit| self.channel_mask & (1 << bit) != 0)
            .collect();
        if bits.len() == self.channels {
            if let Some(name) = bits.get(index).and_then(|bit| names.get(*bit)) {
                return format!("{} · {}", index + 1, name);
            }
        }
        format!("Channel {}", index + 1)
    }
}

#[derive(Clone, Debug)]
pub enum Source {
    Endpoint(DeviceInfo),
    Tone,
}

impl Source {
    pub fn channels(&self) -> usize {
        match self {
            Self::Endpoint(d) => d.channels,
            Self::Tone => 2,
        }
    }
    pub fn sample_rate(&self) -> u32 {
        match self {
            Self::Endpoint(d) => d.sample_rate,
            Self::Tone => 48000,
        }
    }
    pub fn channel_name(&self, index: usize) -> String {
        match self {
            Self::Endpoint(d) => d.channel_name(index),
            Self::Tone => ["Left", "Right"]
                .get(index)
                .unwrap_or(&"Channel")
                .to_string(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct OutputStats {
    pub peaks: Vec<f32>,
    pub clipped: u64,
    pub underruns: u64,
    pub dropped_frames: u64,
    pub buffered_ms: f32,
    pub correction_ppm: f64,
}

pub struct OutputHandle {
    pub device_id: String,
    pub updates: mpsc::Sender<OutputConfig>,
    pub stats: Arc<Mutex<OutputStats>>,
}

#[derive(Debug)]
pub enum EngineEvent {
    Ready(String),
    Error(String),
}

pub struct Engine {
    stop: Arc<AtomicBool>,
    workers: Vec<JoinHandle<()>>,
    pub outputs: Vec<OutputHandle>,
    pub events: mpsc::Receiver<EngineEvent>,
}

impl Engine {
    pub fn start(
        source: Source,
        outputs: Vec<(DeviceInfo, OutputConfig)>,
        latency_ms: u32,
    ) -> Result<Self, String> {
        if outputs.is_empty() {
            return Err("Enable at least one output device".into());
        }
        if !(10..=100).contains(&latency_ms) {
            return Err("Buffer target must be 10–100 ms".into());
        }
        for (device, config) in &outputs {
            if let Source::Endpoint(input) = &source {
                if input.kind == DeviceKind::Playback && input.id == device.id {
                    return Err("Choose a different output than the captured playback endpoint to prevent feedback".into());
                }
            }
            config.validate(source.channels(), device.channels, source.sample_rate())?;
        }
        #[cfg(windows)]
        {
            windows::start(source, outputs, latency_ms)
        }
        #[cfg(not(windows))]
        {
            let _ = source;
            Err("Audio routing requires Windows".into())
        }
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn enumerate_devices() -> Result<Vec<DeviceInfo>, String> {
    #[cfg(windows)]
    {
        // Core Audio COM objects stay on their owner thread, never the egui thread.
        std::thread::spawn(windows::enumerate)
            .join()
            .map_err(|_| "Device enumeration worker panicked".to_string())?
    }
    #[cfg(not(windows))]
    {
        Err("Device enumeration requires Windows".into())
    }
}
