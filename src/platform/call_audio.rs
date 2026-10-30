//! Host microphone and speaker bridge for native one-to-one audio calls.
//!
//! The core transports complete Opus packets. This module owns device access,
//! PCM resampling, Opus encode/decode and the bounded real-time queues around
//! that synchronous core API.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::thread;
use std::time::Duration;

use base64::Engine as _;
use cpal::Sample as _;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use friendshub_core::Client;

use crate::features::calls_attachments;

const OPUS_RATE: u32 = 48_000;
const OPUS_FRAME_SAMPLES: usize = 960;
const CORE_READ_TIMEOUT_MS: u64 = 40;
const CAPTURE_QUEUE_FRAMES: usize = 5;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CallAudioFailure {
    PermissionRequired,
    MicrophoneUnavailable,
    SpeakerUnavailable,
    UnsupportedDeviceFormat,
    CodecUnavailable,
    CoreTransport,
}

struct CallAudioSession {
    stop: Arc<AtomicBool>,
    playback_enabled: Arc<AtomicBool>,
    _input_stream: cpal::Stream,
    _output_stream: cpal::Stream,
}

struct StopGuard(Option<Arc<AtomicBool>>);

impl StopGuard {
    fn disarm(&mut self) {
        self.0.take();
    }
}

impl Drop for StopGuard {
    fn drop(&mut self) {
        if let Some(stop) = &self.0 {
            stop.store(true, Ordering::Release);
        }
    }
}

