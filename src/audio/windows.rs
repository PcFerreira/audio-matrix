use super::*;
use matrix_core::{
    buffer::AudioBuffer,
    dsp::{BlockMetrics, Processor},
};
use std::{
    collections::VecDeque,
    thread,
    time::{Duration, Instant},
};
use wasapi::{DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};

type AudioResult<T> = Result<T, Box<dyn std::error::Error>>;

// Balanced COM initialization on every owner thread. Declare before audio
// objects so its destructor runs after all of their COM interfaces are released.
struct ComApartment(std::marker::PhantomData<*mut ()>);

impl ComApartment {
    fn new() -> AudioResult<Self> {
        wasapi::initialize_mta().ok()?;
        Ok(Self(std::marker::PhantomData))
    }
}

#[link(name = "ole32")]
unsafe extern "system" {
    fn CoUninitialize();
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        // SAFETY: successful CoInitializeEx above is balanced on the same thread;
        // PhantomData prevents moving the apartment guard to another thread.
        unsafe {
            CoUninitialize();
        }
    }
}

pub fn enumerate() -> Result<Vec<DeviceInfo>, String> {
    enumerate_inner().map_err(|e| e.to_string())
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
            if let Ok(info) = info {
                if (1..=32).contains(&info.channels) {
                    result.push(info);
                }
            }
        }
    }
    result.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(result)
}

pub fn start(
    source: Source,
    outputs: Vec<(DeviceInfo, OutputConfig)>,
    latency_ms: u32,
) -> Result<Engine, String> {
    let stop = Arc::new(AtomicBool::new(false));
    let (events_tx, events) = mpsc::channel();
    let mut engine = Engine {
        stop: stop.clone(),
        workers: Vec::new(),
        outputs: Vec::new(),
        events,
    };
    let mut buffers = Vec::new();
    let rate = source.sample_rate();
    let channels = source.channels();
    for (device, config) in outputs {
        let queue = Arc::new(Mutex::new(AudioBuffer::new(channels, rate, latency_ms)));
        buffers.push(queue.clone());
        let stats = Arc::new(Mutex::new(OutputStats::default()));
        let (updates, rx) = mpsc::channel();
        engine.outputs.push(OutputHandle {
            device_id: device.id.clone(),
            updates,
            stats: stats.clone(),
        });
        let stop = stop.clone();
        let events = events_tx.clone();
        let name = device.name.clone();
        let worker = thread::Builder::new()
            .name(format!("render-{name}"))
            .spawn(move || {
                if let Err(error) = render(
                    device, config, rx, queue, stats, &stop, &events, channels, rate, latency_ms,
                ) {
                    stop.store(true, Ordering::Release);
                    let _ = events.send(EngineEvent::Error(format!("Output {name}: {error}")));
                }
            })
            .map_err(|e| e.to_string())?;
        engine.workers.push(worker);
    }
    let capture_stop = stop.clone();
    let worker = thread::Builder::new()
        .name("capture".into())
        .spawn(move || {
            if let Err(error) = capture(source, buffers, &capture_stop, &events_tx) {
                capture_stop.store(true, Ordering::Release);
                let _ = events_tx.send(EngineEvent::Error(format!("Input: {error}")));
            }
        })
        .map_err(|e| e.to_string())?;
    engine.workers.push(worker);
    Ok(engine)
}

fn publish(samples: &[f32], buffers: &[Arc<Mutex<AudioBuffer>>]) {
    for buffer in buffers {
        buffer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(samples);
    }
}

