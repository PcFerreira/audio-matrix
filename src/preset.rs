use matrix_core::{ChannelConfig, EqBand, OutputConfig};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceMode {
    Playback,
    Capture,
    Tone,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preset {
    pub version: u32,
    pub source_mode: SourceMode,
    pub source_id: String,
    pub latency_ms: u32,
    pub outputs: Vec<SavedOutput>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedOutput {
    pub device_id: String,
    pub enabled: bool,
    pub gain_db: f32,
    pub mute: bool,
    pub eq: Vec<SavedBand>,
    pub channels: Vec<SavedChannel>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedChannel {
    pub routes: Vec<f32>,
    pub gain_db: f32,
    pub mute: bool,
    pub invert: bool,
    pub eq: Vec<SavedBand>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedBand {
    pub frequency: f32,
    pub gain_db: f32,
    pub q: f32,
}

impl From<&EqBand> for SavedBand {
    fn from(b: &EqBand) -> Self {
        Self {
            frequency: b.frequency,
            gain_db: b.gain_db,
            q: b.q,
        }
    }
}
impl From<&SavedBand> for EqBand {
    fn from(b: &SavedBand) -> Self {
        Self {
            frequency: b.frequency,
            gain_db: b.gain_db,
            q: b.q,
        }
    }
}

impl SavedOutput {
    pub fn new(device_id: String, enabled: bool, c: &OutputConfig) -> Self {
        Self {
            device_id,
            enabled,
            gain_db: c.gain_db,
            mute: c.mute,
            eq: c.eq.iter().map(SavedBand::from).collect(),
            channels: c
                .channels
                .iter()
                .map(|ch| SavedChannel {
                    routes: ch.routes.clone(),
                    gain_db: ch.gain_db,
                    mute: ch.mute,
                    invert: ch.invert,
                    eq: ch.eq.iter().map(SavedBand::from).collect(),
                })
                .collect(),
        }
    }

    pub fn config(&self) -> OutputConfig {
        OutputConfig {
            gain_db: self.gain_db,
            mute: self.mute,
            eq: self.eq.iter().map(EqBand::from).collect(),
            channels: self
                .channels
                .iter()
                .map(|ch| ChannelConfig {
                    routes: ch.routes.clone(),
                    gain_db: ch.gain_db,
                    mute: ch.mute,
                    invert: ch.invert,
                    eq: ch.eq.iter().map(EqBand::from).collect(),
                })
                .collect(),
        }
    }
}

impl Preset {
    pub fn load(path: &Path) -> Result<Self, String> {
        use std::io::Read;
        let file = std::fs::File::open(path)
            .map_err(|e| format!("Could not load {}: {e}", path.display()))?;
        let mut bytes = Vec::new();
        file.take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| format!("Could not read {}: {e}", path.display()))?;
        if bytes.len() > 1024 * 1024 {
            return Err("Preset exceeds 1 MiB".into());
        }
        let preset: Self =
            serde_json::from_slice(&bytes).map_err(|e| format!("Invalid preset: {e}"))?;
        if preset.version != 1 {
            return Err(format!("Unsupported preset version {}", preset.version));
        }
        if !(10..=100).contains(&preset.latency_ms) {
            return Err("Preset buffer target must be 10–100 ms".into());
        }
        Ok(preset)
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_nanos();
        let temporary = path.with_extension(format!("json.{stamp}.tmp"));
        let mut temporary_created = false;
        let result = (|| -> std::io::Result<()> {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            temporary_created = true;
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&temporary, path)
        })();
        if result.is_err() && temporary_created {
            let _ = std::fs::remove_file(&temporary);
        }
        result.map_err(|e| format!("Could not save {}: {e}", path.display()))
    }
}
