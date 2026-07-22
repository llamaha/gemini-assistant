//! Microphone capture and speaker playback via `cpal`, plus a couple of
//! synthesized beeps for "listening"/"done"/"error" feedback so this doesn't
//! need any sound asset files.
//!
//! The Gemini Live API's native rates are fixed (16kHz mono PCM16 in, 24kHz
//! mono PCM16 out), but the local audio device rarely matches exactly, so
//! everything is resampled at the boundary with simple linear interpolation
//! — adequate for speech, no need for a full resampling crate here.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};

pub const INPUT_SAMPLE_RATE: u32 = 16_000;
pub const OUTPUT_SAMPLE_RATE: u32 = 24_000;

pub fn resample_linear(input: &[i16], from_rate: u32, to_rate: u32) -> Vec<i16> {
    if from_rate == to_rate || input.is_empty() {
        return input.to_vec();
    }
    let ratio = from_rate as f64 / to_rate as f64;
    let out_len = ((input.len() as f64) / ratio).round() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src_pos = i as f64 * ratio;
        let idx = src_pos.floor() as usize;
        let frac = src_pos - idx as f64;
        let a = *input.get(idx).unwrap_or(&0) as f64;
        let b = *input.get(idx + 1).unwrap_or(input.last().unwrap_or(&0)) as f64;
        out.push((a + (b - a) * frac).round() as i16);
    }
    out
}

/// Trim leading/trailing silence, keeping a small padding margin around
/// whatever's left. Dead air at the edges doesn't carry information but the
/// Live API still has to process it as audio context, measurably slowing the
/// response. Only the outer edges are trimmed — a pause mid-question is left
/// alone since cutting silence out of the middle risks clipping speech.
///
/// `threshold` is in raw i16 RMS amplitude (higher = more aggressive
/// trimming); tune it via config/GEMINI_ASSISTANT_VAD_THRESHOLD if your mic's
/// noise floor needs it.
pub fn trim_silence(samples: &[i16], sample_rate: u32, threshold: f64) -> Vec<i16> {
    let window = (sample_rate as usize / 50).max(1); // 20ms
    let padding = (sample_rate as usize / 1000) * 300; // 300ms

    let rms = |w: &[i16]| -> f64 {
        let sum_sq: i64 = w.iter().map(|&s| (s as i64) * (s as i64)).sum();
        ((sum_sq as f64) / (w.len() as f64)).sqrt()
    };

    let mut first_voiced = None;
    let mut last_voiced = None;
    for (i, chunk) in samples.chunks(window).enumerate() {
        if rms(chunk) > threshold {
            let start = i * window;
            first_voiced.get_or_insert(start);
            last_voiced = Some(start + chunk.len());
        }
    }

    match (first_voiced, last_voiced) {
        (Some(start), Some(end)) => {
            let start = start.saturating_sub(padding);
            let end = (end + padding).min(samples.len());
            samples[start..end].to_vec()
        }
        _ => Vec::new(), // nothing exceeded the threshold — all silence
    }
}

fn downmix_to_mono_i16(data: &[f32], channels: u16) -> Vec<i16> {
    let channels = channels.max(1) as usize;
    data.chunks(channels)
        .map(|frame| {
            let sum: f32 = frame.iter().sum();
            let avg = sum / frame.len() as f32;
            (avg.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
        })
        .collect()
}

/// Captures mono 16kHz PCM16 from the default input device and forwards it
/// to a channel as it arrives, rather than buffering the whole take. A live
/// session needs audio reaching the server continuously so its voice-activity
/// detection can decide when you've stopped speaking.
///
/// Dropping this releases the microphone (and turns off the OS mic
/// indicator), which is exactly what "pause" wants.
pub struct StreamingRecorder {
    _stream: cpal::Stream,
}

impl StreamingRecorder {
    pub fn start(tx: std::sync::mpsc::Sender<Vec<i16>>) -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or_else(|| anyhow!("no default input (microphone) device"))?;
        let config = device
            .default_input_config()
            .context("querying default input config")?;
        let device_rate = config.sample_rate();
        let channels = config.channels();
        let sample_format = config.sample_format();
        let stream_config: StreamConfig = config.into();

        let err_fn = |err| eprintln!("(input stream error: {err})");
        let forward = move |mono: Vec<i16>, tx: &std::sync::mpsc::Sender<Vec<i16>>| {
            let resampled = resample_linear(&mono, device_rate, INPUT_SAMPLE_RATE);
            // A send error just means the session ended and the receiver is
            // gone; the stream is about to be dropped, so ignore it.
            let _ = tx.send(resampled);
        };

        let stream = match sample_format {
            SampleFormat::F32 => device.build_input_stream(
                stream_config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    forward(downmix_to_mono_i16(data, channels), &tx)
                },
                err_fn,
                None,
            )?,
            SampleFormat::I16 => device.build_input_stream(
                stream_config,
                move |data: &[i16], _: &cpal::InputCallbackInfo| {
                    let floats: Vec<f32> =
                        data.iter().map(|&s| s as f32 / i16::MAX as f32).collect();
                    forward(downmix_to_mono_i16(&floats, channels), &tx)
                },
                err_fn,
                None,
            )?,
            other => return Err(anyhow!("unsupported input sample format: {other:?}")),
        };
        stream.play().context("starting input stream")?;
        Ok(Self { _stream: stream })
    }
}

