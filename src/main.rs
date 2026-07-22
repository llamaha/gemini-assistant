mod audio;
mod config;
mod session;
mod transcript;

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

/// Linux truncates `/proc/<pid>/comm` to `TASK_COMM_LEN - 1 = 15` bytes, so
/// this 16-char binary name never appears in full even for the real process
/// — comparisons against `comm` must only look at the first 15 bytes.
const PROCESS_NAME: &str = "gemini-assistant";

#[derive(Parser)]
#[command(name = "gemini-assistant", about = "Hotkey-driven voice Q&A via the Gemini Live API")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug, PartialEq)]
enum Command {
    /// Toggle the live session: open one if idle, end it if running. Bind
    /// the hotkey to this (default if no subcommand given).
    Toggle,
    /// Pause/resume the mic without ending the session — conversation
    /// context is kept. Bind a second hotkey to this.
    Pause,
    /// Print the current session state without changing it.
    Status,
    /// Debug utility: send a WAV file straight to the Live API (no mic, no
    /// pidfile) and play back the response.
    SendClip { path: PathBuf },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let command = cli.command.unwrap_or(Command::Toggle);
    match command {
        Command::Toggle => cmd_toggle(),
        Command::Pause => cmd_pause(),
        Command::Status => cmd_status(),
        Command::SendClip { path } => cmd_send_clip(&path),
    }
}

// ---------------------------------------------------------------------------
// Pidfile: content, path, pure claim logic
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PidState {
    Live,
    Paused,
}

impl PidState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Paused => "paused",
        }
    }

    fn parse(s: &str) -> Self {
        if s.trim() == "paused" {
            Self::Paused
        } else {
            Self::Live
        }
    }
}

fn pidfile_path() -> PathBuf {
    let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(base).join("gemini-assistant.pid")
}

/// `<pid>\n<state>\n` — a bare pid with no second line is treated as `Live`
/// (backward compatible with a plain pidfile).
fn read_pidfile(path: &Path) -> Option<(i32, PidState)> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    let pid: i32 = lines.next()?.trim().parse().ok()?;
    let state = lines.next().map(PidState::parse).unwrap_or(PidState::Live);
    Some((pid, state))
}

/// Atomic write-then-rename so a reader never observes a half-written file.
fn write_pidfile(path: &Path, pid: i32, state: PidState) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, format!("{pid}\n{}\n", state.as_str()))
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).context("renaming pidfile into place")?;
    Ok(())
}

/// Compare only the first 15 bytes of `expected` against `comm` — Linux
/// truncates `/proc/<pid>/comm` to `TASK_COMM_LEN - 1 = 15` chars, so
/// `"gemini-assistant"` (16 chars) never appears in full even for our own
/// process. A naive full-string comparison would treat every live session as
/// stale.
fn comm_matches(comm: &str, expected: &str) -> bool {
    let comm = comm.trim_end();
    let truncated_expected: String = expected.chars().take(15).collect();
    comm == truncated_expected
}

fn read_proc_comm(pid: i32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()
}

fn is_alive(pid: i32) -> bool {
    // kill(pid, 0) sends no signal; it only checks whether the pid exists
    // and is signalable by us.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn is_our_process(pid: i32) -> bool {
    read_proc_comm(pid).is_some_and(|comm| comm_matches(&comm, PROCESS_NAME))
}

/// A live, identity-confirmed owner of the pidfile, if any.
fn find_live_owner(path: &Path, is_alive: impl Fn(i32) -> bool, identity_matches: impl Fn(i32) -> bool) -> Option<i32> {
    let (pid, _state) = read_pidfile(path)?;
    (is_alive(pid) && identity_matches(pid)).then_some(pid)
}

#[derive(Debug, PartialEq)]
enum ClaimOutcome {
    Claimed,
    SignaledOwner(i32),
}

/// Try to become the session by exclusively creating the pidfile.
/// `O_CREAT|O_EXCL` is the sole race-closer for two rapid hotkey presses —
/// the loser's `create_new` fails immediately, no read-then-write window.
/// A pidfile whose owner is dead or isn't us is stale and gets cleared,
/// bounded to one retry so a persistently uncreatable path fails loudly
/// instead of looping forever.
fn attempt_claim(
    path: &Path,
    my_pid: i32,
    is_alive: impl Fn(i32) -> bool,
    identity_matches: impl Fn(i32) -> bool,
) -> Result<ClaimOutcome> {
    for _ in 0..2 {
        match std::fs::OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(_file) => {
                write_pidfile(path, my_pid, PidState::Live)?;
                return Ok(ClaimOutcome::Claimed);
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                match find_live_owner(path, &is_alive, &identity_matches) {
                    Some(owner_pid) => return Ok(ClaimOutcome::SignaledOwner(owner_pid)),
                    None => {
                        let _ = std::fs::remove_file(path);
                        continue;
                    }
                }
            }
            Err(e) => return Err(e).context("creating pidfile"),
        }
    }
    anyhow::bail!("could not claim pidfile after clearing a stale entry")
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

