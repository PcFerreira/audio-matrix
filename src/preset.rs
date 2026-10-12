use crate::eq_presets::{self, EqPreset};
use matrix_core::{ChannelConfig, EqBand, GlobalVolume, OutputConfig};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceMode {
    Playback,
    Capture,
    Tone,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preset {
    pub version: u32,
    pub source_mode: SourceMode,
    pub source_id: String,
    pub latency_ms: u32,
    #[serde(default)]
    pub main_clock_id: Option<String>,
    #[serde(default = "default_auto_align")]
    pub auto_align: bool,
    #[serde(default)]
    pub global_gain_db: f32,
    #[serde(default)]
    pub global_mute: bool,
    #[serde(default)]
    pub eq_presets: Vec<SavedEqPreset>,
    pub outputs: Vec<SavedOutput>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedEqPreset {
    pub name: String,
    pub bands: Vec<SavedBand>,
}

impl From<&EqPreset> for SavedEqPreset {
    fn from(preset: &EqPreset) -> Self {
        Self {
            name: preset.name.clone(),
            bands: preset.bands.iter().map(SavedBand::from).collect(),
        }
    }
}

impl From<&SavedEqPreset> for EqPreset {
    fn from(preset: &SavedEqPreset) -> Self {
        Self {
            name: preset.name.clone(),
            bands: preset.bands.iter().map(EqBand::from).collect(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedOutput {
    pub device_id: String,
    pub enabled: bool,
    #[serde(default)]
    pub delay_ms: f32,
    #[serde(default)]
    pub limiter: SavedLimiter,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub stereo_link: bool,
    pub gain_db: f32,
    pub mute: bool,
    pub eq: Vec<SavedBand>,
    pub channels: Vec<SavedChannel>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedLimiter {
    pub enabled: bool,
    pub ceiling_db: f32,
    pub release_ms: f32,
    #[serde(default)]
    pub true_peak: bool,
}

impl Default for SavedLimiter {
    fn default() -> Self {
        let config = matrix_core::limiter::LimiterConfig::default();
        Self {
            enabled: config.enabled,
            ceiling_db: config.ceiling_db,
            release_ms: config.release_ms,
            true_peak: config.true_peak,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedChannel {
    pub routes: Vec<f32>,
    pub gain_db: f32,
    pub mute: bool,
    pub invert: bool,
    pub eq: Vec<SavedBand>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedBand {
    pub frequency: f32,
    pub gain_db: f32,
    pub q: f32,
    #[serde(default)]
    pub kind: SavedFilterType,
    #[serde(default = "default_auto_align")]
    pub enabled: bool,
}

#[derive(Clone, Copy, Default, Serialize, Deserialize)]
pub enum SavedFilterType {
    #[default]
    Bell,
    LowShelf,
    HighShelf,
    HighPass,
    LowPass,
}
impl From<matrix_core::FilterType> for SavedFilterType {
    fn from(kind: matrix_core::FilterType) -> Self {
        match kind {
            matrix_core::FilterType::Bell => Self::Bell,
            matrix_core::FilterType::LowShelf => Self::LowShelf,
            matrix_core::FilterType::HighShelf => Self::HighShelf,
            matrix_core::FilterType::HighPass => Self::HighPass,
            matrix_core::FilterType::LowPass => Self::LowPass,
        }
    }
}
impl From<SavedFilterType> for matrix_core::FilterType {
    fn from(kind: SavedFilterType) -> Self {
        match kind {
            SavedFilterType::Bell => Self::Bell,
            SavedFilterType::LowShelf => Self::LowShelf,
            SavedFilterType::HighShelf => Self::HighShelf,
            SavedFilterType::HighPass => Self::HighPass,
            SavedFilterType::LowPass => Self::LowPass,
        }
    }
}

impl From<&EqBand> for SavedBand {
    fn from(b: &EqBand) -> Self {
        Self {
            frequency: b.frequency,
            gain_db: b.gain_db,
            q: b.q,
            kind: b.kind.into(),
            enabled: b.enabled,
        }
    }
}
impl From<&SavedBand> for EqBand {
    fn from(b: &SavedBand) -> Self {
        Self {
            frequency: b.frequency,
            gain_db: b.gain_db,
            q: b.q,
            kind: b.kind.into(),
            enabled: b.enabled,
        }
    }
}

impl SavedOutput {
    pub fn new(device_id: String, enabled: bool, c: &OutputConfig) -> Self {
        Self {
            device_id,
            enabled,
            delay_ms: c.delay_ms,
            display_name: String::new(),
            stereo_link: false,
            limiter: SavedLimiter {
                enabled: c.limiter.enabled,
                ceiling_db: c.limiter.ceiling_db,
                release_ms: c.limiter.release_ms,
                true_peak: c.limiter.true_peak,
            },
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
            delay_ms: self.delay_ms,
            limiter: matrix_core::limiter::LimiterConfig {
                enabled: self.limiter.enabled,
                ceiling_db: self.limiter.ceiling_db,
                release_ms: self.limiter.release_ms,
                true_peak: self.limiter.true_peak,
            },
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

fn default_auto_align() -> bool {
    true
}

impl Preset {
    pub fn global_volume(&self) -> GlobalVolume {
        GlobalVolume {
            gain_db: self.global_gain_db,
            mute: self.global_mute,
        }
    }
    pub fn validate_controls(&self) -> Result<(), String> {
        self.global_volume().validate()?;
        for output in &self.outputs {
            if output.display_name.chars().count() > 48
                || output.display_name.chars().any(char::is_control)
            {
                return Err(
                    "Device names must be at most 48 characters without control characters".into(),
                );
            }
        }
        eq_presets::validate_library(
            &self
                .eq_presets
                .iter()
                .map(EqPreset::from)
                .collect::<Vec<_>>(),
        )
    }
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
        preset.validate_controls()?;
        Ok(preset)
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        self.validate_controls()?;
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn old_bands_remain_enabled_bells_and_new_types_round_trip() {
        let old: SavedBand =
            serde_json::from_str(r#"{"frequency":1000,"gain_db":6,"q":0.707}"#).unwrap();
        assert!(old.enabled);
        assert_eq!(EqBand::from(&old).kind, matrix_core::FilterType::Bell);
        let mut band = EqBand::from(&old);
        band.kind = matrix_core::FilterType::LowShelf;
        band.enabled = false;
        let decoded: SavedBand =
            serde_json::from_slice(&serde_json::to_vec(&SavedBand::from(&band)).unwrap()).unwrap();
        assert_eq!(EqBand::from(&decoded), band);
        let mut output = OutputConfig::identity(2, 2);
        output.limiter.true_peak = true;
        let decoded: SavedOutput = serde_json::from_slice(
            &serde_json::to_vec(&SavedOutput::new("a".into(), true, &output)).unwrap(),
        )
        .unwrap();
        assert!(decoded.config().limiter.true_peak);
    }

    #[test]
    fn old_presets_default_to_input_clock() {
        let preset: Preset = serde_json::from_str(
            r#"{
            "version":1,"source_mode":"Tone","source_id":"","latency_ms":30,"outputs":[]
        }"#,
        )
        .unwrap();
        assert_eq!(preset.main_clock_id, None);
        assert!(preset.auto_align);
        assert_eq!(preset.global_volume(), GlobalVolume::default());
        assert!(preset.eq_presets.is_empty());
    }

    #[test]
    fn main_clock_selection_survives_serialization() {
        let mut config = OutputConfig::identity(2, 2);
        config.delay_ms = 23.5;
        config.limiter.ceiling_db = -2.5;
        config.limiter.release_ms = 250.0;
        let preset = Preset {
            version: 1,
            source_mode: SourceMode::Tone,
            source_id: String::new(),
            latency_ms: 30,
            main_clock_id: Some("clock-device".into()),
            auto_align: true,
            global_gain_db: -9.0,
            global_mute: true,
            eq_presets: vec![SavedEqPreset::from(&EqPreset {
                name: "My speakers".into(),
                bands: config.eq.clone(),
            })],
            outputs: vec![SavedOutput::new("clock-device".into(), true, &config)],
        };
        let decoded: Preset =
            serde_json::from_slice(&serde_json::to_vec(&preset).unwrap()).unwrap();
        assert_eq!(decoded.main_clock_id, preset.main_clock_id);
        assert!(decoded.auto_align);
        assert_eq!(
            decoded.global_volume(),
            GlobalVolume {
                gain_db: -9.0,
                mute: true
            }
        );
        assert_eq!(decoded.eq_presets[0].name, "My speakers");
        assert_eq!(decoded.outputs[0].config().delay_ms, 23.5);
        assert_eq!(decoded.outputs[0].config().limiter, config.limiter);
    }

    #[test]
    fn device_labels_link_flags_and_output_order_round_trip_without_changing_audio() {
        let mut first = SavedOutput::new("first".into(), true, &OutputConfig::identity(2, 2));
        first.display_name = "Desk speakers".into();
        first.stereo_link = true;
        let second = SavedOutput::new("second".into(), true, &OutputConfig::identity(2, 2));
        let mut preset: Preset = serde_json::from_str(
            r#"{"version":1,"source_mode":"Tone","source_id":"","latency_ms":30,"outputs":[]}"#,
        )
        .unwrap();
        preset.outputs = vec![second, first];
        let round_trip: Preset =
            serde_json::from_slice(&serde_json::to_vec(&preset).unwrap()).unwrap();
        assert_eq!(round_trip.outputs[0].device_id, "second");
        assert_eq!(round_trip.outputs[1].display_name, "Desk speakers");
        assert!(round_trip.outputs[1].stereo_link);
        assert_eq!(round_trip.outputs[1].config(), preset.outputs[1].config());
        preset.outputs[1].display_name = "bad\nname".into();
        assert!(preset.validate_controls().is_err());
    }

    #[test]
    fn old_outputs_default_to_zero_delay_and_invalid_delay_is_rejected() {
        let saved = SavedOutput::new("device".into(), true, &OutputConfig::identity(2, 2));
        let mut json = serde_json::to_value(saved).unwrap();
        json.as_object_mut().unwrap().remove("delay_ms");
        json.as_object_mut().unwrap().remove("limiter");
        let old: SavedOutput = serde_json::from_value(json).unwrap();
        assert_eq!(old.config().delay_ms, 0.0);
        assert_eq!(
            old.config().limiter,
            matrix_core::limiter::LimiterConfig::default()
        );
        for delay in [f32::NAN, f32::INFINITY, -1.0, 501.0] {
            let mut config = old.config();
            config.delay_ms = delay;
            assert!(config.validate(2, 2, 48000).is_err());
        }
    }

    #[test]
    fn global_and_eq_library_validation_rejects_bad_controls() {
        let mut preset: Preset = serde_json::from_str(
            r#"{"version":1,"source_mode":"Tone","source_id":"","latency_ms":100,"outputs":[]}"#,
        )
        .unwrap();
        preset.validate_controls().unwrap();
        preset.global_gain_db = 1.0;
        assert!(preset.validate_controls().is_err());
        preset.global_gain_db = -10.0;
        let custom = EqPreset {
            name: "My EQ".into(),
            bands: matrix_core::flat_eq(),
        };
        preset.eq_presets.push(SavedEqPreset::from(&custom));
        preset.validate_controls().unwrap();
        preset.eq_presets.push(SavedEqPreset::from(&custom));
        assert!(preset.validate_controls().is_err());
    }
}
