use crate::client::Engine;
use crate::ui_state::{History, Meter};
use crate::{
    audio::{self, DeviceInfo, DeviceKind, OutputStats, Source},
    eq_presets::{self, EqPreset},
    preset::{Preset, SavedEqPreset, SavedOutput, SourceMode},
};
use crate::{
    tray::{Action, Tray},
    workflow::{Preferences, Scene, Session},
};
use eframe::egui::{self, Color32, RichText};
use matrix_core::{EqBand, GlobalVolume, OutputConfig, gain_to_db};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    time::{Duration, Instant},
};

const ACCENT: Color32 = Color32::from_rgb(87, 222, 173);
const AMBER: Color32 = Color32::from_rgb(239, 185, 91);
const DIM: Color32 = Color32::from_rgb(145, 156, 175);
type PickerResult = (bool, Result<Option<std::path::PathBuf>, String>);

#[derive(Clone)]
struct OutputState {
    device: DeviceInfo,
    enabled: bool,
    config: OutputConfig,
    alias: String,
    stereo_link: bool,
    available: bool,
}

#[derive(Default)]
struct Runtime {
    watcher: Option<audio::DeviceWatcher>,
    tray: Option<Tray>,
    preferences: Preferences,
    scenes: Vec<Scene>,
    scene_name: String,
    solo_devices: HashSet<String>,
    solo_channels: HashSet<(String, usize)>,
    focused: Option<(String, Option<usize>)>,
    retries: HashMap<String, Instant>,
    settings_dirty: bool,
    disabled_persistence: bool,
    hidden: bool,
    quitting: bool,
    pending_session: Option<Preset>,
    pending_autostart: bool,
    remote: Option<crate::client::Client>,
    remote_snapshot: Option<crate::protocol::Snapshot>,
    remote_revision: u64,
    show_epoch: u64,
    remote_error: bool,
    native_window: u64,
    last_payload: Vec<u8>,
    last_gain: Option<f32>,
    last_mute: Option<bool>,
}

#[derive(Clone, PartialEq)]
struct BoardSnapshot {
    outputs: Vec<(String, bool, String, bool, OutputConfig)>,
    source_mode: SourceMode,
    source_id: String,
    latency_ms: u32,
    main_clock_id: Option<String>,
    auto_align: bool,
    global_volume: GlobalVolume,
    eq_presets: Vec<EqPreset>,
}

pub struct MatrixApp {
    devices: Vec<DeviceInfo>,
    outputs: Vec<OutputState>,
    source_mode: SourceMode,
    source_id: String,
    latency_ms: u32,
    main_clock_id: Option<String>,
    auto_align: bool,
    global_volume: GlobalVolume,
    eq_presets: Vec<EqPreset>,
    engine: Option<Engine>,
    status: String,
    is_error: bool,
    preset_path: String,
    saved: Option<BoardSnapshot>,
    saved_path: Option<String>,
    history_epoch: u64,
    history: History<BoardSnapshot>,
    pending_edit: Option<BoardSnapshot>,
    picker: Option<std::sync::mpsc::Receiver<PickerResult>>,
    runtime: Runtime,
}

fn configure_dark_theme(ctx: &egui::Context) {
    // Fix the preference too: eframe otherwise follows Windows' light theme.
    ctx.set_theme(egui::ThemePreference::Dark);
    ctx.send_viewport_cmd(egui::ViewportCommand::SetTheme(egui::SystemTheme::Dark));
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = Color32::from_rgb(15, 17, 22);
    visuals.faint_bg_color = Color32::from_rgb(23, 26, 33);
    visuals.extreme_bg_color = Color32::from_rgb(9, 11, 15);
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(30, 34, 43);
    visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(30, 34, 43);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(44, 51, 63);
    visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(44, 51, 63);
    visuals.widgets.active.bg_fill = Color32::from_rgb(35, 93, 77);
    visuals.widgets.active.weak_bg_fill = Color32::from_rgb(35, 93, 77);
    visuals.window_fill = Color32::from_rgb(22, 28, 38);
    visuals.selection.bg_fill = Color32::from_rgb(35, 93, 77);
    visuals.selection.stroke.color = ACCENT;
    ctx.set_visuals(visuals);
    ctx.style_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style
            .text_styles
            .insert(egui::TextStyle::Body, egui::FontId::proportional(14.0));
        style
            .text_styles
            .insert(egui::TextStyle::Button, egui::FontId::proportional(13.0));
        style
            .text_styles
            .insert(egui::TextStyle::Small, egui::FontId::proportional(11.0));
        style.spacing.button_padding = egui::vec2(9.0, 5.0);
    });
}

