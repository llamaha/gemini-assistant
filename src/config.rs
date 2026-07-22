//! Config file + env var overrides + API-key-without-a-shell loading.
//!
//! A KDE global shortcut runs the binary directly, with no shell profile
//! sourced, so the API key can't rely on `.bashrc`/`.profile` exporting it —
//! it has to be readable from a file the binary opens itself.

use std::path::PathBuf;

use anyhow::{Context, Result};
use gemini_genai_rs::prelude::{GeminiModel, Voice};
use serde::{Deserialize, Serialize};

/// Google renames/rotates preview Live model ids periodically; check
/// https://ai.google.dev/gemini-api/docs/models if requests start failing.
/// Gemini 3.1 Flash Live Preview, current as of 2026-07 — Google's
/// highest-quality real-time dialogue model, released March 2026.
pub const DEFAULT_MODEL: &str = "models/gemini-3.1-flash-live-preview";

pub const DEFAULT_SYSTEM_INSTRUCTION: &str =
    "You are a fast, concise voice assistant running from a hotkey. \
Answer directly in natural spoken language — no markdown, no bullet lists, no headers. \
Keep answers short unless the question needs detail.";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub model: String,
    pub voice: Option<String>,
    pub system_instruction: String,
    pub google_search: bool,
    pub reminder_secs: u64,
    pub context_window_target_tokens: u32,
    pub vad_threshold: f64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: DEFAULT_MODEL.to_string(),
            voice: None,
            system_instruction: DEFAULT_SYSTEM_INSTRUCTION.to_string(),
            google_search: true,
            reminder_secs: 600,
            context_window_target_tokens: 16_000,
            vad_threshold: 400.0,
        }
    }
}

fn config_home() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            PathBuf::from(home).join(".config")
        })
}

fn config_path() -> PathBuf {
    config_home().join("gemini-assistant").join("config.json")
}

fn env_key_path() -> PathBuf {
    config_home().join("gemini-assistant").join("env")
}

impl Config {
    /// Load from disk, then let env vars override individual fields. Env wins
    /// so a one-off run can try a different model/threshold without editing
    /// (and risking clobbering) the saved config.
    pub fn load() -> Self {
        Self::load_from(&config_path(), |k| std::env::var(k).ok())
    }

    /// Env lookups go through a closure rather than `std::env::var` directly
    /// so tests can inject overrides without mutating process-wide state
    /// (which races across parallel test threads).
    fn load_from(path: &std::path::Path, env: impl Fn(&str) -> Option<String>) -> Self {
        let mut config: Config = std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();

        if let Some(model) = env("GEMINI_LIVE_MODEL") {
            config.model = model;
        }
        if let Some(v) = env("GEMINI_ASSISTANT_REMINDER_SECS").and_then(|s| s.parse().ok()) {
            config.reminder_secs = v;
        }
        if let Some(v) = env("GEMINI_ASSISTANT_VAD_THRESHOLD").and_then(|s| s.parse().ok()) {
            config.vad_threshold = v;
        }
        config
    }

    /// Resolve `model` to a `GeminiModel`, matching known variants and
    /// falling back to `Custom` — this is what keeps a preview model rename
    /// a config edit instead of a rebuild.
    pub fn gemini_model(&self) -> GeminiModel {
        model_from_str(&self.model)
    }

    /// Resolve `voice` the same way; `None` means "let the crate default".
    pub fn gemini_voice(&self) -> Option<Voice> {
        self.voice.as_deref().map(voice_from_str)
    }
}

fn model_from_str(s: &str) -> GeminiModel {
    match s {
        "models/gemini-2.0-flash-live-001" => GeminiModel::Gemini2_0FlashLive,
        "models/gemini-live-2.5-flash-native-audio" => GeminiModel::GeminiLive2_5FlashNativeAudio,
        other => GeminiModel::Custom(other.to_string()),
    }
}

fn voice_from_str(s: &str) -> Voice {
    match s {
        "Aoede" => Voice::Aoede,
        "Charon" => Voice::Charon,
        "Fenrir" => Voice::Fenrir,
        "Kore" => Voice::Kore,
        "Puck" => Voice::Puck,
        other => Voice::Custom(other.to_string()),
    }
}

/// Load the Gemini API key without relying on a shell profile: first
/// `~/.config/gemini-assistant/env` (`GEMINI_API_KEY=...`, one entry per
/// line), falling back to the `GEMINI_API_KEY` environment variable for
/// manual/terminal runs.
pub fn load_api_key() -> Result<String> {
    load_api_key_from(&env_key_path(), || std::env::var("GEMINI_API_KEY").ok())
}

