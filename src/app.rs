use crate::{
    audio::{self, DeviceInfo, DeviceKind, Engine, EngineEvent, OutputStats, Source},
    preset::{Preset, SavedOutput, SourceMode},
};
use eframe::egui::{self, Color32, RichText};
use matrix_core::{EqBand, OutputConfig, gain_to_db};
use std::{collections::HashSet, path::Path, time::Duration};

const ACCENT: Color32 = Color32::from_rgb(87, 222, 173);
const DIM: Color32 = Color32::from_rgb(145, 156, 175);

struct OutputState {
    device: DeviceInfo,
    enabled: bool,
    config: OutputConfig,
}

pub struct MatrixApp {
    devices: Vec<DeviceInfo>,
    outputs: Vec<OutputState>,
    source_mode: SourceMode,
    source_id: String,
    latency_ms: u32,
    selected_output: usize,
    selected_channel: usize,
    engine: Option<Engine>,
    status: String,
    is_error: bool,
    preset_path: String,
    ready_workers: usize,
}

impl MatrixApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = Color32::from_rgb(16, 20, 28);
        visuals.window_fill = Color32::from_rgb(22, 28, 38);
        visuals.selection.bg_fill = Color32::from_rgb(35, 93, 77);
        visuals.selection.stroke.color = ACCENT;
        cc.egui_ctx.set_visuals(visuals);
        cc.egui_ctx.style_mut(|style| {
            style.spacing.item_spacing = egui::vec2(12.0, 10.0);
            style.spacing.button_padding = egui::vec2(12.0, 7.0);
        });
        let mut app = Self {
            devices: Vec::new(),
            outputs: Vec::new(),
            source_mode: SourceMode::Playback,
            source_id: String::new(),
            latency_ms: 30,
            selected_output: 0,
            selected_channel: 0,
            engine: None,
            status: "Choose a source and enable your output devices.".into(),
            is_error: false,
            preset_path: "matrix-preset.json".into(),
            ready_workers: 0,
        };
        app.refresh();
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
        self.devices
            .iter()
            .find(|d| d.id == self.source_id && d.kind == kind)
            .cloned()
            .map(Source::Endpoint)
            .ok_or_else(|| "Select an available input endpoint".into())
    }

    fn choose_source(&mut self) {
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
                if output
                    .config
                    .channels
                    .iter()
                    .any(|ch| ch.routes.len() != source.channels())
                {
                    output.config =
                        OutputConfig::identity(source.channels(), output.device.channels);
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
                self.devices = devices;
                let mut previous = std::mem::take(&mut self.outputs);
                let inputs = self.source().map(|s| s.channels()).unwrap_or(2);
                for device in self
                    .devices
                    .iter()
                    .filter(|d| d.kind == DeviceKind::Playback)
                {
                    if let Some(index) = previous.iter().position(|o| {
                        o.device.id == device.id && o.device.channels == device.channels
                    }) {
                        let mut output = previous.remove(index);
                        output.device = device.clone();
                        self.outputs.push(output);
                    } else {
                        self.outputs.push(OutputState {
                            device: device.clone(),
                            enabled: false,
                            config: OutputConfig::identity(inputs, device.channels),
                        });
                    }
                }
                self.selected_output = self
                    .selected_output
                    .min(self.outputs.len().saturating_sub(1));
                self.choose_source();
                self.report(Ok(format!(
                    "{} playback devices · {} capture devices",
                    self.outputs.len(),
                    self.devices.len() - self.outputs.len()
                )));
            }
            Err(error) => self.report(Err(error)),
        }
    }

    fn start(&mut self) {
        let result = self.source().and_then(|source| {
            Engine::start(
                source,
                self.outputs
                    .iter()
                    .filter(|o| o.enabled)
                    .map(|o| (o.device.clone(), o.config.clone()))
                    .collect(),
                self.latency_ms,
            )
        });
        match result {
            Ok(engine) => {
                self.engine = Some(engine);
                self.ready_workers = 0;
                self.report(Ok("Opening audio streams…".into()));
            }
            Err(error) => self.report(Err(error)),
        }
    }

    fn stop(&mut self) {
        self.engine.take(); // Drop joins the workers and closes their streams.
        self.ready_workers = 0;
    }

    fn poll_engine(&mut self) {
        let mut error = None;
        if let Some(engine) = &self.engine {
            for event in engine.events.try_iter() {
                match event {
                    EngineEvent::Ready(message) => {
                        self.ready_workers += 1;
                        self.status = message;
                    }
                    EngineEvent::Error(message) => error = Some(message),
                }
            }
        }
        if let Some(error) = error {
            self.stop();
            self.report(Err(error));
        }
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
        let preset = Preset {
            version: 1,
            source_mode: self.source_mode,
            source_id: self.source_id.clone(),
            latency_ms: self.latency_ms,
            outputs: self
                .outputs
                .iter()
                .map(|o| SavedOutput::new(o.device.id.clone(), o.enabled, &o.config))
                .collect(),
        };
        let result = preset
            .save(Path::new(&self.preset_path))
            .map(|_| format!("Saved {}", self.preset_path));
        self.report(result);
    }

    fn load(&mut self) {
        let result = (|| -> Result<String, String> {
            let preset = Preset::load(Path::new(&self.preset_path))?;
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
                        .ok_or("Preset input is unavailable; reconnect it and refresh devices")?,
                )
            };
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
                    {
                        if input.kind == DeviceKind::Playback && input.id == output.device.id {
                            return Err("Preset routes captured playback into itself".into());
                        }
                    }
                } else {
                    missing += 1;
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
                } else {
                    output.enabled = false;
                    output.config =
                        OutputConfig::identity(source.channels(), output.device.channels);
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
            self.source_mode = preset.source_mode;
            self.source_id = preset.source_id;
            self.latency_ms = preset.latency_ms;
            self.selected_channel = 0;
            Ok(format!(
                "Loaded {} · {missing} unavailable outputs skipped",
                self.preset_path
            ))
        })();
        self.report(result);
    }
}

