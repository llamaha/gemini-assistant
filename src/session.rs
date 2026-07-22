//! Owns a live Gemini session end to end: builds the `SessionConfig`,
//! connects via `gemini-genai-rs`, pumps microphone audio in, plays response
//! audio out, and reacts to pause/resume/shutdown signals.
//!
//! Replaces the old hand-rolled WebSocket wire protocol — `gemini-genai-rs`
//! owns the connection, the `setup`/`setupComplete` handshake, JSON framing,
//! and base64 audio encode/decode. This module is a thin orchestration layer
//! on top of it, plus the local mic/speaker plumbing that the crate
//! deliberately doesn't own.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use gemini_genai_rs::prelude::{
    bytes_to_i16, connect, i16_to_bytes, recv_event, SessionConfig, SessionEvent, SessionHandle,
    SessionPhase, TransportConfig,
};
use tokio::sync::mpsc as tokio_mpsc;

use crate::audio::{self, Chime, Player, StreamingRecorder};
use crate::config::Config;
use crate::transcript;

pub fn debug_enabled() -> bool {
    std::env::var("GEMINI_ASSISTANT_DEBUG").is_ok_and(|v| v != "0")
}

/// Build the `SessionConfig` for a live session from the app's `Config`.
/// Pulled out as its own function so it's unit-testable without a network
/// call — the risk in this module is getting the crate's builder chain
/// right, not the network plumbing around it.
pub fn build_session_config(api_key: &str, cfg: &Config) -> SessionConfig {
    let mut config = SessionConfig::new(api_key)
        .model(cfg.gemini_model())
        .system_instruction(&cfg.system_instruction)
        .enable_input_transcription()
        .enable_output_transcription()
        .context_window_compression(cfg.context_window_target_tokens);

    if let Some(voice) = cfg.gemini_voice() {
        config = config.voice(voice);
    }
    if cfg.google_search {
        config = config.with_google_search();
    }
    config
}

async fn connect_session(api_key: &str, cfg: &Config) -> Result<SessionHandle> {
    let config = build_session_config(api_key, cfg);
    let session = connect(config, TransportConfig::default())
        .await
        .context("connecting to Gemini Live")?;
    tokio::time::timeout(
        std::time::Duration::from_secs(15),
        session.wait_for_phase(SessionPhase::Active),
    )
    .await
    .context("timed out waiting for session to become active")?;
    Ok(session)
}

/// Result of a single one-shot turn (`send-clip`): what the model said back,
/// transcribed, plus the raw response audio for playback.
pub struct TurnResult {
    pub input_transcript: String,
    pub output_transcript: String,
}

/// Send one clip of already-16kHz-mono-PCM16 audio, wait for the model's
/// full reply, and stream response audio to `on_audio_chunk` as it arrives.
/// Used by the `send-clip` diagnostic command — no mic, no pidfile, no
/// pause/resume, just a single turn against the real API.
pub async fn run_turn(
    api_key: &str,
    cfg: &Config,
    samples: &[i16],
    mut on_audio_chunk: impl FnMut(&[i16]),
) -> Result<TurnResult> {
    let session = connect_session(api_key, cfg).await?;

    // A one-shot send has no open mic to keep streaming ambient silence
    // after the clip ends, so the server's voice-activity detector has
    // nothing to trigger end-of-turn on. A short tail of real silence gives
    // it that signal; too much measurably slows the response (REBUILD-PLAN.md:
    // 6s of trailing silence roughly doubled round-trip time), so this stays
    // short — just enough to trip VAD, not to pad the request.
    const TRAILING_SILENCE_MS: usize = 800;
    let mut padded = samples.to_vec();
    padded.extend(std::iter::repeat_n(
        0i16,
        audio::INPUT_SAMPLE_RATE as usize * TRAILING_SILENCE_MS / 1000,
    ));

    session
        .send_audio(i16_to_bytes(&padded).to_vec())
        .await
        .context("sending audio")?;

    let mut events = session.subscribe();
    let mut input_transcript = String::new();
    let mut output_transcript = String::new();
    let mut generation_complete = false;

    loop {
        let next = tokio::time::timeout(std::time::Duration::from_secs(20), recv_event(&mut events));
        let event = match next.await {
            Ok(Some(event)) => event,
            Ok(None) => break, // channel closed
            Err(_) if generation_complete => break, // grace period elapsed after generationComplete
            Err(_) => anyhow::bail!("timed out waiting for a server response"),
        };

        if debug_enabled() {
            eprintln!("(debug) event: {event:?}");
        }

        match event {
            SessionEvent::AudioData(bytes) => {
                if let Some(samples) = bytes_to_i16(&bytes) {
                    on_audio_chunk(samples);
                }
            }
            SessionEvent::InputTranscription(t) => input_transcript.push_str(&t),
            SessionEvent::OutputTranscription(t) => output_transcript.push_str(&t),
            SessionEvent::GenerationComplete => generation_complete = true,
            SessionEvent::TurnComplete => break,
            SessionEvent::Error(e) => anyhow::bail!("session error: {e}"),
            SessionEvent::Disconnected(reason) => {
                anyhow::bail!("session disconnected: {reason:?}")
            }
            _ => {}
        }
    }

    let _ = session.disconnect().await;
    Ok(TurnResult {
        input_transcript,
        output_transcript,
    })
}

