use super::*;
use ::windows::{
    Win32::{
        Media::Audio::{
            AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
            IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
        },
        System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree},
    },
    core::HSTRING,
};
use matrix_core::{
    alignment::{Alignment, PlaybackTracker},
    dsp::{BlockMetrics, Processor},
    timeline::{Reader, Timeline},
};
use std::{
    sync::RwLock,
    thread,
    time::{Duration, Instant},
};
use wasapi::{DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};

type AudioResult<T> = Result<T, Box<dyn std::error::Error>>;
pub(super) type SyncControl = Arc<RwLock<Alignment>>;

struct Realtime(Option<::windows::Win32::Foundation::HANDLE>);
impl Realtime {
    fn new() -> Self {
        use ::windows::{
            Win32::System::Threading::{
                AVRT_PRIORITY_HIGH, AvSetMmThreadCharacteristicsW, AvSetMmThreadPriority,
            },
            core::w,
        };
        let mut index = 0;
        // MMCSS is scoped to this worker, not REALTIME_PRIORITY_CLASS and not a
        // registry change. Unavailable scheduling degrades to ordinary threads.
        let handle = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut index) }.ok();
        if let Some(handle) = handle {
            let _ = unsafe { AvSetMmThreadPriority(handle, AVRT_PRIORITY_HIGH) };
        }
        Self(handle)
    }
}
impl Drop for Realtime {
    fn drop(&mut self) {
        if let Some(handle) = self.0 {
            let _ = unsafe {
                ::windows::Win32::System::Threading::AvRevertMmThreadCharacteristics(handle)
            };
        }
    }
}

fn qpc_hns() -> u64 {
    use ::windows::Win32::System::Performance::{
        QueryPerformanceCounter, QueryPerformanceFrequency,
    };
    use std::sync::OnceLock;
    static FREQUENCY: OnceLock<i64> = OnceLock::new();
    let frequency = *FREQUENCY.get_or_init(|| {
        let mut value = 0;
        // SAFETY: the output points to a live i64.
        let _ = unsafe { QueryPerformanceFrequency(&mut value) };
        value
    });
    let mut counter = 0;
    // SAFETY: the output points to a live i64.
    if frequency <= 0 || unsafe { QueryPerformanceCounter(&mut counter) }.is_err() {
        return 0;
    }
    (counter as u128 * 10_000_000 / frequency as u128) as u64
}

// Balanced COM initialization on every owner thread. Declare before audio
// objects so its destructor runs after all of their COM interfaces are released.
struct ComApartment(std::marker::PhantomData<*mut ()>);

impl ComApartment {
    fn new() -> AudioResult<Self> {
        wasapi::initialize_mta().ok()?;
        Ok(Self(std::marker::PhantomData))
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        // SAFETY: successful CoInitializeEx above is balanced on the same thread;
        // PhantomData prevents moving the apartment guard to another thread.
        wasapi::deinitialize();
    }
}

pub fn enumerate() -> Result<Vec<DeviceInfo>, String> {
    enumerate_inner().map_err(|e| e.to_string())
}

