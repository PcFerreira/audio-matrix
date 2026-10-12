//! The sole owner of WASAPI, reconnect policy, tray and checkpoints. UI lifetime
//! is independent; a disconnect never drops the graph or changes its settings.
use crate::{
    audio::{self, DeviceInfo, DeviceKind, Engine, EngineEvent, Source},
    protocol::{self, Audition, Reply, Request, Snapshot, State, Update},
    tray::{Action, Tray},
    workflow::{self, Session},
};
use matrix_core::OutputConfig;
use std::{
    collections::HashMap,
    sync::mpsc,
    time::{Duration, Instant},
};

fn same_format(a: &Source, b: &Source) -> bool {
    match (a, b) {
        (Source::Tone, Source::Tone) => true,
        (Source::Endpoint(a), Source::Endpoint(b)) => {
            a.id == b.id
                && a.kind == b.kind
                && a.channels == b.channels
                && a.sample_rate == b.sample_rate
                && a.channel_mask == b.channel_mask
        }
        _ => false,
    }
}
fn source_for(session: &Session, devices: &[DeviceInfo]) -> Option<Source> {
    if session.preset.source_mode == crate::preset::SourceMode::Tone {
        return Some(Source::Tone);
    }
    let kind = if session.preset.source_mode == crate::preset::SourceMode::Playback {
        DeviceKind::Playback
    } else {
        DeviceKind::Capture
    };
    devices
        .iter()
        .find(|d| d.id == session.preset.source_id && d.kind == kind)
        .cloned()
        .map(Source::Endpoint)
}
pub fn effective(config: &OutputConfig, id: &str, audition: &Audition) -> OutputConfig {
    let mut result = config.clone();
    if audition.devices.is_empty() && audition.channels.is_empty() {
        return result;
    }
    let device = audition.devices.iter().any(|d| d == id);
    let channel = audition.channels.iter().any(|(d, _)| d == id);
    result.mute |= !device && !channel;
    if channel && !device {
        for (index, ch) in result.channels.iter_mut().enumerate() {
            ch.mute |= !audition
                .channels
                .iter()
                .any(|(d, c)| d == id && *c == index);
        }
    }
    result
}
pub struct Core {
    session: Option<Session>,
    devices: Vec<DeviceInfo>,
    audition: Audition,
    engine: Option<Engine>,
    wanted: bool,
    expected_source: Option<Source>,
    suspended: bool,
    retry: Instant,
    output_retry: HashMap<String, Instant>,
    sent: HashMap<String, (u64, OutputConfig)>,
    revision: u64,
    message: String,
    is_error: bool,
    recoveries: u64,
    show_epoch: u64,
    test_mode: bool,
    mock_live: bool,
    mock_frames: u64,
    checkpoint_dirty: bool,
    persistence_allowed: bool,
    checkpoint_due: Instant,
    state: State,
    owner: Option<(u32, u64)>,
    launch_due: Instant,
    tray_available: bool,
}
impl Core {
    pub fn new(test_mode: bool) -> Self {
        Self {
            session: None,
            devices: Vec::new(),
            audition: Audition::default(),
            engine: None,
            wanted: false,
            expected_source: None,
            suspended: false,
            retry: Instant::now(),
            output_retry: HashMap::new(),
            sent: HashMap::new(),
            revision: 0,
            message: "Background engine ready".into(),
            is_error: false,
            recoveries: 0,
            show_epoch: 0,
            test_mode,
            mock_live: false,
            mock_frames: 0,
            checkpoint_dirty: false,
            persistence_allowed: !test_mode,
            checkpoint_due: Instant::now(),
            state: State::Stopped,
            owner: None,
            launch_due: Instant::now(),
            tray_available: false,
        }
    }
    fn report(&mut self, message: String, error: bool) {
        self.message = message;
        self.is_error = error;
        self.revision = self.revision.wrapping_add(1);
    }
    fn validate(&self, update: &Update) -> Result<(), String> {
        update.session.validate()?;
        if self.wanted
            && let (Some(expected), Some(requested)) =
                (&self.expected_source, &update.session.source_format)
            && !same_format(expected, requested)
        {
            return Err("Stop routing before changing the input format".into());
        }
        if update.session.preset.outputs.len() > 128
            || update.audition.devices.len() > 128
            || update.audition.channels.len() > 4096
        {
            return Err("Too many outputs/audition targets".into());
        }
        let source = self
            .expected_source
            .clone()
            .filter(|_| self.wanted)
            .or_else(|| source_for(&update.session, &self.devices))
            .or_else(|| update.session.source_format.clone());
        // Older checkpoints lack a native format. Validate IDs/controls even
        // offline; dimension validation follows when the exact input returns.
        if source.as_ref().is_some_and(|source| {
            !(8000..=384000).contains(&source.sample_rate())
                || !(1..=32).contains(&source.channels())
        }) {
            return Err("Unsupported input format".into());
        }
        let inputs = source
            .as_ref()
            .map(Source::channels)
            .or_else(|| {
                update
                    .session
                    .preset
                    .outputs
                    .first()
                    .and_then(|o| o.channels.first())
                    .map(|c| c.routes.len())
            })
            .unwrap_or(2);
        // An old offline checkpoint has no sample rate. Validate its saved DSP
        // at the maximum supported rate; actual Nyquist is checked on recovery.
        let rate = source.as_ref().map_or(384000, Source::sample_rate);
        let mut ids = std::collections::HashSet::new();
        for output in &update.session.preset.outputs {
            if output.device_id.is_empty()
                || output.device_id.len() > 4096
                || !ids.insert(&output.device_id)
            {
                return Err("Invalid/duplicate output IDs".into());
            }
            // Incompatible outputs stay pending. Their saved layout must not
            // prevent edits/recovery of compatible peers or erase calibration.
            output
                .config()
                .validate(inputs, output.channels.len(), rate)?;
            if output.enabled
                && update.session.preset.source_mode == crate::preset::SourceMode::Playback
                && update.session.preset.source_id == output.device_id
            {
                return Err("Cannot route loopback into itself".into());
            }
        }
        if let Some(id) = &update.session.preset.main_clock_id
            && !update
                .session
                .preset
                .outputs
                .iter()
                .any(|o| o.device_id == *id && o.enabled)
        {
            return Err("Main clock must refer to an enabled output".into());
        }
        if self.wanted
            && let Some(current) = &self.session
            && (current.preset.source_id != update.session.preset.source_id
                || current.preset.source_mode != update.session.preset.source_mode
                || current.preset.latency_ms != update.session.preset.latency_ms)
        {
            return Err("Stop routing before changing input/buffer".into());
        }
        Ok(())
    }
    fn configure(&mut self, mut update: Update) -> Result<(), String> {
        self.validate(&update)?;
        if let Some(current) = &self.session {
            if update.base_gain == Some(update.session.preset.global_gain_db) {
                update.session.preset.global_gain_db = current.preset.global_gain_db;
            }
            if update.base_mute == Some(update.session.preset.global_mute) {
                update.session.preset.global_mute = current.preset.global_mute;
            }
        }
        if !self.wanted {
            self.expected_source = update
                .session
                .source_format
                .clone()
                .or_else(|| source_for(&update.session, &self.devices));
        }
        self.session = Some(update.session);
        self.audition = update.audition;
        self.checkpoint_dirty = true;
        self.revision = self.revision.wrapping_add(1);
        self.sync_outputs();
        Ok(())
    }
    fn retire(&mut self) {
        self.engine.take();
        self.mock_live = false;
        self.sent.clear();
        self.output_retry.clear();
    }
    fn pause(&mut self, message: &str, suspended: bool) {
        self.suspended = suspended;
        self.retire();
        self.retry = Instant::now() + Duration::from_millis(600);
        self.state = if !self.wanted {
            State::Stopped
        } else if suspended {
            State::Suspended
        } else {
            State::Recovering
        };
        self.report(message.into(), false);
    }
    pub fn devices(&mut self, devices: Vec<DeviceInfo>) {
        if self.devices == devices {
            return;
        }
        self.devices = devices;
        self.output_retry.clear();
        if self.wanted
            && let (Some(session), Some(expected)) = (&self.session, &self.expected_source)
        {
            let current = source_for(session, &self.devices);
            if current.as_ref().is_none_or(|s| !same_format(expected, s))
                && (self.engine.is_some() || self.mock_live)
            {
                self.wanted &= session.preferences.recover_input;
                self.recoveries += u64::from(self.wanted);
                self.pause(
                    "Input disconnected/format changed; waiting for the same input",
                    false,
                );
            }
        }
        self.revision = self.revision.wrapping_add(1);
    }
    fn try_start(&mut self) {
        if !self.wanted
            || self.suspended
            || self.engine.is_some()
            || self.mock_live
            || Instant::now() < self.retry
        {
            return;
        }
        self.retry = Instant::now() + Duration::from_secs(3);
        let Some(session) = &self.session else {
            return;
        };
        let Some(source) = source_for(session, &self.devices) else {
            self.state = State::Recovering;
            self.report(
                "Input offline: waiting for the selected endpoint".into(),
                false,
            );
            return;
        };
        if self
            .expected_source
            .as_ref()
            .is_some_and(|old| !same_format(old, &source))
        {
            self.state = State::FormatBlocked;
            self.report(
                "Input format changed: stop and reconfigure before restarting".into(),
                true,
            );
            return;
        }
        let update = Update {
            session: session.clone(),
            audition: self.audition.clone(),
            base_gain: None,
            base_mute: None,
        };
        if let Err(error) = self.validate(&update) {
            self.state = State::FormatBlocked;
            self.report(error, true);
            return;
        }
        let session = self.session.as_mut().unwrap();
        session.source_format = Some(source.clone());
        self.expected_source = Some(source.clone());
        let outputs: Vec<_> = session
            .preset
            .outputs
            .iter()
            .filter(|o| o.enabled)
            .filter_map(|o| {
                self.devices
                    .iter()
                    .find(|d| {
                        d.id == o.device_id
                            && d.kind == DeviceKind::Playback
                            && d.channels == o.channels.len()
                    })
                    .map(|d| {
                        (
                            d.clone(),
                            effective(&o.config(), &o.device_id, &self.audition),
                        )
                    })
            })
            .collect();
        if outputs.is_empty() {
            self.state = State::Recovering;
            self.report("Waiting for an enabled output to reconnect".into(), false);
            return;
        }
        if self.test_mode {
            self.mock_live = true;
            self.state = State::Running;
            self.report("TEST ENGINE: no audio streams opened".into(), false);
            return;
        }
        match Engine::start(
            source,
            outputs,
            session.preset.latency_ms,
            session.preset.global_volume(),
        ) {
            Ok(engine) => {
                self.engine = Some(engine);
                self.state = State::Starting;
                self.checkpoint_dirty = true;
                self.report("Opening streams in background engine".into(), false);
                self.sync_outputs();
            }
            Err(error) => {
                self.state = State::Recovering;
                self.report(error, true);
            }
        }
    }
    fn sync_outputs(&mut self) {
        let (Some(engine), Some(session)) = (&mut self.engine, &self.session) else {
            return;
        };
        let mut error = None;
        let active: Vec<_> = engine.outputs.iter().map(|h| h.device_id.clone()).collect();
        for id in active {
            let wanted = session
                .preset
                .outputs
                .iter()
                .find(|o| o.device_id == id && o.enabled);
            let device = self
                .devices
                .iter()
                .find(|d| d.id == id && d.kind == DeviceKind::Playback);
            if wanted.is_none()
                || device.is_none()
                || wanted
                    .zip(device)
                    .is_some_and(|(o, d)| o.channels.len() != d.channels)
            {
                engine.remove_output(&id, None);
                self.sent.remove(&id);
            }
        }
        for saved in session.preset.outputs.iter().filter(|o| o.enabled) {
            let Some(device) = self.devices.iter().find(|d| {
                d.id == saved.device_id
                    && d.kind == DeviceKind::Playback
                    && d.channels == saved.channels.len()
            }) else {
                continue;
            };
            let config = effective(&saved.config(), &saved.device_id, &self.audition);
            if !engine
                .outputs
                .iter()
                .any(|h| h.device_id == saved.device_id)
                && self
                    .output_retry
                    .get(&saved.device_id)
                    .is_none_or(|t| Instant::now() >= *t)
                && let Err(message) = engine.add_output(device.clone(), config.clone())
            {
                self.output_retry.insert(
                    saved.device_id.clone(),
                    Instant::now() + Duration::from_secs(3),
                );
                error = Some(message);
            }
            if let Some(handle) = engine
                .outputs
                .iter()
                .find(|h| h.device_id == saved.device_id)
                && self
                    .sent
                    .get(&saved.device_id)
                    .is_none_or(|(generation, sent)| {
                        *generation != handle.generation || sent != &config
                    })
                && handle.updates.send(config.clone()).is_ok()
            {
                self.sent
                    .insert(saved.device_id.clone(), (handle.generation, config));
            }
        }
        let _ = engine.set_global_volume(session.preset.global_volume());
        engine.set_auto_align(session.preset.auto_align);
        engine.set_spectrum(session.preferences.spectrum);
        let clock = session
            .preset
            .main_clock_id
            .as_deref()
            .filter(|id| engine.outputs.iter().any(|h| h.device_id == *id));
        let _ = engine.set_main_clock(clock);
        if let Some(message) = error {
            self.report(message, true);
        }
    }
    pub fn tick(&mut self) {
        let events: Vec<_> = self
            .engine
            .as_mut()
            .map(|engine| {
                engine.reap_finished_workers();
                engine.events.try_iter().collect()
            })
            .unwrap_or_default();
        for event in events {
            match event {
                EngineEvent::SourceError(message) => {
                    let recover = self
                        .session
                        .as_ref()
                        .is_some_and(|s| s.preferences.recover_input);
                    self.retire();
                    self.wanted &= recover;
                    self.state = if self.wanted {
                        State::Recovering
                    } else {
                        State::Stopped
                    };
                    self.retry = Instant::now() + Duration::from_secs(3);
                    self.recoveries += u64::from(self.wanted);
                    self.report(message, true);
                }
                EngineEvent::OutputError {
                    device_id,
                    generation,
                    message,
                } => {
                    if self
                        .engine
                        .as_mut()
                        .is_some_and(|e| e.remove_output(&device_id, Some(generation)))
                    {
                        self.sent.remove(&device_id);
                        self.output_retry
                            .insert(device_id, Instant::now() + Duration::from_secs(3));
                        self.report(message, true);
                    }
                }
                EngineEvent::SourceReady(message) => {
                    if self.engine.is_some() {
                        self.state = State::Running;
                        self.report(message, false);
                    }
                }
                EngineEvent::OutputReady {
                    device_id,
                    generation,
                    message,
                } => {
                    if self.engine.as_ref().is_some_and(|e| {
                        e.outputs
                            .iter()
                            .any(|o| o.device_id == device_id && o.generation == generation)
                    }) {
                        self.report(message, false);
                    }
                }
            }
        }
        self.try_start();
        self.sync_outputs();
        if self.mock_live {
            self.mock_frames += 480;
        }
        self.checkpoint(false);
    }
    pub fn request(&mut self, peer: u32, request: Request) -> Result<bool, String> {
        match request {
            Request::Status => {}
            Request::Configure(update) => self.configure(*update)?,
            Request::Start(update) => {
                if update.session.preset.source_mode != crate::preset::SourceMode::Tone
                    && update.session.preset.source_id.is_empty()
                {
                    return Err("Select an input endpoint first".into());
                }
                if !update.session.preset.outputs.iter().any(|o| o.enabled) {
                    return Err("Enable at least one output".into());
                }
                self.configure(*update)?;
                self.wanted = true;
                self.suspended = false;
                self.retry = Instant::now();
                self.try_start();
            }
            Request::Stop => {
                self.wanted = false;
                self.suspended = false;
                self.retire();
                self.state = State::Stopped;
                self.report("Routing stopped".into(), false);
            }
            Request::ReAlign => {
                if let Some(engine) = &mut self.engine {
                    engine.realign();
                }
            }
            Request::Show => {
                self.show_epoch = self.show_epoch.wrapping_add(1);
                self.revision = self.revision.wrapping_add(1);
            }
            Request::AttachUi { window } => {
                if !crate::process::window_belongs_to(window, peer) {
                    return Err("Console window does not belong to this client".into());
                }
                self.owner = Some((peer, window));
            }
            Request::DetachUi => {
                if self.owner.is_some_and(|(pid, _)| pid == peer) {
                    self.owner = None;
                }
            }
            Request::Shutdown => {
                self.checkpoint(true);
                self.wanted = false;
                self.retire();
                self.state = State::Stopped;
                return Ok(true);
            }
            Request::TestDevices(devices) => {
                if !self.test_mode {
                    return Err("Test commands disabled".into());
                }
                self.devices(devices);
                self.retry = Instant::now();
            }
            Request::TestPower { resume } => {
                if !self.test_mode {
                    return Err("Test commands disabled".into());
                }
                self.power(resume);
            }
        }
        self.revision = self.revision.wrapping_add(1);
        Ok(false)
    }
    fn power(&mut self, resume: bool) {
        if !resume {
            self.pause("Windows suspended: audio parked", true);
        } else if self.suspended {
            self.wanted &= self
                .session
                .as_ref()
                .is_some_and(|s| s.preferences.recover_input);
            self.recoveries += u64::from(self.wanted);
            self.pause("Windows resumed: reopening the same endpoints", false);
        }
    }
    fn checkpoint(&mut self, force: bool) {
        if !self.persistence_allowed
            || !self.checkpoint_dirty
            || (!force && Instant::now() < self.checkpoint_due)
        {
            return;
        }
        self.checkpoint_due = Instant::now() + Duration::from_secs(2);
        if let (Some(path), Some(session)) = (workflow::session_path(), &self.session) {
            match workflow::write(&path, session) {
                Ok(()) => self.checkpoint_dirty = false,
                Err(error) => self.report(error, true),
            }
        }
    }
    pub fn snapshot(&self) -> Snapshot {
        let mut outputs: Vec<protocol::Output> = self
            .engine
            .as_ref()
            .map(|engine| {
                engine
                    .outputs
                    .iter()
                    .map(|h| protocol::Output {
                        device_id: h.device_id.clone(),
                        generation: h.generation,
                        ready: h.is_ready(),
                        stats: h.stats.try_lock().map(|s| s.clone()).unwrap_or_default(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        if self.mock_live
            && let Some(session) = &self.session
        {
            outputs = session
                .preset
                .outputs
                .iter()
                .filter(|o| {
                    o.enabled
                        && self
                            .devices
                            .iter()
                            .any(|d| d.id == o.device_id && d.channels == o.channels.len())
                })
                .enumerate()
                .map(|(index, o)| protocol::Output {
                    device_id: o.device_id.clone(),
                    generation: index as u64 + 1,
                    ready: true,
                    stats: audio::OutputStats {
                        frames_written: self.mock_frames,
                        peaks: vec![0.0; o.channels.len()],
                        limiter_latency_ms: 5.0,
                        ..Default::default()
                    },
                })
                .collect();
        }
        Snapshot {
            version: protocol::VERSION,
            engine_pid: std::process::id(),
            revision: self.revision,
            state: self.state,
            wanted: self.wanted,
            live: self.mock_live || self.engine.as_ref().is_some_and(Engine::is_live),
            source: self.expected_source.clone(),
            session: self.session.clone(),
            audition: self.audition.clone(),
            devices: self.devices.clone(),
            outputs,
            main_clock_id: self
                .engine
                .as_ref()
                .and_then(|e| e.main_clock_device_id())
                .map(str::to_owned),
            message: self.message.clone(),
            is_error: self.is_error,
            recoveries: self.recoveries,
            show_epoch: self.show_epoch,
            test_mode: self.test_mode,
            tray_available: self.tray_available,
        }
    }
}

pub fn test_devices() -> Vec<DeviceInfo> {
    [
        ("test-a", "Test output A", DeviceKind::Playback),
        ("test-b", "Test output B", DeviceKind::Playback),
        ("test-input", "Test input", DeviceKind::Capture),
    ]
    .into_iter()
    .map(|(id, name, kind)| DeviceInfo {
        id: id.into(),
        name: name.into(),
        kind,
        channels: 2,
        sample_rate: 48000,
        channel_mask: 3,
    })
    .collect()
}

#[cfg(windows)]
pub fn run(instance: &str, test_mode: bool) -> Result<(), String> {
    use std::sync::{Arc, Mutex};
    let identity = crate::ipc::Instance::new(instance)?;
    let Some(_guard) = identity.guard("engine")? else {
        return Ok(());
    };
    if test_mode && instance == "default" {
        return Err("Test engines require an isolated --instance".into());
    }
    let mut core = Core::new(test_mode);
    if instance != "default" {
        core.persistence_allowed = false;
    }
    core.devices = if test_mode {
        test_devices()
    } else {
        audio::enumerate_devices()?
    };
    if core.persistence_allowed
        && let Some(path) = workflow::session_path().filter(|p| p.exists())
    {
        match workflow::read(&path) {
            Ok(session) => {
                core.wanted =
                    session.preferences.restore_session && session.preferences.auto_start_audio;
                core.expected_source = session.source_format.clone();
                core.session = Some(session);
            }
            Err(error) => {
                core.persistence_allowed = false;
                core.report(
                    format!("Checkpoint unreadable; preserving it: {error}"),
                    true,
                );
            }
        }
    }
    let watcher = if test_mode {
        None
    } else {
        Some(audio::DeviceWatcher::start()?)
    };
    let tray = Tray::start(eframe::egui::Context::default(), 0).ok();
    core.tray_available = tray.is_some();
    let (tx, rx) = mpsc::sync_channel::<(u32, Request, mpsc::SyncSender<Reply>)>(32);
    let cached = Arc::new(Mutex::new(core.snapshot()));
    let shared = cached.clone();
    let _listener = crate::ipc::Listener::start(instance, move |peer, bytes| {
        let envelope: protocol::Envelope = crate::ipc::decode(&bytes)?;
        if envelope.version != protocol::VERSION {
            return Err("Unsupported engine protocol".into());
        }
        if matches!(envelope.request, Request::Status) {
            return serde_json::to_vec(&Reply {
                version: protocol::VERSION,
                snapshot: shared.lock().unwrap_or_else(|e| e.into_inner()).clone(),
                error: None,
            })
            .map_err(|e| e.to_string());
        }
        let (reply, result) = mpsc::sync_channel(1);
        tx.try_send((peer, envelope.request, reply))
            .map_err(|_| "Engine command queue full".to_string())?;
        let reply = result
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| "Engine command timed out".to_string())?;
        serde_json::to_vec(&reply).map_err(|e| e.to_string())
    })?;
    let mut running = true;
    let mut publish = Instant::now();
    let mut shown = 0;
    while running {
        if let Some(watcher) = &watcher {
            for result in watcher.events.try_iter() {
                match result {
                    Ok(devices) => core.devices(devices),
                    Err(error) => core.report(error, true),
                }
            }
        }
        if let Some(tray) = &tray {
            for action in tray.events.try_iter() {
                match action {
                    Action::Show => {
                        core.show_epoch = core.show_epoch.wrapping_add(1);
                    }
                    Action::Mute | Action::Louder | Action::Quieter => {
                        if let Some(session) = &mut core.session {
                            match action {
                                Action::Mute => {
                                    session.preset.global_mute = !session.preset.global_mute
                                }
                                Action::Louder => {
                                    session.preset.global_gain_db =
                                        (session.preset.global_gain_db + 1.0).min(0.0)
                                }
                                Action::Quieter => {
                                    session.preset.global_gain_db =
                                        (session.preset.global_gain_db - 1.0).max(-60.0)
                                }
                                _ => {}
                            }
                            core.checkpoint_dirty = true;
                            core.revision = core.revision.wrapping_add(1);
                            core.sync_outputs();
                        }
                    }
                    Action::ToggleRouting => {
                        if core.wanted {
                            let _ = core.request(0, Request::Stop);
                        } else if let Some(session) = core.session.clone() {
                            let _ = core.request(
                                0,
                                Request::Start(Box::new(Update {
                                    session,
                                    audition: core.audition.clone(),
                                    base_gain: None,
                                    base_mute: None,
                                })),
                            );
                        }
                    }
                    Action::Exit => {
                        core.checkpoint(true);
                        running = false;
                    }
                    Action::Suspend => core.power(false),
                    Action::Resume => core.power(true),
                }
            }
        }
        if core.show_epoch != shown {
            shown = core.show_epoch;
            if let Some((pid, window)) = core
                .owner
                .filter(|(pid, window)| crate::process::window_belongs_to(*window, *pid))
            {
                let _ = pid;
                crate::process::show_window(window);
            } else if Instant::now() >= core.launch_due {
                core.launch_due = Instant::now() + Duration::from_secs(3);
                if let Err(error) = crate::process::launch_console(instance, test_mode) {
                    core.report(
                        format!("Could not open console: {error}; audio is unchanged"),
                        true,
                    );
                }
            }
        }
        match rx.recv_timeout(Duration::from_millis(10)) {
            Ok((peer, request, reply)) => {
                let result = core.request(peer, request);
                let shutdown = result.as_ref().is_ok_and(|value| *value);
                if let Err(error) = &result {
                    core.report(error.clone(), true);
                }
                core.tick();
                let snapshot = core.snapshot();
                *cached.lock().unwrap_or_else(|e| e.into_inner()) = snapshot.clone();
                let _ = reply.send(Reply {
                    version: protocol::VERSION,
                    snapshot,
                    error: result.err(),
                });
                if shutdown {
                    running = false;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => core.tick(),
            Err(mpsc::RecvTimeoutError::Disconnected) => running = false,
        }
        if publish.elapsed() >= Duration::from_millis(50) {
            publish = Instant::now();
            *cached.lock().unwrap_or_else(|e| e.into_inner()) = core.snapshot();
        }
    }
    core.wanted = false;
    core.retire();
    core.checkpoint(true);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preset::{Preset, SavedOutput, SourceMode};
    fn update() -> Update {
        let mut config = OutputConfig::identity(2, 2);
        config.delay_ms = 127.0;
        config.gain_db = -12.0;
        config.channels[1].invert = true;
        config.eq[2].gain_db = 3.0;
        config.limiter.true_peak = true;
        let mut preset: Preset = serde_json::from_str(
            r#"{"version":1,"source_mode":"Capture","source_id":"test-input","latency_ms":30,"outputs":[]}"#,
        ).unwrap();
        preset.main_clock_id = Some("test-a".into());
        preset.outputs = vec![
            SavedOutput::new("test-a".into(), true, &config),
            SavedOutput::new("test-b".into(), true, &config),
        ];
        Update {
            session: Session {
                version: 1,
                preferences: Default::default(),
                preset_path: "calibration.json".into(),
                preset,
                scenes: vec![],
                source_format: Some(Source::Endpoint(test_devices()[2].clone())),
            },
            audition: Audition {
                devices: vec!["test-a".into()],
                channels: vec![],
            },
            base_gain: None,
            base_mute: None,
        }
    }
    fn started() -> Core {
        let mut core = Core::new(true);
        core.devices(test_devices());
        core.request(0, Request::Start(Box::new(update()))).unwrap();
        assert!(core.snapshot().live);
        core
    }
    fn retry(core: &mut Core) {
        core.retry = Instant::now();
        core.tick();
    }
    fn mix(core: &Core) -> Vec<u8> {
        serde_json::to_vec(&(core.session.clone(), core.audition.clone())).unwrap()
    }
    #[test]
    fn detach_and_console_crash_do_not_stop_or_change_the_mix() {
        let mut core = started();
        core.owner = Some((111, 222));
        let before = mix(&core);
        core.request(111, Request::DetachUi).unwrap();
        for _ in 0..100 {
            core.tick();
        }
        assert!(core.snapshot().live);
        assert!(core.mock_frames > 0);
        assert_eq!(mix(&core), before);
        // An abandoned owner after a crash is not consulted by audio processing.
        core.owner = Some((111, 222));
        core.tick();
        assert!(core.snapshot().live);
    }
    #[test]
    fn input_recovery_is_exact_and_preserves_calibration_and_audition() {
        let mut core = started();
        let before = mix(&core);
        let mut devices = test_devices();
        devices[2].id = "another-input".into();
        core.devices(devices);
        retry(&mut core);
        assert_eq!(core.state, State::Recovering);
        assert!(!core.snapshot().live);
        assert!(core.wanted);
        core.devices(test_devices());
        retry(&mut core);
        assert_eq!(core.state, State::Running);
        assert_eq!(mix(&core), before);
        assert_eq!(core.recoveries, 1);
    }
    #[test]
    fn changed_input_format_blocks_recovery_until_explicit_reconfiguration() {
        let mut core = started();
        let mut devices = test_devices();
        devices[2].sample_rate = 44100;
        core.devices(devices.clone());
        retry(&mut core);
        assert_eq!(core.state, State::FormatBlocked);
        assert!(!core.mock_live);
        let mut changed = update();
        changed.session.source_format = Some(Source::Endpoint(devices[2].clone()));
        assert!(
            core.request(0, Request::Configure(Box::new(changed.clone())))
                .is_err()
        );
        core.request(0, Request::Stop).unwrap();
        core.request(0, Request::Start(Box::new(changed))).unwrap();
        assert!(core.mock_live);
    }
    #[test]
    fn suspend_resume_is_coalesced_and_stop_cancels_pending_recovery() {
        let mut core = started();
        let before = mix(&core);
        core.power(false);
        assert_eq!(core.state, State::Suspended);
        retry(&mut core);
        assert!(!core.mock_live);
        core.power(true);
        core.power(true);
        assert_eq!(core.recoveries, 1);
        retry(&mut core);
        assert!(core.mock_live);
        assert_eq!(mix(&core), before);
        core.power(false);
        core.request(0, Request::Stop).unwrap();
        core.power(true);
        retry(&mut core);
        assert_eq!(core.state, State::Stopped);
        assert!(!core.wanted && !core.mock_live);
    }
    #[test]
    fn disabled_recovery_does_not_restart_after_disconnect_or_resume() {
        for power in [false, true] {
            let mut core = started();
            core.session.as_mut().unwrap().preferences.recover_input = false;
            if power {
                core.power(false);
                core.power(true);
            } else {
                core.devices(test_devices()[..2].to_vec());
                core.devices(test_devices());
            }
            retry(&mut core);
            assert!(!core.wanted && !core.mock_live);
            assert_eq!(core.state, State::Stopped);
        }
    }
    #[test]
    fn output_disconnect_does_not_restart_input_or_forget_main_clock() {
        let mut core = started();
        let before = mix(&core);
        core.devices(test_devices()[1..].to_vec());
        assert!(core.mock_live);
        assert_eq!(core.snapshot().outputs.len(), 1);
        core.devices(test_devices());
        core.tick();
        assert_eq!(core.snapshot().outputs.len(), 2);
        assert_eq!(mix(&core), before);
    }
    #[test]
    fn incompatible_output_layout_and_parked_input_do_not_block_peer_edits() {
        let mut core = started();
        let mut devices = test_devices();
        devices[0].channels = 6;
        core.devices(devices.clone());
        core.tick();
        assert_eq!(core.snapshot().outputs.len(), 1);
        let mut updated = update();
        updated.session.preset.outputs[1].gain_db = -24.0;
        core.configure(updated.clone()).unwrap();
        assert_eq!(
            core.session.as_ref().unwrap().preset.outputs[0].delay_ms,
            127.0
        );
        devices[2].channels = 6;
        core.devices(devices);
        retry(&mut core);
        assert_eq!(core.state, State::FormatBlocked);
        updated.session.preset.outputs[1].gain_db = -30.0;
        core.configure(updated).unwrap();
        assert_eq!(
            core.session.as_ref().unwrap().preset.outputs[1].gain_db,
            -30.0
        );
        assert!(!core.snapshot().live);
    }
    #[test]
    fn invalid_requests_are_transactional_including_offline_checkpoints() {
        let mut core = started();
        let before = mix(&core);
        let mut bad = update();
        bad.session
            .preset
            .outputs
            .push(bad.session.preset.outputs[0].clone());
        assert!(core.request(0, Request::Configure(Box::new(bad))).is_err());
        let mut bad = update();
        bad.session.preset.outputs[0].channels[0].routes = vec![1.0];
        assert!(core.request(0, Request::Configure(Box::new(bad))).is_err());
        let mut bad = update();
        bad.session
            .preset
            .outputs
            .iter_mut()
            .for_each(|o| o.enabled = false);
        assert!(core.request(0, Request::Start(Box::new(bad))).is_err());
        assert_eq!(mix(&core), before);
        core.request(0, Request::Stop).unwrap();
        core.devices(vec![]);
        let mut bad = update();
        bad.session.source_format = None;
        bad.session
            .preset
            .outputs
            .push(bad.session.preset.outputs[0].clone());
        assert!(core.request(0, Request::Configure(Box::new(bad))).is_err());
    }
    #[test]
    fn feedback_and_mismatched_source_metadata_are_rejected() {
        let core = Core::new(true);
        let mut bad = update();
        bad.session.preset.source_mode = SourceMode::Playback;
        bad.session.preset.source_id = "test-a".into();
        bad.session.source_format = Some(Source::Endpoint(test_devices()[0].clone()));
        assert!(core.validate(&bad).is_err());
        bad = update();
        bad.session.source_format = Some(Source::Tone);
        assert!(core.validate(&bad).is_err());
    }
    #[test]
    fn tray_global_changes_survive_unrelated_ui_edits_but_user_edits_win() {
        let mut core = started();
        core.session.as_mut().unwrap().preset.global_gain_db = -15.0;
        core.session.as_mut().unwrap().preset.global_mute = true;
        let mut stale = update();
        stale.base_gain = Some(0.0);
        stale.base_mute = Some(false);
        core.configure(stale.clone()).unwrap();
        assert_eq!(core.session.as_ref().unwrap().preset.global_gain_db, -15.0);
        assert!(core.session.as_ref().unwrap().preset.global_mute);
        stale.session.preset.global_gain_db = -9.0;
        core.configure(stale).unwrap();
        assert_eq!(core.session.as_ref().unwrap().preset.global_gain_db, -9.0);
    }
    #[test]
    fn stopped_engine_remains_stopped_through_power_events_and_test_commands_are_scoped() {
        let mut core = Core::new(true);
        core.power(false);
        core.power(true);
        retry(&mut core);
        assert_eq!(core.state, State::Stopped);
        assert!(!core.wanted);
        let mut real = Core::new(false);
        real.persistence_allowed = false;
        assert!(
            real.request(0, Request::TestPower { resume: true })
                .is_err()
        );
        assert!(real.request(0, Request::TestDevices(vec![])).is_err());
    }
    #[test]
    fn long_controller_regression_retains_mix_without_audio_or_checkpoint_writes() {
        let mut core = started();
        let before = mix(&core);
        for _ in 0..500 {
            core.power(false);
            core.power(true);
            retry(&mut core);
            core.devices(test_devices()[1..].to_vec());
            core.tick();
            core.devices(test_devices());
            core.tick();
            assert!(core.snapshot().live);
            assert_eq!(mix(&core), before);
        }
        assert_eq!(core.recoveries, 500);
        assert!(!core.persistence_allowed);
        assert_eq!(core.engine.as_ref().map(|_| ()), None);
    }
    #[test]
    #[cfg(windows)]
    #[ignore = "requires Realtek + LG outputs; muted streams, no persistence or physical power changes"]
    fn hardware_live_output_switching_background_lifecycle_and_health() {
        let devices = audio::enumerate_devices().unwrap();
        let outputs: Vec<_> = devices
            .iter()
            .filter(|d| d.kind == DeviceKind::Playback)
            .filter(|d| {
                (d.name.contains("Realtek") && !d.name.contains("Digital"))
                    || d.name.contains("LG ULTRAGEAR")
            })
            .take(2)
            .cloned()
            .collect();
        assert_eq!(outputs.len(), 2, "requires two muted test outputs");
        let mut update = update();
        update.audition = Audition::default();
        update.session.preset.source_mode = SourceMode::Tone;
        update.session.preset.source_id.clear();
        update.session.source_format = Some(Source::Tone);
        update.session.preferences.spectrum = true;
        update.session.preset.global_mute = true;
        update.session.preset.main_clock_id = Some(outputs[0].id.clone());
        update.session.preset.outputs = outputs
            .iter()
            .map(|d| {
                let mut config = OutputConfig::identity(2, d.channels);
                config.mute = true;
                config.delay_ms = 15.0;
                SavedOutput::new(d.id.clone(), true, &config)
            })
            .collect();
        let mut core = Core::new(false);
        core.persistence_allowed = false;
        core.devices(devices.clone());
        core.request(0, Request::Start(Box::new(update))).unwrap();
        let wait = |core: &mut Core| {
            let deadline = Instant::now() + Duration::from_secs(8);
            loop {
                core.tick();
                let snapshot = core.snapshot();
                if snapshot.live
                    && snapshot.outputs.len() == 2
                    && snapshot
                        .outputs
                        .iter()
                        .all(|o| o.ready && o.stats.frames_written > 4800)
                {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "background streams failed: {}",
                    snapshot.message
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        wait(&mut core);
        let generation = core.engine.as_ref().unwrap().outputs[0].generation;
        let before = mix(&core);
        core.request(1, Request::DetachUi).unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            core.tick();
            let snapshot = core.snapshot();
            assert!(snapshot.live);
            assert_eq!(
                core.engine.as_ref().unwrap().outputs[0].generation,
                generation
            );
            assert!(
                snapshot
                    .outputs
                    .iter()
                    .all(|o| o.stats.peaks.iter().all(|p| *p == 0.0))
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let snapshot = core.snapshot();
        assert!(snapshot.outputs.iter().all(|o| o.stats.budget_ms > 0.0
            && o.stats.processing_peak_ms > 0.0
            && o.stats.realtime_scheduled
            && o.stats.spectrum.len() == matrix_core::spectrum::BINS));
        eprintln!(
            "Background renderer health: {}",
            serde_json::to_string(&snapshot.outputs).unwrap()
        );
        core.power(false);
        assert!(!core.snapshot().live);
        core.power(true);
        core.power(true);
        wait(&mut core);
        assert_eq!(mix(&core), before);
        assert_eq!(core.recoveries, 1);
        core.request(0, Request::Stop).unwrap();
        assert!(!core.snapshot().wanted && !core.snapshot().live);
    }
}