impl MatrixApp {
    fn apply_devices(&mut self, devices: Vec<DeviceInfo>) {
        if self.devices == devices
            && (self.engine.is_some()
                || !self.outputs.iter().any(|o| {
                    devices
                        .iter()
                        .any(|d| d.id == o.device.id && d.channels != o.config.channels.len())
                }))
        {
            return;
        }
        self.clear_history();
        let inputs = self.source().map_or(2, |s| s.channels());
        for output in &mut self.outputs {
            let found = devices
                .iter()
                .find(|d| d.kind == DeviceKind::Playback && d.id == output.device.id);
            output.available = found.is_some_and(|d| d.channels == output.config.channels.len());
            if let Some(device) = found {
                if output.available {
                    if !self.devices.iter().any(|d| d.id == device.id) {
                        self.runtime.retries.remove(&device.id);
                    }
                    output.device = device.clone();
                } else if self.engine.is_none() {
                    // A changed speaker layout needs explicit re-enabling. Keep
                    // common channel settings and all device timing/protection.
                    let mut channels = OutputConfig::identity(inputs, device.channels).channels;
                    for (new, old) in channels.iter_mut().zip(&output.config.channels) {
                        *new = old.clone();
                    }
                    output.config.channels = channels;
                    output.device = device.clone();
                    output.available = true;
                    output.enabled = false;
                }
            }
        }
        for device in devices.iter().filter(|d| d.kind == DeviceKind::Playback) {
            if !self.outputs.iter().any(|o| o.device.id == device.id) {
                self.outputs.push(OutputState {
                    device: device.clone(),
                    enabled: false,
                    config: OutputConfig::identity(inputs, device.channels),
                    alias: String::new(),
                    stereo_link: false,
                    available: true,
                });
            }
        }
        self.devices = devices;
    }
    fn poll_devices(&mut self) {
        let updates: Vec<_> = self
            .runtime
            .watcher
            .as_ref()
            .map(|w| w.events.try_iter().collect())
            .unwrap_or_default();
        for update in updates {
            match update {
                Ok(devices) => self.apply_devices(devices),
                Err(error) => self.report(Err(error)),
            }
        }
        let ready = self.runtime.pending_session.as_ref().is_some_and(|p| {
            p.source_mode == SourceMode::Tone || self.devices.iter().any(|d| d.id == p.source_id)
        });
        if ready && let Some(preset) = self.runtime.pending_session.take() {
            match self.apply_preset(preset) {
                Ok(_) => {
                    self.report(Ok("Restored last session".into()));
                    if self.runtime.remote.is_none() && self.runtime.preferences.auto_start_audio {
                        self.runtime.pending_autostart = true;
                    }
                }
                Err(error) => self.report(Err(format!("Session restore: {error}"))),
            }
        }
        if self.runtime.pending_autostart
            && self.engine.is_none()
            && self.source().is_ok()
            && self.outputs.iter().any(|o| o.enabled && o.available)
        {
            self.start();
        }
    }
    #[cfg(test)]
    fn effective_config(&self, output: &OutputState) -> OutputConfig {
        solo_config(
            output,
            &self.runtime.solo_devices,
            &self.runtime.solo_channels,
        )
    }
    fn session_for_engine(&self) -> Session {
        let preset = self
            .runtime
            .pending_session
            .clone()
            .unwrap_or_else(|| self.preset());
        let source_format = self.source().ok().filter(|source| match source {
            Source::Tone => preset.source_mode == SourceMode::Tone,
            Source::Endpoint(device) => {
                device.id == preset.source_id
                    && match preset.source_mode {
                        SourceMode::Playback => device.kind == DeviceKind::Playback,
                        SourceMode::Capture => device.kind == DeviceKind::Capture,
                        SourceMode::Tone => false,
                    }
            }
        });
        Session {
            version: 1,
            preferences: self.runtime.preferences.clone(),
            preset_path: self.preset_path.clone(),
            preset,
            scenes: self.runtime.scenes.clone(),
            source_format,
        }
    }
    fn checkpoint(&mut self, _force: bool) {
        self.publish_configuration();
    }
    fn publish_configuration(&mut self) {
        if self.runtime.remote.is_none() {
            return;
        }
        let session = self.session_for_engine();
        let mut audition = crate::protocol::Audition {
            devices: self.runtime.solo_devices.iter().cloned().collect(),
            channels: self.runtime.solo_channels.iter().cloned().collect(),
        };
        audition.devices.sort();
        audition.channels.sort();
        let payload = match serde_json::to_vec(&(session.clone(), audition.clone())) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.report(Err(error.to_string()));
                return;
            }
        };
        if payload == self.runtime.last_payload {
            return;
        }
        let update = crate::protocol::Update {
            session,
            audition,
            base_gain: self.runtime.last_gain,
            base_mute: self.runtime.last_mute,
        };
        match self
            .runtime
            .remote
            .as_ref()
            .unwrap()
            .send(crate::protocol::Request::Configure(Box::new(update)))
        {
            Ok(()) => {
                self.runtime.last_payload = payload;
                self.runtime.last_gain = Some(self.global_volume.gain_db);
                self.runtime.last_mute = Some(self.global_volume.mute);
                self.runtime.settings_dirty = false;
            }
            Err(error) => self.report(Err(error)),
        }
    }
    fn attach_engine(&mut self, ctx: &egui::Context, window: u64) {
        let args: Vec<_> = std::env::args().collect();
        let instance = args
            .windows(2)
            .find(|p| p[0] == "--instance")
            .map_or("default", |p| p[1].as_str());
        let test_mode = args.iter().any(|arg| arg == "--test-engine");
        self.runtime.disabled_persistence = instance != "default";
        self.runtime.native_window = window;
        match crate::client::Client::connect(instance, test_mode, ctx.clone(), window) {
            Ok((client, reply)) => {
                if test_mode {
                    self.devices.clear();
                    self.outputs.clear();
                    self.source_id.clear();
                    self.source_mode = SourceMode::Tone;
                }
                let snapshot = reply.snapshot;
                self.runtime.remote_snapshot = Some(snapshot.clone());
                self.apply_devices(snapshot.devices.clone());
                self.runtime.solo_devices = snapshot.audition.devices.iter().cloned().collect();
                self.runtime.solo_channels = snapshot.audition.channels.iter().cloned().collect();
                if let Some(session) = &snapshot.session {
                    self.runtime.preferences = session.preferences.clone();
                    self.runtime.scenes = session.scenes.clone();
                    if snapshot.wanted || session.preferences.restore_session {
                        self.preset_path = session.preset_path.clone();
                        self.source_mode = session.preset.source_mode;
                        self.source_id = session.preset.source_id.clone();
                        if let Err(error) = self.apply_preset(session.preset.clone()) {
                            self.runtime.pending_session = Some(session.preset.clone());
                            self.report(Err(error));
                        }
                    }
                    self.runtime.last_gain = Some(session.preset.global_gain_db);
                    self.runtime.last_mute = Some(session.preset.global_mute);
                }
                self.runtime.remote = Some(client);
                self.runtime.last_payload.clear();
                self.runtime.remote_error = false;
                self.engine = snapshot.wanted.then(|| Engine::from_snapshot(&snapshot));
                self.runtime.remote_revision = snapshot.revision;
                self.runtime.show_epoch = snapshot.show_epoch;
                self.report(if let Some(error) = reply.error {
                    Err(error)
                } else if snapshot.is_error {
                    Err(snapshot.message.clone())
                } else {
                    Ok(snapshot.message.clone())
                });
            }
            Err(error) => {
                self.runtime.remote_error = true;
                self.report(Err(error));
            }
        }
    }
    fn close_engine(&mut self) {
        let Some(remote) = &self.runtime.remote else {
            return;
        };
        // Flush in the same FIFO as live edits; never write calibration files.
        let update = crate::protocol::Update {
            session: self.session_for_engine(),
            audition: crate::protocol::Audition {
                devices: self.runtime.solo_devices.iter().cloned().collect(),
                channels: self.runtime.solo_channels.iter().cloned().collect(),
            },
            base_gain: self.runtime.last_gain,
            base_mute: self.runtime.last_mute,
        };
        remote.finish(update, self.runtime.quitting);
    }
    fn save_scene(&mut self) {
        let result = self.source().and_then(|source| {
            let scene = Scene {
                name: self.runtime.scene_name.trim().into(),
                source_mode: self.source_mode,
                source_id: self.source_id.clone(),
                sample_rate: source.sample_rate(),
                inputs: source.channels(),
                global_gain_db: self.global_volume.gain_db,
                global_mute: self.global_volume.mute,
                outputs: self.preset().outputs,
            };
            scene.validate()?;
            if let Some(existing) = self
                .runtime
                .scenes
                .iter_mut()
                .find(|s| s.name.eq_ignore_ascii_case(&scene.name))
            {
                *existing = scene;
            } else {
                if self.runtime.scenes.len() >= 32 {
                    return Err("At most 32 scenes are supported".into());
                }
                self.runtime.scenes.push(scene);
            }
            self.runtime.settings_dirty = true;
            Ok("Scene stored; timing and limiter settings are excluded from recall".into())
        });
        self.report(result);
    }
    fn recall_scene(&mut self, index: usize) {
        let result = (|| -> Result<String, String> {
            let scene = self.runtime.scenes.get(index).ok_or("Scene unavailable")?;
            scene.validate()?;
            let source = self.source()?;
            if scene.source_mode != self.source_mode
                || scene.source_id != self.source_id
                || scene.sample_rate != source.sample_rate()
                || scene.inputs != source.channels()
            {
                return Err(
                    "This scene belongs to a different input/format; select that input first"
                        .into(),
                );
            }
            for saved in &scene.outputs {
                if let Some(output) = self.outputs.iter().find(|o| o.device.id == saved.device_id) {
                    saved.config().validate(
                        source.channels(),
                        output.device.channels,
                        source.sample_rate(),
                    )?;
                }
            }
            for output in &mut self.outputs {
                if let Some(saved) = scene
                    .outputs
                    .iter()
                    .find(|s| s.device_id == output.device.id)
                {
                    let delay = output.config.delay_ms;
                    let limiter = output.config.limiter;
                    output.config = saved.config();
                    output.config.delay_ms = delay;
                    output.config.limiter = limiter;
                    output.enabled = saved.enabled;
                    output.stereo_link = saved.stereo_link;
                } else {
                    output.enabled = false;
                }
            }
            self.global_volume = GlobalVolume {
                gain_db: scene.global_gain_db,
                mute: scene.global_mute,
            };
            Ok(format!(
                "Scene {} recalled; calibration and protection retained",
                scene.name
            ))
        })();
        self.report(result);
    }
    fn tray_actions(&mut self, ctx: &egui::Context) {
        let actions: Vec<_> = self
            .runtime
            .tray
            .as_ref()
            .map(|t| t.events.try_iter().collect())
            .unwrap_or_default();
        for action in actions {
            match action {
                Action::Show => {
                    self.runtime.hidden = false;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Title("Audio Matrix".into()));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                Action::Mute => self.global_volume.mute = !self.global_volume.mute,
                Action::Louder => {
                    self.global_volume.gain_db = (self.global_volume.gain_db + 1.0).min(0.0)
                }
                Action::Quieter => {
                    self.global_volume.gain_db = (self.global_volume.gain_db - 1.0).max(-60.0)
                }
                Action::ToggleRouting => {
                    if self.engine.is_some() {
                        self.stop();
                    } else {
                        self.start();
                    }
                }
                Action::Suspend | Action::Resume => {}
                Action::Exit => {
                    self.runtime.quitting = true;
                    self.checkpoint(true);
                    self.stop();
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
    }
    fn shortcuts(&mut self, ctx: &egui::Context) -> bool {
        use egui::{Key, KeyboardShortcut, Modifiers};
        let consumed = |key, shift| {
            ctx.input_mut(|i| {
                i.consume_shortcut(&KeyboardShortcut::new(
                    Modifiers {
                        ctrl: true,
                        shift,
                        ..Default::default()
                    },
                    key,
                ))
            })
        };
        if consumed(Key::S, true) {
            self.open_picker(true);
        } else if consumed(Key::S, false) {
            self.save();
        }
        if consumed(Key::M, false) {
            self.global_volume.mute = !self.global_volume.mute;
        }
        let snapshot = if consumed(Key::Z, true) || consumed(Key::Y, false) {
            self.history.redo(self.snapshot())
        } else if consumed(Key::Z, false) {
            self.history.undo(self.snapshot())
        } else {
            None
        };
        if let Some(snapshot) = snapshot {
            self.restore(snapshot);
            return true;
        }
        // Single-letter audition keys never run while editing text/numeric fields.
        if !ctx.wants_keyboard_input()
            && let Some((id, channel)) = self.runtime.focused.clone()
        {
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::S)) {
                if let Some(channel) = channel {
                    toggle_set(&mut self.runtime.solo_channels, (id.clone(), channel));
                } else {
                    toggle_set(&mut self.runtime.solo_devices, id.clone());
                }
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::M))
                && let Some(output) = self.outputs.iter_mut().find(|o| o.device.id == id)
            {
                if let Some(channel) = channel.and_then(|c| output.config.channels.get_mut(c)) {
                    channel.mute = !channel.mute;
                } else {
                    output.config.mute = !output.config.mute;
                }
            }
        }
        false
    }
    fn clear_history(&mut self) {
        self.history = History::default();
        self.pending_edit = None;
        self.history_epoch = self.history_epoch.wrapping_add(1);
    }
    fn snapshot(&self) -> BoardSnapshot {
        BoardSnapshot {
            outputs: self
                .outputs
                .iter()
                .map(|o| {
                    (
                        o.device.id.clone(),
                        o.enabled,
                        o.alias.clone(),
                        o.stereo_link,
                        o.config.clone(),
                    )
                })
                .collect(),
            source_mode: self.source_mode,
            source_id: self.source_id.clone(),
            latency_ms: self.latency_ms,
            main_clock_id: self.main_clock_id.clone(),
            auto_align: self.auto_align,
            global_volume: self.global_volume,
            eq_presets: self.eq_presets.clone(),
        }
    }
    fn restore(&mut self, snapshot: BoardSnapshot) {
        let order: HashMap<_, _> = snapshot
            .outputs
            .iter()
            .enumerate()
            .map(|(i, o)| (o.0.clone(), i))
            .collect();
        for (id, enabled, alias, link, config) in snapshot.outputs {
            if let Some(output) = self.outputs.iter_mut().find(|o| o.device.id == id) {
                output.enabled = enabled;
                output.alias = alias;
                output.stereo_link = link;
                output.config = config;
            }
        }
        self.outputs
            .sort_by_key(|o| order.get(&o.device.id).copied().unwrap_or(usize::MAX));
        self.source_mode = snapshot.source_mode;
        self.source_id = snapshot.source_id;
        self.latency_ms = snapshot.latency_ms;
        self.main_clock_id = snapshot.main_clock_id;
        self.auto_align = snapshot.auto_align;
        self.global_volume = snapshot.global_volume;
        self.eq_presets = snapshot.eq_presets;
        self.pending_edit = None;
    }
    fn open_picker(&mut self, save: bool) {
        if self.picker.is_some() {
            return;
        }
        let path = self.preset_path.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        match std::thread::Builder::new()
            .name("preset-picker".into())
            .spawn(move || {
                let _ = tx.send((save, crate::dialogs::choose(save, &path)));
            }) {
            Ok(_) => self.picker = Some(rx),
            Err(error) => self.report(Err(error.to_string())),
        }
    }
    fn poll_picker(&mut self) {
        let result = self.picker.as_ref().and_then(|rx| match rx.try_recv() {
            Ok(result) => Some(result),
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                Some((false, Err("File picker closed unexpectedly".into())))
            }
        });
        if let Some((save, result)) = result {
            self.picker = None;
            match result {
                Ok(Some(path)) if save || self.engine.is_none() => {
                    let old_path = self.preset_path.clone();
                    self.preset_path = path.to_string_lossy().into_owned();
                    if save {
                        self.save();
                    } else {
                        self.load();
                    }
                    if self.is_error {
                        self.preset_path = old_path;
                    }
                }
                Ok(Some(_)) => self.report(Err("Stop routing before opening a preset".into())),
                Ok(None) => {}
                Err(error) => self.report(Err(error)),
            }
        }
    }
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        configure_dark_theme(&cc.egui_ctx);
        #[cfg(windows)]
        let native_window = {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            use windows::Win32::{
                Foundation::HWND,
                Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute},
            };
            if let Ok(handle) = cc.window_handle()
                && let RawWindowHandle::Win32(window) = handle.as_raw()
            {
                let enabled = 1_i32;
                // SAFETY: the handle belongs to this live window and the attribute
                // receives a readable four-byte BOOL. Older Windows may reject it;
                // failure only leaves the OS title bar unchanged.
                let _ = unsafe {
                    DwmSetWindowAttribute(
                        HWND(window.hwnd.get() as *mut core::ffi::c_void),
                        DWMWA_USE_IMMERSIVE_DARK_MODE,
                        (&enabled as *const i32).cast(),
                        core::mem::size_of_val(&enabled) as u32,
                    )
                };
                window.hwnd.get() as usize
            } else {
                0
            }
        };
        #[cfg(not(windows))]
        let native_window = 0;
        let mut app = Self {
            devices: Vec::new(),
            outputs: Vec::new(),
            source_mode: SourceMode::Playback,
            source_id: String::new(),
            latency_ms: 30,
            main_clock_id: None,
            auto_align: true,
            global_volume: GlobalVolume::default(),
            eq_presets: Vec::new(),
            engine: None,
            status: "Choose a source and enable your output devices.".into(),
            is_error: false,
            preset_path: "matrix-preset.json".into(),
            saved: None,
            saved_path: None,
            history_epoch: 0,
            history: History::default(),
            pending_edit: None,
            picker: None,
            runtime: Runtime::default(),
        };
        app.refresh();
        // UI-only preview: no streams are opened and no preset is changed.
        if std::env::args()
            .any(|arg| arg == "--preview-board" || arg == "--preview-eq" || arg == "--preview-tray")
        {
            app.runtime.disabled_persistence = true;
            app.source_mode = SourceMode::Tone;
            app.choose_source();
            for output in app.outputs.iter_mut().take(3) {
                output.enabled = true;
            }
            if std::env::args().any(|arg| arg == "--preview-tray") {
                app.runtime.preferences.close_to_tray = true;
                app.runtime.tray = Tray::start(cc.egui_ctx.clone(), native_window).ok();
            }
        } else {
            app.attach_engine(&cc.egui_ctx, native_window as u64);
        }
        app
    }

    fn report(&mut self, result: Result<String, String>) {
        match result {
            Ok(message) => {
                self.status = message;
                self.is_error = false;
            }
            Err(error) => {
                self.status = error;
                self.is_error = true;
            }
        }
    }

    fn source(&self) -> Result<Source, String> {
        if self.source_mode == SourceMode::Tone {
            return Ok(Source::Tone);
        }
        let kind = if self.source_mode == SourceMode::Playback {
            DeviceKind::Playback
        } else {
            DeviceKind::Capture
        };
        if self.engine.is_some()
            && let Some(Source::Endpoint(device)) = self
                .runtime
                .remote_snapshot
                .as_ref()
                .and_then(|s| s.source.as_ref())
            && device.id == self.source_id
            && device.kind == kind
        {
            return Ok(Source::Endpoint(device.clone()));
        }
        self.devices
            .iter()
            .find(|d| d.id == self.source_id && d.kind == kind)
            .cloned()
            .or_else(|| {
                self.runtime
                    .remote_snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.source.as_ref())
                    .and_then(|source| match source {
                        Source::Endpoint(device)
                            if device.id == self.source_id && device.kind == kind =>
                        {
                            Some(device.clone())
                        }
                        _ => None,
                    })
            })
            .map(Source::Endpoint)
            .ok_or_else(|| "Select an available input endpoint".into())
    }

    fn choose_source(&mut self) {
        self.runtime.pending_session = None;
        self.runtime.pending_autostart = false;
        if self.source_mode != SourceMode::Tone {
            let kind = if self.source_mode == SourceMode::Playback {
                DeviceKind::Playback
            } else {
                DeviceKind::Capture
            };
            if !self
                .devices
                .iter()
                .any(|d| d.id == self.source_id && d.kind == kind)
            {
                self.source_id = self
                    .devices
                    .iter()
                    .find(|d| d.kind == kind)
                    .map(|d| d.id.clone())
                    .unwrap_or_default();
            }
        }
        if let Ok(source) = self.source() {
            for output in &mut self.outputs {
                // Explicit input reconfiguration changes route dimensions only;
                // keep device/channel processing and timing calibration intact.
                for channel in &mut output.config.channels {
                    channel.routes.resize(source.channels(), 0.0);
                }
                // Keep a flat default EQ usable even for low-rate speech endpoints.
                for band in output
                    .config
                    .eq
                    .iter_mut()
                    .chain(output.config.channels.iter_mut().flat_map(|ch| &mut ch.eq))
                {
                    band.frequency = band.frequency.min(source.sample_rate() as f32 * 0.45);
                }
            }
        }
    }

    fn refresh(&mut self) {
        match audio::enumerate_devices() {
            Ok(devices) => {
                self.apply_devices(devices);
                if self.source_id.is_empty()
                    || (self.engine.is_none()
                        && self.runtime.pending_session.is_none()
                        && self.source().is_ok())
                {
                    self.choose_source();
                }
                self.report(Ok(format!(
                    "{} playback devices · {} capture devices",
                    self.devices
                        .iter()
                        .filter(|d| d.kind == DeviceKind::Playback)
                        .count(),
                    self.devices
                        .iter()
                        .filter(|d| d.kind == DeviceKind::Capture)
                        .count()
                )));
            }
            Err(error) => self.report(Err(error)),
        }
    }

    fn start(&mut self) {
        self.runtime.pending_autostart = false;
        let Some(remote) = &self.runtime.remote else {
            self.report(Err(
                "Background engine unavailable; no local audio fallback is used".into(),
            ));
            return;
        };
        let update = crate::protocol::Update {
            session: self.session_for_engine(),
            audition: crate::protocol::Audition {
                devices: self.runtime.solo_devices.iter().cloned().collect(),
                channels: self.runtime.solo_channels.iter().cloned().collect(),
            },
            base_gain: self.runtime.last_gain,
            base_mute: self.runtime.last_mute,
        };
        let result = remote.send(crate::protocol::Request::Start(Box::new(update)));
        if result.is_ok() {
            if let Some(snapshot) = &self.runtime.remote_snapshot {
                let mut starting = snapshot.clone();
                starting.wanted = true;
                starting.live = false;
                self.engine = Some(Engine::from_snapshot(&starting));
            }
            self.clear_history();
            self.report(Ok("Starting background routing…".into()));
        } else {
            self.report(result.map(|_| String::new()));
        }
    }
    fn stop(&mut self) {
        self.runtime.pending_autostart = false;
        self.clear_history();
        if let Some(remote) = &self.runtime.remote {
            let result = remote.send(crate::protocol::Request::Stop);
            if let Err(error) = result {
                self.report(Err(error));
            }
        }
        self.engine.take();
    }
    fn poll_engine(&mut self, ctx: &egui::Context) {
        let replies: Vec<_> = self
            .runtime
            .remote
            .as_ref()
            .map(|client| client.drain())
            .unwrap_or_default();
        for result in replies {
            match result {
                Err(error) => {
                    self.runtime.remote_error = self
                        .runtime
                        .remote
                        .as_ref()
                        .is_none_or(|c| !c.is_connected());
                    self.report(Err(if self.runtime.remote_error {
                        format!("Engine control connection lost: {error}. Audio may still be running; use its tray or reconnect.")
                    } else { error }));
                }
                Ok(reply) => {
                    let snapshot = reply.snapshot;
                    self.runtime.remote_error = false;
                    self.apply_devices(snapshot.devices.clone());
                    let idle = self
                        .runtime
                        .remote
                        .as_ref()
                        .is_some_and(|client| client.idle());
                    if idle
                        && !ctx.input(|i| i.pointer.any_down())
                        && let Some(session) = &snapshot.session
                    {
                        self.global_volume = session.preset.global_volume();
                        self.runtime.last_gain = Some(self.global_volume.gain_db);
                        self.runtime.last_mute = Some(self.global_volume.mute);
                    }
                    if idle {
                        self.engine = snapshot.wanted.then(|| Engine::from_snapshot(&snapshot));
                    }
                    if snapshot.show_epoch != self.runtime.show_epoch {
                        self.runtime.show_epoch = snapshot.show_epoch;
                        self.runtime.hidden = false;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Title("Audio Matrix".into()));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    }
                    if snapshot.revision != self.runtime.remote_revision {
                        self.runtime.remote_revision = snapshot.revision;
                        self.report(if snapshot.is_error {
                            Err(snapshot.message.clone())
                        } else {
                            Ok(snapshot.message.clone())
                        });
                    }
                    if let Some(error) = reply.error {
                        self.report(Err(error));
                    }
                    self.runtime.remote_snapshot = Some(snapshot);
                }
            }
        }
    }
    fn sync_engine_outputs(&mut self) {
        if self
            .main_clock_id
            .as_ref()
            .is_some_and(|id| !self.outputs.iter().any(|o| o.enabled && o.device.id == *id))
        {
            self.main_clock_id = None;
        }
        self.publish_configuration();
    }

    fn save(&mut self) {
        let source = match self.source() {
            Ok(s) => s,
            Err(e) => {
                self.report(Err(e));
                return;
            }
        };
        for output in &self.outputs {
            if let Err(error) = output.config.validate(
                source.channels(),
                output.device.channels,
                source.sample_rate(),
            ) {
                self.report(Err(error));
                return;
            }
        }
        let preset = self.preset();
        let result = preset
            .save(Path::new(&self.preset_path))
            .map(|_| format!("Saved {}", self.preset_path));
        if result.is_ok() {
            self.saved = Some(self.snapshot());
            self.saved_path = Some(self.preset_path.clone());
        }
        self.report(result);
    }

    fn preset(&self) -> Preset {
        Preset {
            version: 1,
            source_mode: self.source_mode,
            source_id: self.source_id.clone(),
            latency_ms: self.latency_ms,
            main_clock_id: self.main_clock_id.clone(),
            auto_align: self.auto_align,
            global_gain_db: self.global_volume.gain_db,
            global_mute: self.global_volume.mute,
            eq_presets: self.eq_presets.iter().map(SavedEqPreset::from).collect(),
            outputs: self
                .outputs
                .iter()
                .map(|o| {
                    let mut saved = SavedOutput::new(o.device.id.clone(), o.enabled, &o.config);
                    saved.display_name = o.alias.clone();
                    saved.stereo_link = o.stereo_link;
                    saved
                })
                .collect(),
        }
    }

    fn load(&mut self) {
        let result = (|| -> Result<String, String> {
            let preset = Preset::load(Path::new(&self.preset_path))?;
            self.apply_preset(preset)
        })();
        if result.is_ok() {
            self.runtime.pending_session = None;
            self.saved = Some(self.snapshot());
            self.saved_path = Some(self.preset_path.clone());
            self.clear_history();
        }
        self.report(result);
    }

    fn apply_preset(&mut self, preset: Preset) -> Result<String, String> {
        preset.validate_controls()?;
        if preset.version != 1 || !(10..=100).contains(&preset.latency_ms) {
            return Err("Invalid preset version or buffer target".into());
        }
        let source = if preset.source_mode == SourceMode::Tone {
            Source::Tone
        } else {
            let kind = if preset.source_mode == SourceMode::Playback {
                DeviceKind::Playback
            } else {
                DeviceKind::Capture
            };
            Source::Endpoint(
                self.devices
                    .iter()
                    .find(|d| d.id == preset.source_id && d.kind == kind)
                    .cloned()
                    .or_else(|| {
                        self.runtime
                            .remote_snapshot
                            .as_ref()
                            .and_then(|s| s.source.as_ref())
                            .and_then(|s| match s {
                                Source::Endpoint(d)
                                    if d.id == preset.source_id && d.kind == kind =>
                                {
                                    Some(d.clone())
                                }
                                _ => None,
                            })
                    })
                    .ok_or("Preset input is unavailable; reconnect it and refresh devices")?,
            )
        };
        if let Some(id) = &preset.main_clock_id
            && !preset
                .outputs
                .iter()
                .any(|o| &o.device_id == id && o.enabled)
        {
            return Err("Preset main clock must refer to an enabled output".into());
        }
        let mut seen = HashSet::new();
        let mut missing = 0;
        // Validate everything before touching the current configuration.
        for saved in &preset.outputs {
            if !seen.insert(&saved.device_id) {
                return Err("Duplicate output IDs in preset".into());
            }
            if let Some(output) = self.outputs.iter().find(|o| o.device.id == saved.device_id) {
                saved.config().validate(
                    source.channels(),
                    output.device.channels,
                    source.sample_rate(),
                )?;
                if saved.enabled
                    && let Source::Endpoint(input) = &source
                    && input.kind == DeviceKind::Playback
                    && input.id == output.device.id
                {
                    return Err("Preset routes captured playback into itself".into());
                }
            } else {
                saved.config().validate(
                    source.channels(),
                    saved.channels.len(),
                    source.sample_rate(),
                )?;
                missing += 1;
            }
        }
        // Keep offline outputs as pending cards, rather than dropping their calibration.
        for saved in &preset.outputs {
            if !self.outputs.iter().any(|o| o.device.id == saved.device_id) {
                self.outputs.push(OutputState {
                    device: DeviceInfo {
                        id: saved.device_id.clone(),
                        name: if saved.display_name.is_empty() {
                            "Unavailable saved output".into()
                        } else {
                            saved.display_name.clone()
                        },
                        kind: DeviceKind::Playback,
                        channels: saved.channels.len(),
                        sample_rate: source.sample_rate(),
                        channel_mask: 0,
                    },
                    enabled: saved.enabled,
                    config: saved.config(),
                    alias: saved.display_name.clone(),
                    stereo_link: saved.stereo_link,
                    available: false,
                });
            }
        }
        for output in &mut self.outputs {
            if let Some(saved) = preset
                .outputs
                .iter()
                .find(|o| o.device_id == output.device.id)
            {
                output.config = saved.config();
                output.enabled = saved.enabled;
                output.alias = saved.display_name.clone();
                output.stereo_link = saved.stereo_link;
            } else {
                output.enabled = false;
                output.config = OutputConfig::identity(source.channels(), output.device.channels);
                for band in output
                    .config
                    .eq
                    .iter_mut()
                    .chain(output.config.channels.iter_mut().flat_map(|ch| &mut ch.eq))
                {
                    band.frequency = band.frequency.min(source.sample_rate() as f32 * 0.45);
                }
            }
        }
        // Follow the saved device order, retaining newly discovered outputs at the end.
        let order: HashMap<_, _> = preset
            .outputs
            .iter()
            .enumerate()
            .map(|(i, o)| (o.device_id.clone(), i))
            .collect();
        self.outputs
            .sort_by_key(|o| order.get(&o.device.id).copied().unwrap_or(usize::MAX));
        self.source_mode = preset.source_mode;
        self.source_id = preset.source_id.clone();
        self.latency_ms = preset.latency_ms;
        self.auto_align = preset.auto_align;
        self.global_volume = preset.global_volume();
        self.eq_presets = preset.eq_presets.iter().map(EqPreset::from).collect();
        self.main_clock_id = preset
            .main_clock_id
            .filter(|id| self.outputs.iter().any(|o| o.device.id == *id && o.enabled));
        Ok(format!(
            "Loaded {} · {missing} unavailable outputs retained for reconnection",
            self.preset_path
        ))
    }
}

