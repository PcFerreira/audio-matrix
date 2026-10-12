//! Tonal starting points, not measured speaker/headphone correction profiles.
use matrix_core::{EqBand, OutputConfig, flat_eq};

#[derive(Clone, Debug, PartialEq)]
pub struct EqPreset {
    pub name: String,
    pub bands: Vec<EqBand>,
}

pub fn builtins() -> Vec<EqPreset> {
    [
        ("Flat", [0.0; 5]),
        ("Bass boost", [4.0, 2.0, 0.0, 0.0, 0.0]),
        ("Bass cut", [-6.0, -3.0, 0.0, 0.0, 0.0]),
        ("Voice clarity", [-3.0, -2.0, 1.0, 3.0, 0.0]),
        ("Warm", [2.0, 1.0, 0.0, -1.0, -2.0]),
        ("Bright", [0.0, -1.0, 0.0, 2.0, 3.0]),
    ]
    .into_iter()
    .map(|(name, gains)| {
        let mut bands = flat_eq();
        for (band, gain) in bands.iter_mut().zip(gains) {
            band.gain_db = gain;
        }
        EqPreset {
            name: name.into(),
            bands,
        }
    })
    .collect()
}

pub fn at_rate(bands: &[EqBand], rate: u32) -> Vec<EqBand> {
    bands
        .iter()
        .map(|band| EqBand {
            frequency: band.frequency.min(20000.0_f32.min(rate as f32 * 0.45)),
            ..*band
        })
        .collect()
}

pub fn matches(bands: &[EqBand], preset: &EqPreset, rate: u32) -> bool {
    let candidate = at_rate(&preset.bands, rate);
    bands.len() == candidate.len()
        && bands.iter().zip(candidate).all(|(a, b)| {
            (a.frequency - b.frequency).abs() < 0.01
                && (a.gain_db - b.gain_db).abs() < 0.001
                && (a.q - b.q).abs() < 0.001
                && a.kind == b.kind
                && a.enabled == b.enabled
        })
}

impl EqPreset {
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty()
            || self.name.chars().count() > 48
            || self.name.chars().any(char::is_control)
            || !(1..=16).contains(&self.bands.len())
        {
            return Err("EQ preset needs a name (1–48 characters) and 1–16 bands".into());
        }
        let mut config = OutputConfig::identity(1, 1);
        config.eq.clone_from(&self.bands);
        config.validate(1, 1, 48000)
    }
}

pub fn save_custom(
    library: &mut Vec<EqPreset>,
    name: &str,
    bands: &[EqBand],
) -> Result<(), String> {
    let preset = EqPreset {
        name: name.trim().into(),
        bands: bands.to_vec(),
    };
    preset.validate()?;
    if builtins()
        .iter()
        .any(|p| p.name.eq_ignore_ascii_case(&preset.name))
    {
        return Err("Choose a name different from the built-in presets".into());
    }
    if let Some(existing) = library
        .iter_mut()
        .find(|p| p.name.eq_ignore_ascii_case(&preset.name))
    {
        *existing = preset;
    } else {
        if library.len() >= 64 {
            return Err("At most 64 custom EQ presets are supported".into());
        }
        library.push(preset);
    }
    Ok(())
}

pub fn validate_library(library: &[EqPreset]) -> Result<(), String> {
    if library.len() > 64 {
        return Err("At most 64 custom EQ presets are supported".into());
    }
    let mut names = std::collections::HashSet::new();
    for preset in library {
        preset.validate()?;
        if !names.insert(preset.name.trim().to_lowercase())
            || builtins()
                .iter()
                .any(|p| p.name.eq_ignore_ascii_case(preset.name.trim()))
        {
            return Err("EQ preset names must be unique and different from built-ins".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn presets_validate_at_all_supported_rates_and_do_not_change_routing_or_delay() {
        for preset in builtins() {
            preset.validate().unwrap();
            for rate in [8000, 16000, 44100, 48000, 96000] {
                let mut config = OutputConfig::identity(2, 2);
                config.delay_ms = 123.0;
                config.channels[0].routes = vec![0.3, 0.7];
                let before = config.clone();
                config.eq = at_rate(&preset.bands, rate);
                for channel in &mut config.channels {
                    channel.eq = at_rate(&channel.eq, rate);
                }
                config.validate(2, 2, rate).unwrap();
                assert!(matches(&config.eq, &preset, rate));
                assert_eq!(config.delay_ms, before.delay_ms);
                assert_eq!(config.channels[0].routes, before.channels[0].routes);
            }
        }
    }
    #[test]
    fn custom_library_copies_shapes_and_overwrites_only_matching_names() {
        let mut library = Vec::new();
        let mut bands = flat_eq();
        save_custom(&mut library, " My speakers ", &bands).unwrap();
        bands[0].gain_db = 3.0;
        assert_eq!(library[0].bands[0].gain_db, 0.0);
        save_custom(&mut library, "MY SPEAKERS", &bands).unwrap();
        assert_eq!(library.len(), 1);
        assert_eq!(library[0].bands[0].gain_db, 3.0);
        assert!(save_custom(&mut library, "Flat", &bands).is_err());
        assert!(save_custom(&mut library, "", &bands).is_err());
        bands[0].q = f32::NAN;
        assert!(save_custom(&mut library, "Broken", &bands).is_err());
        validate_library(&library).unwrap();
        library.push(library[0].clone());
        assert!(validate_library(&library).is_err());
    }
}
