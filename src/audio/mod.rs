use matrix_core::{GlobalVolume, OutputConfig};
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

#[derive(Clone, Debug, PartialEq, Eq)]
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
        if bits.len() == self.channels
            && let Some(name) = bits.get(index).and_then(|bit| names.get(*bit))
        {
            return format!("{} · {}", index + 1, name);
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
    pub true_peak: f32,
    pub spectrum: Vec<f32>,
    pub channel_spectra: Vec<Vec<f32>>,
    pub frames_written: u64,
    pub peaks: Vec<f32>,
    pub clipped: u64,
    pub overload_samples: u64,
    pub limited_frames: u64,
    pub gain_reduction_db: f32,
    pub pre_limiter_peak: f32,
    pub limiter_latency_ms: f32,
    pub underruns: u64,
    pub dropped_frames: u64,
    pub buffered_ms: f32,
    pub correction_ppm: f64,
    pub auto_delay_ms: f32,
    pub manual_delay_ms: f32,
    pub alignment_error_ms: Option<f64>,
    pub clock_available: bool,
    pub clock_supported: bool,
    pub measuring: bool,
}

pub struct OutputHandle {
    pub device_id: String,
    pub generation: u64,
    pub updates: mpsc::Sender<OutputConfig>,
    pub stats: Arc<Mutex<OutputStats>>,
    ready: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl OutputHandle {
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }
}

#[derive(Debug)]
pub enum EngineEvent {
    SourceReady(String),
    SourceError(String),
    OutputReady {
        device_id: String,
        generation: u64,
        message: String,
    },
    OutputError {
        device_id: String,
        generation: u64,
        message: String,
    },
}

pub struct Engine {
    spectrum_enabled: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    source: Source,
    source_ready: Arc<AtomicBool>,
    capture_worker: Option<JoinHandle<()>>,
    retired_workers: Vec<JoinHandle<()>>,
    events_tx: mpsc::Sender<EngineEvent>,
    next_generation: u64,
    latency_ms: u32,
    global_volume: Arc<Mutex<GlobalVolume>>,
    #[cfg(windows)]
    alignment: windows::SyncControl,
    #[cfg(windows)]
    timeline: Arc<Mutex<matrix_core::timeline::Timeline>>,
    pub outputs: Vec<OutputHandle>,
    pub events: mpsc::Receiver<EngineEvent>,
}

impl Engine {
    pub fn set_spectrum(&self, enabled: bool) {
        self.spectrum_enabled.store(enabled, Ordering::Release);
    }
    pub fn start(
        source: Source,
        outputs: Vec<(DeviceInfo, OutputConfig)>,
        latency_ms: u32,
        global_volume: GlobalVolume,
    ) -> Result<Self, String> {
        global_volume.validate()?;
        if outputs.is_empty() {
            return Err("Enable at least one output device".into());
        }
        if !(10..=100).contains(&latency_ms) {
            return Err("Buffer target must be 10–100 ms".into());
        }
        for (device, config) in &outputs {
            validate_output(&source, device, config)?;
        }
        #[cfg(windows)]
        {
            windows::start(source, outputs, latency_ms, global_volume)
        }
        #[cfg(not(windows))]
        {
            let _ = source;
            Err("Audio routing requires Windows".into())
        }
    }

    pub fn is_live(&self) -> bool {
        self.source_ready.load(Ordering::Acquire) && !self.stop.load(Ordering::Acquire)
    }

    pub fn set_global_volume(&mut self, volume: GlobalVolume) -> Result<(), String> {
        volume.validate()?;
        *self.global_volume.lock().unwrap_or_else(|e| e.into_inner()) = volume;
        Ok(())
    }

    pub fn add_output(&mut self, device: DeviceInfo, config: OutputConfig) -> Result<(), String> {
        if self.stop.load(Ordering::Acquire) {
            return Err("The input has stopped; restart routing".into());
        }
        if self.outputs.iter().any(|o| o.device_id == device.id) {
            return Err("Output is already enabled".into());
        }
        validate_output(&self.source, &device, &config)?;
        #[cfg(windows)]
        {
            let generation = self.next_generation;
            self.next_generation += 1;
            let handle = windows::spawn_output(self, device, config, generation)?;
            self.outputs.push(handle);
            Ok(())
        }
        #[cfg(not(windows))]
        {
            Err("Audio routing requires Windows".into())
        }
    }