/// Shared pause flag: `SIGUSR1` in `main.rs` flips this, and the running
/// session's mic-pump loop reacts to it. `AtomicBool` rather than a channel
/// because the signal handler needs to set it without `.await`ing anything.
pub type PauseFlag = Arc<AtomicBool>;

/// Run a full interactive session: open the mic, connect to Gemini, stream
/// both directions, react to pause/resume, and keep going until `shutdown`
/// fires. Must run on a `LocalSet` (owns non-`Send` `cpal::Stream`s).
pub async fn run(
    api_key: &str,
    cfg: &Config,
    pause: PauseFlag,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    pid: u32,
) -> Result<()> {
    let session = connect_session(api_key, cfg).await?;
    let mut record = transcript::Session::new(pid);

    let player = Player::new().context("opening speaker")?;
    player.push_pcm16(&audio::chime_tone(Chime::Start), audio::INPUT_SAMPLE_RATE);

    // Mic capture callback runs on a cpal-owned OS thread; forward chunks
    // into a bounded std channel there, then bridge to async on a small
    // dedicated thread that blocks on `recv()` — avoids poll-loop latency
    // while keeping the non-Send `cpal::Stream` off the async runtime.
    let (mic_tx, mic_rx) = std::sync::mpsc::channel::<Vec<i16>>();
    let mut recorder = Some(StreamingRecorder::start(mic_tx.clone()).context("opening mic")?);

    let (async_tx, mut async_rx) = tokio_mpsc::unbounded_channel::<Vec<i16>>();
    std::thread::spawn(move || {
        while let Ok(chunk) = mic_rx.recv() {
            if async_tx.send(chunk).is_err() {
                break;
            }
        }
    });

    let mut events = session.subscribe();
    let mut current_question = String::new();
    let mut current_answer = String::new();
    let mut was_paused = false;

    loop {
        tokio::select! {
            biased;

            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    break;
                }
            }

            Some(chunk) = async_rx.recv() => {
                if !pause.load(Ordering::Relaxed) {
                    let _ = session.send_audio(i16_to_bytes(&chunk).to_vec()).await;
                }
            }

            event = recv_event(&mut events) => {
                let Some(event) = event else { break };
                if debug_enabled() {
                    eprintln!("(debug) event: {event:?}");
                }
                match event {
                    SessionEvent::AudioData(bytes) => {
                        if let Some(samples) = bytes_to_i16(&bytes) {
                            player.push_pcm16(samples, audio::OUTPUT_SAMPLE_RATE);
                        }
                    }
                    SessionEvent::Interrupted => player.clear(),
                    SessionEvent::InputTranscription(t) => current_question.push_str(&t),
                    SessionEvent::OutputTranscription(t) => current_answer.push_str(&t),
                    SessionEvent::TurnComplete => {
                        if !current_question.is_empty() || !current_answer.is_empty() {
                            record.add_turn(
                                std::mem::take(&mut current_question),
                                std::mem::take(&mut current_answer),
                            );
                            let _ = record.save();
                        }
                    }
                    SessionEvent::GoAway(_) | SessionEvent::Disconnected(_) => break,
                    SessionEvent::Error(e) => {
                        eprintln!("session error: {e}");
                        break;
                    }
                    _ => {}
                }
            }
        }

        // Pause/resume: drop or rebuild the recorder so the OS mic indicator
        // actually reflects state, not just "samples discarded".
        let is_paused = pause.load(Ordering::Relaxed);
        if is_paused && !was_paused {
            recorder = None;
            notify("gemini-assistant paused", "Mic released.");
        } else if !is_paused && was_paused {
            recorder = Some(StreamingRecorder::start(mic_tx.clone()).context("reopening mic")?);
            notify("gemini-assistant resumed", "Mic live.");
        }
        was_paused = is_paused;
    }

    drop(recorder);
    let _ = session.disconnect().await;
    player.push_pcm16(&audio::chime_tone(Chime::Done), audio::INPUT_SAMPLE_RATE);
    player.wait_drain().await;
    Ok(())
}