pub(super) fn watch_devices(
    tx: mpsc::Sender<Result<Vec<DeviceInfo>, String>>,
    stop: mpsc::Receiver<()>,
) {
    let run = || -> AudioResult<()> {
        let _apartment = ComApartment::new()?;
        let enumerator = DeviceEnumerator::new()?;
        let (wake, notifications) = mpsc::sync_channel(1);
        let mut callbacks = wasapi::DeviceEventCallbacks::new();
        let signal = wake.clone();
        callbacks.set_device_state_callback(move |_, _| {
            let _ = signal.try_send(());
        });
        let signal = wake.clone();
        callbacks.set_device_added_callback(move |_| {
            let _ = signal.try_send(());
        });
        let signal = wake.clone();
        callbacks.set_device_removed_callback(move |_| {
            let _ = signal.try_send(());
        });
        callbacks.set_property_value_callback(move |_, _| {
            let _ = wake.try_send(());
        });
        // Retained registration is unregistered before COM uninitialization.
        let _registration = enumerator.register_notification_callback(callbacks)?;
        let mut previous = Vec::new();
        let mut scan = Instant::now() - Duration::from_secs(10);
        loop {
            match stop.recv_timeout(Duration::from_millis(100)) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if notifications.try_iter().next().is_some() || scan.elapsed() >= Duration::from_secs(5)
            {
                scan = Instant::now();
                match enumerate_inner() {
                    Ok(devices) if devices != previous => {
                        previous.clone_from(&devices);
                        if tx.send(Ok(devices)).is_err() {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(error) => {
                        let _ = tx.send(Err(error.to_string()));
                    }
                }
            }
        }
        Ok(())
    };
    if let Err(error) = run() {
        let _ = tx.send(Err(format!("Endpoint watcher: {error}")));
    }
}

fn enumerate_inner() -> AudioResult<Vec<DeviceInfo>> {
    let _apartment = ComApartment::new()?;
    let enumerator = DeviceEnumerator::new()?;
    let mut result = Vec::new();
    for (direction, kind) in [
        (Direction::Render, DeviceKind::Playback),
        (Direction::Capture, DeviceKind::Capture),
    ] {
        let collection = enumerator.get_device_collection(&direction)?;
        for index in 0..collection.get_nbr_devices()? {
            let device = collection.get_device_at_index(index)?;
            // A disappearing device should not prevent listing the other endpoints.
            let info = (|| -> AudioResult<DeviceInfo> {
                let format = device.get_iaudioclient()?.get_mixformat()?;
                Ok(DeviceInfo {
                    id: device.get_id()?,
                    name: device.get_friendlyname()?,
                    kind,
                    channels: format.get_nchannels() as usize,
                    sample_rate: format.get_samplespersec(),
                    channel_mask: format.get_dwchannelmask(),
                })
            })();
            if let Ok(info) = info
                && (1..=32).contains(&info.channels)
            {
                result.push(info);
            }
        }
    }
    result.sort_by_cached_key(|a| a.name.to_lowercase());
    Ok(result)
}

pub fn start(
    source: Source,
    outputs: Vec<(DeviceInfo, OutputConfig)>,
    latency_ms: u32,
    global_volume: GlobalVolume,
) -> Result<Engine, String> {
    let stop = Arc::new(AtomicBool::new(false));
    let (events_tx, events) = mpsc::channel();
    let mut engine = Engine {
        spectrum_enabled: Arc::new(AtomicBool::new(false)),
        stop: stop.clone(),
        source: source.clone(),
        source_ready: Arc::new(AtomicBool::new(false)),
        capture_worker: None,
        retired_workers: Vec::new(),
        events_tx: events_tx.clone(),
        next_generation: 1,
        latency_ms,
        global_volume: Arc::new(Mutex::new(global_volume)),
        alignment: Arc::new(RwLock::new(Alignment::new(source.sample_rate()))),
        timeline: Arc::new(Mutex::new(Timeline::new(
            source.channels(),
            source.sample_rate(),
        ))),
        outputs: Vec::new(),
        events,
    };
    for (device, config) in outputs {
        engine.add_output(device, config)?;
    }
    let capture_stop = stop.clone();
    let buffers = engine.timeline.clone();
    let ready = engine.source_ready.clone();
    let worker = thread::Builder::new()
        .name("capture".into())
        .spawn(move || {
            if let Err(error) = capture(source, buffers, &capture_stop, &events_tx, &ready) {
                capture_stop.store(true, Ordering::Release);
                let _ = events_tx.send(EngineEvent::SourceError(format!("Input: {error}")));
            }
            ready.store(false, Ordering::Release);
        })
        .map_err(|e| e.to_string())?;
    engine.capture_worker = Some(worker);
    Ok(engine)
}

pub(super) fn spawn_output(
    engine: &Engine,
    device: DeviceInfo,
    config: OutputConfig,
    generation: u64,
) -> Result<OutputHandle, String> {
    let rate = engine.source.sample_rate();
    let channels = engine.source.channels();
    let latency_ms = engine.latency_ms;
    engine
        .alignment
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .register(generation);
    let stats = Arc::new(Mutex::new(OutputStats {
        peaks: vec![0.0; device.channels],
        ..Default::default()
    }));
    let ready = Arc::new(AtomicBool::new(false));
    let local_stop = Arc::new(AtomicBool::new(false));
    let (updates, rx) = mpsc::channel();
    let device_id = device.id.clone();
    let mut handle = OutputHandle {
        device_id: device_id.clone(),
        generation,
        updates,
        stats: stats.clone(),
        ready: ready.clone(),
        stop: local_stop.clone(),
        worker: None,
    };
    let global_stop = engine.stop.clone();
    let events = engine.events_tx.clone();
    let name = device.name.clone();
    let worker_timeline = engine.timeline.clone();
    let worker_buffers = engine.alignment.clone();
    let worker_volume = engine.global_volume.clone();
    let worker_spectrum = engine.spectrum_enabled.clone();
    let worker = thread::Builder::new()
        .name(format!("render-{name}"))
        .spawn(move || {
            if let Err(error) = render(
                device,
                config,
                rx,
                worker_timeline,
                worker_volume,
                worker_spectrum,
                stats,
                &global_stop,
                &local_stop,
                &events,
                &ready,
                generation,
                &worker_buffers,
                channels,
                rate,
                latency_ms,
            ) {
                let _ = events.send(EngineEvent::OutputError {
                    device_id,
                    generation,
                    message: format!("Output {name}: {error}"),
                });
            }
            ready.store(false, Ordering::Release);
            let mut fanout = worker_buffers.write().unwrap_or_else(|e| e.into_inner());
            fanout.remove(generation);
        })
        .map_err(|e| {
            engine
                .alignment
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .remove(generation);
            e.to_string()
        })?;
    handle.worker = Some(worker);
    Ok(handle)
}

fn publish(samples: &[f32], timeline: &Arc<Mutex<Timeline>>) {
    timeline
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(samples);
}

fn capture(
    source: Source,
    buffers: Arc<Mutex<Timeline>>,
    stop: &AtomicBool,
    events: &mpsc::Sender<EngineEvent>,
    ready: &AtomicBool,
) -> AudioResult<()> {
    let _realtime = Realtime::new();
    if let Source::Tone = source {
        ready.store(true, Ordering::Release);
        let _ = events.send(EngineEvent::SourceReady(
            "Test tone · 440 Hz · -30 dBFS".into(),
        ));
        let start = Instant::now();
        let mut produced = 0_u64;
        let mut samples = vec![0.0_f32; 480 * 2];
        while !stop.load(Ordering::Acquire) {
            let expected = (start.elapsed().as_secs_f64() * 48000.0) as u64;
            if produced + 480 > expected {
                thread::sleep(Duration::from_millis(1));
                continue;
            }
            // Skip stale tone frames after a scheduler stall instead of flooding queues.
            if expected.saturating_sub(produced) > 4800 {
                produced = expected.saturating_sub(480);
            }
            for (index, frame) in samples.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                let sample = (std::f64::consts::TAU * 440.0 * (produced + index as u64) as f64
                    / 48000.0)
                    .sin() as f32
                    * 0.03162278;
                frame.fill(sample);
            }
            publish(&samples, &buffers);
            produced += 480;
        }
        return Ok(());
    }
    let Source::Endpoint(info) = source else {
        unreachable!()
    };
    let _apartment = ComApartment::new()?;
    // Use the native capture interface because the pinned wasapi-rs buffer-copy
    // helpers dereference the packet pointer even when Windows marks it silent.
    // SAFETY: COM is initialized on this owner thread and the endpoint ID is a
    // NUL-terminated HSTRING. These interfaces never leave the worker thread.
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
    let device = unsafe { enumerator.GetDevice(&HSTRING::from(&info.id))? };
    let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None)? };
    let format_pointer = unsafe { client.GetMixFormat()? };
    // SAFETY: GetMixFormat provides a complete WAVEFORMATEX/EXTENSIBLE allocation
    // owned by the caller. Parse copies it before we free the COM allocation.
    let format = unsafe { WaveFormat::parse(format_pointer) };
    unsafe {
        CoTaskMemFree(Some(format_pointer.cast()));
    }
    let format = format?;
    if format.get_nchannels() as usize != info.channels
        || format.get_samplespersec() != info.sample_rate
    {
        return Err("Input format changed; stop and refresh devices".into());
    }
    let block_align = format.get_blockalign() as usize;
    let bits = format.get_bitspersample();
    let sample_type = format.get_subformat()?;
    match (&sample_type, bits) {
        (SampleType::Float, 32 | 64) | (SampleType::Int, 16 | 24 | 32) => {}
        _ => return Err(format!("Unsupported capture format: {sample_type:?} {bits}-bit").into()),
    }
    // A render endpoint opened for capture is WASAPI endpoint loopback. Capturing
    // the exact mix format avoids relying on loopback format conversion support.
    let flags = if info.kind == DeviceKind::Playback {
        AUDCLNT_STREAMFLAGS_LOOPBACK
    } else {
        0
    };
    // SAFETY: native shared mix format is valid for this endpoint. The source
    // endpoint determines whether this is ordinary capture or render loopback.
    unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            flags,
            200_000,
            0,
            format.as_waveformatex_ref(),
            None,
        )?;
    }
    let capture: IAudioCaptureClient = unsafe { client.GetService()? };
    let capacity = unsafe { client.GetBufferSize()? } as usize * block_align;
    let mut bytes = Vec::with_capacity(capacity * 2);
    let mut samples = Vec::with_capacity(capacity / (bits as usize / 8));
    unsafe {
        client.Start()?;
    }
    ready.store(true, Ordering::Release);
    let _ = events.send(EngineEvent::SourceReady(format!(
        "Input {} · {} ch · {} Hz",
        info.name, info.channels, info.sample_rate
    )));
    let result = (|| -> AudioResult<()> {
        while !stop.load(Ordering::Acquire) {
            while !stop.load(Ordering::Acquire) && unsafe { capture.GetNextPacketSize()? } > 0 {
                let mut pointer = std::ptr::null_mut();
                let mut frames = 0;
                let mut flags = 0;
                // SAFETY: all outputs are valid, and each successful nonempty
                // GetBuffer is paired with ReleaseBuffer on this same thread.
                unsafe {
                    capture.GetBuffer(&mut pointer, &mut frames, &mut flags, None, None)?;
                }
                let copied = unsafe {
                    copy_capture_packet(
                        pointer,
                        frames as usize * block_align,
                        flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0,
                        &mut bytes,
                    )
                };
                let released = if frames > 0 {
                    unsafe { capture.ReleaseBuffer(frames) }
                } else {
                    Ok(())
                };
                copied?;
                released?;
                samples.clear();
                for chunk in bytes.chunks_exact(bits as usize / 8) {
                    let sample = match (&sample_type, bits) {
                        (SampleType::Float, 32) => f32::from_le_bytes(chunk.try_into().unwrap()),
                        (SampleType::Float, 64) => {
                            f64::from_le_bytes(chunk.try_into().unwrap()) as f32
                        }
                        (SampleType::Int, 16) => {
                            i16::from_le_bytes(chunk.try_into().unwrap()) as f32 / 32768.0
                        }
                        (SampleType::Int, 24) => {
                            ((i32::from_le_bytes([0, chunk[0], chunk[1], chunk[2]]) >> 8) as f32)
                                / 8388608.0
                        }
                        (SampleType::Int, 32) => {
                            i32::from_le_bytes(chunk.try_into().unwrap()) as f32 / 2147483648.0
                        }
                        _ => unreachable!(),
                    };
                    samples.push(if sample.is_finite() { sample } else { 0.0 });
                }
                publish(&samples, &buffers);
                bytes.clear();
            }
            thread::sleep(Duration::from_millis(2));
        }
        Ok(())
    })();
    let _ = unsafe { client.Stop() };
    result
}

