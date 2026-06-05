//! Voice configuration for adele-gtk: pick between the standalone voice
//! **daemon** (`org.desktopAssistant.Voice`, the existing D-Bus path) and an
//! **embedded** in-process engine that does dictation + reply playback with the
//! [`adele_voice_module`] crate — no daemon, no wake word (issue #65).
//!
//! Loaded from `~/.config/adele-gtk/voice.toml`. The file is optional and the
//! whole struct is `#[serde(default)]`, so a missing or partial file yields the
//! **daemon** mode — i.e. nothing changes for existing users who never write
//! this file. To switch on the embedded engine a user sets:
//!
//! ```toml
//! mode = "embedded"
//!
//! [tts]
//! backend = "kokoro"   # or "piper" (both local); "polly" is cloud/opt-in
//! ```
//!
//! The `[audio]` / `[vad]` / `[stt]` / `[tts]` sections are the voice module's
//! own config types, so the embedded engine shares the exact knobs (and model
//! defaults) the daemon uses; an embedding client just deserializes them here.

use std::path::PathBuf;

use adele_voice_module::config::{AudioConfig, SttConfig, TtsConfig, VadConfig};
use serde::Deserialize;

/// Which voice backend the mic button and reply playback use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VoiceMode {
    /// The standalone voice daemon over D-Bus (`org.desktopAssistant.Voice`).
    /// The default, so current behavior is unchanged when no config is present.
    #[default]
    Daemon,
    /// The in-process [`adele_voice_module`] engine: dictation + playback run
    /// inside adele-gtk, requiring no voice daemon. No wake word (daemon-only).
    Embedded,
}

/// adele-gtk's voice settings. Deserialized from `voice.toml`; every field has
/// a default so an absent/partial file is valid and resolves to the daemon path.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct VoiceConfig {
    /// Daemon (default) vs. the embedded in-process engine.
    pub mode: VoiceMode,
    /// Audio input/output device names for the embedded engine.
    pub audio: AudioConfig,
    /// Voice-activity-detection (endpointing) tuning for embedded dictation.
    pub vad: VadConfig,
    /// Speech-to-text (Whisper) settings for embedded dictation.
    pub stt: SttConfig,
    /// Text-to-speech backend + voice for embedded reply playback. `backend`
    /// selects "kokoro" (local, default) / "piper" (local) / "polly" (cloud).
    pub tts: TtsConfig,
}

impl VoiceConfig {
    /// Load the voice config from the default path
    /// (`~/.config/adele-gtk/voice.toml`).
    ///
    /// A missing file is **not** an error — it returns the default (daemon)
    /// config so the app behaves exactly as before for users who never opt in.
    /// A present-but-unparseable file is logged and likewise degrades to the
    /// default, mirroring `ProfileStore::load`'s corrupt-file tolerance, so a
    /// typo in `voice.toml` can never stop the app from starting.
    pub fn load() -> Self {
        Self::load_from(&default_config_path())
    }

    /// Load from an explicit path (the seam the tests drive). Same tolerance as
    /// [`VoiceConfig::load`]: absent → default; unparseable → default + warn.
    pub fn load_from(path: &std::path::Path) -> Self {
        let data = match std::fs::read_to_string(path) {
            Ok(data) => data,
            // Absent file is the common case (no opt-in) — stay quiet.
            Err(_) => return Self::default(),
        };
        match toml::from_str::<VoiceConfig>(&data) {
            Ok(cfg) => cfg,
            Err(e) => {
                tracing::warn!(
                    "ignoring unparseable {} ({e}); using default (daemon) voice config",
                    path.display()
                );
                Self::default()
            }
        }
    }

    /// Whether the embedded in-process engine is selected.
    pub fn is_embedded(&self) -> bool {
        self.mode == VoiceMode::Embedded
    }
}

/// `~/.config/adele-gtk/voice.toml`. Mirrors `profile::default_config_dir` so
/// all of adele-gtk's per-user files live in the same directory.
fn default_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("adele-gtk")
        .join("voice.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_daemon_mode() {
        // The critical no-regression guarantee: with no config, the existing
        // daemon path is used.
        let cfg = VoiceConfig::default();
        assert_eq!(cfg.mode, VoiceMode::Daemon);
        assert!(!cfg.is_embedded());
    }

    #[test]
    fn missing_file_yields_daemon_default() {
        let path = std::env::temp_dir().join(format!(
            "adele-gtk-voice-missing-{}-{}.toml",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        // The file does not exist.
        assert!(!path.exists());
        let cfg = VoiceConfig::load_from(&path);
        assert_eq!(cfg.mode, VoiceMode::Daemon);
    }

    #[test]
    fn parses_embedded_mode_with_backend() {
        let cfg: VoiceConfig = toml::from_str(
            r#"
                mode = "embedded"

                [tts]
                backend = "piper"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.mode, VoiceMode::Embedded);
        assert!(cfg.is_embedded());
        assert_eq!(cfg.tts.backend, "piper");
        // Unspecified module fields fall back to the module's own defaults.
        assert_eq!(cfg.stt.language, "en");
    }

    #[test]
    fn parses_daemon_mode_explicitly() {
        let cfg: VoiceConfig = toml::from_str(r#"mode = "daemon""#).unwrap();
        assert_eq!(cfg.mode, VoiceMode::Daemon);
    }

    #[test]
    fn empty_document_is_daemon_default() {
        // A `voice.toml` that only has comments / is empty must still parse.
        let cfg: VoiceConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.mode, VoiceMode::Daemon);
    }

    #[test]
    fn unparseable_file_degrades_to_default() {
        let path = std::env::temp_dir().join(format!(
            "adele-gtk-voice-bad-{}-{}.toml",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&path, "this is = = not valid toml").unwrap();
        let cfg = VoiceConfig::load_from(&path);
        assert_eq!(cfg.mode, VoiceMode::Daemon);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn unknown_mode_value_is_rejected_then_degrades() {
        // An invalid `mode` is a parse error; `load_from` swallows it to the
        // default rather than failing startup.
        let path = std::env::temp_dir().join(format!(
            "adele-gtk-voice-unknownmode-{}-{}.toml",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&path, r#"mode = "telepathy""#).unwrap();
        let cfg = VoiceConfig::load_from(&path);
        assert_eq!(cfg.mode, VoiceMode::Daemon);
        let _ = std::fs::remove_file(&path);
    }
}