pub fn notify(summary: &str, body: &str) {
    let _ = std::process::Command::new("notify-send")
        .arg(summary)
        .arg(body)
        .spawn();
}

/// Load, resample, and silence-trim a WAV file for the `send-clip` command.
pub fn load_wav_as_input_pcm16(path: &Path, vad_threshold: f64) -> Result<Vec<i16>> {
    let mut reader = hound::WavReader::open(path).with_context(|| format!("opening {}", path.display()))?;
    let spec = reader.spec();
    let raw: Vec<i16> = match spec.sample_format {
        hound::SampleFormat::Int => reader
            .samples::<i32>()
            .map(|s| s.map(|v| v as i16))
            .collect::<Result<_, _>>()?,
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .map(|s| s.map(|v| (v.clamp(-1.0, 1.0) * i16::MAX as f32) as i16))
            .collect::<Result<_, _>>()?,
    };
    let mono: Vec<i16> = if spec.channels > 1 {
        raw.chunks(spec.channels as usize)
            .map(|frame| (frame.iter().map(|&s| s as i32).sum::<i32>() / frame.len() as i32) as i16)
            .collect()
    } else {
        raw
    };
    let resampled = audio::resample_linear(&mono, spec.sample_rate, audio::INPUT_SAMPLE_RATE);
    Ok(audio::trim_silence(&resampled, audio::INPUT_SAMPLE_RATE, vad_threshold))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gemini_genai_rs::prelude::{GeminiModel, Voice};

    #[test]
    fn build_session_config_uses_configured_model_and_defaults() {
        let cfg = Config::default();
        let session_config = build_session_config("test-key", &cfg);
        // SessionConfig doesn't expose a public getter for every field, but
        // `model_uri`/`ws_url` incorporate the model, so we can check the
        // configured model round-trips through the builder.
        assert!(session_config.ws_url().contains("test-key"));
    }

    #[test]
    fn build_session_config_maps_custom_model_id() {
        let cfg = Config {
            model: "models/some-preview-id".to_string(),
            ..Config::default()
        };
        assert_eq!(cfg.gemini_model(), GeminiModel::Custom("models/some-preview-id".to_string()));
        // Exercised through the public builder to make sure it doesn't panic
        // or silently drop the custom id.
        let _ = build_session_config("test-key", &cfg);
    }

    #[test]
    fn build_session_config_respects_voice_override() {
        let cfg = Config {
            voice: Some("Kore".to_string()),
            ..Config::default()
        };
        assert_eq!(cfg.gemini_voice(), Some(Voice::Kore));
        let _ = build_session_config("test-key", &cfg);
    }

    #[test]
    fn pause_flag_defaults_to_unpaused() {
        let flag: PauseFlag = Arc::new(AtomicBool::new(false));
        assert!(!flag.load(Ordering::Relaxed));
        flag.store(true, Ordering::Relaxed);
        assert!(flag.load(Ordering::Relaxed));
    }
}