impl Drop for CallAudioSession {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

fn sessions() -> &'static Mutex<HashMap<String, CallAudioSession>> {
    static SESSIONS: OnceLock<Mutex<HashMap<String, CallAudioSession>>> = OnceLock::new();
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Open microphone/speaker streams and start the Opus transfer loops.
///
/// The macOS executable embeds its microphone usage metadata so CoreAudio can
/// request recording permission. Android permission is checked before opening
/// the AAudio streams.
pub fn start(
    client: Arc<Client>,
    call_id: String,
    on_failure: impl Fn(CallAudioFailure) + Send + Sync + 'static,
) -> Result<(), CallAudioFailure> {
    #[cfg(target_os = "android")]
    if crate::platform::notifications::microphone_permission_state()
        != crate::platform::notifications::MicrophonePermissionState::Granted
    {
        let _ = (client, call_id, on_failure);
        return Err(CallAudioFailure::PermissionRequired);
    }

    let mut registry = sessions()
        .lock()
        .map_err(|_| CallAudioFailure::CoreTransport)?;
    if registry.contains_key(&call_id) {
        return Ok(());
    }
    let session = start_supported(client, &call_id, Arc::new(on_failure))?;
    registry.insert(call_id, session);
    Ok(())
}

/// Stop audio immediately after the call closes. Worker loops use bounded
/// waits, so they release their core handle promptly without joining on UI.
pub fn stop(call_id: &str) {
    let session = sessions()
        .lock()
        .ok()
        .and_then(|mut sessions| sessions.remove(call_id));
    drop(session);
}

/// Mute or restore remote playback while keeping the device stream alive.
pub fn set_playback(call_id: &str, enabled: bool) -> bool {
    let Ok(sessions) = sessions().lock() else {
        return false;
    };
    let Some(session) = sessions.get(call_id) else {
        return false;
    };
    session.playback_enabled.store(enabled, Ordering::Release);
    true
}

fn start_supported(
    client: Arc<Client>,
    call_id: &str,
    on_failure: Arc<dyn Fn(CallAudioFailure) + Send + Sync>,
) -> Result<CallAudioSession, CallAudioFailure> {
    let host = cpal::default_host();
    let input_device = host
        .default_input_device()
        .ok_or(CallAudioFailure::MicrophoneUnavailable)?;
    let output_device = host
        .default_output_device()
        .ok_or(CallAudioFailure::SpeakerUnavailable)?;
    let input_supported = input_device
        .default_input_config()
        .map_err(|_| CallAudioFailure::MicrophoneUnavailable)?;
    let output_supported = output_device
        .default_output_config()
        .map_err(|_| CallAudioFailure::SpeakerUnavailable)?;
    let input_format = input_supported.sample_format();
    let output_format = output_supported.sample_format();
    let input_config: cpal::StreamConfig = input_supported.into();
    let output_config: cpal::StreamConfig = output_supported.into();
    if input_config.channels == 0 || output_config.channels == 0 {
        return Err(CallAudioFailure::UnsupportedDeviceFormat);
    }

    let stop = Arc::new(AtomicBool::new(false));
    let mut stop_guard = StopGuard(Some(stop.clone()));
    let playback_enabled = Arc::new(AtomicBool::new(true));
    let error_reported = Arc::new(AtomicBool::new(false));
    let output = Arc::new(Mutex::new(PcmQueue::new(
        (output_config.sample_rate as usize / 5).max(OPUS_FRAME_SAMPLES),
    )));
    let (capture_tx, capture_rx) = mpsc::sync_channel(CAPTURE_QUEUE_FRAMES);

    let input_stream = build_input_stream(
        &input_device,
        input_config.clone(),
        input_format,
        capture_tx,
        stop.clone(),
        error_reported.clone(),
        on_failure.clone(),
    )?;
    let output_stream = build_output_stream(
        &output_device,
        output_config.clone(),
        output_format,
        output.clone(),
        playback_enabled.clone(),
        stop.clone(),
        error_reported.clone(),
        on_failure.clone(),
    )?;

    let encoder_stop = stop.clone();
    let encoder_error = on_failure.clone();
    let encoder_client = client.clone();
    let encoder_call_id = call_id.to_owned();
    thread::Builder::new()
        .name("friendshub-call-audio-send".into())
        .spawn(move || {
            let mut encoder = match opus::Encoder::new(
                OPUS_RATE,
                opus::Channels::Mono,
                opus::Application::Voip,
            ) {
                Ok(encoder) => encoder,
                Err(_) => {
                    encoder_stop.store(true, Ordering::Release);
                    encoder_error(CallAudioFailure::CodecUnavailable);
                    return;
                }
            };
            let mut encoded = vec![0_u8; 4_000];
            while !encoder_stop.load(Ordering::Acquire) {
                let samples = match capture_rx.recv_timeout(Duration::from_millis(40)) {
                    Ok(samples) => samples,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };
                let length = match encoder.encode_float(&samples, &mut encoded) {
                    Ok(length) if length > 0 => length,
                    _ => {
                        encoder_stop.store(true, Ordering::Release);
                        encoder_error(CallAudioFailure::CodecUnavailable);
                        break;
                    }
                };
                let data = base64::engine::general_purpose::STANDARD.encode(&encoded[..length]);
                if calls_attachments::write_audio_frame(
                    &encoder_client,
                    &encoder_call_id,
                    &data,
                    20,
                )
                .is_err()
                {
                    if !encoder_stop.load(Ordering::Acquire) {
                        encoder_stop.store(true, Ordering::Release);
                        encoder_error(CallAudioFailure::CoreTransport);
                    }
                    break;
                }
            }
        })
        .map_err(|_| CallAudioFailure::CoreTransport)?;

    let decoder_stop = stop.clone();
    let decoder_error = on_failure;
    let decoder_call_id = call_id.to_owned();
    thread::Builder::new()
        .name("friendshub-call-audio-receive".into())
        .spawn(move || {
            let mut decoder = match opus::Decoder::new(OPUS_RATE, opus::Channels::Mono) {
                Ok(decoder) => decoder,
                Err(_) => {
                    decoder_stop.store(true, Ordering::Release);
                    decoder_error(CallAudioFailure::CodecUnavailable);
                    return;
                }
            };
            let mut decoded = vec![0.0_f32; 5_760];
            let mut resampler = PcmResampler::new(output_config.sample_rate);
            while !decoder_stop.load(Ordering::Acquire) {
                let frame = match calls_attachments::read_audio_frame(
                    &client,
                    &decoder_call_id,
                    CORE_READ_TIMEOUT_MS,
                ) {
                    Ok(calls_attachments::FrameRead::Timeout) => continue,
                    Ok(calls_attachments::FrameRead::Frame(frame)) => frame,
                    Err(_) => {
                        if !decoder_stop.load(Ordering::Acquire) {
                            decoder_stop.store(true, Ordering::Release);
                            decoder_error(CallAudioFailure::CoreTransport);
                        }
                        break;
                    }
                };
                let packet = match base64::engine::general_purpose::STANDARD
                    .decode(frame.data_base64.as_bytes())
                {
                    Ok(packet) if !packet.is_empty() => packet,
                    _ => {
                        decoder_stop.store(true, Ordering::Release);
                        decoder_error(CallAudioFailure::CodecUnavailable);
                        break;
                    }
                };
                let length = match decoder.decode_float(&packet, &mut decoded, false) {
                    Ok(length) if length > 0 => length,
                    _ => {
                        decoder_stop.store(true, Ordering::Release);
                        decoder_error(CallAudioFailure::CodecUnavailable);
                        break;
                    }
                };
                let output_samples = resampler.push(&decoded[..length]);
                if let Ok(mut queue) = output.lock() {
                    queue.push(&output_samples);
                }
            }
        })
        .map_err(|_| CallAudioFailure::CoreTransport)?;

    input_stream
        .play()
        .map_err(|_| CallAudioFailure::MicrophoneUnavailable)?;
    output_stream
        .play()
        .map_err(|_| CallAudioFailure::SpeakerUnavailable)?;

    stop_guard.disarm();
    Ok(CallAudioSession {
        stop,
        playback_enabled,
        _input_stream: input_stream,
        _output_stream: output_stream,
    })
}

fn build_input_stream(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    format: cpal::SampleFormat,
    tx: mpsc::SyncSender<Vec<f32>>,
    stop: Arc<AtomicBool>,
    error_reported: Arc<AtomicBool>,
    on_failure: Arc<dyn Fn(CallAudioFailure) + Send + Sync>,
) -> Result<cpal::Stream, CallAudioFailure> {
    macro_rules! build {
        ($sample:ty) => {{
            build_input_stream_typed::<$sample>(
                device,
                config,
                tx,
                stop,
                error_reported,
                on_failure,
            )
        }};
    }
    match format {
        cpal::SampleFormat::F32 => build!(f32),
        cpal::SampleFormat::F64 => build!(f64),
        cpal::SampleFormat::I8 => build!(i8),
        cpal::SampleFormat::I16 => build!(i16),
        cpal::SampleFormat::I24 => build!(cpal::I24),
        cpal::SampleFormat::I32 => build!(i32),
        cpal::SampleFormat::I64 => build!(i64),
        cpal::SampleFormat::U8 => build!(u8),
        cpal::SampleFormat::U16 => build!(u16),
        cpal::SampleFormat::U24 => build!(cpal::U24),
        cpal::SampleFormat::U32 => build!(u32),
        cpal::SampleFormat::U64 => build!(u64),
        _ => Err(CallAudioFailure::UnsupportedDeviceFormat),
    }
}

fn build_input_stream_typed<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    tx: mpsc::SyncSender<Vec<f32>>,
    stop: Arc<AtomicBool>,
    error_reported: Arc<AtomicBool>,
    on_failure: Arc<dyn Fn(CallAudioFailure) + Send + Sync>,
) -> Result<cpal::Stream, CallAudioFailure>
where
    T: cpal::Sample + cpal::SizedSample + Send + 'static,
    f32: cpal::FromSample<T>,
{
    let mut assembler = FrameAssembler::new(config.sample_rate, config.channels as usize);
    let callback_stop = stop.clone();
    let error_stop = stop;
    let error_reported_callback = error_reported;
    let error_callback = move |_| {
        error_stop.store(true, Ordering::Release);
        notify_once(
            &error_reported_callback,
            &on_failure,
            CallAudioFailure::MicrophoneUnavailable,
        );
    };
    device
        .build_input_stream(
            config,
            move |input: &[T], _| {
                if callback_stop.load(Ordering::Acquire) {
                    return;
                }
                let samples = input
                    .iter()
                    .map(|sample| (*sample).to_sample::<f32>())
                    .collect::<Vec<_>>();
                assembler.push(&samples, &tx, &callback_stop);
            },
            error_callback,
            None,
        )
        .map_err(|_| CallAudioFailure::MicrophoneUnavailable)
}

