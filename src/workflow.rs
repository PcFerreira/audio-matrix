//! App preferences/checkpoints are separate from user-authored calibration files.
use crate::preset::{Preset, SavedOutput, SourceMode};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Preferences {
    pub restore_session: bool,
    pub auto_start_audio: bool,
    pub start_with_windows: bool,
    pub close_to_tray: bool,
    pub spectrum: bool,
    pub recover_input: bool,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            restore_session: true,
            auto_start_audio: false,
            start_with_windows: false,
            close_to_tray: false,
            spectrum: false,
            recover_input: true,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scene {
    pub name: String,
    pub source_mode: SourceMode,
    pub source_id: String,
    pub sample_rate: u32,
    pub inputs: usize,
    pub global_gain_db: f32,
    pub global_mute: bool,
    pub outputs: Vec<SavedOutput>,
}
impl Scene {
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty()
            || self.name.chars().count() > 48
            || self.name.chars().any(char::is_control)
            || self.outputs.len() > 128
            || !(1..=32).contains(&self.inputs)
            || self.sample_rate < 8000
        {
            return Err("Scene needs a name (1-48 characters) and at most 128 outputs".into());
        }
        matrix_core::GlobalVolume {
            gain_db: self.global_gain_db,
            mute: self.global_mute,
        }
        .validate()?;
        let mut ids = std::collections::HashSet::new();
        for output in &self.outputs {
            if !ids.insert(&output.device_id) {
                return Err("Duplicate output in scene".into());
            }
            output
                .config()
                .validate(self.inputs, output.channels.len(), self.sample_rate)?;
            if output.enabled
                && self.source_mode == SourceMode::Playback
                && output.device_id == self.source_id
            {
                return Err("Scene cannot feed captured playback into itself".into());
            }
        }
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Session {
    pub version: u32,
    pub preferences: Preferences,
    pub preset_path: String,
    pub preset: Preset,
    pub scenes: Vec<Scene>,
    #[serde(default)]
    pub source_format: Option<crate::audio::Source>,
}
impl Session {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 || self.scenes.len() > 32 {
            return Err("Unsupported session / too many scenes".into());
        }
        if self.preset.version != 1 || !(10..=100).contains(&self.preset.latency_ms) {
            return Err("Invalid session preset version/buffer target".into());
        }
        self.preset.validate_controls()?;
        if let Some(source) = &self.source_format {
            use crate::audio::{DeviceKind, Source};
            let matches = match source {
                Source::Tone => self.preset.source_mode == SourceMode::Tone,
                Source::Endpoint(device) => {
                    device.id == self.preset.source_id
                        && !device.id.is_empty()
                        && device.id.len() <= 4096
                        && match self.preset.source_mode {
                            SourceMode::Playback => device.kind == DeviceKind::Playback,
                            SourceMode::Capture => device.kind == DeviceKind::Capture,
                            SourceMode::Tone => false,
                        }
                }
            };
            if !matches
                || !(1..=32).contains(&source.channels())
                || !(8000..=384000).contains(&source.sample_rate())
            {
                return Err(
                    "Session input metadata does not match its selected input/format".into(),
                );
            }
        }
        let mut names = std::collections::HashSet::new();
        for scene in &self.scenes {
            scene.validate()?;
            if !names.insert(scene.name.trim().to_lowercase()) {
                return Err("Scene names must be unique".into());
            }
        }
        Ok(())
    }
}

pub fn session_path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(|p| PathBuf::from(p).join("AudioMatrix").join("session.json"))
}
pub fn read(path: &Path) -> Result<Session, String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(|e| e.to_string())?
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err("Session exceeds 4 MiB".into());
    }
    let session: Session = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    session.validate()?;
    Ok(session)
}
pub fn write(path: &Path, session: &Session) -> Result<(), String> {
    session.validate()?;
    let bytes = serde_json::to_vec_pretty(session).map_err(|e| e.to_string())?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err("Session exceeds 4 MiB".into());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    let temporary = path.with_extension(format!("{stamp}.tmp"));
    let mut created = false;
    let result = (|| -> std::io::Result<()> {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        created = true;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() && created {
        let _ = std::fs::remove_file(&temporary);
    }
    result.map_err(|e| format!("Session checkpoint: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn checkpoint_round_trip_and_failed_validation_preserve_existing_file() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "matrix-session-test-{}-{stamp}",
            std::process::id()
        ));
        let path = directory.join("session.json");
        let preset: Preset = serde_json::from_str(
            r#"{"version":1,"source_mode":"Tone","source_id":"","latency_ms":30,"outputs":[]}"#,
        )
        .unwrap();
        let mut session = Session {
            version: 1,
            preferences: Preferences::default(),
            preset_path: "user-calibration.json".into(),
            preset,
            scenes: Vec::new(),
            source_format: None,
        };
        write(&path, &session).unwrap();
        let original = std::fs::read(&path).unwrap();
        let loaded = read(&path).unwrap();
        assert_eq!(loaded.preset_path, "user-calibration.json");
        assert!(!loaded.preferences.auto_start_audio);
        assert!(!directory.join("user-calibration.json").exists());
        session.preset.latency_ms = 1000;
        assert!(write(&path, &session).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
    #[test]
    fn startup_playback_and_background_close_are_opt_in() {
        let prefs = Preferences::default();
        assert!(!prefs.auto_start_audio && !prefs.start_with_windows && !prefs.close_to_tray);
        assert!(prefs.restore_session);
    }
    #[test]
    fn scene_validates_dimensions_and_rejects_bad_names() {
        let mut scene = Scene {
            name: "Desk".into(),
            source_mode: SourceMode::Tone,
            source_id: String::new(),
            sample_rate: 48000,
            inputs: 2,
            global_gain_db: -6.0,
            global_mute: false,
            outputs: vec![SavedOutput::new(
                "a".into(),
                true,
                &matrix_core::OutputConfig::identity(2, 2),
            )],
        };
        scene.validate().unwrap();
        scene.inputs = 1;
        assert!(scene.validate().is_err());
        scene.inputs = 2;
        scene.name = "bad\nname".into();
        assert!(scene.validate().is_err());
    }
}
