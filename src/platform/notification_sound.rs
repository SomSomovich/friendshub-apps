//! Short, app-generated previews for the notification sounds in Settings.
//!
//! Audio-device initialization and playback run on one bounded worker. No
//! external files or platform shell commands are used.

use std::io;
use std::num::{NonZeroU16, NonZeroU32};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

static PREVIEW_RUNNING: AtomicBool = AtomicBool::new(false);

const SAMPLE_RATE: u32 = 44_100;
const MAX_AMPLITUDE: f32 = 0.10;
const FADE_MILLIS: usize = 6;
const GAP_MILLIS: usize = 32;

#[derive(Clone, Copy)]
struct Tone {
    frequency_hz: u16,
    duration_millis: u16,
}

const CHIME: &[Tone] = &[
    Tone {
        frequency_hz: 660,
        duration_millis: 105,
    },
    Tone {
        frequency_hz: 880,
        duration_millis: 155,
    },
];
const PING: &[Tone] = &[Tone {
    frequency_hz: 988,
    duration_millis: 135,
}];
const POP: &[Tone] = &[Tone {
    frequency_hz: 440,
    duration_millis: 75,
}];

/// The request was queued, disabled, or suppressed because another preview is
/// already playing. Suppression prevents rapid clicks from creating threads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreviewRequest {
    Started,
    Disabled,
    AlreadyPlaying,
}

/// Start one short sound preview. Device errors are delivered to `completion`
/// from the worker; an immediate error means no worker was started.
pub fn preview(
    sound: &str,
    completion: impl FnOnce(Result<(), io::Error>) + Send + 'static,
) -> Result<PreviewRequest, io::Error> {
    let tones = match sound {
        "chime" => CHIME,
        "ping" => PING,
        "pop" => POP,
        "none" => return Ok(PreviewRequest::Disabled),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsupported notification sound",
            ));
        }
    };

    if PREVIEW_RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Ok(PreviewRequest::AlreadyPlaying);
    }

    match thread::Builder::new()
        .name("friendshub-sound-preview".into())
        .spawn(move || {
            let _running = PreviewGuard;
            let result = play(tones);
            completion(result);
        }) {
        Ok(_) => Ok(PreviewRequest::Started),
        Err(error) => {
            PREVIEW_RUNNING.store(false, Ordering::Release);
            Err(error)
        }
    }
}

struct PreviewGuard;

impl Drop for PreviewGuard {
    fn drop(&mut self) {
        PREVIEW_RUNNING.store(false, Ordering::Release);
    }
}

fn play(tones: &[Tone]) -> Result<(), io::Error> {
    let samples = samples(tones);
    let stream = rodio::DeviceSinkBuilder::open_default_sink()
        .map_err(|error| io::Error::other(error.to_string()))?;
    let player = rodio::Player::connect_new(&stream.mixer());
    let source = rodio::buffer::SamplesBuffer::new(
        NonZeroU16::new(1).expect("mono channel count is nonzero"),
        NonZeroU32::new(SAMPLE_RATE).expect("sample rate is nonzero"),
        samples,
    );
    player.append(source);
    player.sleep_until_end();
    Ok(())
}

fn samples(tones: &[Tone]) -> Vec<f32> {
    let total_millis = tones
        .iter()
        .map(|tone| usize::from(tone.duration_millis))
        .sum::<usize>()
        + GAP_MILLIS * tones.len().saturating_sub(1);
    let capacity = total_millis.saturating_mul(SAMPLE_RATE as usize) / 1_000;
    let mut samples = Vec::with_capacity(capacity);
    let fade_samples = FADE_MILLIS * SAMPLE_RATE as usize / 1_000;

    for (index, tone) in tones.iter().enumerate() {
        let frame_count = usize::from(tone.duration_millis) * SAMPLE_RATE as usize / 1_000;
        for frame in 0..frame_count {
            let from_start = frame;
            let from_end = frame_count.saturating_sub(frame + 1);
            let edge_distance = from_start.min(from_end).min(fade_samples);
            let envelope = (edge_distance as f32 / fade_samples as f32).min(1.0);
            let phase = std::f32::consts::TAU * f32::from(tone.frequency_hz) * frame as f32
                / SAMPLE_RATE as f32;
            samples.push(phase.sin() * MAX_AMPLITUDE * envelope);
        }
        if index + 1 < tones.len() {
            let gap_frames = GAP_MILLIS * SAMPLE_RATE as usize / 1_000;
            samples.resize(samples.len() + gap_frames, 0.0);
        }
    }
    samples
}