fn load_api_key_from(env_file: &std::path::Path, env_var: impl Fn() -> Option<String>) -> Result<String> {
    if let Ok(text) = std::fs::read_to_string(env_file) {
        for line in text.lines() {
            let line = line.trim();
            if let Some(value) = line.strip_prefix("GEMINI_API_KEY=") {
                let value = value.trim();
                if !value.is_empty() {
                    return Ok(value.to_string());
                }
            }
        }
    }
    env_var()
        .context("GEMINI_API_KEY not set and no key found in ~/.config/gemini-assistant/env")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn default_config_has_verified_model_and_search_on() {
        let config = Config::default();
        assert_eq!(config.model, DEFAULT_MODEL);
        assert!(config.google_search);
        assert_eq!(config.reminder_secs, 600);
    }

    #[test]
    fn load_from_missing_file_returns_defaults() {
        let path = std::path::Path::new("/nonexistent/gemini-assistant-test/config.json");
        let config = Config::load_from(path, |_| None);
        assert_eq!(config, Config::default());
    }

    #[test]
    fn load_from_file_reads_saved_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let custom = Config {
            model: "models/custom-test".to_string(),
            reminder_secs: 99,
            ..Config::default()
        };
        std::fs::write(&path, serde_json::to_string_pretty(&custom).unwrap()).unwrap();

        let loaded = Config::load_from(&path, |_| None);
        assert_eq!(loaded.model, "models/custom-test");
        assert_eq!(loaded.reminder_secs, 99);
    }

    #[test]
    fn env_vars_override_file_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&Config::default()).unwrap(),
        )
        .unwrap();

        let loaded = Config::load_from(&path, |key| match key {
            "GEMINI_LIVE_MODEL" => Some("models/env-override".to_string()),
            "GEMINI_ASSISTANT_REMINDER_SECS" => Some("5".to_string()),
            "GEMINI_ASSISTANT_VAD_THRESHOLD" => Some("123.5".to_string()),
            _ => None,
        });

        assert_eq!(loaded.model, "models/env-override");
        assert_eq!(loaded.reminder_secs, 5);
        assert_eq!(loaded.vad_threshold, 123.5);
    }

    #[test]
    fn model_from_str_maps_known_ids_and_falls_back_to_custom() {
        assert_eq!(
            model_from_str("models/gemini-2.0-flash-live-001"),
            GeminiModel::Gemini2_0FlashLive
        );
        assert_eq!(
            model_from_str("models/gemini-live-2.5-flash-native-audio"),
            GeminiModel::GeminiLive2_5FlashNativeAudio
        );
        assert_eq!(
            model_from_str(DEFAULT_MODEL),
            GeminiModel::Custom(DEFAULT_MODEL.to_string())
        );
    }

    #[test]
    fn voice_from_str_maps_known_names_and_falls_back_to_custom() {
        assert_eq!(voice_from_str("Kore"), Voice::Kore);
        assert_eq!(voice_from_str("Someone"), Voice::Custom("Someone".to_string()));
    }

    #[test]
    fn config_resolves_gemini_model_and_voice() {
        let config = Config {
            voice: Some("Aoede".to_string()),
            ..Config::default()
        };
        assert_eq!(
            config.gemini_model(),
            GeminiModel::Custom(DEFAULT_MODEL.to_string())
        );
        assert_eq!(config.gemini_voice(), Some(Voice::Aoede));

        let config = Config::default();
        assert_eq!(config.gemini_voice(), None);
    }

    #[test]
    fn load_api_key_reads_from_env_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("env");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "# comment\nGEMINI_API_KEY=file-key-123\n").unwrap();
        assert_eq!(
            load_api_key_from(&path, || Some("should-be-ignored".to_string())).unwrap(),
            "file-key-123"
        );
    }

    #[test]
    fn load_api_key_falls_back_to_env_var() {
        let path = std::path::Path::new("/nonexistent/gemini-assistant-test/env");
        let result = load_api_key_from(path, || Some("env-var-key".to_string()));
        assert_eq!(result.unwrap(), "env-var-key");
    }

    #[test]
    fn load_api_key_errors_when_neither_source_has_it() {
        let path = std::path::Path::new("/nonexistent/gemini-assistant-test/env");
        assert!(load_api_key_from(path, || None).is_err());
    }
}