fn capture(
    source: Source,
    buffers: Vec<Arc<Mutex<AudioBuffer>>>,
    stop: &AtomicBool,
    events: &mpsc::Sender<EngineEvent>,
) -> AudioResult<()> {
    if let Source::Tone = source {
        let _ = events.send(EngineEvent::Ready("Test tone · 440 Hz · -30 dBFS".into()));
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
            for (index, frame) in samples.chunks_exact_mut(2).enumerate() {
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
    let enumerator = DeviceEnumerator::new()?;
    let device = enumerator.get_device(&info.id)?;
    let mut client = device.get_iaudioclient()?;
    let format = client.get_mixformat()?;
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
    let mode = StreamMode::PollingShared {
        autoconvert: false,
        buffer_duration_hns: 200_000,
    };
    client.initialize_client(&format, &Direction::Capture, &mode)?;
    let capture = client.get_audiocaptureclient()?;
    let capacity = client.get_buffer_size()? as usize * block_align;
    let mut bytes = VecDeque::with_capacity(capacity * 2);
    let mut samples = Vec::with_capacity(capacity / (bits as usize / 8));
    client.start_stream()?;
    let _ = events.send(EngineEvent::Ready(format!(
        "Input {} · {} ch · {} Hz",
        info.name, info.channels, info.sample_rate
    )));
    let result = (|| -> AudioResult<()> {
        while !stop.load(Ordering::Acquire) {
            while capture.get_next_packet_size()?.unwrap_or(0) > 0 {
                capture.read_from_device_to_deque(&mut bytes)?;
                samples.clear();
                let contiguous = bytes.make_contiguous();
                for chunk in contiguous.chunks_exact(bits as usize / 8) {
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
    let _ = client.stop_stream();
    result
}

#[allow(clippy::too_many_arguments)]
fn render(
    info: DeviceInfo,
    config: OutputConfig,
    updates: mpsc::Receiver<OutputConfig>,
    queue: Arc<Mutex<AudioBuffer>>,
    stats: Arc<Mutex<OutputStats>>,
    stop: &AtomicBool,
    events: &mpsc::Sender<EngineEvent>,
    inputs: usize,
    rate: u32,
    latency_ms: u32,
) -> AudioResult<()> {
    let _apartment = ComApartment::new()?;
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
    let mut processor = Processor::new(inputs, rate, config)?;
    let mut metrics = BlockMetrics::default();
    render.write_to_device(max_frames, &bytes, None)?;
    client.start_stream()?;
    let _ = events.send(EngineEvent::Ready(format!(
        "Output {} · {} ch",
        info.name, info.channels
    )));
    let result = (|| -> AudioResult<()> {
        let mut missed_events = 0;
        while !stop.load(Ordering::Acquire) {
            if event.wait_for_event(50).is_err() {
                // A bounded wait lets Stop always terminate. Repeated timeouts
                // indicate an endpoint that is no longer advancing.
                missed_events += 1;
                if missed_events >= 100 {
                    return Err("Device stopped delivering audio events".into());
                }
                continue;
            }
            missed_events = 0;
            if let Some(config) = updates.try_iter().last() {
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
            let (underruns, dropped_frames, buffered_ms, correction_ppm) = {
                let mut queue = queue.lock().unwrap_or_else(|e| e.into_inner());
                queue.pull(input);
                (
                    queue.underruns,
                    queue.dropped_frames,
                    queue.frames() as f32 / rate as f32 * 1000.0,
                    queue.correction_ppm(),
                )
            };
            let output = &mut output[..frames * info.channels];
            processor.process(input, output, &mut metrics);
            let bytes = &mut bytes[..output.len() * 4];
            for (sample, chunk) in output.iter().zip(bytes.chunks_exact_mut(4)) {
                chunk.copy_from_slice(&sample.to_le_bytes());
            }
            render.write_to_device(frames, bytes, None)?;
            if let Ok(mut stats) = stats.try_lock() {
                stats.peaks.clone_from(&metrics.peaks);
                stats.clipped += metrics.clipped_samples;
                stats.underruns = underruns;
                stats.dropped_frames = dropped_frames;
                stats.buffered_ms = buffered_ms;
                stats.correction_ppm = correction_ppm;
            }
        }
        Ok(())
    })();
    let _ = client.stop_stream();
    result
}