fn display_name(output: &OutputState) -> &str {
    if output.alias.trim().is_empty() {
        &output.device.name
    } else {
        output.alias.trim()
    }
}

fn toggle_set<T: std::hash::Hash + Eq>(set: &mut HashSet<T>, value: T) {
    if !set.remove(&value) {
        set.insert(value);
    }
}
#[cfg(test)]
fn solo_config(
    output: &OutputState,
    devices: &HashSet<String>,
    channels: &HashSet<(String, usize)>,
) -> OutputConfig {
    let mut config = output.config.clone();
    if devices.is_empty() && channels.is_empty() {
        return config;
    }
    let device_solo = devices.contains(&output.device.id);
    let channel_solo = channels.iter().any(|(id, _)| id == &output.device.id);
    config.mute |= !device_solo && !channel_solo;
    if channel_solo && !device_solo {
        for (index, channel) in config.channels.iter_mut().enumerate() {
            channel.mute |= !channels.contains(&(output.device.id.clone(), index));
        }
    }
    config
}

fn link_pair(before: &[matrix_core::ChannelConfig], config: &mut OutputConfig) {
    if before.len() != 2 || config.channels.len() < 2 {
        return;
    }
    for (from, to) in [(0, 1), (1, 0)] {
        if config.channels[from].gain_db != before[from].gain_db {
            config.channels[to].gain_db = (before[to].gain_db + config.channels[from].gain_db
                - before[from].gain_db)
                .clamp(-60.0, 12.0);
            break;
        }
    }
    for (from, to) in [(0, 1), (1, 0)] {
        if config.channels[from].mute != before[from].mute {
            config.channels[to].mute = config.channels[from].mute;
            break;
        }
    }
    for (from, to) in [(0, 1), (1, 0)] {
        if config.channels[from].eq != before[from].eq {
            config.channels[to].eq = config.channels[from].eq.clone();
            break;
        }
    }
}

fn meter_state(ui: &egui::Ui, key: &str, peak: f32) -> Meter {
    let id = ui.id().with(key);
    let now = ui.input(|input| input.time);
    let mut meter = ui
        .data_mut(|data| data.get_temp::<Meter>(id))
        .unwrap_or_default();
    meter.update(peak, now);
    ui.data_mut(|data| data.insert_temp(id, meter));
    meter
}

fn horizontal_meter(ui: &mut egui::Ui, label: &str, peak: f32) {
    let meter = meter_state(ui, label, peak);
    ui.label(RichText::new(label).small().color(DIM));
    let (rect, _) = ui.allocate_exact_size(egui::vec2(180.0, 10.0), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, Color32::from_rgb(8, 11, 16));
    let x = rect.left() + rect.width() * ((meter.level_db + 60.0) / 60.0).clamp(0.0, 1.0);
    painter.rect_filled(
        egui::Rect::from_min_max(rect.left_top(), egui::pos2(x, rect.bottom())),
        2.0,
        if meter.level_db >= 0.0 {
            Color32::LIGHT_RED
        } else if meter.level_db > -6.0 {
            AMBER
        } else {
            ACCENT
        },
    );
    let hold = rect.left() + rect.width() * ((meter.hold_db + 60.0) / 60.0).clamp(0.0, 1.0);
    painter.line_segment(
        [
            egui::pos2(hold, rect.top()),
            egui::pos2(hold, rect.bottom()),
        ],
        egui::Stroke::new(1.0_f32, Color32::WHITE),
    );
    ui.label(
        RichText::new(if meter.hold_db <= -60.0 {
            "−∞ dBFS".into()
        } else {
            format!("{:.1} dBFS", meter.hold_db)
        })
        .small()
        .color(DIM),
    );
}

impl eframe::App for MatrixApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.render(ctx);
    }
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.close_engine();
    }
}