/// Streams PCM16 audio out to the default output device as it's pushed in,
/// resampling from whatever rate the caller provides to the device's native
/// rate.
pub struct Player {
    _stream: cpal::Stream,
    queue: Arc<Mutex<VecDeque<i16>>>,
    device_rate: u32,
    device_channels: u16,
}

impl Player {
    pub fn new() -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow!("no default output (speaker) device"))?;
        let config = device
            .default_output_config()
            .context("querying default output config")?;
        let device_rate = config.sample_rate();
        let device_channels = config.channels();
        let sample_format = config.sample_format();
        let stream_config: StreamConfig = config.into();

        let queue: Arc<Mutex<VecDeque<i16>>> = Arc::new(Mutex::new(VecDeque::new()));
        let queue_cb = queue.clone();
        let err_fn = |err| eprintln!("(output stream error: {err})");

        let stream = match sample_format {
            SampleFormat::F32 => device.build_output_stream(
                stream_config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let mut q = queue_cb.lock().unwrap();
                    for sample in data.iter_mut() {
                        *sample = q.pop_front().map(|s| s as f32 / i16::MAX as f32).unwrap_or(0.0);
                    }
                },
                err_fn,
                None,
            )?,
            SampleFormat::I16 => device.build_output_stream(
                stream_config,
                move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                    let mut q = queue_cb.lock().unwrap();
                    for sample in data.iter_mut() {
                        *sample = q.pop_front().unwrap_or(0);
                    }
                },
                err_fn,
                None,
            )?,
            other => return Err(anyhow!("unsupported output sample format: {other:?}")),
        };
        stream.play().context("starting output stream")?;

        Ok(Self {
            _stream: stream,
            queue,
            device_rate,
            device_channels,
        })
    }

    /// Enqueue PCM16 mono audio at `sample_rate` for playback, resampling and
    /// duplicating across channels as needed.
    pub fn push_pcm16(&self, data: &[i16], sample_rate: u32) {
        let resampled = resample_linear(data, sample_rate, self.device_rate);
        let mut q = self.queue.lock().unwrap();
        for sample in resampled {
            for _ in 0..self.device_channels.max(1) {
                q.push_back(sample);
            }
        }
    }

    /// Drop everything still queued. Used on barge-in: when the server says
    /// it was interrupted, whatever it was mid-sentence about is obsolete and
    /// should stop immediately rather than talk over you.
    pub fn clear(&self) {
        self.queue.lock().unwrap().clear();
    }

    /// Number of queued audio frames (per channel) not yet played.
    pub fn queued_frames(&self) -> usize {
        self.queue.lock().unwrap().len() / self.device_channels.max(1) as usize
    }

    /// Block until the queue drains (playback caught up), polling briefly.
    pub async fn wait_drain(&self) {
        while self.queued_frames() > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
}

fn sine_tone(freq: f32, duration_ms: u32, sample_rate: u32) -> Vec<i16> {
    let n = (sample_rate as f32 * duration_ms as f32 / 1000.0) as usize;
    (0..n)
        .map(|i| {
            let t = i as f32 / sample_rate as f32;
            let envelope = ((n - i).min(i) as f32 / (sample_rate as f32 * 0.01)).min(1.0);
            (((t * freq * std::f32::consts::TAU).sin()) * 0.2 * envelope * i16::MAX as f32) as i16
        })
        .collect()
}

#[derive(Clone, Copy)]
pub enum Chime {
    Start,
    Done,
    Error,
}

/// Samples for a chime at `INPUT_SAMPLE_RATE`. Prefer queuing this onto an
/// already-open `Player` (via `push_pcm16`) over `play_chime_blocking` when
/// one exists — e.g. the "done" chime should play through the *same* stream
/// that's still draining response audio, not a second, competing stream that
/// could race with or interrupt it.
pub fn chime_tone(chime: Chime) -> Vec<i16> {
    match chime {
        Chime::Start => sine_tone(880.0, 120, INPUT_SAMPLE_RATE),
        Chime::Done => sine_tone(660.0, 150, INPUT_SAMPLE_RATE),
        Chime::Error => sine_tone(220.0, 300, INPUT_SAMPLE_RATE),
    }
}

