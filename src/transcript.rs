//! Append-only local record of what was said in a session.
//!
//! One file per session process. This is a human-readable record only —
//! conversation *context* lives server-side for the life of a live session,
//! and Gemini's input-audio transcription is best-effort (it sometimes
//! arrives truncated, sometimes not at all), so nothing here ever feeds back
//! into the model. Treat a stored `question` as a possibly-incomplete label,
//! never as ground truth.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Turn {
    pub question: String,
    pub answer: String,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Session {
    pub id: String,
    pub started: u64,
    pub turns: Vec<Turn>,
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn data_dir() -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            PathBuf::from(home).join(".local/share")
        });
    base.join("gemini-assistant").join("sessions")
}

impl Session {
    /// Start a new session record, identified by the current process id so
    /// it never collides with a concurrent session (the pidfile already
    /// guarantees there's at most one live session at a time).
    pub fn new(pid: u32) -> Self {
        let now = now_secs();
        Self {
            id: format!("{now}-{pid}"),
            started: now,
            turns: Vec::new(),
        }
    }

    fn path_in(&self, dir: &std::path::Path) -> PathBuf {
        dir.join(format!("{}.json", self.id))
    }

    pub fn add_turn(&mut self, question: String, answer: String) {
        self.turns.push(Turn {
            question,
            answer,
            timestamp: now_secs(),
        });
    }

    pub fn save(&self) -> Result<()> {
        self.save_in(&data_dir())
    }

    fn save_in(&self, dir: &std::path::Path) -> Result<()> {
        let path = self.path_in(dir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(self)?)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_session_id_includes_pid() {
        let session = Session::new(12345);
        assert!(session.id.ends_with("-12345"));
        assert!(session.turns.is_empty());
    }

    #[test]
    fn add_turn_appends_with_timestamp() {
        let mut session = Session::new(1);
        session.add_turn("what time is it".into(), "it's noon".into());
        assert_eq!(session.turns.len(), 1);
        assert_eq!(session.turns[0].question, "what time is it");
        assert_eq!(session.turns[0].answer, "it's noon");
    }

    #[test]
    fn save_and_reload_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::new(999);
        session.add_turn("q1".into(), "a1".into());
        session.save_in(dir.path()).unwrap();

        let path = session.path_in(dir.path());
        assert!(path.exists());
        let text = std::fs::read_to_string(&path).unwrap();
        let loaded: Session = serde_json::from_str(&text).unwrap();
        assert_eq!(loaded, session);
    }
}