impl MatrixApp {
    fn render(&mut self, ctx: &egui::Context) {
        self.poll_picker();
        self.poll_devices();
        self.tray_actions(ctx);
        self.poll_engine(ctx);
        let frame_before = self.snapshot();
        let frame_epoch = self.history_epoch;
        let mut restored = self.shortcuts(ctx);
        let mut move_device = None;
        let mut recall_scene = None;
        let mut delete_scene = None;
        if ctx.input(|i| i.viewport().close_requested())
            && !self.runtime.quitting
            && self.runtime.preferences.close_to_tray
            && (self.runtime.tray.is_some()
                || self
                    .runtime
                    .remote_snapshot
                    .as_ref()
                    .is_some_and(|s| s.tray_available))
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            self.runtime.hidden = true;
        }
        if self.runtime.hidden {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!(
                "Audio Matrix | {:.1} dB{}",
                self.global_volume.gain_db,
                if self.global_volume.mute {
                    " | MUTED"
                } else {
                    ""
                }
            )));
            self.sync_engine_outputs();
            self.checkpoint(false);
            ctx.request_repaint_after(Duration::from_millis(100));
            return;
        }
        ctx.request_repaint_after(Duration::from_millis(33));
        let running = self.engine.is_some();
        let live = self.engine.as_ref().is_some_and(Engine::is_live);
        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("AUDIO MATRIX")
                        .size(22.0)
                        .strong()
                        .color(ACCENT),
                );
                ui.label(RichText::new("ROUTING CONSOLE").color(DIM));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if running {
                        if ui.button("■  Stop routing").clicked() {
                            self.stop();
                            self.report(Ok("Routing stopped".into()));
                        }
                    } else if ui
                        .add(
                            egui::Button::new("▶  Start routing")
                                .fill(Color32::from_rgb(35, 93, 77)),
                        )
                        .clicked()
                    {
                        self.start();
                    }
                    ui.label(
                        RichText::new(if self.runtime.remote_error { "CONTROL LOST" }
                        else if self.runtime.remote_snapshot.as_ref().is_some_and(|s|s.state==crate::protocol::State::Recovering) { "RECOVERING" }
                        else if self.runtime.remote_snapshot.as_ref().is_some_and(|s|s.state==crate::protocol::State::Suspended) { "SUSPENDED" }
                        else if self.runtime.remote_snapshot.as_ref().is_some_and(|s|s.state==crate::protocol::State::FormatBlocked) { "FORMAT CHANGED" }
                        else if live {
                            "● LIVE"
                        } else if running {
                            "● STARTING"
                        } else {
                            "● STOPPED"
                        })
                        .color(if live { ACCENT } else { DIM }),
                    );
                });
            });
            ui.horizontal(|ui| {
                ui.label(RichText::new("Global volume").strong().color(ACCENT));
                ui.spacing_mut().slider_width = 180.0;
                ui.add(egui::Slider::new(&mut self.global_volume.gain_db, -60.0..=0.0).suffix(" dB"))
                    .on_hover_text("Controls every router output without changing device/channel faders. Applied before clipping; unity is 0 dB.");
                if ui.add(egui::Button::new(if self.global_volume.mute { "GLOBAL MUTED" } else { "Mute all" })
                    .fill(if self.global_volume.mute { Color32::from_rgb(112, 43, 53) } else { Color32::from_rgb(30, 34, 43) })).clicked() {
                    self.global_volume.mute = !self.global_volume.mute;
                }
            });
            ui.horizontal(|ui| {
                let peak = self.engine.as_ref().map(|e| e.outputs.iter().filter_map(|h| h.stats.try_lock().ok())
                    .flat_map(|s| s.peaks.clone()).fold(0.0_f32, f32::max)).unwrap_or(0.0);
                horizontal_meter(ui, "OUTPUT PEAK", peak);
                if ui.add_enabled(self.history.can_undo(), egui::Button::new("Undo")).clicked()
                    && let Some(snapshot) = self.history.undo(self.snapshot()) { self.restore(snapshot); restored = true; }
                if ui.add_enabled(self.history.can_redo(), egui::Button::new("Redo")).clicked()
                    && let Some(snapshot) = self.history.redo(self.snapshot()) { self.restore(snapshot); restored = true; }
                ui.menu_button("Scenes", |ui| {
                    for (index, scene) in self.runtime.scenes.iter().enumerate() {
                        ui.horizontal(|ui| {
                            if ui.button(&scene.name).clicked() { recall_scene = Some(index); ui.close(); }
                            if ui.small_button("x").on_hover_text("Delete this scene").clicked() { delete_scene = Some(index); }
                        });
                    }
                    ui.separator();
                    ui.add(egui::TextEdit::singleline(&mut self.runtime.scene_name).hint_text("Scene name").char_limit(48));
                    if ui.button("Store / update scene").clicked() { self.save_scene(); }
                    ui.label("Recall keeps delay, clock and limiter calibration.");
                });
                if ui.add_enabled(!self.runtime.solo_devices.is_empty() || !self.runtime.solo_channels.is_empty(),
                    egui::Button::new(RichText::new("Clear solo").color(AMBER))).clicked() {
                        self.runtime.solo_devices.clear(); self.runtime.solo_channels.clear();
                }
                ui.menu_button("App", |ui| {
                    let before = self.runtime.preferences.clone();
                    ui.checkbox(&mut self.runtime.preferences.restore_session, "Restore last session");
                    ui.add_enabled_ui(self.runtime.preferences.restore_session, |ui| {
                        ui.checkbox(&mut self.runtime.preferences.auto_start_audio, "Start audio after restoring (opt-in)");
                    });
                    ui.checkbox(&mut self.runtime.preferences.close_to_tray, "Close window to tray");
                    ui.checkbox(&mut self.runtime.preferences.recover_input, "Recover selected input / Windows resume");
                    if ui.add_enabled(!self.runtime.disabled_persistence, egui::Checkbox::new(&mut self.runtime.preferences.start_with_windows, "Run at Windows sign-in")).changed()
                        && let Err(error) = crate::tray::set_startup(self.runtime.preferences.start_with_windows) {
                        self.runtime.preferences.start_with_windows = before.start_with_windows;
                        self.report(Err(error));
                    }
                    ui.checkbox(&mut self.runtime.preferences.spectrum, "Live output spectrum");
                    if before != self.runtime.preferences { self.runtime.settings_dirty = true; }
                    if ui.add_enabled(self.runtime.tray.is_some() || self.runtime.remote_snapshot.as_ref().is_some_and(|s|s.tray_available), egui::Button::new("Hide to tray")).clicked() {
                        self.runtime.hidden = true; ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false)); ui.close();
                    }
                    if ui.button("Exit and stop audio").clicked() {
                        self.runtime.quitting = true; self.checkpoint(true); self.stop();
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    ui.separator();
                    if self.runtime.remote_error && ui.button("Reconnect engine control").clicked() {
                        self.runtime.remote.take(); self.attach_engine(ctx,self.runtime.native_window);
                    }
                    if let Some(snapshot)=&self.runtime.remote_snapshot { ui.label(format!("Engine PID {} · recoveries {}",snapshot.engine_pid,snapshot.recoveries)); }
                    ui.label("Closing this console keeps background audio running.");
                    ui.label("Ctrl+S Save / Ctrl+Shift+S Save as");
                    ui.label("Ctrl+Z Undo / Ctrl+Y Redo / Ctrl+M Mute all");
                    ui.label("Select a device/channel: M mute / S solo");
                });
            });
            ui.add_space(4.0);
        });
        if let Some(index) = recall_scene {
            self.recall_scene(index);
        }
        if let Some(index) = delete_scene {
            self.runtime.scenes.remove(index);
            self.runtime.settings_dirty = true;
        }
        // Start/Stop can change the engine during this frame.
        let running = self.engine.is_some();
        egui::TopBottomPanel::bottom("footer").show(ctx, |ui| {
            // Fixed diagnostics row: rising counters never add/remove widgets.
            let mut budget = 0.0_f32;
            let mut misses = 0_u64;
            let mut underruns = 0_u64;
            let mut analysis_dropped = 0_u64;
            let mut scheduled = 0;
            let mut total = 0;
            if let Some(engine) = &self.engine {
                for output in &engine.outputs {
                    if let Ok(stats) = output.stats.try_lock() {
                        budget = budget.max(stats.budget_p95_percent);
                        misses += stats.deadline_misses;
                        underruns += stats.underruns;
                        analysis_dropped += stats.analysis_dropped;
                        scheduled += usize::from(stats.realtime_scheduled);
                        total += 1;
                    }
                }
            }
            ui.add_sized([ui.available_width(),20.0], egui::Label::new(RichText::new(format!(
                "ENGINE HEALTH  ·  block load p95 ≈ {budget:.0}%  ·  over-budget {misses}  ·  underruns {underruns}  ·  skipped FFT {analysis_dropped}  ·  MMCSS {scheduled}/{total}"
            )).small().color(if misses > 0 || underruns > 0 { AMBER } else { DIM })).truncate())
                .on_hover_text("Approximate processing-time budget, not whole-process CPU usage. Counters are cumulative per renderer; skipped visualization frames never discard audio.");
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(&self.status).color(if self.is_error {
                    Color32::from_rgb(255, 126, 126)
                } else {
                    DIM
                }));
            });
            ui.horizontal_wrapped(|ui| {
                let name = Path::new(&self.preset_path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("Untitled");
                let dirty = self
                    .saved
                    .as_ref()
                    .is_none_or(|saved| *saved != self.snapshot())
                    || self.saved_path.as_deref() != Some(self.preset_path.as_str());
                ui.label(
                    RichText::new(format!(
                        "{name}{}",
                        if dirty {
                            "  • modified"
                        } else {
                            "  • saved"
                        }
                    ))
                    .color(if dirty { AMBER } else { DIM }),
                )
                .on_hover_text(&self.preset_path);
                if ui
                    .add_enabled(
                        !running && self.picker.is_none(),
                        egui::Button::new("Open…"),
                    )
                    .clicked()
                {
                    self.open_picker(false);
                }
                if ui.button("Save").clicked() {
                    self.save();
                }
                if ui
                    .add_enabled(self.picker.is_none(), egui::Button::new("Save as…"))
                    .clicked()
                {
                    self.open_picker(true);
                }
                egui::CollapsingHeader::new("Path").show(ui, |ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.preset_path).desired_width(300.0));
                    if ui
                        .add_enabled(!running, egui::Button::new("Load path"))
                        .clicked()
                    {
                        self.load();
                    }
                });
                ui.label(
                    RichText::new("WASAPI SHARED · FLOAT DSP")
                        .small()
                        .color(DIM),
                );
            });
        });
        egui::SidePanel::left("devices").default_width(216.0).min_width(190.0).max_width(360.0).resizable(true).show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.add_space(10.0);
                ui.label(RichText::new("INPUT").strong().color(DIM));
                let before = (self.source_mode, self.source_id.clone());
                ui.add_enabled_ui(!running, |ui| {
                    egui::ComboBox::from_id_salt("source-mode").width((ui.available_width() - 12.0).max(150.0)).truncate().selected_text(match self.source_mode {
                        SourceMode::Playback => "Capture playback (loopback)", SourceMode::Capture => "Capture device / virtual cable", SourceMode::Tone => "Test tone · -30 dBFS",
                    }).show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.source_mode, SourceMode::Playback, "Capture playback (loopback)");
                        ui.selectable_value(&mut self.source_mode, SourceMode::Capture, "Capture device / virtual cable");
                        ui.selectable_value(&mut self.source_mode, SourceMode::Tone, "Test tone · -30 dBFS");
                    });
                    if self.source_mode != SourceMode::Tone {
                        let kind = if self.source_mode == SourceMode::Playback { DeviceKind::Playback } else { DeviceKind::Capture };
                        let selected = self.devices.iter().find(|d| d.id == self.source_id && d.kind == kind).map(|d| d.name.as_str()).unwrap_or("Choose endpoint");
                        egui::ComboBox::from_id_salt("source-device").width((ui.available_width() - 12.0).max(150.0)).truncate().selected_text(selected).show_ui(ui, |ui| {
                            for device in self.devices.iter().filter(|d| d.kind == kind) {
                                ui.selectable_value(&mut self.source_id, device.id.clone(), &device.name);
                            }
                        });
                    }
                    ui.label(RichText::new("Buffer target").small().color(DIM));
                    ui.add(egui::Slider::new(&mut self.latency_ms, 10..=100).suffix(" ms"));
                    if ui.button("Refresh devices").clicked() { self.refresh(); }
                });
                if before != (self.source_mode, self.source_id.clone()) { self.choose_source(); }
                if let Ok(source) = self.source() { ui.label(RichText::new(format!("{} channels / {} Hz", source.channels(), source.sample_rate())).small().color(DIM)); }
                ui.add_space(10.0);
                egui::CollapsingHeader::new("Input setup help").show(ui, |ui| {
                    ui.label("Send apps to a virtual cable's playback endpoint and capture its recording endpoint here.");
                    ui.label("Our own virtual endpoint remains a driver milestone.");
                });
                ui.add_space(16.0);
                ui.label(RichText::new("CLOCK").strong().color(DIM));
                let clock_id = self.engine.as_ref().and_then(|e| e.main_clock_device_id()).or(self.main_clock_id.as_deref());
                let clock_name = clock_id.and_then(|id| self.outputs.iter().find(|o| o.device.id == id)).map(|o| o.device.name.as_str()).unwrap_or("Input clock (default)");
                ui.label(RichText::new(clock_name).small());
                ui.label(RichText::new("Choose one Main clock in its output card.").small().color(DIM));
                ui.checkbox(&mut self.auto_align, "Auto-align outputs").on_hover_text(
                    "Use Windows presentation clocks to delay faster outputs to match the slowest. Does not measure internal speaker/monitor processing.");
                if ui.add_enabled(running && self.auto_align, egui::Button::new("Re-align")).clicked()
                    && let Some(remote) = &self.runtime.remote { remote.realign(); }
                ui.label(RichText::new("Manual trim covers latency Windows cannot see.").small().color(DIM));
                ui.add_space(10.0);
                ui.label(RichText::new("OUTPUT DEVICES").strong().color(DIM));
                ui.label(RichText::new("Switch outputs on/off while live").small().color(DIM));
                for output in &mut self.outputs {
                    ui.push_id(output.device.id.clone(), |ui| {
                        egui::Frame::group(ui.style()).show(ui, |ui| {
                            ui.horizontal(|ui| {
                                let name = display_name(output).to_owned();
                                let checkbox = ui.checkbox(&mut output.enabled, "");
                                let label = ui.add(egui::Label::new(name).truncate().sense(egui::Sense::click())).on_hover_text(&output.device.name);
                                if label.clicked() { output.enabled = !output.enabled; }
                                checkbox.labelled_by(label.id);
                            });
                            ui.label(RichText::new(if output.available { format!("{} channels · {} Hz", output.device.channels, output.device.sample_rate) }
                                else { "OFFLINE / changed format - settings retained".into() }).small().color(if output.available { DIM } else { AMBER }));
                        });
                    });
                }
                if self.outputs.is_empty() { ui.label("No active output devices. Connect a device and refresh."); }
            });
        });
        egui::CentralPanel::default().show(ctx, |ui| {
            let source = match self.source() {
                Ok(source) => source,
                Err(_) => {
                    ui.heading("Select an input to begin");
                    return;
                }
            };
            let mut board_scroll = egui::ScrollArea::vertical().id_salt("soundboard");
            if std::env::args().any(|arg| arg == "--preview-eq") {
                board_scroll = board_scroll.vertical_scroll_offset(660.0);
            }
            board_scroll.show(ui, |ui| {
                ui.heading("OUTPUT SOUNDBOARD");
                ui.label(
                    RichText::new("All enabled devices · faders, mutes and routing update live")
                        .color(DIM),
                );
                let enabled = self.outputs.iter().filter(|o| o.enabled).count();
                if enabled == 0 {
                    ui.add_space(24.0);
                    ui.label("Enable outputs on the left to add them to the board.");
                    if running {
                        ui.label("Input stays live even with every output switched off.");
                    }
                    return;
                }
                let gap = 10.0;
                let columns = (((ui.available_width() + gap) / 310.0) as usize).clamp(1, enabled);
                let width = (ui.available_width() - gap * (columns - 1) as f32) / columns as f32;
                egui::Grid::new("output-board")
                    .spacing([gap, gap])
                    .show(ui, |ui| {
                        let mut column = 0;
                        for output in self.outputs.iter_mut().filter(|o| o.enabled) {
                            let handle = self.engine.as_ref().and_then(|e| {
                                e.outputs.iter().find(|h| h.device_id == output.device.id)
                            });
                            let ready = handle.is_some_and(|h| h.is_ready());
                            let stats = handle
                                .and_then(|h| h.stats.try_lock().ok().map(|s| s.clone()))
                                .unwrap_or_default();
                            ui.push_id(output.device.id.clone(), |ui| {
                                egui::Frame::group(ui.style())
                                    .fill(Color32::from_rgb(22, 26, 33))
                                    .show(ui, |ui| {
                                        // Grid cells inherit a horizontal layout; strips need their own vertical layout.
                                        ui.vertical(|ui| {
                                            ui.set_width((width - 18.0).max(264.0));
                                            output_strip(
                                                ui,
                                                output,
                                                &source,
                                                &stats,
                                                running,
                                                ready,
                                                &mut self.main_clock_id,
                                                &mut self.eq_presets,
                                                &mut move_device,
                                                &mut self.runtime,
                                            );
                                        });
                                    });
                            });
                            column += 1;
                            if column == columns {
                                ui.end_row();
                                column = 0;
                            }
                        }
                        if column != 0 {
                            ui.end_row();
                        }
                    });
            });
        });
        if let Some((id, direction)) = move_device
            && let Some(index) = self.outputs.iter().position(|o| o.device.id == id)
        {
            let destination = if direction < 0 {
                (0..index).rev().find(|i| self.outputs[*i].enabled)
            } else {
                (index + 1..self.outputs.len()).find(|i| self.outputs[*i].enabled)
            };
            if let Some(destination) = destination {
                self.outputs.swap(index, destination);
            }
        }
        self.sync_engine_outputs();
        self.runtime
            .solo_devices
            .retain(|id| self.outputs.iter().any(|o| o.enabled && o.device.id == *id));
        self.runtime.solo_channels.retain(|(id, index)| {
            self.outputs
                .iter()
                .any(|o| o.enabled && o.device.id == *id && *index < o.config.channels.len())
        });
        if frame_epoch != self.history_epoch {
            self.pending_edit = None;
        }
        if frame_epoch == self.history_epoch
            && !restored
            && self.snapshot() != frame_before
            && self.pending_edit.is_none()
        {
            self.pending_edit = Some(frame_before);
        }
        if !ctx.input(|input| input.pointer.any_down())
            && let Some(before) = self.pending_edit.take()
            && before != self.snapshot()
        {
            self.history.push(before);
        }
        if !ctx.input(|i| i.pointer.any_down()) {
            self.checkpoint(false);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn output_strip(
    ui: &mut egui::Ui,
    output: &mut OutputState,
    source: &Source,
    stats: &OutputStats,
    running: bool,
    ready: bool,
    main_clock: &mut Option<String>,
    eq_presets: &mut Vec<EqPreset>,
    move_device: &mut Option<(String, isize)>,
    runtime: &mut Runtime,
) {
    let pair_before = output
        .config
        .channels
        .iter()
        .take(2)
        .cloned()
        .collect::<Vec<_>>();
    let heading = ui
        .add(
            egui::Label::new(RichText::new(display_name(output)).strong().size(16.0))
                .truncate()
                .sense(egui::Sense::click()),
        )
        .on_hover_text(format!("{}\nRight-click to rename.", output.device.name));
    heading.context_menu(|ui| {
        ui.label("Friendly device name");
        ui.add(
            egui::TextEdit::singleline(&mut output.alias)
                .char_limit(48)
                .hint_text(&output.device.name),
        );
        if ui.button("Use Windows name").clicked() {
            output.alias.clear();
            ui.close();
        }
    });
    if heading.clicked() {
        runtime.focused = Some((output.device.id.clone(), None));
    }
    ui.horizontal(|ui| {
        let state = if !output.available {
            "OFFLINE"
        } else if ready {
            "● LIVE"
        } else if running {
            "● OPENING"
        } else {
            "● STOPPED"
        };
        let status = format!(
            "{state}{}{}",
            if stats.gain_reduction_db > 0.2 {
                " · LIMIT"
            } else {
                ""
            },
            if stats.clipped > 0 {
                " · CLIP HISTORY"
            } else {
                ""
            }
        );
        let color = if stats.clipped > 0 {
            Color32::LIGHT_RED
        } else if stats.gain_reduction_db > 0.2 {
            AMBER
        } else if ready {
            ACCENT
        } else {
            DIM
        };
        ui.add_sized(
            [(ui.available_width() - 94.0).max(90.0), 20.0],
            egui::Label::new(RichText::new(&status).color(color)).truncate(),
        )
        .on_hover_text(status);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .small_button("x")
                .on_hover_text("Disable this output")
                .clicked()
            {
                output.enabled = false;
            }
            if ui
                .small_button(">")
                .on_hover_text("Move device later")
                .clicked()
            {
                *move_device = Some((output.device.id.clone(), 1));
            }
            if ui
                .small_button("<")
                .on_hover_text("Move device earlier")
                .clicked()
            {
                *move_device = Some((output.device.id.clone(), -1));
            }
        });
    });
    ui.label(
        RichText::new(format!(
            "{} ch / {} Hz",
            output.device.channels, output.device.sample_rate
        ))
        .small()
        .color(DIM),
    );
    let mut selected_clock = main_clock.as_deref() == Some(output.device.id.as_str());
    if ui.checkbox(&mut selected_clock, "Main clock").on_hover_text(
        "Use this output's measured Windows playback clock as the resampling reference. Every output reads the source directly; selecting a main clock adds no second audio buffer."
    ).changed() {
        *main_clock = selected_clock.then(|| output.device.id.clone());
    }
    ui.horizontal(|ui| {
        ui.label("Manual delay");
        ui.add(egui::DragValue::new(&mut output.config.delay_ms).range(0.0..=500.0).speed(0.1).suffix(" ms"))
            .on_hover_text("Extra delay for a faster speaker or monitor. Added to automatic delay; editable while live.");
    });
    if running && ready {
        let state = if !stats.clock_supported {
            "Clock unavailable (manual only)"
        } else if !stats.clock_available {
            "Measuring clock"
        } else if stats.measuring {
            "Measuring alignment"
        } else {
            "Clock tracking"
        };
        ui.label(
            RichText::new(format!("Auto +{:.1} ms · {state}", stats.auto_delay_ms))
                .small()
                .color(DIM),
        );
        if let Some(error) = stats.alignment_error_ms {
            ui.label(
                RichText::new(format!(
                    "Windows offset {error:+.1} ms (includes manual trim)"
                ))
                .small()
                .color(DIM),
            );
        }
    } else {
        ui.label(
            RichText::new("Automatic alignment measures while live")
                .small()
                .color(DIM),
        );
    }
    ui.horizontal(|ui| {
        let solo = runtime.solo_devices.contains(&output.device.id);
        if ui
            .selectable_label(
                solo,
                RichText::new("Solo device").color(if solo { AMBER } else { DIM }),
            )
            .clicked()
        {
            toggle_set(&mut runtime.solo_devices, output.device.id.clone());
            runtime.focused = Some((output.device.id.clone(), None));
        }
        if runtime
            .focused
            .as_ref()
            .is_some_and(|(id, ch)| id == &output.device.id && ch.is_none())
        {
            ui.label(RichText::new("SELECTED").small().color(ACCENT));
        }
    });
    if output.config.channels.len() >= 2 {
        ui.checkbox(&mut output.stereo_link, "Stereo link").on_hover_text("Gang the first two channels' gain, mute and EQ. Existing gain offsets are retained. Routes, polarity and delay remain independent.");
    }
    ui.separator();
    egui::ScrollArea::horizontal()
        .id_salt("faders")
        .show(ui, |ui| {
            ui.horizontal_top(|ui| {
                ui.push_id("master", |ui| {
                    fader(
                        ui,
                        "Master",
                        &mut output.config.gain_db,
                        &mut output.config.mute,
                        stats.peaks.iter().copied().reduce(f32::max).unwrap_or(0.0),
                    );
                });
                for (index, channel) in output.config.channels.iter_mut().enumerate() {
                    ui.push_id(index, |ui| {
                        ui.vertical(|ui| {
                            fader(
                                ui,
                                &output.device.channel_name(index),
                                &mut channel.gain_db,
                                &mut channel.mute,
                                stats.peaks.get(index).copied().unwrap_or(0.0),
                            );
                            ui.checkbox(&mut channel.invert, "Invert");
                            let key = (output.device.id.clone(), index);
                            let selected = runtime
                                .focused
                                .as_ref()
                                .is_some_and(|(id, ch)| id == &key.0 && *ch == Some(index));
                            if ui.selectable_label(selected, "Select").clicked() {
                                runtime.focused = Some((key.0.clone(), Some(index)));
                            }
                            if ui
                                .selectable_label(
                                    runtime.solo_channels.contains(&key),
                                    RichText::new("Solo").color(AMBER),
                                )
                                .clicked()
                            {
                                toggle_set(&mut runtime.solo_channels, key);
                                runtime.focused = Some((output.device.id.clone(), Some(index)));
                            }
                        });
                    });
                }
            });
        });
    limiter_controls(ui, &mut output.config.limiter, stats);
    routing_editor(ui, output, source);
    ui.separator();
    ui.label(RichText::new("Device equalizer").strong().color(ACCENT));
    eq_editor(
        ui,
        "device-eq",
        &mut output.config.eq,
        source.sample_rate(),
        eq_presets,
        if runtime.preferences.spectrum {
            Some(&stats.spectrum)
        } else {
            None
        },
    );
    for (index, channel) in output.config.channels.iter_mut().enumerate() {
        ui.push_id(index, |ui| {
            ui.separator();
            ui.label(
                RichText::new(format!(
                    "{} · Channel EQ",
                    output.device.channel_name(index)
                ))
                .strong()
                .color(ACCENT),
            );
            eq_editor(
                ui,
                "channel-eq",
                &mut channel.eq,
                source.sample_rate(),
                eq_presets,
                if runtime.preferences.spectrum {
                    stats.channel_spectra.get(index).map(Vec::as_slice)
                } else {
                    None
                },
            );
            ui.label(RichText::new("Source mix coefficients").small().color(DIM));
            for (input, route) in channel.routes.iter_mut().enumerate() {
                ui.add(egui::Slider::new(route, -1.0..=1.0).text(source.channel_name(input)));
            }
        });
    }
    egui::CollapsingHeader::new("Stream diagnostics").show(ui, |ui| {
        draw_stats(ui, stats, &output.device);
    });
    if output.stereo_link {
        link_pair(&pair_before, &mut output.config);
    }
    if ui.small_button("Reset DSP (keep routing)").clicked() {
        let routes: Vec<_> = output
            .config
            .channels
            .iter()
            .map(|ch| ch.routes.clone())
            .collect();
        let delay_ms = output.config.delay_ms;
        let limiter = output.config.limiter;
        output.config = OutputConfig::identity(source.channels(), output.device.channels);
        output.config.delay_ms = delay_ms;
        output.config.limiter = limiter;
        for (channel, routes) in output.config.channels.iter_mut().zip(routes) {
            channel.routes = routes;
        }
        clamp_eq(&mut output.config, source.sample_rate());
    }
}