impl eframe::App for MatrixApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_engine();
        ctx.request_repaint_after(Duration::from_millis(50));
        let running = self.engine.is_some();
        let live = self
            .engine
            .as_ref()
            .is_some_and(|e| self.ready_workers == e.outputs.len() + 1);
        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("AUDIO MATRIX")
                        .size(24.0)
                        .strong()
                        .color(ACCENT),
                );
                ui.label(RichText::new("0.1 / Windows audio router").color(DIM));
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
                        RichText::new(if live {
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
            ui.add_space(10.0);
        });
        // Start/Stop can change the engine during this frame.
        let running = self.engine.is_some();
        egui::TopBottomPanel::bottom("footer").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(&self.status).color(if self.is_error {
                    Color32::from_rgb(255, 126, 126)
                } else {
                    DIM
                }));
            });
            ui.horizontal(|ui| {
                ui.label("Preset");
                ui.add(egui::TextEdit::singleline(&mut self.preset_path).desired_width(230.0));
                if ui.button("Save").clicked() {
                    self.save();
                }
                if ui
                    .add_enabled(!running, egui::Button::new("Load"))
                    .clicked()
                {
                    self.load();
                }
                ui.label(
                    RichText::new("WASAPI shared · float DSP")
                        .small()
                        .color(DIM),
                );
            });
        });
        egui::SidePanel::left("devices").default_width(290.0).min_width(245.0).show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.add_space(10.0);
                ui.label(RichText::new("INPUT").strong().color(DIM));
                let before = (self.source_mode, self.source_id.clone());
                ui.add_enabled_ui(!running, |ui| {
                    egui::ComboBox::from_id_salt("source-mode").width(250.0).selected_text(match self.source_mode {
                        SourceMode::Playback => "Capture playback (loopback)", SourceMode::Capture => "Capture device / virtual cable", SourceMode::Tone => "Test tone · -30 dBFS",
                    }).show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.source_mode, SourceMode::Playback, "Capture playback (loopback)");
                        ui.selectable_value(&mut self.source_mode, SourceMode::Capture, "Capture device / virtual cable");
                        ui.selectable_value(&mut self.source_mode, SourceMode::Tone, "Test tone · -30 dBFS");
                    });
                    if self.source_mode != SourceMode::Tone {
                        let kind = if self.source_mode == SourceMode::Playback { DeviceKind::Playback } else { DeviceKind::Capture };
                        let selected = self.devices.iter().find(|d| d.id == self.source_id && d.kind == kind).map(|d| d.name.as_str()).unwrap_or("Choose endpoint");
                        egui::ComboBox::from_id_salt("source-device").width(250.0).selected_text(selected).show_ui(ui, |ui| {
                            for device in self.devices.iter().filter(|d| d.kind == kind) {
                                ui.selectable_value(&mut self.source_id, device.id.clone(), &device.name);
                            }
                        });
                    }
                    ui.add(egui::Slider::new(&mut self.latency_ms, 10..=100).suffix(" ms").text("Buffer target"));
                    if ui.button("Refresh devices").clicked() { self.refresh(); }
                });
                if before != (self.source_mode, self.source_id.clone()) { self.choose_source(); }
                if let Ok(source) = self.source() { ui.label(RichText::new(format!("{} channels / {} Hz", source.channels(), source.sample_rate())).small().color(DIM)); }
                ui.add_space(10.0);
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.label(RichText::new("Virtual input setup").strong());
                    ui.label("With an installed virtual cable: send apps to its playback endpoint, then select its capture endpoint here.");
                    ui.label(RichText::new("Our own Windows virtual endpoint is the next driver milestone.").small().color(DIM));
                });
                ui.add_space(16.0);
                ui.label(RichText::new("OUTPUT DEVICES").strong().color(DIM));
                ui.label(RichText::new("Enable to route · select to edit").small().color(DIM));
                for (index, output) in self.outputs.iter_mut().enumerate() {
                    ui.push_id(&output.device.id, |ui| {
                        egui::Frame::group(ui.style()).show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.add_enabled(!running, egui::Checkbox::without_text(&mut output.enabled));
                                if ui.selectable_label(self.selected_output == index, &output.device.name).clicked() {
                                    self.selected_output = index; self.selected_channel = 0;
                                }
                            });
                            ui.label(RichText::new(format!("{} channels · {} Hz", output.device.channels, output.device.sample_rate)).small().color(DIM));
                        });
                    });
                }
                if self.outputs.is_empty() { ui.label("No active output devices. Connect a device and refresh."); }
            });
        });
        egui::CentralPanel::default().show(ctx, |ui| {
            let source = match self.source() { Ok(s) => s, Err(_) => { ui.heading("Select an input to begin"); return; } };
            let Some(output) = self.outputs.get_mut(self.selected_output) else { return; };
            let stats = self.engine.as_ref().and_then(|e| e.outputs.iter().find(|o| o.device_id == output.device.id))
                .and_then(|o| o.stats.try_lock().ok().map(|s| s.clone())).unwrap_or_default();
            let before = output.config.clone();
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.add_space(8.0);
                ui.heading(&output.device.name);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(if output.enabled { "ENABLED OUTPUT" } else { "OUTPUT DISABLED" }).small().color(if output.enabled { ACCENT } else { DIM }));
                    ui.label(RichText::new(format!("{} output channels", output.device.channels)).small().color(DIM));
                });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.add(egui::Slider::new(&mut output.config.gain_db, -60.0..=12.0).suffix(" dB").text("Device volume"));
                    ui.checkbox(&mut output.config.mute, "Mute device");
                    if ui.button("Reset DSP").clicked() {
                        let routes: Vec<_> = output.config.channels.iter().map(|ch| ch.routes.clone()).collect();
                        output.config = OutputConfig::identity(source.channels(), output.device.channels);
                        for (ch, routes) in output.config.channels.iter_mut().zip(routes) { ch.routes = routes; }
                        clamp_eq(&mut output.config, source.sample_rate());
                    }
                });
                ui.add_space(8.0);
                ui.label(RichText::new("CHANNEL ROUTING").strong().color(DIM));
                ui.label("Rows are output channels; columns are source channels. Select a cell to connect at unity gain.");
                egui::ScrollArea::horizontal().id_salt("matrix-scroll").show(ui, |ui| {
                    egui::Grid::new("routing-matrix").spacing([8.0, 6.0]).striped(true).show(ui, |ui| {
                        ui.label("Output ↓ / Input →");
                        for input in 0..source.channels() { ui.label(source.channel_name(input)); }
                        ui.label("Volume"); ui.label("Mute"); ui.end_row();
                        for (index, channel) in output.config.channels.iter_mut().enumerate() {
                            if ui.selectable_label(self.selected_channel == index, output.device.channel_name(index)).clicked() { self.selected_channel = index; }
                            for route in &mut channel.routes {
                                let active = *route != 0.0;
                                if ui.add_sized([58.0, 27.0], egui::Button::new(if active { format!("{:.2}", *route) } else { "—".into() })
                                    .fill(if active { Color32::from_rgb(35, 93, 77) } else { Color32::from_rgb(29, 36, 48) })).clicked() {
                                    *route = if active { 0.0 } else { 1.0 };
                                }
                            }
                            ui.add(egui::DragValue::new(&mut channel.gain_db).range(-60.0..=12.0).speed(0.2).suffix(" dB"));
                            ui.checkbox(&mut channel.mute, ""); ui.end_row();
                        }
                    });
                });
                ui.label(RichText::new("Multiple connections sum. Lower route gains below to keep headroom.").small().color(DIM));
                ui.add_space(12.0);
                egui::CollapsingHeader::new("Device equalizer · applied to every output channel").default_open(true).show(ui, |ui| {
                    eq_editor(ui, "device-eq", &mut output.config.eq, source.sample_rate());
                });
                self.selected_channel = self.selected_channel.min(output.config.channels.len().saturating_sub(1));
                if let Some(channel) = output.config.channels.get_mut(self.selected_channel) {
                    ui.add_space(10.0);
                    ui.label(RichText::new(format!("{} / CHANNEL PROCESSING", output.device.channel_name(self.selected_channel))).strong().color(ACCENT));
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut channel.mute, "Mute channel"); ui.checkbox(&mut channel.invert, "Invert polarity");
                    });
                    egui::CollapsingHeader::new("Source mix coefficients").show(ui, |ui| {
                        for (index, route) in channel.routes.iter_mut().enumerate() {
                            ui.add(egui::Slider::new(route, -1.0..=1.0).text(source.channel_name(index)));
                        }
                    });
                    eq_editor(ui, "channel-eq", &mut channel.eq, source.sample_rate());
                }
                ui.add_space(14.0);
                draw_stats(ui, &stats, &output.device);
            });
            if before != output.config && let Some(engine) = &self.engine {
                if let Some(handle) = engine.outputs.iter().find(|h| h.device_id == output.device.id) {
                    let _ = handle.updates.send(output.config.clone());
                }
            }
        });
    }
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

