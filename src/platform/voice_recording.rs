//! Capture microphone input and encode a standards-compliant Ogg Opus file.
//!
//! Recording is kept separate from the Slint event loop. The caller starts a
//! single process-wide input stream, then stops it and receives the encoded
//! file from a worker callback.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

const OPUS_SAMPLE_RATE: u32 = 48_000;
const OPUS_FRAME_SAMPLES: usize = 960;
const MIN_RECORDING_SAMPLES: usize = OPUS_SAMPLE_RATE as usize / 2;
pub const MAX_RECORDING_SECONDS: u64 = 180;
const MAX_ENCODED_PACKET: usize = 4_000;

static ACTIVE: OnceLock<Mutex<Option<Recorder>>> = OnceLock::new();
static FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub enum RecordingError {
    Busy,
    NotRecording,
    MicrophoneUnavailable,
    UnsupportedDeviceFormat,
    TooShort,
    TooLong,
    Encoding,
    WorkerUnavailable,
    Io(io::Error),
}

impl std::fmt::Display for RecordingError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => formatter.write_str("A voice recording is already active."),
            Self::NotRecording => formatter.write_str("There is no active voice recording."),
            Self::MicrophoneUnavailable => {
                formatter.write_str("The microphone could not be opened.")
            }
            Self::UnsupportedDeviceFormat => {
                formatter.write_str("The microphone format is unsupported.")
            }
            Self::TooShort => {
                formatter.write_str("A voice message must be at least half a second long.")
            }
            Self::TooLong => {
                formatter.write_str("The voice message reached the three-minute limit.")
            }
            Self::Encoding => formatter.write_str("The voice message could not be encoded."),
            Self::WorkerUnavailable => {
                formatter.write_str("The audio worker could not be started.")
            }
            Self::Io(error) => write!(formatter, "The voice recording could not be saved: {error}"),
        }
    }
}

impl std::error::Error for RecordingError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for RecordingError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub struct RecordingFile {
    pub path: PathBuf,
    pub duration_seconds: u64,
    pub size_bytes: u64,
}

struct Recorder {
    stream: cpal::Stream,
    samples: Arc<Mutex<Vec<f32>>>,
    input_rate: u32,
    started_at: Instant,
    input_failed: Arc<AtomicBool>,
    too_long: Arc<AtomicBool>,
}

pub fn start() -> Result<(), RecordingError> {
    let mut active = active()
        .lock()
        .map_err(|_| RecordingError::MicrophoneUnavailable)?;
    if active.is_some() {
        return Err(RecordingError::Busy);
    }
    *active = Some(Recorder::open()?);
    Ok(())
}

pub fn is_recording() -> bool {
    active().lock().is_ok_and(|active| active.is_some())
}

pub fn elapsed_seconds() -> Option<u64> {
    active().lock().ok().and_then(|active| {
        active
            .as_ref()
            .map(|recorder| recorder.started_at.elapsed().as_secs())
    })
}

pub fn cancel() {
    if let Ok(mut active) = active().lock() {
        active.take();
    }
}

/// Stop capture and encode on a worker so the UI thread does not wait for the
/// Opus encoder or filesystem.
pub fn finish_async(
    output_dir: PathBuf,
    completion: impl FnOnce(Result<RecordingFile, RecordingError>) + Send + 'static,
) -> Result<(), RecordingError> {
    let recorder = active()
        .lock()
        .map_err(|_| RecordingError::MicrophoneUnavailable)?
        .take()
        .ok_or(RecordingError::NotRecording)?;
    thread::Builder::new()
        .name("friendshub-voice-note-encode".into())
        .spawn(move || completion(recorder.finish(&output_dir)))
        .map(|_| ())
        .map_err(|_| RecordingError::WorkerUnavailable)
}

impl Recorder {
    fn open() -> Result<Self, RecordingError> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or(RecordingError::MicrophoneUnavailable)?;
        let supported = device
            .default_input_config()
            .map_err(|_| RecordingError::MicrophoneUnavailable)?;
        let format = supported.sample_format();
        let input_rate = supported.sample_rate();
        let config: cpal::StreamConfig = supported.into();
        let channels = config.channels as usize;
        if input_rate == 0 || channels == 0 {
            return Err(RecordingError::UnsupportedDeviceFormat);
        }