fn limiter_controls(
    ui: &mut egui::Ui,
    config: &mut matrix_core::limiter::LimiterConfig,
    stats: &OutputStats,
) {
    ui.separator();
    ui.horizontal(|ui| {
        ui.checkbox(&mut config.enabled, "Limiter").on_hover_text(
            "Linked protection after EQ and volume. Bypass retains 5 ms look-ahead and an emergency clamp, but can distort overloads.");
        ui.label(RichText::new("Linked · 5 ms").small().color(DIM));
    });
    ui.checkbox(&mut config.true_peak, "4x inter-sample protection").on_hover_text(
        "Opt-in conservative windowed-sinc protection. Prevents overshoot at the detector's four phases, with linked gain and the same 5 ms delay. Can attenuate more than sample-peak mode. Not a certified BS.1770 meter or a guarantee after Windows/DAC conversion.");
    ui.horizontal_top(|ui| {
        ui.vertical(|ui| {
            ui.label(RichText::new("Ceiling dBFS").small().color(DIM));
            ui.push_id("limiter-ceiling", |ui| {
                knob(
                    ui,
                    &mut config.ceiling_db,
                    KnobSpec {
                        label: "Limiter ceiling",
                        range: -12.0..=0.0,
                        default: -1.0,
                        logarithmic: false,
                        unit: "dB",
                    },
                );
            });
        });
        ui.vertical(|ui| {
            ui.label(RichText::new("Release").small().color(DIM));
            ui.push_id("limiter-release", |ui| {
                knob(
                    ui,
                    &mut config.release_ms,
                    KnobSpec {
                        label: "Limiter release",
                        range: 20.0..=1000.0,
                        default: 120.0,
                        logarithmic: true,
                        unit: "ms",
                    },
                );
            });
        });
        ui.vertical(|ui| {
            ui.label(RichText::new("Gain reduction").small().color(DIM));
            ui.label(
                RichText::new(format!("{:.1} dB", stats.gain_reduction_db)).color(
                    if stats.gain_reduction_db > 3.0 {
                        AMBER
                    } else {
                        ACCENT
                    },
                ),
            );
            ui.add(
                egui::ProgressBar::new((stats.gain_reduction_db / 12.0).clamp(0.0, 1.0))
                    .desired_width(100.0),
            );
            ui.label(
                RichText::new(format!(
                    "Input {:+.1} dBFS",
                    gain_to_db(stats.pre_limiter_peak)
                ))
                .small()
                .color(DIM),
            );
            ui.label(
                RichText::new(if config.true_peak {
                    format!("4x TP est. {:.1} dB", gain_to_db(stats.true_peak))
                } else {
                    "Sample-peak mode".into()
                })
                .small()
                .color(DIM),
            );
        });
    });
    limiter_warning(ui, config.enabled, stats.gain_reduction_db, stats.clipped);
}

// Reserve two lines even when clear: changing meter values must never move
// the routing matrix/EQ under the pointer. Truncation keeps narrow cards stable.
fn limiter_warning(
    ui: &mut egui::Ui,
    enabled: bool,
    reduction: f32,
    clipped: u64,
) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 38.0), egui::Sense::hover());
    ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
        let message = if !enabled {
            "BYPASS: overloads may hard-clip"
        } else if reduction > 3.0 {
            "Heavy limiting: reduce gain / EQ boosts"
        } else {
            ""
        };
        ui.add(
            egui::Label::new(RichText::new(message).small().color(if enabled {
                AMBER
            } else {
                Color32::LIGHT_RED
            }))
            .truncate(),
        );
        ui.add(
            egui::Label::new(
                RichText::new(if clipped > 0 {
                    format!("{clipped} emergency clips: enable protection / reduce gain")
                } else {
                    String::new()
                })
                .small()
                .color(Color32::LIGHT_RED),
            )
            .truncate(),
        );
    });
    response
}

fn routing_editor(ui: &mut egui::Ui, output: &mut OutputState, source: &Source) {
    ui.separator();
    ui.label(RichText::new("CHANNEL ROUTING").strong().small().color(DIM));
    egui::ScrollArea::horizontal().id_salt("card-matrix-scroll").show(ui, |ui| {
        egui::Grid::new("card-routing").striped(true).spacing([6.0, 5.0]).show(ui, |ui| {
            ui.label("Output / Input");
            for input in 0..source.channels() { ui.label(source.channel_name(input)); }
            ui.end_row();
            for (index, channel) in output.config.channels.iter_mut().enumerate() {
                ui.label(output.device.channel_name(index));
                for (input, route) in channel.routes.iter_mut().enumerate() {
                    ui.push_id((index, input), |ui| {
                        let active = *route != 0.0;
                        if ui.add_sized([52.0, 25.0], egui::Button::new(
                            if active { format!("{route:.2}") } else { "—".into() }
                        ).fill(if active { Color32::from_rgb(35, 93, 77) } else { Color32::from_rgb(30, 34, 43) }))
                            .on_hover_text("Toggle unity gain. Fine coefficients are below the channel's visible EQ.")
                            .clicked() { *route = if active { 0.0 } else { 1.0 }; }
                    });
                }
                ui.end_row();
            }
        });
    });
}

fn fader(ui: &mut egui::Ui, label: &str, gain: &mut f32, mute: &mut bool, peak: f32) {
    let meter = meter_state(ui, "channel-level", peak);
    ui.vertical(|ui| {
        ui.set_width(82.0);
        ui.label(RichText::new(label).small());
        ui.horizontal(|ui| {
            ui.spacing_mut().slider_width = 145.0;
            ui.add(
                egui::Slider::new(gain, -60.0..=12.0)
                    .vertical()
                    .show_value(false),
            );
            let (scale, _) = ui.allocate_exact_size(egui::vec2(24.0, 145.0), egui::Sense::hover());
            let scale_painter = ui.painter_at(scale);
            for db in [12.0_f32, 0.0, -12.0, -24.0, -36.0, -48.0, -60.0] {
                let y = scale.bottom() - (db + 60.0) / 72.0 * scale.height();
                let color = if db == 0.0 { Color32::WHITE } else { DIM };
                scale_painter.line_segment(
                    [
                        egui::pos2(scale.left(), y),
                        egui::pos2(scale.left() + 3.0, y),
                    ],
                    egui::Stroke::new(1.0_f32, color),
                );
                scale_painter.text(
                    egui::pos2(scale.left() + 5.0, y),
                    if db == 12.0 {
                        egui::Align2::LEFT_TOP
                    } else if db == -60.0 {
                        egui::Align2::LEFT_BOTTOM
                    } else {
                        egui::Align2::LEFT_CENTER
                    },
                    if db > 0.0 {
                        format!("+{db:.0}")
                    } else {
                        format!("{db:.0}")
                    },
                    egui::FontId::proportional(10.0),
                    color,
                );
            }
            let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 145.0), egui::Sense::hover());
            let painter = ui.painter_at(rect);
            painter.rect_filled(rect, 2.0, Color32::from_rgb(11, 15, 22));
            let fraction = ((meter.level_db + 60.0) / 60.0).clamp(0.0, 1.0);
            let fill = egui::Rect::from_min_max(
                egui::pos2(rect.left(), rect.bottom() - rect.height() * fraction),
                rect.right_bottom(),
            );
            painter.rect_filled(
                fill,
                1.0,
                if meter.level_db >= 0.0 {
                    Color32::LIGHT_RED
                } else if meter.level_db > -6.0 {
                    AMBER
                } else {
                    ACCENT
                },
            );
            let hold_y =
                rect.bottom() - rect.height() * ((meter.hold_db + 60.0) / 60.0).clamp(0.0, 1.0);
            painter.line_segment(
                [
                    egui::pos2(rect.left(), hold_y),
                    egui::pos2(rect.right(), hold_y),
                ],
                egui::Stroke::new(1.0_f32, Color32::WHITE),
            );
            for db in [-60.0_f32, -36.0, -18.0, -6.0, 0.0] {
                let y = rect.bottom() - (db + 60.0) / 60.0 * rect.height();
                painter.line_segment(
                    [egui::pos2(rect.left() - 4.0, y), egui::pos2(rect.left(), y)],
                    egui::Stroke::new(1.0_f32, DIM),
                );
            }
        });
        ui.add(
            egui::DragValue::new(gain)
                .range(-60.0..=12.0)
                .speed(0.2)
                .suffix(" dB"),
        );
        if ui
            .add_sized(
                [76.0, 27.0],
                egui::Button::new(if *mute { "MUTED" } else { "Mute" }).fill(if *mute {
                    Color32::from_rgb(112, 43, 53)
                } else {
                    Color32::from_rgb(30, 34, 43)
                }),
            )
            .clicked()
        {
            *mute = !*mute;
        }
    });
}

