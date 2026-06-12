//! In-process voice engine: dictation + reply playback with no daemon.
//!
//! The embedded alternative to `voice_client` (the D-Bus daemon path). It wraps
//! the [`adele_voice_module`] primitives — [`Dictation`] (mic → Silero VAD
//! endpoint → Whisper) and [`Speaker`] (configured TTS backend → audio sink) —
//! so a machine with **no voice daemon** still gets a working mic button and
//! spoken replies (issue #65). There is deliberately **no wake word** here; the
//! always-on wake word stays daemon-only (epic voice#34).
//!
//! ## Threading
//!
//! The GTK main thread can't block, and the adapters (cpal mic/sink, the
//! Whisper/ONNX models) are heavy, so:
//!
//! - Construction is **lazy**: the models load and the mic opens only on the
//!   first [`EmbeddedVoice::dictate`] call (and the speaker on the first
//!   [`EmbeddedVoice::say`]) — never at startup. A user who never clicks the
//!   mic never pays the load cost and the **microphone is never opened**.
//! - All work runs on the shared Tokio runtime (via [`spawn_on_runtime`]); the
//!   GTK side hands in oneshot/mpsc channels and only plain values cross back.
//! - A `tokio::sync::Mutex` serializes access, so a second mic click while a
//!   turn is in flight waits its turn instead of opening a second mic stream.
//!
//! `EmbeddedVoice` is cheap to clone (just `Arc`s) and `Send + Sync`, so the
//! window can share one handle between the mic-button handler and the
//! reply-playback hook.

use std::sync::Arc;

use adele_voice_module::config::{AudioConfig, SttConfig, TtsConfig, VadConfig};
use adele_voice_module::{Dictation, Speaker, TtsBackend, build_dictation, build_speaker};
use adele_voice_stt_whisper::WhisperStt;
use adele_voice_vad_silero::SileroVad;
use tokio::sync::Mutex;

/// The speakable-sentence chunker now lives in the shared `client-voice` crate
/// (desktop-assistant#274) so the GTK and TUI clients can't drift. Re-exported
/// at its original path so the in-process speak path keeps calling
/// `voice_embedded::into_speakable_sentences`.
pub use adele_voice_client_common::into_speakable_sentences;

/// The concrete dictation type the config builder wires (Silero VAD + Whisper).
type EmbeddedDictation = Dictation<SileroVad, WhisperStt>;

/// Shared, lazily-initialized in-process dictation + playback.
///
/// Clone to share the same underlying engine (the `Arc`s are shared, so the
/// mic-button handler and the reply hook drive one `Dictation`/`Speaker`).
#[derive(Clone)]
pub struct EmbeddedVoice {
    /// The voice-module config sections (audio/vad/stt/tts) used to build the
    /// adapters on first use.
    cfg: Arc<EmbeddedConfig>,
    /// Built on the first `dictate()`; `None` until then so models/mic load
    /// lazily. The `Mutex` also serializes dictation turns.
    dictation: Arc<Mutex<Option<EmbeddedDictation>>>,
    /// Built on the first `say()`; `None` until then. `Speaker` is itself cheap
    /// to clone, but we hold one instance so the sink/backend are shared.
    speaker: Arc<Mutex<Option<Speaker<TtsBackend>>>>,
}

/// The subset of [`crate::voice_config::VoiceConfig`] the engine needs — the
/// module's own config types. Snapshotted into an `Arc` so the engine is
/// self-contained (no borrow of the larger app config).
pub struct EmbeddedConfig {
    pub audio: AudioConfig,
    pub vad: VadConfig,
    pub stt: SttConfig,
    pub tts: TtsConfig,
}

impl EmbeddedVoice {
    /// Create an engine from the voice config. Builds **nothing** yet — the
    /// adapters (and the mic) come up lazily on first use.
    pub fn new(cfg: EmbeddedConfig) -> Self {
        Self {
            cfg: Arc::new(cfg),
            dictation: Arc::new(Mutex::new(None)),
            speaker: Arc::new(Mutex::new(None)),
        }
    }

