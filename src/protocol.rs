use crate::{
    audio::{DeviceInfo, OutputStats, Source},
    workflow::Session,
};
use serde::{Deserialize, Serialize};
pub const VERSION: u32 = 1;

#[derive(Clone, Default, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Audition {
    pub devices: Vec<String>,
    pub channels: Vec<(String, usize)>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Update {
    pub session: Session,
    pub audition: Audition,
    /// Tray mutations are preserved unless the UI actually edited this control.
    pub base_gain: Option<f32>,
    pub base_mute: Option<bool>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Request {
    Status,
    Configure(Box<Update>),
    Start(Box<Update>),
    Stop,
    ReAlign,
    Show,
    AttachUi {
        window: u64,
    },
    DetachUi,
    Shutdown,
    /// Isolated no-audio smoke instances only; rejected by real engines.
    TestDevices(Vec<DeviceInfo>),
    TestPower {
        resume: bool,
    },
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub version: u32,
    pub request: Request,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum State {
    #[default]
    Stopped,
    Starting,
    Running,
    Recovering,
    Suspended,
    FormatBlocked,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Output {
    pub device_id: String,
    pub generation: u64,
    pub ready: bool,
    pub stats: OutputStats,
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u32,
    pub engine_pid: u32,
    pub revision: u64,
    pub state: State,
    pub wanted: bool,
    pub live: bool,
    pub source: Option<Source>,
    pub session: Option<Session>,
    pub audition: Audition,
    pub devices: Vec<DeviceInfo>,
    pub outputs: Vec<Output>,
    pub main_clock_id: Option<String>,
    pub message: String,
    pub is_error: bool,
    pub recoveries: u64,
    pub show_epoch: u64,
    pub test_mode: bool,
    pub tray_available: bool,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Reply {
    pub version: u32,
    pub snapshot: Snapshot,
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn protocol_rejects_unknown_commands_and_preserves_version() {
        assert!(serde_json::from_str::<Envelope>(r#"{"version":1,"request":"Unknown"}"#).is_err());
        let envelope = Envelope {
            version: VERSION,
            request: Request::Status,
        };
        let decoded: Envelope =
            serde_json::from_slice(&serde_json::to_vec(&envelope).unwrap()).unwrap();
        assert_eq!(decoded.version, VERSION);
    }
}