fn cmd_toggle() -> Result<()> {
    let path = pidfile_path();
    let my_pid = std::process::id() as i32;

    match attempt_claim(&path, my_pid, is_alive, is_our_process)? {
        ClaimOutcome::SignaledOwner(owner_pid) => {
            unsafe {
                libc::kill(owner_pid, libc::SIGTERM);
            }
            println!("stopping session (pid {owner_pid})");
            Ok(())
        }
        ClaimOutcome::Claimed => run_session_process(path, my_pid as u32),
    }
}

fn cmd_pause() -> Result<()> {
    let path = pidfile_path();
    match find_live_owner(&path, is_alive, is_our_process) {
        Some(pid) => {
            unsafe {
                libc::kill(pid, libc::SIGUSR1);
            }
            println!("toggled pause");
        }
        None => println!("no live session"),
    }
    Ok(())
}

fn cmd_status() -> Result<()> {
    let path = pidfile_path();
    let status = match read_pidfile(&path) {
        Some((pid, state)) if is_alive(pid) && is_our_process(pid) => state.as_str(),
        _ => "stopped",
    };
    println!("{status}");
    Ok(())
}

fn cmd_send_clip(path: &Path) -> Result<()> {
    let api_key = config::load_api_key()?;
    let cfg = config::Config::load();
    let samples = session::load_wav_as_input_pcm16(path, cfg.vad_threshold)?;
    println!(
        "loaded {:.1}s of audio after resampling/trimming",
        samples.len() as f32 / audio::INPUT_SAMPLE_RATE as f32
    );

    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async move {
        let player = audio::Player::new().context("opening speaker")?;
        let turn = session::run_turn(&api_key, &cfg, &samples, |chunk| {
            player.push_pcm16(chunk, audio::OUTPUT_SAMPLE_RATE);
        })
        .await?;
        player.wait_drain().await;
        println!("input transcript:  {}", turn.input_transcript);
        println!("output transcript: {}", turn.output_transcript);
        anyhow::Ok(())
    })
}

/// Removes the pidfile when dropped, regardless of which return path is
/// taken. Startup can fail before the session ever opens a socket (e.g. a
/// missing API key), and that failure must not leave a claimed pidfile
/// behind — a leftover claim is self-healing via the stale-pid check on the
/// next `toggle`, but only after wastefully bouncing off it once first.
struct PidfileGuard(PathBuf);

impl Drop for PidfileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Become the live session: open the mic, connect to Gemini, and run until
/// signaled. Runs on a `current_thread` runtime + `LocalSet` because the
/// `cpal::Stream`s owned deep inside `session::run` aren't `Send` on Linux.
///
/// Launched from a KDE global shortcut, this has no attached terminal — any
/// error here is otherwise invisible, so failures are also pushed through a
/// desktop notification, not just printed.
fn run_session_process(pidfile: PathBuf, pid: u32) -> Result<()> {
    let _guard = PidfileGuard(pidfile.clone());
    let result = try_run_session_process(pidfile, pid);
    if let Err(e) = &result {
        eprintln!("failed to start session: {e}");
        session::notify("gemini-assistant failed to start", &e.to_string());
    }
    result
}

fn try_run_session_process(pidfile: PathBuf, pid: u32) -> Result<()> {
    let api_key = config::load_api_key()?;
    let cfg = config::Config::load();

    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, run_session_local(pidfile, pid, api_key, cfg))
}