fn clamp_eq(config: &mut OutputConfig, rate: u32) {
    for band in config
        .eq
        .iter_mut()
        .chain(config.channels.iter_mut().flat_map(|ch| &mut ch.eq))
    {
        band.frequency = band.frequency.min(rate as f32 * 0.45);
    }
}

fn eq_editor(
    ui: &mut egui::Ui,
    id: &str,
    eq: &mut Vec<EqBand>,
    rate: u32,
    library: &mut Vec<EqPreset>,
    spectrum: Option<&[f32]>,
) {
    ui.push_id(id, |ui| {
        let builtins = eq_presets::builtins();
        let current = builtins.iter().chain(library.iter()).find(|preset| eq_presets::matches(eq, preset, rate))
            .map(|preset| preset.name.as_str()).unwrap_or("Custom");
        let mut selected = None;
        ui.horizontal(|ui| {
            ui.label("EQ preset");
            egui::ComboBox::from_id_salt("eq-preset").width(130.0).selected_text(current).show_ui(ui, |ui| {
                ui.label(RichText::new("Built-in").small().color(DIM));
                for preset in &builtins {
                    if ui.selectable_label(current == preset.name, &preset.name).clicked() { selected = Some(preset.bands.clone()); }
                }
                if !library.is_empty() {
                    ui.separator(); ui.label(RichText::new("My presets").small().color(DIM));
                    for preset in library.iter() {
                        if ui.selectable_label(current == preset.name, &preset.name).clicked() { selected = Some(preset.bands.clone()); }
                    }
                }
            });
        });
        if let Some(bands) = selected { *eq = eq_presets::at_rate(&bands, rate); }
        let compare_id = ui.id().with("ab-comparison");
        let mut compare = ui.data_mut(|d| d.get_temp::<EqComparison>(compare_id))
            .unwrap_or_else(|| EqComparison { a: eq.clone(), b: eq.clone(), active_b: false });
        if compare.active_b { compare.b = eq.clone(); } else { compare.a = eq.clone(); }
        ui.horizontal(|ui| {
            let clipboard = egui::Id::new("router-eq-clipboard");
            if ui.small_button("Copy").clicked() { ui.data_mut(|d| d.insert_temp(clipboard, eq.clone())); }
            if ui.add_enabled(ui.data_mut(|d| d.get_temp::<Vec<EqBand>>(clipboard)).is_some(), egui::Button::new("Paste").small()).clicked()
                && let Some(bands) = ui.data_mut(|d| d.get_temp::<Vec<EqBand>>(clipboard)) { *eq = eq_presets::at_rate(&bands, rate); }
            for (label, active_b) in [("A",false),("B",true)] {
                if ui.selectable_label(compare.active_b == active_b, label).clicked() && compare.active_b != active_b {
                    compare.active_b = active_b;
                    *eq = eq_presets::at_rate(if active_b { &compare.b } else { &compare.a }, rate);
                }
            }
            if ui.small_button("A > B").on_hover_text("Copy comparison A into B").clicked() { compare.b = compare.a.clone(); if compare.active_b { *eq = compare.b.clone(); } }
        });
        ui.data_mut(|d| d.insert_temp(compare_id, compare));
        eq_curve_spectrum(ui, eq, rate, spectrum);
        egui::ScrollArea::horizontal().id_salt("eq-knobs").show(ui, |ui| {
            ui.spacing_mut().item_spacing = egui::vec2(6.0, 2.0);
            ui.horizontal_top(|ui| {
                for (index, band) in eq.iter_mut().enumerate() {
                    ui.push_id(index, |ui| {
                        ui.vertical(|ui| {
                            ui.set_width(48.0);
                            ui.label(RichText::new(format!("Band {}", index + 1)).small().color(DIM));
                            ui.checkbox(&mut band.enabled, "On");
                            egui::ComboBox::from_id_salt("filter-kind").width(48.0).selected_text(match band.kind {
                                matrix_core::FilterType::Bell => "Bell", matrix_core::FilterType::LowShelf => "LS",
                                matrix_core::FilterType::HighShelf => "HS", matrix_core::FilterType::HighPass => "HP",
                                matrix_core::FilterType::LowPass => "LP",
                            }).show_ui(ui, |ui| {
                                for (kind, label) in [(matrix_core::FilterType::Bell,"Bell"),
                                    (matrix_core::FilterType::LowShelf,"Low shelf"), (matrix_core::FilterType::HighShelf,"High shelf"),
                                    (matrix_core::FilterType::HighPass,"High pass"), (matrix_core::FilterType::LowPass,"Low pass")] {
                                    ui.selectable_value(&mut band.kind, kind, label);
                                }
                            });
                            let default_frequency = matrix_core::flat_eq().get(index).map_or(band.frequency, |b| b.frequency);
                            ui.push_id("gain", |ui| {
                                ui.add_enabled_ui(band.enabled && matches!(band.kind, matrix_core::FilterType::Bell | matrix_core::FilterType::LowShelf | matrix_core::FilterType::HighShelf), |ui| {
                                knob(ui, &mut band.gain_db, KnobSpec { label: "Gain", range: -18.0..=18.0,
                                    default: 0.0, logarithmic: false, unit: "dB" });
                                });
                            });
                            ui.push_id("frequency", |ui| {
                                knob(ui, &mut band.frequency, KnobSpec { label: "Frequency", range: 20.0..=20000.0_f32.min(rate as f32 * 0.45),
                                    default: default_frequency, logarithmic: true, unit: "Hz" });
                            });
                            ui.push_id("q", |ui| {
                                knob(ui, &mut band.q, KnobSpec { label: "Q", range: 0.1..=10.0,
                                    default: 0.707, logarithmic: true, unit: "Q" });
                            });
                        });
                    });
                }
            });
        });
        let name_id = ui.id().with("custom-name");
        let status_id = ui.id().with("custom-status");
        let mut name = ui.data_mut(|data| data.get_temp::<String>(name_id)).unwrap_or_default();
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut name).desired_width(120.0).char_limit(48).hint_text("Name this EQ"));
            if ui.small_button("Save EQ").on_hover_text("Save this EQ shape for any device/channel. Same name updates a custom preset. Use the footer Save to persist the library.").clicked() {
                let status = match eq_presets::save_custom(library, &name, eq) {
                    Ok(()) => format!("Saved {} · footer Save persists it", name.trim()), Err(error) => error,
                };
                ui.data_mut(|data| data.insert_temp(status_id, status));
            }
        });
        ui.data_mut(|data| data.insert_temp(name_id, name));
        if let Some(status) = ui.data_mut(|data| data.get_temp::<String>(status_id)) {
            ui.label(RichText::new(status).small().color(DIM));
        }
    });
}

struct KnobSpec<'a> {
    label: &'a str,
    range: std::ops::RangeInclusive<f32>,
    default: f32,
    logarithmic: bool,
    unit: &'a str,
}

fn knob_fraction(value: f32, range: &std::ops::RangeInclusive<f32>, logarithmic: bool) -> f32 {
    let (min, max) = (*range.start(), *range.end());
    let value = value.clamp(min, max);
    if logarithmic {
        ((value / min).ln() / (max / min).ln()).clamp(0.0, 1.0)
    } else {
        ((value - min) / (max - min)).clamp(0.0, 1.0)
    }
}

fn knob_value(fraction: f32, range: &std::ops::RangeInclusive<f32>, logarithmic: bool) -> f32 {
    let (min, max) = (*range.start(), *range.end());
    let fraction = fraction.clamp(0.0, 1.0);
    if logarithmic {
        min * (max / min).powf(fraction)
    } else {
        min + (max - min) * fraction
    }
}

fn knob(ui: &mut egui::Ui, value: &mut f32, spec: KnobSpec<'_>) -> egui::Response {
    let (rect, mut response) =
        ui.allocate_exact_size(egui::vec2(48.0, 60.0), egui::Sense::click_and_drag());
    let before = *value;
    if response.clicked() {
        response.request_focus();
    }
    if response.double_clicked() {
        *value = spec.default.clamp(*spec.range.start(), *spec.range.end());
    }
    if response.dragged() {
        let start = knob_fraction(*value, &spec.range, spec.logarithmic);
        let delta = response.drag_delta();
        let fine = ui.input(|input| if input.modifiers.shift { 0.1 } else { 1.0 });
        *value = knob_value(
            start + (delta.x - delta.y) / 180.0 * fine,
            &spec.range,
            spec.logarithmic,
        );
    }
    if response.has_focus() {
        let delta = ui.input(|input| {
            let up =
                input.key_pressed(egui::Key::ArrowUp) || input.key_pressed(egui::Key::ArrowRight);
            let down =
                input.key_pressed(egui::Key::ArrowDown) || input.key_pressed(egui::Key::ArrowLeft);
            (i32::from(up) - i32::from(down)) as f32
                * if input.modifiers.shift { 0.001 } else { 0.01 }
        });
        if delta != 0.0 {
            *value = knob_value(
                knob_fraction(*value, &spec.range, spec.logarithmic) + delta,
                &spec.range,
                spec.logarithmic,
            );
        }
        if ui.input(|input| input.key_pressed(egui::Key::Home)) {
            *value = spec.default.clamp(*spec.range.start(), *spec.range.end());
        }
    }
    if *value != before {
        response.mark_changed();
    }
    response.widget_info(|| egui::WidgetInfo::slider(ui.is_enabled(), *value as f64, spec.label));
    let fraction = knob_fraction(*value, &spec.range, spec.logarithmic);
    let painter = ui.painter_at(rect);
    let center = egui::pos2(rect.center().x, rect.top() + 25.0);
    let start_angle = std::f32::consts::PI * 0.75;
    let sweep = std::f32::consts::PI * 1.5;
    let arc = |amount: f32| {
        (0..=32)
            .map(|index| {
                let angle = start_angle + sweep * amount * index as f32 / 32.0;
                center + egui::vec2(angle.cos(), angle.sin()) * 22.0
            })
            .collect::<Vec<_>>()
    };
    painter.add(egui::Shape::line(
        arc(1.0),
        egui::Stroke::new(2.0_f32, Color32::from_rgb(50, 58, 72)),
    ));
    painter.add(egui::Shape::line(
        arc(fraction),
        egui::Stroke::new(2.0_f32, ACCENT),
    ));
    painter.circle_filled(center, 17.0, Color32::from_rgb(30, 36, 46));
    painter.circle_stroke(
        center,
        17.0,
        egui::Stroke::new(
            1.0_f32,
            if response.hovered() || response.has_focus() {
                ACCENT
            } else {
                DIM
            },
        ),
    );
    let angle = start_angle + sweep * fraction;
    painter.line_segment(
        [
            center + egui::vec2(angle.cos(), angle.sin()) * 7.0,
            center + egui::vec2(angle.cos(), angle.sin()) * 14.0,
        ],
        egui::Stroke::new(2.0_f32, ACCENT),
    );
    let text = match spec.unit {
        "Hz" => {
            if *value >= 1000.0 {
                format!("{:.1} kHz", *value / 1000.0)
            } else {
                format!("{value:.0} Hz")
            }
        }
        "dB" => format!("{value:+.1} dB"),
        "ms" => format!("{value:.0} ms"),
        _ => format!("Q {value:.2}"),
    };
    painter.text(
        egui::pos2(rect.center().x, rect.bottom() - 8.0),
        egui::Align2::CENTER_CENTER,
        text,
        egui::FontId::proportional(10.0),
        DIM,
    );
    response.on_hover_text(format!("{}: {:.3} {}\nDrag up/right to increase. Shift = fine adjustment. Double-click or Home = reset. Arrow keys adjust.", spec.label, value, spec.unit))
}

#[derive(Clone)]
struct EqComparison {
    a: Vec<EqBand>,
    b: Vec<EqBand>,
    active_b: bool,
}

#[cfg(test)]
fn eq_curve(ui: &mut egui::Ui, eq: &mut [EqBand], rate: u32) -> egui::Response {
    eq_curve_spectrum(ui, eq, rate, None)
}
fn eq_curve_spectrum(
    ui: &mut egui::Ui,
    eq: &mut [EqBand],
    rate: u32,
    spectrum: Option<&[f32]>,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width().clamp(220.0, 1000.0), 100.0),
        egui::Sense::hover(),
    );
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 4.0, Color32::from_rgb(11, 15, 22));
    if let Some(spectrum) = spectrum.filter(|s| s.len() > 1) {
        let points = spectrum
            .iter()
            .enumerate()
            .map(|(i, db)| {
                egui::pos2(
                    rect.left() + (i as f32 + 0.5) / spectrum.len() as f32 * rect.width(),
                    rect.bottom() - ((db + 90.0) / 90.0).clamp(0.0, 1.0) * rect.height(),
                )
            })
            .collect();
        painter.add(egui::Shape::line(
            points,
            egui::Stroke::new(1.5_f32, Color32::from_rgb(73, 101, 136)),
        ));
        painter.text(
            rect.right_top() + egui::vec2(-4.0, 4.0),
            egui::Align2::RIGHT_TOP,
            "Output spectrum -90..0 dBFS",
            egui::FontId::proportional(9.0),
            DIM,
        );
    }
    painter.line_segment(
        [
            egui::pos2(rect.left(), rect.center().y),
            egui::pos2(rect.right(), rect.center().y),
        ],
        egui::Stroke::new(1.0_f32, Color32::from_rgb(55, 65, 81)),
    );
    let maximum = 20000.0_f32.min(rate as f32 * 0.45);
    let points: Vec<_> = (0..200)
        .map(|i| {
            let t = i as f32 / 199.0;
            let frequency = 20.0 * (maximum / 20.0).powf(t);
            let gain: f64 = eq
                .iter()
                .map(|band| matrix_core::eq::response_db(*band, frequency, rate))
                .sum();
            egui::pos2(
                rect.left() + t * rect.width(),
                rect.center().y - gain.clamp(-24.0, 24.0) as f32 / 48.0 * rect.height(),
            )
        })
        .collect();
    painter.add(egui::Shape::line(
        points,
        egui::Stroke::new(2.0_f32, ACCENT),
    ));
    for frequency in [100.0_f32, 1000.0, 10000.0] {
        if frequency <= maximum {
            let x = rect.left() + (frequency / 20.0).ln() / (maximum / 20.0).ln() * rect.width();
            painter.line_segment(
                [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
                egui::Stroke::new(1.0_f32, Color32::from_rgb(35, 43, 54)),
            );
            if rect.right() - x > 60.0 {
                painter.text(
                    egui::pos2(x, rect.bottom() - 5.0),
                    egui::Align2::CENTER_BOTTOM,
                    if frequency >= 1000.0 {
                        format!("{:.0}k", frequency / 1000.0)
                    } else {
                        format!("{frequency:.0}")
                    },
                    egui::FontId::proportional(10.0),
                    DIM,
                );
            }
        }
    }
    for (index, band) in eq.iter_mut().enumerate() {
        let point = egui::pos2(
            rect.left() + (band.frequency / 20.0).ln() / (maximum / 20.0).ln() * rect.width(),
            rect.center().y
                - if matches!(
                    band.kind,
                    matrix_core::FilterType::HighPass | matrix_core::FilterType::LowPass
                ) {
                    0.0
                } else {
                    band.gain_db / 48.0 * rect.height()
                },
        );
        let hit = egui::Rect::from_center_size(point, egui::vec2(18.0, 18.0));
        let response = ui.interact(hit, ui.id().with(("eq-point", index)), egui::Sense::drag());
        if response.dragged() && band.enabled {
            let delta = response.drag_delta();
            let fine = ui.input(|i| if i.modifiers.shift { 0.1 } else { 1.0 });
            band.frequency = (band.frequency
                * (maximum / 20.0).powf(delta.x / rect.width() * fine))
            .clamp(20.0, maximum);
            if !matches!(
                band.kind,
                matrix_core::FilterType::HighPass | matrix_core::FilterType::LowPass
            ) {
                band.gain_db =
                    (band.gain_db - delta.y / rect.height() * 48.0 * fine).clamp(-18.0, 18.0);
            }
        }
        painter.circle_filled(
            point,
            5.0,
            if !band.enabled {
                DIM
            } else if response.hovered() || response.dragged() {
                Color32::WHITE
            } else {
                ACCENT
            },
        );
        response.on_hover_text(format!("Band {} · {:.0} Hz · {:+.1} dB\nDrag horizontally for frequency, vertically for gain. Shift = fine.",index+1,band.frequency,band.gain_db));
    }
    painter.text(
        rect.left_top() + egui::vec2(6.0, 4.0),
        egui::Align2::LEFT_TOP,
        "+24 dB",
        egui::FontId::proportional(10.0),
        DIM,
    );
    painter.text(
        rect.left_bottom() + egui::vec2(6.0, -4.0),
        egui::Align2::LEFT_BOTTOM,
        "20 Hz",
        egui::FontId::proportional(10.0),
        DIM,
    );
    painter.text(
        rect.right_bottom() + egui::vec2(-6.0, -4.0),
        egui::Align2::RIGHT_BOTTOM,
        format!("{:.1}k", maximum / 1000.0),
        egui::FontId::proportional(10.0),
        DIM,
    );
    response
}