fn build_output_stream(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    format: cpal::SampleFormat,
    queue: Arc<Mutex<PcmQueue>>,
    playback_enabled: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    error_reported: Arc<AtomicBool>,
    on_failure: Arc<dyn Fn(CallAudioFailure) + Send + Sync>,
) -> Result<cpal::Stream, CallAudioFailure> {
    macro_rules! build {
        ($sample:ty) => {{
            build_output_stream_typed::<$sample>(
                device,
                config,
                queue,
                playback_enabled,
                stop,
                error_reported,
                on_failure,
            )
        }};
    }
    match format {
        cpal::SampleFormat::F32 => build!(f32),
        cpal::SampleFormat::F64 => build!(f64),
        cpal::SampleFormat::I8 => build!(i8),
        cpal::SampleFormat::I16 => build!(i16),
        cpal::SampleFormat::I24 => build!(cpal::I24),
        cpal::SampleFormat::I32 => build!(i32),
        cpal::SampleFormat::I64 => build!(i64),
        cpal::SampleFormat::U8 => build!(u8),
        cpal::SampleFormat::U16 => build!(u16),
        cpal::SampleFormat::U24 => build!(cpal::U24),
        cpal::SampleFormat::U32 => build!(u32),
        cpal::SampleFormat::U64 => build!(u64),
        _ => Err(CallAudioFailure::UnsupportedDeviceFormat),
    }
}