/// Play a short synthesized beep and block until it finishes. Opens its own
/// output stream, so only use this when no `Player` is already active (e.g.
/// the "start recording" chime, before any response stream exists).
pub fn play_chime_blocking(chime: Chime) -> Result<()> {
    let tone = chime_tone(chime);
    let player = Player::new()?;
    player.push_pcm16(&tone, INPUT_SAMPLE_RATE);
    while player.queued_frames() > 0 {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    // Give the device a moment to flush the final buffer before the stream drops.
    std::thread::sleep(std::time::Duration::from_millis(50));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine_i16(freq: f32, sample_rate: u32, n: usize) -> Vec<i16> {
        (0..n)
            .map(|i| {
                let t = i as f32 / sample_rate as f32;
                ((t * freq * std::f32::consts::TAU).sin() * i16::MAX as f32 * 0.8) as i16
            })
            .collect()
    }

    #[test]
    fn resample_same_rate_is_identity() {
        let input = vec![1i16, 2, 3, 4, 5];
        assert_eq!(resample_linear(&input, 16_000, 16_000), input);
    }

    #[test]
    fn resample_empty_input_is_empty() {
        assert!(resample_linear(&[], 16_000, 24_000).is_empty());
    }

    #[test]
    fn resample_single_sample_holds_value() {
        let out = resample_linear(&[500], 16_000, 24_000);
        assert!(out.iter().all(|&s| s == 500));
    }

    #[test]
    fn upsample_preserves_approximate_duration() {
        // 1 second of 16kHz audio, upsampled to 24kHz, should be ~1 second
        // of 24kHz audio (sample count scales with the rate ratio).
        let input = sine_i16(440.0, 16_000, 16_000);
        let out = resample_linear(&input, 16_000, 24_000);
        let expected = 24_000usize;
        assert!(
            (out.len() as i64 - expected as i64).abs() < 10,
            "expected ~{expected} samples, got {}",
            out.len()
        );
    }

    #[test]
    fn downsample_preserves_approximate_duration() {
        // 1 second of 24kHz audio, downsampled to 16kHz, should be ~1 second
        // of 16kHz audio.
        let input = sine_i16(440.0, 24_000, 24_000);
        let out = resample_linear(&input, 24_000, 16_000);
        let expected = 16_000usize;
        assert!(
            (out.len() as i64 - expected as i64).abs() < 10,
            "expected ~{expected} samples, got {}",
            out.len()
        );
    }

    #[test]
    fn resample_roundtrip_preserves_peak_amplitude_within_tolerance() {
        // A 1kHz tone at 16kHz, upsampled to 24kHz and back down to 16kHz,
        // should still peak near the original amplitude — linear
        // interpolation loses some energy but shouldn't gut it.
        let input = sine_i16(1000.0, 16_000, 1600); // 100ms
        let up = resample_linear(&input, 16_000, 24_000);
        let back = resample_linear(&up, 24_000, 16_000);

        let orig_peak = input.iter().map(|&s| s.unsigned_abs()).max().unwrap();
        let round_trip_peak = back.iter().map(|&s| s.unsigned_abs()).max().unwrap();
        let ratio = round_trip_peak as f64 / orig_peak as f64;
        assert!(
            (0.8..=1.05).contains(&ratio),
            "round-trip peak amplitude drifted too far: orig={orig_peak}, round_trip={round_trip_peak}, ratio={ratio}"
        );
    }

    #[test]
    fn trim_silence_removes_leading_and_trailing_silence() {
        let sample_rate = 16_000;
        let silence = vec![0i16; sample_rate as usize / 2]; // 500ms
        let voice = sine_i16(440.0, sample_rate, sample_rate as usize / 2); // 500ms loud tone
        let mut samples = silence.clone();
        samples.extend(&voice);
        samples.extend(&silence);

        let trimmed = trim_silence(&samples, sample_rate, 1000.0);
        // Should be much shorter than the original (silence stripped from
        // both ends, only padding + voiced region remains) but still contain
        // the voiced region.
        assert!(trimmed.len() < samples.len());
        assert!(trimmed.len() >= voice.len());
    }

    #[test]
    fn trim_silence_all_silence_returns_empty() {
        let samples = vec![0i16; 16_000];
        assert!(trim_silence(&samples, 16_000, 400.0).is_empty());
    }

    #[test]
    fn trim_silence_empty_input_returns_empty() {
        assert!(trim_silence(&[], 16_000, 400.0).is_empty());
    }

    #[test]
    fn chime_tones_are_nonempty_and_within_i16_range() {
        for chime in [Chime::Start, Chime::Done, Chime::Error] {
            let tone = chime_tone(chime);
            assert!(!tone.is_empty());
            // sine_tone scales by 0.2 * i16::MAX, so it should never clip.
            assert!(tone.iter().all(|&s| s.unsigned_abs() <= i16::MAX as u16 / 4));
        }
    }
}