fn draw_stats(ui: &mut egui::Ui, stats: &OutputStats, device: &DeviceInfo) {
    ui.label(
        RichText::new(format!(
            "Audio work {:.3} ms avg / {:.3} ms peak · block budget {:.2} ms",
            stats.processing_ms, stats.processing_peak_ms, stats.budget_ms
        ))
        .small()
        .color(DIM),
    );
    ui.label(
        RichText::new(format!(
            "Approx. p95 {:.0}% budget · deadlines {} · FFT drops {} · {}",
            stats.budget_p95_percent,
            stats.deadline_misses,
            stats.analysis_dropped,
            if stats.realtime_scheduled {
                "MMCSS Pro Audio"
            } else {
                "ordinary scheduling"
            }
        ))
        .small()
        .color(if stats.budget_p95_percent > 80.0 {
            AMBER
        } else {
            DIM
        }),
    );
    ui.label(RichText::new("OUTPUT METERS").strong().color(DIM));
    for (index, peak) in stats.peaks.iter().enumerate() {
        ui.horizontal(|ui| {
            ui.label(device.channel_name(index));
            let db = gain_to_db(*peak);
            ui.add(
                egui::ProgressBar::new(((db + 60.0) / 60.0).clamp(0.0, 1.0))
                    .desired_width(220.0)
                    .fill(if *peak > 1.0 {
                        Color32::from_rgb(255, 110, 110)
                    } else {
                        ACCENT
                    })
                    .text(if *peak < 0.000001 {
                        "−∞ dBFS".into()
                    } else {
                        format!("{db:.1} dBFS")
                    }),
            );
        });
    }
    ui.label(RichText::new(format!("Buffered {:.1} ms · Drift {:+.0} ppm · Underruns {} · Dropped frames {} · Clipped samples {}",
        stats.buffered_ms, stats.correction_ppm, stats.underruns, stats.dropped_frames, stats.clipped)).small().color(DIM));
    ui.label(RichText::new(format!("Limiter look-ahead {:.2} ms · Limited frames {} · Pre-limiter overload samples {} · Reduction {:.1} dB",
        stats.limiter_latency_ms, stats.limited_frames, stats.overload_samples, stats.gain_reduction_db)).small().color(DIM));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limiter_warning_and_following_control_never_move() {
        let ctx = egui::Context::default();
        let mut positions = Vec::new();
        for (enabled, reduction, clipped) in [
            (true, 0.0, 0),
            (true, 8.0, 0),
            (false, 0.0, 7),
            (true, 0.0, 0),
        ] {
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.set_width(264.0);
                    let area = limiter_warning(ui, enabled, reduction, clipped).rect;
                    let next = ui.button("Next control").rect;
                    positions.push((area.size(), next.top()));
                });
            });
        }
        assert!(positions.windows(2).all(|p| p[0] == p[1]));
        assert_eq!(positions[0].0.y, 38.0);
        // Also exercise the complete strip: status badges, bypass and clipping
        // must not cause horizontal wrapping or move the routing grid below.
        for width in [264.0, 400.0] {
            let mut app = board();
            let mut heights = Vec::new();
            for (enabled, reduction, clipped) in [(true, 0.0, 0), (true, 8.0, 0), (false, 0.0, 700)]
            {
                app.outputs[0].config.limiter.enabled = enabled;
                let stats = OutputStats {
                    gain_reduction_db: reduction,
                    clipped,
                    ..Default::default()
                };
                for _ in 0..3 {
                    let _ = ctx.run(
                        egui::RawInput {
                            screen_rect: Some(egui::Rect::from_min_size(
                                egui::Pos2::ZERO,
                                egui::vec2(600.0, 3200.0),
                            )),
                            ..Default::default()
                        },
                        |ctx| {
                            egui::CentralPanel::default().show(ctx, |ui| {
                                ui.set_width(width);
                                let mut move_device = None;
                                let rect = ui
                                    .vertical(|ui| {
                                        output_strip(
                                            ui,
                                            &mut app.outputs[0],
                                            &Source::Tone,
                                            &stats,
                                            true,
                                            false,
                                            &mut app.main_clock_id,
                                            &mut app.eq_presets,
                                            &mut move_device,
                                            &mut app.runtime,
                                        )
                                    })
                                    .response
                                    .rect;
                                heights.push(rect.height());
                            });
                        },
                    );
                }
            }
            assert_eq!(heights[2], heights[5]);
            assert_eq!(heights[5], heights[8]);
        }
    }

    #[test]
    fn keyboard_audition_global_mute_and_undo_target_the_selected_controls() {
        let ctx = egui::Context::default();
        let mut app = board();
        app.runtime.focused = Some(("device-0".into(), Some(1)));
        let mut press = |key, modifiers| {
            let _ = ctx.run(
                egui::RawInput {
                    modifiers,
                    events: vec![egui::Event::Key {
                        key,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers,
                    }],
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1280.0, 820.0),
                    )),
                    ..Default::default()
                },
                |ctx| app.render(ctx),
            );
        };
        press(egui::Key::S, egui::Modifiers::NONE);
        press(egui::Key::M, egui::Modifiers::NONE);
        press(egui::Key::M, egui::Modifiers::CTRL);
        press(egui::Key::Z, egui::Modifiers::CTRL);
        assert!(app.runtime.solo_channels.contains(&("device-0".into(), 1)));
        assert!(app.outputs[0].config.channels[1].mute);
        assert!(!app.outputs[0].config.channels[0].mute);
        assert!(!app.global_volume.mute);
        assert!(app.engine.is_none());
    }

    #[test]
    fn opted_in_startup_waits_for_outputs_and_explicit_stop_cancels_it() {
        let mut app = board();
        for output in &mut app.outputs {
            output.available = false;
        }
        app.runtime.pending_autostart = true;
        app.poll_devices();
        assert!(app.engine.is_none());
        assert!(app.runtime.pending_autostart);
        app.stop();
        app.poll_devices();
        assert!(!app.runtime.pending_autostart);
        assert!(app.engine.is_none());
    }
    #[test]
    fn explicit_input_layout_reconfiguration_preserves_dsp_and_calibration() {
        let mut app = board();
        let mut config = app.outputs[0].config.clone();
        config.delay_ms = 127.0;
        config.gain_db = -18.0;
        config.limiter.true_peak = true;
        config.eq[2].gain_db = 3.0;
        config.channels[0].routes = vec![0.3, 0.7];
        app.outputs[0].config = config.clone();
        app.source_mode = SourceMode::Capture;
        app.source_id = "changed-input".into();
        app.devices.push(DeviceInfo {
            id: app.source_id.clone(),
            name: "Changed input".into(),
            kind: DeviceKind::Capture,
            channels: 4,
            sample_rate: 48000,
            channel_mask: 0,
        });
        app.choose_source();
        for channel in &mut config.channels {
            channel.routes.resize(4, 0.0);
        }
        assert_eq!(app.outputs[0].config, config);
        assert_eq!(
            app.outputs[0].config.channels[0].routes,
            vec![0.3, 0.7, 0.0, 0.0]
        );
    }
    #[test]
    fn letter_shortcuts_do_not_toggle_audio_while_typing() {
        let ctx = egui::Context::default();
        let mut app = board();
        app.runtime.focused = Some(("device-0".into(), None));
        let mut text = String::new();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let response = ui.text_edit_singleline(&mut text);
                response.request_focus();
            });
        });
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: egui::Key::M,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.text_edit_singleline(&mut text);
                });
                app.shortcuts(ctx);
            },
        );
        assert!(!app.outputs[0].config.mute);
        assert!(app.runtime.solo_devices.is_empty());
    }
    #[test]
    fn changed_channel_layout_requires_reenable_and_keeps_manual_delay() {
        let mut app = board();
        app.devices = app.outputs.iter().map(|o| o.device.clone()).collect();
        let mut devices = app.devices.clone();
        devices[0].channels = 6;
        devices[0].channel_mask = 63;
        app.outputs[0].config.delay_ms = 100.0;
        app.outputs[0].config.gain_db = -18.0;
        app.apply_devices(devices.clone());
        assert!(!app.outputs[0].enabled);
        assert_eq!(app.outputs[0].config.channels.len(), 6);
        assert_eq!(app.outputs[0].config.delay_ms, 100.0);
        assert_eq!(app.outputs[0].config.gain_db, -18.0);
        app.apply_devices(devices);
        assert_eq!(app.outputs[0].config.delay_ms, 100.0);
    }

    #[test]
    fn solo_is_temporary_and_never_overwrites_saved_mutes_or_calibration() {
        let mut app = board();
        app.outputs[0].config.delay_ms = 100.0;
        app.outputs[0].config.channels[1].mute = true;
        let original = app.snapshot();
        app.runtime.solo_channels.insert(("device-0".into(), 0));
        assert!(!app.effective_config(&app.outputs[0]).channels[0].mute);
        assert!(app.effective_config(&app.outputs[0]).channels[1].mute);
        assert!(app.effective_config(&app.outputs[1]).mute);
        assert!(app.snapshot() == original);
        app.runtime.solo_channels.clear();
        assert_eq!(app.effective_config(&app.outputs[0]), app.outputs[0].config);
        assert_eq!(app.effective_config(&app.outputs[0]).delay_ms, 100.0);
    }

    #[test]
    fn unplug_replug_retains_enabled_output_order_and_calibration() {
        let mut app = board();
        app.devices = app.outputs.iter().map(|o| o.device.clone()).collect();
        app.outputs[0].alias = "Monitor".into();
        app.outputs[0].config.delay_ms = 100.0;
        app.outputs[0].config.eq[2].gain_db = 5.0;
        app.main_clock_id = Some("device-0".into());
        let original = app.snapshot();
        let devices = app.devices.clone();
        app.apply_devices(vec![devices[1].clone()]);
        assert!(!app.outputs[0].available);
        assert!(app.outputs[0].enabled);
        assert!(app.snapshot() == original);
        app.apply_devices(devices);
        assert!(app.outputs[0].available);
        assert!(app.snapshot() == original);
        assert!(app.engine.is_none());
    }

    #[test]
    fn loading_offline_outputs_retains_their_configuration() {
        let mut app = board();
        let mut preset = app.preset();
        let mut config = OutputConfig::identity(2, 2);
        config.delay_ms = 100.0;
        config.eq[0].gain_db = 6.0;
        preset
            .outputs
            .push(SavedOutput::new("offline".into(), true, &config));
        app.apply_preset(preset).unwrap();
        let output = app
            .outputs
            .iter()
            .find(|o| o.device.id == "offline")
            .unwrap();
        assert!(output.enabled && !output.available);
        assert_eq!(output.config, config);
    }

    #[test]
    fn scenes_recall_live_controls_but_leave_calibration_clock_and_limiter_alone() {
        let mut app = board();
        app.outputs[0].config.delay_ms = 20.0;
        app.runtime.scene_name = "Desk".into();
        app.save_scene();
        assert_eq!(app.runtime.scenes.len(), 1);
        app.outputs[0].config.delay_ms = 100.0;
        app.outputs[0].config.limiter.ceiling_db = -3.0;
        app.outputs[0].config.gain_db = -24.0;
        app.outputs[0].config.eq[2].gain_db = 7.0;
        app.main_clock_id = Some("device-1".into());
        app.recall_scene(0);
        assert!(!app.is_error);
        assert_eq!(app.outputs[0].config.delay_ms, 100.0);
        assert_eq!(app.outputs[0].config.limiter.ceiling_db, -3.0);
        assert_eq!(app.outputs[0].config.gain_db, -12.0);
        assert_eq!(app.outputs[0].config.eq[2].gain_db, 0.0);
        assert_eq!(app.main_clock_id.as_deref(), Some("device-1"));
        assert!(app.engine.is_none());
    }

    #[test]
    fn incompatible_scene_is_rejected_before_any_mutation() {
        let mut app = board();
        app.runtime.scene_name = "Desk".into();
        app.save_scene();
        app.runtime.scenes[0].source_id = "different-source".into();
        app.outputs[0].config.delay_ms = 100.0;
        let original = app.snapshot();
        app.recall_scene(0);
        assert!(app.is_error);
        assert!(app.snapshot() == original);
    }

    fn board() -> MatrixApp {
        let outputs = (0..2)
            .map(|index| OutputState {
                device: DeviceInfo {
                    id: format!("device-{index}"),
                    name: format!("Test Output {index}"),
                    kind: DeviceKind::Playback,
                    channels: 2,
                    sample_rate: 48000,
                    channel_mask: 3,
                },
                enabled: true,
                config: OutputConfig::identity(2, 2),
                alias: String::new(),
                stereo_link: false,
                available: true,
            })
            .collect();
        MatrixApp {
            devices: Vec::new(),
            outputs,
            source_mode: SourceMode::Tone,
            source_id: String::new(),
            latency_ms: 30,
            main_clock_id: None,
            auto_align: true,
            global_volume: GlobalVolume::default(),
            eq_presets: Vec::new(),
            engine: None,
            status: String::new(),
            is_error: false,
            preset_path: "unused.json".into(),
            saved: None,
            saved_path: None,
            history_epoch: 0,
            history: History::default(),
            pending_edit: None,
            picker: None,
            runtime: Runtime {
                disabled_persistence: true,
                ..Default::default()
            },
        }
    }

    fn text_shapes(shape: &egui::Shape, texts: &mut Vec<(String, egui::Rect)>) {
        match shape {
            egui::Shape::Text(text) => {
                texts.push((text.galley.job.text.clone(), text.visual_bounding_rect()))
            }
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    text_shapes(shape, texts);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn dragging_eq_nodes_changes_only_that_bands_frequency_and_gain() {
        let ctx = egui::Context::default();
        let mut bands = matrix_core::flat_eq();
        let before = bands.clone();
        let mut rect = egui::Rect::NOTHING;
        let mut frame = |events| {
            let _ = ctx.run(
                egui::RawInput {
                    events,
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(600.0, 300.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        rect = eq_curve(ui, &mut bands, 48000).rect;
                    });
                },
            );
            (rect, bands.clone())
        };
        let (rect, _) = frame(Vec::new());
        let point = egui::pos2(
            rect.left() + (1000.0_f32 / 20.0).ln() / (20000.0_f32 / 20.0).ln() * rect.width(),
            rect.center().y,
        );
        frame(vec![
            egui::Event::PointerMoved(point),
            egui::Event::PointerButton {
                pos: point,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
        ]);
        let (_, after) = frame(vec![egui::Event::PointerMoved(
            point + egui::vec2(24.0, -12.0),
        )]);
        assert!(after[2].frequency > before[2].frequency && after[2].gain_db > 0.0);
        assert_eq!(after[2].q, before[2].q);
        assert_eq!(after[0], before[0]);
        assert_eq!(after[4], before[4]);
    }

    #[test]
    fn eq_copy_paste_and_ab_comparison_recall_distinct_shapes() {
        let ctx = egui::Context::default();
        let mut bands = matrix_core::flat_eq();
        let mut library = Vec::new();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 800.0));
        let render =
            |ctx: &egui::Context, bands: &mut Vec<EqBand>, library: &mut Vec<EqPreset>, events| {
                ctx.run(
                    egui::RawInput {
                        events,
                        screen_rect: Some(screen),
                        ..Default::default()
                    },
                    |ctx| {
                        egui::CentralPanel::default().show(ctx, |ui| {
                            eq_editor(ui, "test-eq", bands, 48000, library, None)
                        });
                    },
                )
            };
        let click = |label: &str, bands: &mut Vec<EqBand>, library: &mut Vec<EqPreset>| {
            let output = render(&ctx, bands, library, Vec::new());
            let mut texts = Vec::new();
            for shape in output.shapes {
                text_shapes(&shape.shape, &mut texts);
            }
            let point = texts
                .iter()
                .find(|(text, _)| text == label)
                .unwrap()
                .1
                .center();
            render(
                &ctx,
                bands,
                library,
                vec![
                    egui::Event::PointerMoved(point),
                    egui::Event::PointerButton {
                        pos: point,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
            render(
                &ctx,
                bands,
                library,
                vec![egui::Event::PointerButton {
                    pos: point,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                }],
            );
        };
        render(&ctx, &mut bands, &mut library, Vec::new());
        bands[2].gain_db = 6.0;
        click("B", &mut bands, &mut library);
        assert_eq!(bands[2].gain_db, 0.0);
        bands[2].gain_db = -3.0;
        click("A", &mut bands, &mut library);
        assert_eq!(bands[2].gain_db, 6.0);
        click("Copy", &mut bands, &mut library);
        bands[2].gain_db = -8.0;
        click("Paste", &mut bands, &mut library);
        assert_eq!(bands[2].gain_db, 6.0);
    }

    #[test]
    fn stereo_link_preserves_gain_offsets_routes_polarity_delay_and_limiter() {
        let mut config = OutputConfig::identity(2, 2);
        config.channels[1].gain_db = -6.0;
        config.channels[1].invert = true;
        config.delay_ms = 123.0;
        let before = config.clone();
        config.channels[0].gain_db = -3.0;
        config.channels[0].mute = true;
        config.channels[0].eq[0].gain_db = 2.0;
        link_pair(&before.channels, &mut config);
        assert_eq!(config.channels[1].gain_db, -9.0);
        assert!(config.channels[1].mute);
        assert_eq!(config.channels[1].eq, config.channels[0].eq);
        assert_eq!(config.channels[1].routes, before.channels[1].routes);
        assert_eq!(config.channels[1].invert, before.channels[1].invert);
        assert_eq!(config.delay_ms, before.delay_ms);
        assert_eq!(config.limiter, before.limiter);
    }

    #[test]
    fn snapshots_restore_device_order_calibration_and_mixing_edits_without_opening_audio() {
        let mut app = board();
        app.outputs[0].config.delay_ms = 100.0;
        app.outputs[0].alias = "Monitor".into();
        let original = app.snapshot();
        app.saved = Some(original.clone());
        app.outputs.swap(0, 1);
        app.global_volume.gain_db = -12.0;
        app.outputs[1].config.eq[0].gain_db = 6.0;
        let changed = app.snapshot();
        assert!(changed != original);
        app.history.push(original.clone());
        let restored = app.history.undo(changed.clone()).unwrap();
        app.restore(restored);
        assert!(app.snapshot() == original);
        assert!(app.engine.is_none());
        assert_eq!(app.outputs[0].config.delay_ms, 100.0);
        let restored = app.history.redo(app.snapshot()).unwrap();
        app.restore(restored);
        assert!(app.snapshot() == changed);
        assert!(app.engine.is_none());
    }

    #[test]
    fn native_picker_cancellation_and_errors_leave_preset_and_audio_state_unchanged() {
        let mut app = board();
        let original = app.snapshot();
        let path = app.preset_path.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        app.picker = Some(rx);
        tx.send((false, Ok(None))).unwrap();
        app.poll_picker();
        assert!(app.picker.is_none());
        assert_eq!(app.preset_path, path);
        assert!(app.snapshot() == original);
        let (tx, rx) = std::sync::mpsc::channel();
        app.picker = Some(rx);
        tx.send((true, Err("test failure".into()))).unwrap();
        app.poll_picker();
        assert!(app.is_error);
        assert!(app.snapshot() == original);
        assert!(app.engine.is_none());
    }

    #[test]
    fn dark_theme_remains_dark_when_windows_reports_light() {
        let ctx = egui::Context::default();
        ctx.set_theme(egui::Theme::Light);
        configure_dark_theme(&ctx);
        let _ = ctx.run(
            egui::RawInput {
                system_theme: Some(egui::Theme::Light),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |_| {});
            },
        );
        assert_eq!(ctx.theme(), egui::Theme::Dark);
        assert_eq!(
            ctx.style().visuals.panel_fill,
            Color32::from_rgb(15, 17, 22)
        );
    }

    #[test]
    fn main_clock_checkboxes_are_exclusive_and_disabling_main_clears_selection() {
        let ctx = egui::Context::default();
        let mut app = board();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 1000.0));
        for _ in 0..3 {
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(screen),
                    ..Default::default()
                },
                |ctx| app.render(ctx),
            );
        }
        for (index, expected) in [(0, Some("device-0")), (1, Some("device-1")), (1, None)] {
            let mut texts = Vec::new();
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(screen),
                    ..Default::default()
                },
                |ctx| app.render(ctx),
            );
            for shape in output.shapes {
                text_shapes(&shape.shape, &mut texts);
            }
            let position = texts
                .iter()
                .filter(|(text, _)| text == "Main clock")
                .nth(index)
                .unwrap()
                .1
                .center();
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(screen),
                    events: vec![
                        egui::Event::PointerMoved(position),
                        egui::Event::PointerButton {
                            pos: position,
                            button: egui::PointerButton::Primary,
                            pressed: true,
                            modifiers: egui::Modifiers::NONE,
                        },
                        egui::Event::PointerButton {
                            pos: position,
                            button: egui::PointerButton::Primary,
                            pressed: false,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                    ..Default::default()
                },
                |ctx| app.render(ctx),
            );
            assert_eq!(app.main_clock_id.as_deref(), expected);
        }
        app.main_clock_id = Some("device-0".into());
        app.outputs[0].enabled = false;
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            },
            |ctx| app.render(ctx),
        );
        assert_eq!(app.main_clock_id, None);
        assert!(app.engine.is_none());
    }

    #[test]
    fn automatic_alignment_toggle_preserves_each_devices_manual_trim() {
        let ctx = egui::Context::default();
        let mut app = board();
        app.outputs[0].config.delay_ms = 12.5;
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 1000.0));
        for expected in [false, true] {
            let mut texts = Vec::new();
            for _ in 0..3 {
                texts.clear();
                let output = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(screen),
                        ..Default::default()
                    },
                    |ctx| app.render(ctx),
                );
                for shape in output.shapes {
                    text_shapes(&shape.shape, &mut texts);
                }
            }
            let position = texts
                .iter()
                .find(|(text, _)| text == "Auto-align outputs")
                .unwrap()
                .1
                .center();
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(screen),
                    events: vec![
                        egui::Event::PointerMoved(position),
                        egui::Event::PointerButton {
                            pos: position,
                            button: egui::PointerButton::Primary,
                            pressed: true,
                            modifiers: egui::Modifiers::NONE,
                        },
                        egui::Event::PointerButton {
                            pos: position,
                            button: egui::PointerButton::Primary,
                            pressed: false,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                    ..Default::default()
                },
                |ctx| app.render(ctx),
            );
            assert_eq!(app.auto_align, expected);
            assert_eq!(app.outputs[0].config.delay_ms, 12.5);
            assert_eq!(app.outputs[1].config.delay_ms, 0.0);
            assert!(app.engine.is_none());
        }
    }

    #[test]
    fn knob_mapping_and_multi_frame_drag_are_bounded_and_resettable() {
        for logarithmic in [false, true] {
            let range = if logarithmic {
                20.0..=20000.0
            } else {
                -18.0..=18.0
            };
            for fraction in [0.0, 0.25, 0.5, 0.75, 1.0] {
                let value = knob_value(fraction, &range, logarithmic);
                assert!((knob_fraction(value, &range, logarithmic) - fraction).abs() < 0.0001);
            }
            assert_eq!(knob_value(-1.0, &range, logarithmic), *range.start());
            assert!((knob_value(2.0, &range, logarithmic) - *range.end()).abs() < 0.01);
        }
        let ctx = egui::Context::default();
        let mut value = 0.0;
        let mut rect = egui::Rect::NOTHING;
        let mut knob_id = egui::Id::NULL;
        let mut frame = |events: Vec<egui::Event>, modifiers: egui::Modifiers| {
            let _ = ctx.run(
                egui::RawInput {
                    events,
                    modifiers,
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(300.0, 300.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        let response = knob(
                            ui,
                            &mut value,
                            KnobSpec {
                                label: "Gain",
                                range: -18.0..=18.0,
                                default: 0.0,
                                logarithmic: false,
                                unit: "dB",
                            },
                        );
                        rect = response.rect;
                        knob_id = response.id;
                    });
                },
            );
            (value, rect, knob_id)
        };
        let (_, rect, id) = frame(Vec::new(), egui::Modifiers::NONE);
        let center = rect.center();
        frame(
            vec![
                egui::Event::PointerMoved(center),
                egui::Event::PointerButton {
                    pos: center,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            egui::Modifiers::NONE,
        );
        let first = frame(
            vec![egui::Event::PointerMoved(center + egui::vec2(0.0, -15.0))],
            egui::Modifiers::NONE,
        )
        .0;
        let second = frame(
            vec![egui::Event::PointerMoved(center + egui::vec2(0.0, -30.0))],
            egui::Modifiers::NONE,
        )
        .0;
        assert!(first > 0.0 && second > first);
        let last = center + egui::vec2(0.0, -30.0);
        frame(
            vec![egui::Event::PointerButton {
                pos: last,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
            egui::Modifiers::NONE,
        );
        ctx.memory_mut(|memory| memory.request_focus(id));
        let reset = frame(
            vec![egui::Event::Key {
                key: egui::Key::Home,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            egui::Modifiers::NONE,
        )
        .0;
        assert_eq!(reset, 0.0);
    }

    #[test]
    fn enabled_outputs_share_one_board_at_supported_window_sizes() {
        for size in [
            [960.0, 640.0],
            [1280.0, 820.0],
            [1920.0, 1080.0],
            [1280.0, 2600.0],
        ] {
            let ctx = egui::Context::default();
            let mut app = board();
            let mut texts = Vec::new();
            // Grid sizing stabilizes after the initial layout pass.
            for _ in 0..3 {
                texts.clear();
                let output = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size.into())),
                        ..Default::default()
                    },
                    |ctx| app.render(ctx),
                );
                for shape in output.shapes {
                    text_shapes(&shape.shape, &mut texts);
                }
            }
            for index in 0..2 {
                let headings: Vec<_> = texts
                    .iter()
                    .filter(|(text, rect)| {
                        text == &format!("Test Output {index}") && rect.left() > 180.0
                    })
                    .collect();
                assert!(
                    !headings.is_empty(),
                    "output {index} missing from board at {size:?}: {texts:?}"
                );
                assert!(
                    headings.iter().all(|(_, rect)| rect.right() <= size[0]),
                    "output heading exceeds window at {size:?}: {headings:?}"
                );
            }
            assert_eq!(texts.iter().filter(|(text, _)| text == "Master").count(), 2);
            assert_eq!(
                texts
                    .iter()
                    .filter(|(text, _)| text == "Global volume")
                    .count(),
                1
            );
            assert_eq!(
                texts
                    .iter()
                    .filter(|(text, _)| text == "Manual delay")
                    .count(),
                2
            );
            assert_eq!(
                texts
                    .iter()
                    .filter(|(text, _)| text == "Main clock")
                    .count(),
                2
            );
            if size[1] > 2000.0 {
                for label in [
                    "Device equalizer",
                    "CHANNEL ROUTING",
                    "Limiter",
                    "Gain reduction",
                ] {
                    assert_eq!(texts.iter().filter(|(text, _)| text == label).count(), 2);
                }
                assert_eq!(
                    texts
                        .iter()
                        .filter(|(text, _)| text.contains("· Channel EQ"))
                        .count(),
                    4
                );
                assert_eq!(
                    texts.iter().filter(|(text, _)| text == "EQ preset").count(),
                    6
                );
                assert_eq!(texts.iter().filter(|(text, _)| text == "Band 1").count(), 6);
            }
            assert!(
                app.engine.is_none(),
                "drawing the board must never open audio"
            );
        }
    }
}