    pub fn set_main_clock(&mut self, device_id: Option<&str>) -> Result<(), String> {
        let generation = match device_id {
            Some(id) => {
                self.outputs
                    .iter()
                    .find(|o| o.device_id == id)
                    .ok_or("Enable the device before selecting it as the main clock")?
                    .generation
            }
            None => 0,
        };
        #[cfg(windows)]
        {
            self.alignment
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .select_clock(generation);
        }
        #[cfg(not(windows))]
        {
            let _ = generation;
        }
        Ok(())
    }

    pub fn main_clock_device_id(&self) -> Option<&str> {
        #[cfg(windows)]
        {
            let main = self
                .alignment
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .main;
            self.outputs
                .iter()
                .find(|o| o.generation == main)
                .map(|o| o.device_id.as_str())
        }
        #[cfg(not(windows))]
        {
            None
        }
    }

    pub fn set_auto_align(&mut self, enabled: bool) {
        #[cfg(windows)]
        self.alignment
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .set_automatic(enabled);
        #[cfg(not(windows))]
        let _ = enabled;
    }

    pub fn realign(&mut self) {
        #[cfg(windows)]
        self.alignment
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .realign();
    }

    /// Retire only this renderer. Capture and the other renderers keep advancing.
    pub fn remove_output(&mut self, device_id: &str, generation: Option<u64>) -> bool {
        let Some(index) = self.outputs.iter().position(|o| {
            o.device_id == device_id
                && generation.is_none_or(|generation| generation == o.generation)
        }) else {
            return false;
        };
        let mut output = self.outputs.remove(index);
        output.stop.store(true, Ordering::Release);
        #[cfg(windows)]
        self.alignment
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(output.generation);
        if let Some(worker) = output.worker.take() {
            self.retired_workers.push(worker);
        }
        true
    }

    pub fn reap_finished_workers(&mut self) {
        for index in (0..self.retired_workers.len()).rev() {
            if self.retired_workers[index].is_finished() {
                let _ = self.retired_workers.swap_remove(index).join();
            }
        }
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        for output in &mut self.outputs {
            output.stop.store(true, Ordering::Release);
            if let Some(worker) = output.worker.take() {
                let _ = worker.join();
            }
        }
        if let Some(worker) = self.capture_worker.take() {
            let _ = worker.join();
        }
        for worker in self.retired_workers.drain(..) {
            let _ = worker.join();
        }
    }
}

fn validate_output(
    source: &Source,
    device: &DeviceInfo,
    config: &OutputConfig,
) -> Result<(), String> {
    if device.kind != DeviceKind::Playback {
        return Err("Output must be a playback endpoint".into());
    }
    if let Source::Endpoint(input) = source
        && input.kind == DeviceKind::Playback
        && input.id == device.id
    {
        return Err(
            "Choose a different output than the captured playback endpoint to prevent feedback"
                .into(),
        );
    }
    config.validate(source.channels(), device.channels, source.sample_rate())
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

/// Device callbacks only wake an owner thread; COM enumeration never runs on
/// the audio callback or GUI threads. Failed scans do not imply disconnection.
pub struct DeviceWatcher {
    pub events: mpsc::Receiver<Result<Vec<DeviceInfo>, String>>,
    stop: mpsc::Sender<()>,
    worker: Option<JoinHandle<()>>,
}
impl DeviceWatcher {
    pub fn start() -> Result<Self, String> {
        let (tx, events) = mpsc::channel();
        let (stop, rx) = mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("endpoint-watch".into())
            .spawn(move || {
                #[cfg(windows)]
                windows::watch_devices(tx, rx);
                #[cfg(not(windows))]
                {
                    let _ = (tx, rx);
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            events,
            stop,
            worker: Some(worker),
        })
    }
}
impl Drop for DeviceWatcher {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        // An unavailable OS device must not block closing the GUI.
        if let Some(worker) = self.worker.take().filter(|w| w.is_finished()) {
            let _ = worker.join();
        }
    }
}