fn build_output_stream_typed<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    queue: Arc<Mutex<PcmQueue>>,
    playback_enabled: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    error_reported: Arc<AtomicBool>,
    on_failure: Arc<dyn Fn(CallAudioFailure) + Send + Sync>,
) -> Result<cpal::Stream, CallAudioFailure>
where
    T: cpal::Sample + cpal::SizedSample + Send + 'static + cpal::FromSample<f32>,
{
    let channels = config.channels as usize;
    let callback_stop = stop.clone();
    let error_stop = stop;
    let error_callback = move |_| {
        error_stop.store(true, Ordering::Release);
        notify_once(
            &error_reported,
            &on_failure,
            CallAudioFailure::SpeakerUnavailable,
        );
    };
    device
        .build_output_stream(
            config,
            move |output: &mut [T], _| {
                if callback_stop.load(Ordering::Acquire) {
                    output.fill(0.0_f32.to_sample::<T>());
                    return;
                }
                let Ok(mut queue) = queue.try_lock() else {
                    output.fill(0.0_f32.to_sample::<T>());
                    return;
                };
                for frame in output.chunks_mut(channels) {
                    let sample = if playback_enabled.load(Ordering::Acquire) {
                        queue.pop().unwrap_or(0.0)
                    } else {
                        0.0
                    };
                    let converted = sample.to_sample::<T>();
                    frame.fill(converted);
                }
            },
            error_callback,
            None,
        )
        .map_err(|_| CallAudioFailure::SpeakerUnavailable)
}

fn notify_once(
    reported: &AtomicBool,
    callback: &Arc<dyn Fn(CallAudioFailure) + Send + Sync>,
    failure: CallAudioFailure,
) {
    if !reported.swap(true, Ordering::AcqRel) {
        callback(failure);
    }
}

struct FrameAssembler {
    input_rate: u32,
    channels: usize,
    phase: f64,
    previous: Option<f32>,
    frame: Vec<f32>,
}

impl FrameAssembler {
    fn new(input_rate: u32, channels: usize) -> Self {
        Self {
            input_rate: input_rate.max(1),
            channels: channels.max(1),
            phase: 0.0,
            previous: None,
            frame: Vec::with_capacity(OPUS_FRAME_SAMPLES),
        }
    }

    fn push(&mut self, input: &[f32], tx: &mpsc::SyncSender<Vec<f32>>, stop: &AtomicBool) {
        let step = self.input_rate as f64 / OPUS_RATE as f64;
        for samples in input.chunks_exact(self.channels) {
            if stop.load(Ordering::Acquire) {
                return;
            }
            let sample = samples.iter().copied().sum::<f32>() / self.channels as f32;
            if let Some(previous) = self.previous {
                while self.phase < 1.0 {
                    let phase = self.phase as f32;
                    self.frame
                        .push(previous + (sample - previous) * phase.clamp(0.0, 1.0));
                    self.phase += step;
                    if self.frame.len() == OPUS_FRAME_SAMPLES {
                        let frame = std::mem::replace(
                            &mut self.frame,
                            Vec::with_capacity(OPUS_FRAME_SAMPLES),
                        );
                        match tx.try_send(frame) {
                            Ok(()) => {}
                            // Drop this packet under backpressure. Keeping it
                            // would fill the next frame beyond 960 samples.
                            Err(mpsc::TrySendError::Full(_)) => {}
                            Err(mpsc::TrySendError::Disconnected(_)) => {
                                stop.store(true, Ordering::Release);
                                return;
                            }
                        }
                    }
                }
                self.phase -= 1.0;
            }
            self.previous = Some(sample);
        }
    }
}

struct PcmResampler {
    output_rate: u32,
    phase: f64,
    previous: Option<f32>,
}

impl PcmResampler {
    fn new(output_rate: u32) -> Self {
        Self {
            output_rate: output_rate.max(1),
            phase: 0.0,
            previous: None,
        }
    }

    fn push(&mut self, input: &[f32]) -> Vec<f32> {
        let step = OPUS_RATE as f64 / self.output_rate as f64;
        let mut output = Vec::with_capacity(input.len().saturating_mul(2));
        for sample in input.iter().copied() {
            if let Some(previous) = self.previous {
                while self.phase < 1.0 {
                    let phase = self.phase as f32;
                    output.push(previous + (sample - previous) * phase.clamp(0.0, 1.0));
                    self.phase += step;
                }
                self.phase -= 1.0;
            }
            self.previous = Some(sample);
        }
        output
    }
}

struct PcmQueue {
    samples: VecDeque<f32>,
    max_samples: usize,
}

impl PcmQueue {
    fn new(max_samples: usize) -> Self {
        Self {
            samples: VecDeque::with_capacity(max_samples),
            max_samples,
        }
    }

    fn push(&mut self, samples: &[f32]) {
        if samples.len() >= self.max_samples {
            self.samples.clear();
            self.samples
                .extend(samples[samples.len() - self.max_samples..].iter().copied());
            return;
        }
        let overflow = self
            .samples
            .len()
            .saturating_add(samples.len())
            .saturating_sub(self.max_samples);
        self.samples.drain(..overflow);
        self.samples.extend(samples.iter().copied());
    }

    fn pop(&mut self) -> Option<f32> {
        self.samples.pop_front()
    }
}