    /// Capture and transcribe one utterance.
    ///
    /// Opens the mic (building the engine on first call), waits for the user to
    /// speak and stop (VAD endpointing), and returns the transcript — or `None`
    /// when nothing was said / the capture was near-silent. Errors (model load
    /// failure, audio device gone) surface as `Err(String)` for the caller to
    /// report. Runs entirely on the Tokio runtime; never blocks the GTK thread.
    pub async fn dictate(&self) -> Result<Option<String>, String> {
        let mut guard = self.dictation.lock().await;
        if guard.is_none() {
            // First use: load Silero + Whisper and bind the mic.
            let d = build_dictation(&self.cfg.audio, &self.cfg.vad, &self.cfg.stt)
                .map_err(|e| format!("voice init failed: {e}"))?;
            *guard = Some(d);
        }
        // Safe: just populated above.
        let dictation = guard.as_mut().expect("dictation built");
        dictation.dictate().await.map_err(|e| e.to_string())
    }

    /// Speak `text` through the configured TTS backend.
    ///
    /// Builds the speaker on first call (local-first Kokoro→Piper fallback).
    /// A failure to synthesize/play surfaces as `Err(String)`. Independent of
    /// the mic — speaking never listens.
    pub async fn say(&self, text: &str) -> Result<(), String> {
        let speaker = self.ensure_speaker().await;
        speaker.say(text).await.map_err(|e| e.to_string())
    }

    /// Stop any in-progress playback (barge-in). A no-op if the speaker hasn't
    /// been built yet (nothing has played, so nothing to stop).
    pub async fn stop_speaking(&self) -> Result<(), String> {
        let guard = self.speaker.lock().await;
        match guard.as_ref() {
            Some(speaker) => speaker.stop().map_err(|e| e.to_string()),
            None => Ok(()),
        }
    }

    /// Whether the speaker is currently playing audio. `false` before the
    /// speaker is built (nothing has ever played).
    pub async fn is_playing(&self) -> bool {
        let guard = self.speaker.lock().await;
        guard.as_ref().is_some_and(|s| s.is_playing())
    }

    /// Get (building on first call) a clone of the shared [`Speaker`]. Cloning
    /// the speaker lets the lock be released before the (awaited) synthesis, so
    /// `say` does not hold the mutex across playback.
    async fn ensure_speaker(&self) -> Speaker<TtsBackend> {
        let mut guard = self.speaker.lock().await;
        if guard.is_none() {
            let s = build_speaker(&self.cfg.tts, &self.cfg.audio).await;
            *guard = Some(s);
        }
        guard.as_ref().expect("speaker built").clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> EmbeddedVoice {
        EmbeddedVoice::new(EmbeddedConfig {
            audio: AudioConfig::default(),
            vad: VadConfig::default(),
            stt: SttConfig::default(),
            tts: TtsConfig::default(),
        })
    }

    /// Security/privacy property (issue #65): constructing the engine must not
    /// open any audio device or load any model — that only happens on the first
    /// explicit `dictate()`/`say()`. Before any use, the speaker is unbuilt, so
    /// it reports not-playing and `stop_speaking()` is a harmless no-op. (This
    /// deliberately never calls `dictate()`, which would open the mic.)
    #[tokio::test]
    async fn fresh_engine_has_not_touched_audio() {
        let engine = engine();
        assert!(
            !engine.is_playing().await,
            "a freshly built engine must report no playback (speaker not built)"
        );
        assert!(
            engine.stop_speaking().await.is_ok(),
            "stop on an unbuilt speaker is a no-op, not an error"
        );
    }

    /// The engine is cheap to clone and shares state: clones must observe the
    /// same (unbuilt) speaker, so cloning for the mic handler + reply hook is
    /// sound and still hasn't opened audio.
    #[tokio::test]
    async fn clone_shares_state_without_building() {
        let engine = engine();
        let clone = engine.clone();
        assert!(!clone.is_playing().await);
        assert!(clone.stop_speaking().await.is_ok());
    }

    // The `into_speakable_sentences` chunking tests moved with the function to
    // the shared `adele-voice-client-common` crate (desktop-assistant#274).
}