fn eq_editor(ui: &mut egui::Ui, id: &str, eq: &mut [EqBand], rate: u32) {
    ui.push_id(id, |ui| {
        egui::ScrollArea::horizontal()
            .id_salt("eq-scroll")
            .show(ui, |ui| {
                eq_curve(ui, eq, rate);
                egui::Grid::new("bands")
                    .spacing([12.0, 6.0])
                    .striped(true)
                    .show(ui, |ui| {
                        ui.label("Band");
                        ui.label("Frequency");
                        ui.label("Gain");
                        ui.label("Q");
                        ui.end_row();
                        for (index, band) in eq.iter_mut().enumerate() {
                            ui.label(format!("Peak {}", index + 1));
                            ui.add(
                                egui::DragValue::new(&mut band.frequency)
                                    .range(20.0..=(20000.0_f32.min(rate as f32 * 0.45)))
                                    .speed(5.0)
                                    .suffix(" Hz"),
                            );
                            ui.add(
                                egui::Slider::new(&mut band.gain_db, -18.0..=18.0).suffix(" dB"),
                            );
                            ui.add(
                                egui::DragValue::new(&mut band.q)
                                    .range(0.1..=10.0)
                                    .speed(0.01),
                            );
                            ui.end_row();
                        }
                    });
            });
    });
}

