//! Platform-independent routing and DSP. No OS handles or external dependencies.
pub mod alignment;
pub mod buffer;
pub mod dsp;
pub mod eq;
pub mod limiter;
pub mod resample;
pub mod spectrum;
pub mod timeline;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FilterType {
    #[default]
    Bell,
    LowShelf,
    HighShelf,
    HighPass,
    LowPass,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EqBand {
    pub frequency: f32,
    pub gain_db: f32,
    pub q: f32,
    pub kind: FilterType,
    pub enabled: bool,
}

pub fn flat_eq() -> Vec<EqBand> {
    [80.0, 250.0, 1000.0, 4000.0, 12000.0]
        .into_iter()
        .map(|frequency| EqBand {
            frequency,
            gain_db: 0.0,
            q: 0.707,
            kind: FilterType::Bell,
            enabled: true,
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChannelConfig {
    /// One linear mixing coefficient per input channel.
    pub routes: Vec<f32>,
    pub gain_db: f32,
    pub mute: bool,
    pub invert: bool,
    pub eq: Vec<EqBand>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OutputConfig {
    /// Additional user calibration delay, independent of automatic compensation.
    pub delay_ms: f32,
    pub limiter: limiter::LimiterConfig,
    pub gain_db: f32,
    pub mute: bool,
    pub eq: Vec<EqBand>,
    pub channels: Vec<ChannelConfig>,
}

impl OutputConfig {
    pub fn identity(inputs: usize, outputs: usize) -> Self {
        Self {
            delay_ms: 0.0,
            limiter: limiter::LimiterConfig::default(),
            gain_db: -12.0,
            mute: false,
            eq: flat_eq(),
            channels: (0..outputs)
                .map(|out| {
                    let mut routes = vec![0.0; inputs];
                    // Mono duplicates to the first stereo pair; surround channels stay silent.
                    if out < inputs {
                        routes[out] = 1.0;
                    } else if inputs == 1 && out == 1 {
                        routes[0] = 1.0;
                    }
                    ChannelConfig {
                        routes,
                        gain_db: 0.0,
                        mute: false,
                        invert: false,
                        eq: flat_eq(),
                    }
                })
                .collect(),
        }
    }

    pub fn validate(&self, inputs: usize, outputs: usize, sample_rate: u32) -> Result<(), String> {
        self.limiter.validate()?;
        if !self.delay_ms.is_finite() || !(0.0..=500.0).contains(&self.delay_ms) {
            return Err("Manual delay must be between 0 and 500 ms".into());
        }
        if !(1..=32).contains(&inputs) || !(1..=32).contains(&outputs) || sample_rate < 8000 {
            return Err("Unsupported channel count or sample rate".into());
        }
        if self.channels.len() != outputs {
            return Err("Output channel count changed; refresh the device".into());
        }
        fn validate_gain(gain: f32) -> Result<(), String> {
            if !gain.is_finite() || !(-60.0..=12.0).contains(&gain) {
                return Err("Gain must be between -60 and +12 dB".into());
            }
            Ok(())
        }
        fn validate_eq(bands: &[EqBand], sample_rate: u32) -> Result<(), String> {
            if bands.len() > 16 {
                return Err("At most 16 EQ bands are supported".into());
            }
            for band in bands {
                if !band.frequency.is_finite()
                    || !(20.0..=20000.0).contains(&band.frequency)
                    || band.frequency >= sample_rate as f32 / 2.0
                    || !band.gain_db.is_finite()
                    || !(-18.0..=18.0).contains(&band.gain_db)
                    || !band.q.is_finite()
                    || !(0.1..=10.0).contains(&band.q)
                {
                    return Err(
                        "Invalid EQ: frequency must be below Nyquist, gain +/-18 dB, Q 0.1–10"
                            .into(),
                    );
                }
            }
            Ok(())
        }
        validate_gain(self.gain_db)?;
        validate_eq(&self.eq, sample_rate)?;
        for channel in &self.channels {
            validate_gain(channel.gain_db)?;
            validate_eq(&channel.eq, sample_rate)?;
            if channel.routes.len() != inputs
                || channel
                    .routes
                    .iter()
                    .any(|v| !v.is_finite() || !(-1.0..=1.0).contains(v))
            {
                return Err("Routing matrix dimensions or coefficients are invalid".into());
            }
        }
        Ok(())
    }
}

pub fn db_to_gain(db: f32) -> f32 {
    10.0_f32.powf(db / 20.0)
}
pub fn gain_to_db(gain: f32) -> f32 {
    20.0 * gain.max(0.000001).log10()
}

/// Router-wide attenuation. Unity is the backward-compatible default.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GlobalVolume {
    pub gain_db: f32,
    pub mute: bool,
}

impl GlobalVolume {
    pub fn validate(self) -> Result<(), String> {
        if !self.gain_db.is_finite() || !(-60.0..=0.0).contains(&self.gain_db) {
            return Err("Global volume must be between -60 and 0 dB".into());
        }
        Ok(())
    }
    pub fn gain(self) -> f32 {
        if self.mute {
            0.0
        } else {
            db_to_gain(self.gain_db)
        }
    }
}