        let max_samples = (input_rate as usize).saturating_mul(MAX_RECORDING_SECONDS as usize);
        let samples = Arc::new(Mutex::new(Vec::with_capacity(
            (input_rate as usize * 5).min(max_samples),
        )));
        let input_failed = Arc::new(AtomicBool::new(false));
        let too_long = Arc::new(AtomicBool::new(false));
        let stream = build_input_stream(
            &device,
            config,
            format,
            channels,
            max_samples,
            samples.clone(),
            input_failed.clone(),
            too_long.clone(),
        )?;
        stream
            .play()
            .map_err(|_| RecordingError::MicrophoneUnavailable)?;

        Ok(Self {
            stream,
            samples,
            input_rate,
            started_at: Instant::now(),
            input_failed,
            too_long,
        })
    }

    fn finish(self, output_dir: &Path) -> Result<RecordingFile, RecordingError> {
        let Self {
            stream,
            samples,
            input_rate,
            input_failed,
            too_long,
            ..
        } = self;
        drop(stream);

        if input_failed.load(Ordering::Acquire) {
            return Err(RecordingError::MicrophoneUnavailable);
        }
        let captured = samples
            .lock()
            .map_err(|_| RecordingError::MicrophoneUnavailable)?
            .clone();
        let pcm = resample_mono(&captured, input_rate);
        if pcm.is_empty() {
            return Err(RecordingError::MicrophoneUnavailable);
        }
        if pcm.len() < MIN_RECORDING_SAMPLES {
            return Err(RecordingError::TooShort);
        }
        // Input is capped at exactly three minutes. If the callback crossed
        // the cap before the UI timer stopped it, the captured prefix is still
        // a complete, playable voice message.
        let duration_seconds = if too_long.load(Ordering::Acquire) {
            MAX_RECORDING_SECONDS
        } else {
            (pcm.len() as u64) / OPUS_SAMPLE_RATE as u64
        };

        let (mut output, path) = create_recording_file(output_dir)?;
        let encode_result = encode_ogg_opus(&pcm, &mut output);
        if let Err(error) = encode_result {
            drop(output);
            let _ = fs::remove_file(&path);
            return Err(error);
        }
        let size_bytes = match output.sync_all().and_then(|()| output.metadata()) {
            Ok(metadata) => metadata.len(),
            Err(error) => {
                drop(output);
                let _ = fs::remove_file(&path);
                return Err(RecordingError::Io(error));
            }
        };
        drop(output);

        Ok(RecordingFile {
            path,
            duration_seconds,
            size_bytes,
        })
    }
}

fn active() -> &'static Mutex<Option<Recorder>> {
    ACTIVE.get_or_init(|| Mutex::new(None))
}

fn build_input_stream(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    format: cpal::SampleFormat,
    channels: usize,
    max_samples: usize,
    samples: Arc<Mutex<Vec<f32>>>,
    input_failed: Arc<AtomicBool>,
    too_long: Arc<AtomicBool>,
) -> Result<cpal::Stream, RecordingError> {
    macro_rules! build {
        ($sample:ty) => {
            build_input_stream_typed::<$sample>(
                device,
                config,
                channels,
                max_samples,
                samples,
                input_failed,
                too_long,
            )
        };
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
        _ => Err(RecordingError::UnsupportedDeviceFormat),
    }
}

fn build_input_stream_typed<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    channels: usize,
    max_samples: usize,
    samples: Arc<Mutex<Vec<f32>>>,
    input_failed: Arc<AtomicBool>,
    too_long: Arc<AtomicBool>,
) -> Result<cpal::Stream, RecordingError>
where
    T: cpal::Sample + cpal::SizedSample + Send + 'static,
    f32: cpal::FromSample<T>,
{
    let data_failed = input_failed.clone();
    device
        .build_input_stream(
            config,
            move |input: &[T], _| {
                let Ok(mut samples) = samples.lock() else {
                    data_failed.store(true, Ordering::Release);
                    return;
                };
                let available = max_samples.saturating_sub(samples.len());
                if available == 0 {
                    too_long.store(true, Ordering::Release);
                    return;
                }
                let frames = input.chunks_exact(channels);
                let mut captured = 0usize;
                for frame in frames.take(available) {
                    let mono = frame
                        .iter()
                        .map(|sample| (*sample).to_sample::<f32>())
                        .sum::<f32>()
                        / channels as f32;
                    samples.push(if mono.is_finite() {
                        mono.clamp(-1.0, 1.0)
                    } else {
                        0.0
                    });
                    captured += 1;
                }
                if captured == available && input.len() / channels > captured {
                    too_long.store(true, Ordering::Release);
                }
            },
            move |_| input_failed.store(true, Ordering::Release),
            None,
        )
        .map_err(|_| RecordingError::MicrophoneUnavailable)
}