/// Copy a held WASAPI packet, honoring SILENT before touching its pointer.
///
/// # Safety
/// A non-silent, nonempty packet must point to `length` readable bytes, valid
/// until this function returns. Silent packets do not require a valid pointer.
unsafe fn copy_capture_packet(
    pointer: *const u8,
    length: usize,
    silent: bool,
    bytes: &mut Vec<u8>,
) -> AudioResult<()> {
    bytes.resize(length, 0);
    if silent {
        bytes.fill(0);
    } else if length > 0 {
        if pointer.is_null() {
            return Err("Windows returned an empty pointer for a non-silent packet".into());
        }
        // SAFETY: the caller holds this valid native packet until ReleaseBuffer.
        bytes.copy_from_slice(unsafe { std::slice::from_raw_parts(pointer, length) });
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn render(
    info: DeviceInfo,
    config: OutputConfig,
    updates: mpsc::Receiver<OutputConfig>,
    timeline: Arc<Mutex<Timeline>>,
    global_volume: Arc<Mutex<GlobalVolume>>,
    spectrum_enabled: Arc<AtomicBool>,
    stats: Arc<Mutex<OutputStats>>,
    stop: &AtomicBool,
    local_stop: &AtomicBool,
    events: &mpsc::Sender<EngineEvent>,
    ready: &AtomicBool,
    generation: u64,
    buffers: &SyncControl,
    inputs: usize,
    rate: u32,
    latency_ms: u32,
) -> AudioResult<()> {
    let _apartment = ComApartment::new()?;
    let realtime = Realtime::new();
    let enumerator = DeviceEnumerator::new()?;
    let device = enumerator.get_device(&info.id)?;
    let mut client = device.get_iaudioclient()?;
    let native = client.get_mixformat()?;
    if native.get_nchannels() as usize != info.channels
        || native.get_dwchannelmask() != info.channel_mask
    {
        return Err("Output channel layout changed; stop and refresh devices".into());
    }
    let format = WaveFormat::new(
        32,
        32,
        &SampleType::Float,
        rate as usize,
        info.channels,
        Some(info.channel_mask),
    );
    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: latency_ms as i64 * 10_000,
    };
    client.initialize_client(&format, &Direction::Render, &mode)?;
    let event = client.set_get_eventhandle()?;
    let render = client.get_audiorenderclient()?;
    let max_frames = client.get_buffer_size()? as usize;
    let mut source = vec![0.0_f32; max_frames * inputs];
    let mut output = vec![0.0_f32; max_frames * info.channels];
    let mut bytes = vec![0_u8; max_frames * info.channels * 4];
    let mut manual_delay_ms = config.delay_ms;
    let volume = *global_volume.lock().unwrap_or_else(|e| e.into_inner());
    let mut processor = Processor::with_global_volume(inputs, rate, config, volume)?;
    let clock = client.get_audioclock().ok().and_then(|clock| {
        let frequency = clock.get_frequency().ok()?;
        (frequency > 0).then_some((clock, frequency))
    });
    // Limiter/bypass share a fixed delay. Shift every source mapping by that
    // exact number of frames so Windows-clock alignment remains truthful.
    let mut tracker =
        PlaybackTracker::with_processing_delay(rate, max_frames, processor.latency_frames());
    let mut reader = Reader::new(rate, latency_ms);
    let mut metrics = BlockMetrics::default();
    let mut analyzer = super::analysis::Tap::new(
        info.channels,
        rate,
        output.len(),
        spectrum_enabled.clone(),
        stats.clone(),
    )?;
    let mut timing = super::health::Timing::default();
    let (mut written, mut clipped, mut overloads, mut limited) = (0_u64, 0_u64, 0_u64, 0_u64);
    render.write_to_device(max_frames, &bytes, None)?;
    client.start_stream()?;
    ready.store(true, Ordering::Release);
    let _ = events.send(EngineEvent::OutputReady {
        device_id: info.id.clone(),
        generation,
        message: format!("Output {} · {} ch", info.name, info.channels),
    });
    let result = (|| -> AudioResult<()> {
        let mut missed_events = 0;
        let mut fade_remaining = None;
        let fade_frames = (rate / 100).max(1) as usize;
        while !stop.load(Ordering::Acquire) {
            if event.wait_for_event(50).is_err() {
                if local_stop.load(Ordering::Acquire) {
                    break;
                }
                // A bounded wait lets Stop always terminate. Repeated timeouts
                // indicate an endpoint that is no longer advancing.
                missed_events += 1;
                if missed_events >= 100 {
                    return Err("Device stopped delivering audio events".into());
                }
                continue;
            }
            missed_events = 0;
            let block_started = Instant::now();
            processor
                .set_global_volume(*global_volume.lock().unwrap_or_else(|e| e.into_inner()))?;
            if let Some(config) = updates.try_iter().last() {
                manual_delay_ms = config.delay_ms;
                processor.apply_config(config)?;
            }
            let frames = client.get_available_space_in_frames()? as usize;
            if frames == 0 {
                continue;
            }
            if frames > max_frames {
                return Err("Unexpected WASAPI buffer size".into());
            }
            let input = &mut source[..frames * inputs];
            let now = qpc_hns();
            let observation = clock.as_ref().and_then(|(clock, frequency)| {
                let (position, qpc) = clock.get_position().ok()?;
                tracker.observe(position, *frequency, qpc)
            });
            let plan = {
                let mut alignment = buffers.write().unwrap_or_else(|e| e.into_inner());
                alignment.observe(generation, observation, now);
                alignment.plan(generation, now)
            };
            let mapping = {
                let timeline = timeline.lock().unwrap_or_else(|e| e.into_inner());
                reader.pull(
                    &timeline,
                    input,
                    manual_delay_ms as f64 + plan.auto_delay_ms,
                    plan.reference_step,
                )
            };
            let output = &mut output[..frames * info.channels];
            processor.process(input, output, &mut metrics);
            if local_stop.load(Ordering::Acquire) {
                let remaining = fade_remaining.get_or_insert(fade_frames);
                for frame in output.chunks_exact_mut(info.channels) {
                    let gain = *remaining as f32 / fade_frames as f32;
                    for sample in frame {
                        *sample *= gain;
                    }
                    *remaining = remaining.saturating_sub(1);
                }
            }
            let bytes = &mut bytes[..output.len() * 4];
            for (sample, chunk) in output.iter().zip(bytes.as_chunks_mut::<4>().0.iter_mut()) {
                chunk.copy_from_slice(&sample.to_le_bytes());
            }
            render.write_to_device(frames, bytes, None)?;
            tracker.record(frames, mapping);
            let analyze = spectrum_enabled.load(Ordering::Acquire);
            timing.record(
                block_started.elapsed().as_secs_f32() * 1000.0,
                frames as f32 * 1000.0 / rate as f32,
            );
            if analyze {
                analyzer.offer(output);
            }
            written += frames as u64;
            clipped += metrics.clipped_samples;
            overloads += metrics.overload_samples;
            limited += metrics.limited_frames;
            if let Ok(mut stats) = stats.try_lock() {
                if !analyze {
                    stats.spectrum.clear();
                    stats.channel_spectra.clear();
                }
                stats.processing_ms = timing.mean_ms;
                stats.processing_peak_ms = timing.peak_ms;
                stats.budget_ms = timing.budget_ms;
                stats.budget_p95_percent = timing.p95_percent();
                stats.deadline_misses = timing.misses;
                stats.analysis_dropped = analyzer.dropped;
                stats.realtime_scheduled = realtime.0.is_some();
                stats.peaks.clone_from(&metrics.peaks);
                stats.frames_written = written;
                stats.clipped = clipped;
                stats.overload_samples = overloads;
                stats.limited_frames = limited;
                stats.gain_reduction_db = metrics.gain_reduction_db;
                stats.true_peak = metrics.true_peak;
                stats.pre_limiter_peak = metrics.input_peaks.iter().copied().fold(0.0, f32::max);
                stats.limiter_latency_ms = processor.latency_frames() as f32 * 1000.0 / rate as f32;
                stats.underruns = reader.underruns;
                stats.dropped_frames = reader.dropped_frames;
                stats.buffered_ms = reader.buffered_ms;
                stats.correction_ppm = (mapping.step - 1.0) * 1_000_000.0;
                stats.auto_delay_ms = plan.auto_delay_ms as f32;
                stats.manual_delay_ms = manual_delay_ms;
                stats.alignment_error_ms = plan.error_ms;
                stats.clock_available = observation.is_some();
                stats.clock_supported = clock.is_some();
                stats.measuring = plan.measuring;
            }
            if fade_remaining == Some(0) {
                // Let the submitted fade actually reach the device before Stop
                // discards queued audio. This happens only on the retiring worker.
                let deadline = Instant::now() + Duration::from_millis(latency_ms as u64 + 100);
                while !stop.load(Ordering::Acquire)
                    && Instant::now() < deadline
                    && client.get_current_padding()? > 0
                {
                    let _ = event.wait_for_event(20);
                }
                break;
            }
        }
        Ok(())
    })();
    let _ = client.stop_stream();
    result
}

#[cfg(test)]
mod tests {
    use super::copy_capture_packet;
    use super::*;

    #[test]
    fn selecting_clock_keeps_parallel_timeline_and_reader_positions() {
        let timeline = Arc::new(Mutex::new(Timeline::new(2, 48000)));
        let control: SyncControl = Arc::new(RwLock::new(Alignment::new(48000)));
        control.write().unwrap().register(1);
        control.write().unwrap().register(2);
        publish(&[0.25; 9600], &timeline);
        let mut a = Reader::new(48000, 30);
        let mut b = Reader::new(48000, 30);
        let mut x = [0.0; 960];
        let mut y = x;
        let before = a.pull(&timeline.lock().unwrap(), &mut x, 0.0, None);
        control.write().unwrap().select_clock(1);
        let after = b.pull(&timeline.lock().unwrap(), &mut y, 0.0, None);
        assert_eq!(before.source_start, after.source_start);
        assert_eq!(x, y);
        assert_eq!(timeline.lock().unwrap().end(), 4800);
        control.write().unwrap().remove(2);
        publish(&[0.5; 960], &timeline);
        control.write().unwrap().select_clock(0);
        assert!(a.pull(&timeline.lock().unwrap(), &mut x, 0.0, None).valid);
        assert_eq!(a.underruns, 0);
    }

    /// Opens real playback streams, but every sample stays muted.
    #[test]
    #[ignore = "requires two connected physical outputs; opens muted WASAPI streams"]
    fn hardware_live_output_switching_keeps_other_stream_advancing() {
        let devices = enumerate().unwrap();
        let first = devices
            .iter()
            .find(|d| {
                d.kind == DeviceKind::Playback
                    && d.name.starts_with("Alto-falantes")
                    && d.name.contains("Realtek")
            })
            .or_else(|| {
                devices.iter().find(|d| {
                    d.kind == DeviceKind::Playback
                        && d.name.contains("Realtek")
                        && !d.name.contains("Digital")
                })
            })
            .expect("a Realtek analog output");
        let second = devices
            .iter()
            .find(|d| d.kind == DeviceKind::Playback && d.name.contains("LG ULTRAGEAR"))
            .expect("a connected LG ULTRAGEAR output");
        let muted = |device: &DeviceInfo| {
            let mut config = OutputConfig::identity(2, device.channels);
            config.mute = true;
            config
        };
        let mut engine = Engine::start(
            Source::Tone,
            vec![(first.clone(), muted(first))],
            30,
            GlobalVolume::default(),
        )
        .unwrap();
        let wait = |engine: &Engine, id: &str| {
            let deadline = Instant::now() + Duration::from_secs(8);
            while Instant::now() < deadline {
                if engine.outputs.iter().any(|h| {
                    h.device_id == id
                        && h.is_ready()
                        && h.stats.lock().unwrap().frames_written > 4800
                }) {
                    return;
                }
                for event in engine.events.try_iter() {
                    if let EngineEvent::OutputError { message, .. }
                    | EngineEvent::SourceError(message) = event
                    {
                        panic!("{message}");
                    }
                }
                thread::sleep(Duration::from_millis(10));
            }
            panic!("stream did not advance: {id}");
        };
        wait(&engine, &first.id);
        let first_generation = engine.outputs[0].generation;
        let wait_progress = |engine: &Engine| {
            let before: Vec<_> = engine
                .outputs
                .iter()
                .map(|h| h.stats.lock().unwrap().frames_written)
                .collect();
            let deadline = Instant::now() + Duration::from_secs(8);
            loop {
                if engine
                    .outputs
                    .iter()
                    .zip(&before)
                    .all(|(h, old)| h.stats.lock().unwrap().frames_written > old + 4800)
                {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "streams stalled after clock change"
                );
                thread::sleep(Duration::from_millis(10));
            }
        };
        for cycle in 0..3 {
            let volume = GlobalVolume {
                gain_db: if cycle == 0 { -18.0 } else { 0.0 },
                mute: cycle == 1,
            };
            engine.set_global_volume(volume).unwrap();
            assert_eq!(*engine.global_volume.lock().unwrap(), volume);
            assert!(
                engine
                    .set_global_volume(GlobalVolume {
                        gain_db: f32::NAN,
                        mute: false
                    })
                    .is_err()
            );
            assert_eq!(*engine.global_volume.lock().unwrap(), volume);
            let frames_before = engine.outputs[0].stats.lock().unwrap().frames_written;
            let mut updated = muted(first);
            updated.gain_db = -24.0;
            updated.eq[2].gain_db = 6.0;
            updated.channels[0].routes = vec![0.5, 0.5];
            updated.channels[1].gain_db = -6.0;
            updated.limiter.enabled = cycle != 1;
            updated.limiter.ceiling_db = -2.0;
            updated.limiter.release_ms = 200.0;
            updated.limiter.true_peak = cycle != 1;
            engine.set_spectrum(cycle != 1);
            engine.outputs[0].updates.send(updated).unwrap();
            engine.add_output(second.clone(), muted(second)).unwrap();
            assert_eq!(
                *engine.global_volume.lock().unwrap(),
                volume,
                "new renderer inherits router volume"
            );
            wait(&engine, &second.id);
            engine.set_main_clock(Some(&first.id)).unwrap();
            assert_eq!(engine.main_clock_device_id(), Some(first.id.as_str()));
            wait_progress(&engine);
            if cycle != 1 {
                assert!(
                    engine
                        .outputs
                        .iter()
                        .all(|h| h.stats.lock().unwrap().spectrum.len()
                            == matrix_core::spectrum::BINS)
                );
            }
            assert!(
                engine.outputs.iter().all(|h| {
                    let stats = h.stats.lock().unwrap();
                    stats.limiter_latency_ms == 5.0
                        && stats.clipped == 0
                        && stats.peaks.iter().all(|peak| *peak == 0.0)
                }),
                "live limiter edits preserve fixed latency and muted output"
            );
            engine.set_main_clock(Some(&second.id)).unwrap();
            assert_eq!(engine.main_clock_device_id(), Some(second.id.as_str()));
            wait_progress(&engine);
            let old_generation = engine.outputs[1].generation;
            assert!(engine.remove_output(&second.id, Some(old_generation)));
            assert_eq!(
                engine.main_clock_device_id(),
                None,
                "removing main falls back to input"
            );
            engine.add_output(second.clone(), muted(second)).unwrap();
            assert!(
                !engine.remove_output(&second.id, Some(old_generation)),
                "stale failure must not remove replacement"
            );
            wait(&engine, &second.id);
            assert_eq!(engine.outputs[0].generation, first_generation);
            assert!(engine.outputs[0].stats.lock().unwrap().frames_written > frames_before);
            assert!(engine.is_live());
            assert!(
                engine.outputs[0]
                    .stats
                    .lock()
                    .unwrap()
                    .peaks
                    .iter()
                    .all(|p| *p == 0.0)
            );
            engine.remove_output(&second.id, None);
            engine.reap_finished_workers();
        }
        // Verify real Windows presentation-clock convergence, then live manual
        // trim. This measures stream timing only, never acoustic speaker delay.
        engine.add_output(second.clone(), muted(second)).unwrap();
        wait(&engine, &second.id);
        engine.set_main_clock(Some(&first.id)).unwrap();
        engine.realign();
        let wait_alignment = |engine: &Engine, second_trim: f64| {
            let started = Instant::now();
            let deadline = started + Duration::from_secs(12);
            let mut stable = 0;
            loop {
                let snapshots: Vec<_> = engine
                    .outputs
                    .iter()
                    .map(|h| h.stats.lock().unwrap().clone())
                    .collect();
                let aligned = snapshots.iter().enumerate().all(|(index, stats)| {
                    let expected = if index == 1 { -second_trim } else { 0.0 };
                    stats.clock_available
                        && !stats.measuring
                        && stats
                            .alignment_error_ms
                            .is_some_and(|error| (error - expected).abs() < 4.0)
                });
                if aligned && started.elapsed() > Duration::from_secs(2) {
                    stable += 1;
                } else {
                    stable = 0;
                }
                if stable >= 20 {
                    eprintln!(
                        "Windows clock alignment (manual trim {second_trim} ms): {snapshots:#?}"
                    );
                    assert!(snapshots.iter().all(|s| s.peaks.iter().all(|p| *p == 0.0)));
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "alignment did not converge: {snapshots:#?}"
                );
                thread::sleep(Duration::from_millis(20));
            }
        };
        wait_alignment(&engine, 0.0);
        let mut trimmed = muted(second);
        trimmed.delay_ms = 15.0;
        engine.outputs[1].updates.send(trimmed).unwrap();
        wait_alignment(&engine, 15.0);
        engine.set_auto_align(false);
        wait_progress(&engine);
        assert_eq!(
            engine.outputs[1].stats.lock().unwrap().manual_delay_ms,
            15.0
        );
        assert!(
            engine
                .outputs
                .iter()
                .all(|h| h.stats.lock().unwrap().auto_delay_ms == 0.0)
        );
        engine.set_auto_align(true);
        engine.remove_output(&second.id, None);
        engine.set_main_clock(None).unwrap();
        // An invalid endpoint fails locally; it must not stop the input or the survivor.
        let mut unavailable = second.clone();
        unavailable.id = "not-a-windows-endpoint".into();
        engine
            .add_output(unavailable.clone(), muted(&unavailable))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(8);
        let failed_generation = loop {
            if let Some(generation) = engine.events.try_iter().find_map(|event| match event {
                EngineEvent::OutputError {
                    device_id,
                    generation,
                    ..
                } if device_id == unavailable.id => Some(generation),
                EngineEvent::SourceError(message) => panic!("{message}"),
                _ => None,
            }) {
                break generation;
            }
            assert!(Instant::now() < deadline, "missing local endpoint error");
            thread::sleep(Duration::from_millis(10));
        };
        assert!(engine.remove_output(&unavailable.id, Some(failed_generation)));
        assert!(engine.set_main_clock(Some(&unavailable.id)).is_err());
        assert!(engine.is_live());
        assert_eq!(engine.outputs[0].generation, first_generation);
        engine.remove_output(&first.id, None);
        assert!(engine.outputs.is_empty());
        assert!(
            engine.is_live(),
            "input continues with all outputs disabled"
        );
        engine.add_output(first.clone(), muted(first)).unwrap();
        wait(&engine, &first.id);
        engine.stop();
        assert!(!engine.is_live());
    }

    #[test]
    fn silent_capture_does_not_dereference_null_or_reuse_old_audio() {
        let mut bytes = vec![255; 16];
        unsafe {
            copy_capture_packet(std::ptr::null(), 8, true, &mut bytes).unwrap();
        }
        assert_eq!(bytes, vec![0; 8]);
    }

    #[test]
    #[ignore = "requires Windows endpoints; registers notifications and enumerates only, never opens audio"]
    fn native_endpoint_watcher_registers_and_enumerates_without_audio() {
        let watcher = super::super::DeviceWatcher::start().unwrap();
        let devices = watcher
            .events
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap();
        assert!(!devices.is_empty());
        drop(watcher);
    }

    #[test]
    fn capture_copies_the_complete_packet() {
        let packet = [0, 1, 2, 3, 4, 5, 6, 7];
        let mut bytes = Vec::new();
        unsafe {
            copy_capture_packet(packet.as_ptr(), packet.len(), false, &mut bytes).unwrap();
        }
        assert_eq!(bytes, packet);
    }

    #[test]
    fn invalid_non_silent_capture_is_reported() {
        let mut bytes = Vec::new();
        assert!(unsafe { copy_capture_packet(std::ptr::null(), 8, false, &mut bytes) }.is_err());
    }
}