async fn run_session_local(pidfile: PathBuf, pid: u32, api_key: String, cfg: config::Config) -> Result<()> {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigusr1 = signal(SignalKind::user_defined1())?;

    let pause: session::PauseFlag = Arc::new(AtomicBool::new(false));
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let pause_for_signals = pause.clone();
    let pidfile_for_signals = pidfile.clone();
    tokio::task::spawn_local(async move {
        loop {
            tokio::select! {
                _ = sigterm.recv() => {
                    let _ = shutdown_tx.send(true);
                    break;
                }
                _ = sigint.recv() => {
                    let _ = shutdown_tx.send(true);
                    break;
                }
                _ = sigusr1.recv() => {
                    let now_paused = !pause_for_signals.fetch_xor(true, Ordering::Relaxed);
                    let state = if now_paused { PidState::Paused } else { PidState::Live };
                    let _ = write_pidfile(&pidfile_for_signals, pid as i32, state);
                }
            }
        }
    });

    session::notify("gemini-assistant", "Session started.");
    let result = session::run(&api_key, &cfg, pause, shutdown_rx, pid).await;
    if let Err(e) = &result {
        eprintln!("session error: {e}");
        let _ = audio::play_chime_blocking(audio::Chime::Error);
        session::notify("gemini-assistant error", &e.to_string());
    } else {
        session::notify("gemini-assistant", "Session ended.");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pidfile_guard_removes_file_on_drop_even_after_early_return() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.pid");
        write_pidfile(&path, 111, PidState::Live).unwrap();
        assert!(path.exists());

        fn fails_before_cleanup(path: &Path) -> Result<()> {
            let _guard = PidfileGuard(path.to_path_buf());
            anyhow::bail!("simulated startup failure, e.g. a missing API key")
        }

        let result = fails_before_cleanup(&path);
        assert!(result.is_err());
        assert!(!path.exists(), "guard must remove the pidfile even on an early error return");
    }

    #[test]
    fn default_command_is_toggle() {
        let cli = Cli::parse_from(["gemini-assistant"]);
        assert_eq!(cli.command, None);
        assert_eq!(cli.command.unwrap_or(Command::Toggle), Command::Toggle);
    }

    #[test]
    fn send_clip_parses_path_argument() {
        let cli = Cli::parse_from(["gemini-assistant", "send-clip", "/tmp/clip.wav"]);
        assert_eq!(cli.command, Some(Command::SendClip { path: PathBuf::from("/tmp/clip.wav") }));
    }

    #[test]
    fn pause_and_status_parse() {
        assert_eq!(Cli::parse_from(["gemini-assistant", "pause"]).command, Some(Command::Pause));
        assert_eq!(Cli::parse_from(["gemini-assistant", "status"]).command, Some(Command::Status));
        assert_eq!(Cli::parse_from(["gemini-assistant", "toggle"]).command, Some(Command::Toggle));
    }

    #[test]
    fn unknown_subcommand_errors() {
        assert!(Cli::try_parse_from(["gemini-assistant", "not-a-real-command"]).is_err());
    }

    #[test]
    fn comm_matches_handles_the_15_char_truncation() {
        // The real /proc/<pid>/comm content for our own 16-char binary name
        // is truncated to 15 chars with a trailing newline from `cat`-style
        // reads (read_to_string preserves it; comm_matches must trim it).
        assert!(comm_matches("gemini-assistan\n", PROCESS_NAME));
        assert!(comm_matches("gemini-assistan", PROCESS_NAME));
        // A full untruncated 16-char match should also be accepted if it
        // ever occurs (e.g. a future rename to a <=15 char binary name).
        assert!(!comm_matches("gemini-assistant", "short-name"));
        // An unrelated process must never match.
        assert!(!comm_matches("bash\n", PROCESS_NAME));
        assert!(!comm_matches("sleep\n", PROCESS_NAME));
    }

    #[test]
    fn pidfile_round_trips_pid_and_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.pid");
        write_pidfile(&path, 4242, PidState::Paused).unwrap();
        assert_eq!(read_pidfile(&path), Some((4242, PidState::Paused)));
    }

    #[test]
    fn pidfile_with_no_state_line_defaults_to_live() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.pid");
        std::fs::write(&path, "4242\n").unwrap();
        assert_eq!(read_pidfile(&path), Some((4242, PidState::Live)));
    }

    #[test]
    fn read_pidfile_missing_file_is_none() {
        let path = Path::new("/nonexistent/gemini-assistant-test/some.pid");
        assert_eq!(read_pidfile(path), None);
    }

    #[test]
    fn claim_succeeds_when_pidfile_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.pid");
        let outcome = attempt_claim(&path, 111, |_| false, |_| false).unwrap();
        assert_eq!(outcome, ClaimOutcome::Claimed);
        assert_eq!(read_pidfile(&path), Some((111, PidState::Live)));
    }

    #[test]
    fn claim_signals_a_live_confirmed_owner_instead_of_claiming() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.pid");
        write_pidfile(&path, 999, PidState::Live).unwrap();

        let outcome = attempt_claim(&path, 111, |_| true, |_| true).unwrap();
        assert_eq!(outcome, ClaimOutcome::SignaledOwner(999));
        // The pidfile must be left untouched — it's still the real owner's.
        assert_eq!(read_pidfile(&path), Some((999, PidState::Live)));
    }

    #[test]
    fn claim_clears_a_stale_pidfile_from_a_dead_pid_and_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.pid");
        write_pidfile(&path, 999, PidState::Live).unwrap();

        // is_alive always false -> the existing entry is stale.
        let outcome = attempt_claim(&path, 111, |_| false, |_| true).unwrap();
        assert_eq!(outcome, ClaimOutcome::Claimed);
        assert_eq!(read_pidfile(&path), Some((111, PidState::Live)));
    }

    #[test]
    fn claim_clears_a_pidfile_whose_identity_does_not_match_and_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.pid");
        // A live pid that belongs to some other, unrelated program (pid
        // recycled, or a leftover pidfile from a different binary).
        write_pidfile(&path, 999, PidState::Live).unwrap();

        let outcome = attempt_claim(&path, 111, |_| true, |_| false).unwrap();
        assert_eq!(outcome, ClaimOutcome::Claimed);
        assert_eq!(read_pidfile(&path), Some((111, PidState::Live)));
    }

    #[test]
    fn find_live_owner_returns_none_for_stale_or_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.pid");
        assert_eq!(find_live_owner(&path, |_| true, |_| true), None);

        write_pidfile(&path, 999, PidState::Live).unwrap();
        assert_eq!(find_live_owner(&path, |_| false, |_| true), None);
        assert_eq!(find_live_owner(&path, |_| true, |_| false), None);
        assert_eq!(find_live_owner(&path, |_| true, |_| true), Some(999));
    }
}