fn resample_mono(source: &[f32], source_rate: u32) -> Vec<f32> {
    if source_rate == OPUS_SAMPLE_RATE {
        return source.to_vec();
    }
    let target_len = (source.len() as u64)
        .saturating_mul(OPUS_SAMPLE_RATE as u64)
        .checked_div(source_rate as u64)
        .unwrap_or(0) as usize;
    let mut output = Vec::with_capacity(target_len);
    for index in 0..target_len {
        let source_position = index as f64 * source_rate as f64 / OPUS_SAMPLE_RATE as f64;
        let left_index = (source_position.floor() as usize).min(source.len() - 1);
        let right_index = (left_index + 1).min(source.len() - 1);
        let fraction = (source_position - left_index as f64) as f32;
        output.push(source[left_index] + (source[right_index] - source[left_index]) * fraction);
    }
    output
}

fn encode_ogg_opus(pcm: &[f32], output: &mut File) -> Result<(), RecordingError> {
    let mut encoder = opus::Encoder::new(
        OPUS_SAMPLE_RATE,
        opus::Channels::Mono,
        opus::Application::Voip,
    )
    .map_err(|_| RecordingError::Encoding)?;
    let lookahead: u16 = encoder
        .get_lookahead()
        .map_err(|_| RecordingError::Encoding)?
        .try_into()
        .map_err(|_| RecordingError::Encoding)?;
    let serial = (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u32)
        ^ FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed) as u32;
    let mut writer = ogg::writing::PacketWriter::new(output);

    let mut head = Vec::with_capacity(19);
    head.extend_from_slice(b"OpusHead");
    head.push(1); // mapping family 0 / Opus version 1
    head.push(1); // mono
    head.extend_from_slice(&lookahead.to_le_bytes());
    head.extend_from_slice(&OPUS_SAMPLE_RATE.to_le_bytes());
    head.extend_from_slice(&0i16.to_le_bytes()); // output gain
    head.push(0); // channel mapping family 0
    writer
        .write_packet(
            head.into_boxed_slice(),
            serial,
            ogg::writing::PacketWriteEndInfo::EndPage,
            0,
        )
        .map_err(|_| RecordingError::Encoding)?;

    let vendor = b"FriendsHub native";
    let mut tags = Vec::with_capacity(32 + vendor.len());
    tags.extend_from_slice(b"OpusTags");
    tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    tags.extend_from_slice(vendor);
    tags.extend_from_slice(&0u32.to_le_bytes()); // no comments
    writer
        .write_packet(
            tags.into_boxed_slice(),
            serial,
            ogg::writing::PacketWriteEndInfo::NormalPacket,
            0,
        )
        .map_err(|_| RecordingError::Encoding)?;

    let mut packet_input = vec![0.0f32; OPUS_FRAME_SAMPLES];
    let mut packet_output = vec![0u8; MAX_ENCODED_PACKET];
    let mut source_offset = 0usize;
    let mut encoded_samples = 0u64;
    while source_offset < pcm.len() {
        packet_input.fill(0.0);
        let sample_count = (pcm.len() - source_offset).min(OPUS_FRAME_SAMPLES);
        packet_input[..sample_count]
            .copy_from_slice(&pcm[source_offset..source_offset + sample_count]);
        let packet_size = encoder
            .encode_float(&packet_input, &mut packet_output)
            .map_err(|_| RecordingError::Encoding)?;
        if packet_size == 0 || packet_size > packet_output.len() {
            return Err(RecordingError::Encoding);
        }
        source_offset += sample_count;
        encoded_samples = encoded_samples.saturating_add(OPUS_FRAME_SAMPLES as u64);
        let is_last = source_offset == pcm.len();
        let granule = if is_last {
            (pcm.len() as u64).saturating_add(lookahead as u64)
        } else {
            encoded_samples.saturating_add(lookahead as u64)
        };
        writer
            .write_packet(
                packet_output[..packet_size].to_vec().into_boxed_slice(),
                serial,
                if is_last {
                    ogg::writing::PacketWriteEndInfo::EndStream
                } else {
                    ogg::writing::PacketWriteEndInfo::NormalPacket
                },
                granule,
            )
            .map_err(|_| RecordingError::Encoding)?;
    }
    Ok(())
}

fn create_recording_file(directory: &Path) -> Result<(File, PathBuf), RecordingError> {
    fs::create_dir_all(directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
    }
    loop {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let sequence = FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!("voice-note-{timestamp:x}-{sequence:x}.ogg"));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => return Ok((file, path)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
}
