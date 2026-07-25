//! One-shot spoken question — a lightweight alternative to a full session.
//!
//! Press the hotkey to start talking, press again to send; you hear a single
//! spoken answer and the connection closes. No persistent session, no
//! accumulated context.
//!
//! The key difference from a live session is *manual* turn signalling. A
//! session leaves server-side voice-activity detection on and lets the model
//! decide when you've stopped talking. Here we disable that
//! (`automaticActivityDetection.disabled`) and bracket the recording with
//! explicit `activityStart` / `activityEnd`, so the end of the question is
//! deterministic: the model responds the instant we say "done", not after it
//! guesses you've paused. (This is also the robust version of what the
//! `send-clip` diagnostic tries to do with trailing silence.)

use anyhow::{Context, Result};
use gemini_genai_rs::prelude::{
    bytes_to_i16, connect, i16_to_bytes, recv_event, AutomaticActivityDetection, SessionEvent,
    SessionHandle, SessionPhase, TransportConfig,
};
use tokio::sync::mpsc as tokio_mpsc;

use crate::audio::{self, Chime, Player, StreamingRecorder};
use crate::config::Config;
use crate::session::{build_session_config, debug_enabled};

/// Connect with automatic activity detection turned off, so the turn is driven
/// entirely by our `signal_activity_start` / `signal_activity_end` calls.
async fn connect_ask(api_key: &str, cfg: &Config) -> Result<SessionHandle> {
    let config = build_session_config(api_key, cfg).server_vad(AutomaticActivityDetection {
        disabled: Some(true),
        start_of_speech_sensitivity: None,
        end_of_speech_sensitivity: None,
        prefix_padding_ms: None,
        silence_duration_ms: None,
    });
    let session = connect(config, TransportConfig::default())
        .await
        .context("connecting to Gemini Live")?;
    tokio::time::timeout(
        std::time::Duration::from_secs(15),
        session.wait_for_phase(SessionPhase::Active),
    )
    .await
    .context("timed out waiting for connection to become active")?;
    Ok(session)
}

/// Run one spoken question end to end.
///
/// `send` flips true on the second hotkey press (via `SIGUSR1`), ending the
/// recording and committing the turn. A missed second press is covered by
/// `max_secs`, so a forgotten mic still completes and cleans up rather than
/// streaming (and billing) forever.
pub async fn run(
    api_key: &str,
    cfg: &Config,
    mut send: tokio::sync::watch::Receiver<bool>,
    max_secs: u64,
) -> Result<()> {
    let session = connect_ask(api_key, cfg).await?;
    // Subscribe before ending the turn so no response event is missed.
    let mut events = session.subscribe();

    let player = Player::new().context("opening speaker")?;
    player.push_pcm16(&audio::chime_tone(Chime::Start), audio::INPUT_SAMPLE_RATE);

    // Same mic plumbing as a session: cpal callback -> std channel (on its own
    // OS thread) -> bridge thread -> async channel, keeping the non-Send
    // `cpal::Stream` off the runtime.
    let (mic_tx, mic_rx) = std::sync::mpsc::channel::<Vec<i16>>();
    let recorder = StreamingRecorder::start(mic_tx).context("opening mic")?;
    let (async_tx, mut async_rx) = tokio_mpsc::unbounded_channel::<Vec<i16>>();
    std::thread::spawn(move || {
        while let Ok(chunk) = mic_rx.recv() {
            if async_tx.send(chunk).is_err() {
                break;
            }
        }
    });

    session
        .signal_activity_start()
        .await
        .context("signalling activity start")?;
    crate::session::notify("gemini-assistant ask", "Listening — press again to send.");

    let deadline = tokio::time::sleep(std::time::Duration::from_secs(max_secs.max(1)));
    tokio::pin!(deadline);
    let mut hit_cap = false;

    // Stream the question until the second press (or the safety cap).
    loop {
        tokio::select! {
            biased;
            _ = send.changed() => break,
            _ = &mut deadline => { hit_cap = true; break; }
            Some(chunk) = async_rx.recv() => {
                let _ = session.send_audio(i16_to_bytes(&chunk).to_vec()).await;
            }
        }
    }

    // Release the mic before we wait on the answer — nothing more should be
    // captured, and the OS mic indicator should go dark.
    drop(recorder);
    session
        .signal_activity_end()
        .await
        .context("signalling activity end")?;
    if hit_cap {
        crate::session::notify(
            "gemini-assistant ask",
            "Reached the max question length — sending.",
        );
    } else {
        crate::session::notify("gemini-assistant ask", "Thinking…");
    }

    // Play the spoken answer as it streams in, until the turn completes.
    loop {
        match recv_event(&mut events).await {
            Some(SessionEvent::AudioData(bytes)) => {
                if let Some(samples) = bytes_to_i16(&bytes) {
                    player.push_pcm16(samples, audio::OUTPUT_SAMPLE_RATE);
                }
            }
            // All response audio has arrived by `GenerationComplete`;
            // `TurnComplete` is the tidier marker but can lag well behind on
            // longer answers, so either ends the wait.
            Some(SessionEvent::TurnComplete) | Some(SessionEvent::GenerationComplete) => break,
            Some(SessionEvent::Error(e)) => {
                anyhow::bail!("ask error: {e}");
            }
            Some(SessionEvent::GoAway(_)) | Some(SessionEvent::Disconnected(_)) | None => break,
            Some(other) => {
                if debug_enabled() {
                    eprintln!("(debug) ask event: {other:?}");
                }
            }
        }
    }

    player.wait_drain().await;
    let _ = session.disconnect().await;
    Ok(())
}