fn eq_curve(ui: &mut egui::Ui, eq: &[EqBand], rate: u32) {
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width().clamp(320.0, 1000.0), 88.0),
        egui::Sense::hover(),
    );
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 4.0, Color32::from_rgb(11, 15, 22));
    painter.line_segment(
        [
            egui::pos2(rect.left(), rect.center().y),
            egui::pos2(rect.right(), rect.center().y),
        ],
        egui::Stroke::new(1.0, Color32::from_rgb(55, 65, 81)),
    );
    let maximum = 20000.0_f32.min(rate as f32 * 0.45);
    let points: Vec<_> = (0..200)
        .map(|i| {
            let t = i as f32 / 199.0;
            let frequency = 20.0 * (maximum / 20.0).powf(t);
            let w = std::f64::consts::TAU * frequency as f64 / rate as f64;
            let gain: f64 = eq
                .iter()
                .map(|band| {
                    let a = 10.0_f64.powf(band.gain_db as f64 / 40.0);
                    let w0 = std::f64::consts::TAU * band.frequency as f64 / rate as f64;
                    let alpha = w0.sin() / (2.0 * band.q as f64);
                    let magnitude = |b: [f64; 3]| {
                        let re = b[0] + b[1] * w.cos() + b[2] * (2.0 * w).cos();
                        let im = -b[1] * w.sin() - b[2] * (2.0 * w).sin();
                        re * re + im * im
                    };
                    let n = magnitude([1.0 + alpha * a, -2.0 * w0.cos(), 1.0 - alpha * a]);
                    let d = magnitude([1.0 + alpha / a, -2.0 * w0.cos(), 1.0 - alpha / a]);
                    10.0 * (n / d.max(1e-30)).max(1e-30).log10()
                })
                .sum();
            egui::pos2(
                rect.left() + t * rect.width(),
                rect.center().y - gain.clamp(-24.0, 24.0) as f32 / 48.0 * rect.height(),
            )
        })
        .collect();
    painter.add(egui::Shape::line(points, egui::Stroke::new(2.0, ACCENT)));
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
        format!("{:.0} kHz", maximum / 1000.0),
        egui::FontId::proportional(10.0),
        DIM,
    );
}

fn draw_stats(ui: &mut egui::Ui, stats: &OutputStats, device: &DeviceInfo) {
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
}
