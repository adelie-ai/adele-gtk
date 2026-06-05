//! Client for the standalone voice daemon (`org.desktopAssistant.Voice`).
//!
//! The voice daemon (`adelie-ai/voice`) is a **separate** D-Bus service from
//! the orchestrator the rest of this app talks to: it owns the bus name
//! `org.desktopAssistant.Voice` (distinct from `org.desktopAssistant`) and
//! speaks its own typed interface. Voice is just another client of it, so this
//! module talks to it directly over zbus rather than through the
//! `TransportClient` command channel.
//!
//! The wiring mirrors `theme.rs`: the zbus [`Connection`] and the
//! `StateChanged` signal stream live on the shared Tokio runtime (where zbus
//! has a reactor), and only plain values cross back to the GTK main thread via
//! a `tokio::sync::mpsc` channel consumed by `glib::spawn_future_local`. The
//! controls then mutate non-`Send` GTK widgets on the main thread.
//!
//! ## Graceful degradation
//!
//! When the daemon isn't running, the bus name has no owner. Each RPC then
//! fails (zbus returns `ServiceUnknown`/`NameHasNoOwner`); callers treat that
//! as "voice unavailable" and disable the controls rather than surfacing an
//! error. [`VoiceController::connect`] probes ownership once up front so the UI
//! can start out disabled, and the live `StateChanged` listener keeps it in
//! sync if the daemon comes and goes (via `NameOwnerChanged`).

use std::pin::pin;

use tokio::sync::mpsc;
use zbus::export::futures_core::Stream;

use crate::async_bridge::spawn_on_runtime;

/// The voice pipeline state, mirrored from the daemon's `StateChanged` signal
/// and `GetState` reply.
///
/// The daemon sends the state as a string (`"Idle"`, `"Listening"`,
/// `"Processing"`, `"Speaking"`); [`VoiceState::from_dbus`] parses it. An
/// unrecognised value maps to [`VoiceState::Idle`] (the safe resting state)
/// rather than failing the whole listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VoiceState {
    /// Resting; always-on wake-word detection only (or fully disabled).
    #[default]
    Idle,
    /// Actively recording the user's speech.
    Listening,
    /// Transcribing / awaiting the assistant's response.
    Processing,
    /// Playing back the assistant's spoken response.
    Speaking,
}

impl VoiceState {
    /// Parse the daemon's state string. Unknown values fall back to `Idle`.
    pub fn from_dbus(s: &str) -> Self {
        match s {
            "Listening" => Self::Listening,
            "Processing" => Self::Processing,
            "Speaking" => Self::Speaking,
            // "Idle" and anything unrecognised resolve to the resting state.
            _ => Self::Idle,
        }
    }

    /// Short human-readable label for the status line / tooltip.
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "Idle",
            Self::Listening => "Listening…",
            Self::Processing => "Processing…",
            Self::Speaking => "Speaking…",
        }
    }
}

/// A TTS voice enumerated by the daemon's `ListVoices`.
///
/// Mirrors the D-Bus `(sssu)` tuple: id, display name, language, speaker count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceInfo {
    pub id: String,
    pub display_name: String,
    pub language: String,
    /// Number of distinct speakers the voice model offers (1 for a
    /// single-speaker voice; `>1` for a multi-speaker model).
    pub num_speakers: u32,
}

/// The currently active voice as reported by `GetVoice`: voice id plus a
/// speaker index (`-1` when unset / single-speaker).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentVoice {
    pub id: String,
    pub speaker: i32,
}

/// Where a mic-button push-to-talk turn should be routed.
///
/// This is the pure decision behind the mic button: when the user has a
/// conversation open, the spoken prompt and reply must land in *that*
/// conversation (`PushToTalkInConversation(<id>)`); with nothing open we fall
/// back to the daemon's own session (`PushToTalk()`). Kept as a standalone
/// value so the routing can be unit-tested without a live bus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PttRoute {
    /// Dictate into this orchestrator conversation id.
    InConversation(String),
    /// No conversation open — use the daemon's own session.
    DaemonSession,
}

impl PttRoute {
    /// Decide the route from the active conversation id (the window's
    /// `current_conversation_id`). A `Some` with a **non-empty** id routes into
    /// that conversation; `None` *or* an empty/whitespace id falls back to the
    /// daemon session (an empty id would otherwise mean "daemon session" to the
    /// daemon anyway, so we normalise to the explicit `PushToTalk()`).
    pub fn for_conversation(active_conversation: Option<&str>) -> Self {
        match active_conversation {
            Some(id) if !id.trim().is_empty() => Self::InConversation(id.to_string()),
            _ => Self::DaemonSession,
        }
    }
}

/// Typed zbus proxy for the voice daemon.
///
/// zbus derives each D-Bus method name by PascalCasing the Rust fn name, which
/// matches the daemon's own interface (`get_state` → `GetState`,
/// `push_to_talk` → `PushToTalk`, …), so no per-method `#[zbus(name = …)]`
/// overrides are needed. Only the methods the GTK UI drives are declared.
#[zbus::proxy(
    interface = "org.desktopAssistant.Voice",
    default_service = "org.desktopAssistant.Voice",
    default_path = "/org/desktopAssistant/Voice"
)]
pub trait Voice {
    /// Current pipeline state ("Idle" | "Listening" | "Processing" | "Speaking").
    fn get_state(&self) -> zbus::Result<String>;

    /// Enable/disable always-on "Hey Adele" wake-word detection.
    fn set_enabled(&self, enabled: bool) -> zbus::Result<()>;

    /// Whether always-on wake-word detection is enabled.
    fn get_enabled(&self) -> zbus::Result<bool>;

    /// Start listening immediately (push-to-talk; works even with wake off).
    /// The spoken turn lands in the daemon's own session.
    fn push_to_talk(&self) -> zbus::Result<()>;

    /// Push-to-talk routed into a specific orchestrator conversation, so the
    /// spoken prompt and reply appear in the conversation the user is viewing.
    /// An empty `conversation_id` falls back to the daemon's own session,
    /// matching [`VoiceProxy::push_to_talk`].
    fn push_to_talk_in_conversation(&self, conversation_id: &str) -> zbus::Result<()>;

    /// Stop any in-progress TTS playback (barge-in).
    fn stop_speaking(&self) -> zbus::Result<()>;

    /// List installed voices as (id, display name, language, num_speakers).
    fn list_voices(&self) -> zbus::Result<Vec<(String, String, String, u32)>>;

    /// Current voice as (id, speaker_id); speaker_id is -1 if unset.
    fn get_voice(&self) -> zbus::Result<(String, i32)>;

    /// Set the active voice (speaker -1 = default/single-speaker).
    fn set_voice(&self, voice_id: &str, speaker: i32) -> zbus::Result<()>;

    /// Emitted when the pipeline state changes.
    #[zbus(signal)]
    fn state_changed(&self, state: &str) -> zbus::Result<()>;
}

/// Handle to the voice daemon, usable from the GTK main thread.
///
/// Cheap to clone (just an `Arc`-backed zbus proxy). Each method spawns its
/// own work on the Tokio runtime; results are delivered back to the caller's
/// supplied channel so the GTK widgets are only ever touched on the main
/// thread. A `None` proxy (connect failed entirely — e.g. no session bus) makes
/// every call a graceful no-op / "unavailable".
#[derive(Clone)]
pub struct VoiceController {
    /// `None` when even establishing the session-bus connection failed; the
    /// controller is then inert and reports the service as unavailable.
    proxy: Option<VoiceProxy<'static>>,
}

impl VoiceController {
    /// Connect to the session bus and build the voice proxy. Returns a
    /// controller whose proxy is `None` only when the bus connection itself
    /// fails (rare — no session bus at all); a missing *daemon* still yields a
    /// live proxy, with availability probed separately via
    /// [`VoiceController::is_available`].
    pub async fn connect() -> Self {
        let proxy = match zbus::Connection::session().await {
            Ok(conn) => match VoiceProxy::new(&conn).await {
                Ok(proxy) => Some(proxy),
                Err(error) => {
                    tracing::warn!(%error, "failed to build voice proxy; voice controls disabled");
                    None
                }
            },
            Err(error) => {
                tracing::warn!(%error, "no session bus for voice; voice controls disabled");
                None
            }
        };
        Self { proxy }
    }

    /// An inert controller with no proxy. Every call is a graceful no-op and
    /// [`VoiceController::is_available`] reports `false`. Used as a stand-in
    /// when a consumer needs a controller before the real one has connected.
    pub fn unavailable() -> Self {
        Self { proxy: None }
    }

    /// Whether the voice daemon currently owns its bus name.
    ///
    /// Used at startup to decide the controls' initial sensitivity. A `false`
    /// here is normal (daemon not running / models unprovisioned) and must not
    /// be treated as an error.
    pub async fn is_available(&self) -> bool {
        let Some(proxy) = &self.proxy else {
            return false;
        };
        // A cheap round-trip that only succeeds when the name has an owner.
        proxy.get_state().await.is_ok()
    }

    /// Fire a push-to-talk request. Errors (including "daemon absent") are
    /// logged, not surfaced — the caller has already gated on availability.
    pub async fn push_to_talk(&self) -> Result<(), String> {
        let Some(proxy) = &self.proxy else {
            return Err("voice service unavailable".to_string());
        };
        proxy.push_to_talk().await.map_err(|e| e.to_string())
    }

    /// Fire a push-to-talk request routed into a specific conversation, so the
    /// spoken prompt and reply land in the conversation the user is viewing
    /// (mirrors voice#24). An empty `conversation_id` is equivalent to
    /// [`VoiceController::push_to_talk`] (the daemon's own session).
    pub async fn push_to_talk_in_conversation(&self, conversation_id: &str) -> Result<(), String> {
        let Some(proxy) = &self.proxy else {
            return Err("voice service unavailable".to_string());
        };
        proxy
            .push_to_talk_in_conversation(conversation_id)
            .await
            .map_err(|e| e.to_string())
    }

    /// Dispatch a push-to-talk turn according to [`PttRoute::for_conversation`]:
    /// into the active conversation when one is open, else the daemon session.
    ///
    /// This is the single entry point the mic button uses; the routing decision
    /// itself is the pure [`PttRoute`] so it can be unit-tested without a bus.
    pub async fn push_to_talk_routed(
        &self,
        active_conversation: Option<&str>,
    ) -> Result<(), String> {
        match PttRoute::for_conversation(active_conversation) {
            PttRoute::InConversation(id) => self.push_to_talk_in_conversation(&id).await,
            PttRoute::DaemonSession => self.push_to_talk().await,
        }
    }

    /// Stop any in-progress TTS playback (barge-in before re-listening).
    pub async fn stop_speaking(&self) -> Result<(), String> {
        let Some(proxy) = &self.proxy else {
            return Err("voice service unavailable".to_string());
        };
        proxy.stop_speaking().await.map_err(|e| e.to_string())
    }

    /// Read the wake-word enabled flag. `None` when the service is unavailable.
    pub async fn get_enabled(&self) -> Option<bool> {
        let proxy = self.proxy.as_ref()?;
        match proxy.get_enabled().await {
            Ok(enabled) => Some(enabled),
            Err(error) => {
                tracing::debug!(%error, "voice get_enabled failed (service likely absent)");
                None
            }
        }
    }

    /// Set the wake-word enabled flag.
    pub async fn set_enabled(&self, enabled: bool) -> Result<(), String> {
        let Some(proxy) = &self.proxy else {
            return Err("voice service unavailable".to_string());
        };
        proxy.set_enabled(enabled).await.map_err(|e| e.to_string())
    }

    /// List installed voices. `None` when the service is unavailable.
    pub async fn list_voices(&self) -> Option<Vec<VoiceInfo>> {
        let proxy = self.proxy.as_ref()?;
        match proxy.list_voices().await {
            Ok(raw) => Some(
                raw.into_iter()
                    .map(|(id, display_name, language, num_speakers)| VoiceInfo {
                        id,
                        display_name,
                        language,
                        num_speakers,
                    })
                    .collect(),
            ),
            Err(error) => {
                tracing::debug!(%error, "voice list_voices failed (service likely absent)");
                None
            }
        }
    }

    /// Read the active voice. `None` when the service is unavailable.
    pub async fn get_voice(&self) -> Option<CurrentVoice> {
        let proxy = self.proxy.as_ref()?;
        match proxy.get_voice().await {
            Ok((id, speaker)) => Some(CurrentVoice { id, speaker }),
            Err(error) => {
                tracing::debug!(%error, "voice get_voice failed (service likely absent)");
                None
            }
        }
    }

    /// Set the active voice (speaker `-1` = default/single-speaker).
    pub async fn set_voice(&self, voice_id: String, speaker: i32) -> Result<(), String> {
        let Some(proxy) = &self.proxy else {
            return Err("voice service unavailable".to_string());
        };
        proxy
            .set_voice(&voice_id, speaker)
            .await
            .map_err(|e| e.to_string())
    }

    /// Subscribe to pipeline-state changes, forwarding each into `tx` as a
    /// [`VoiceState`] on the GTK main thread.
    ///
    /// Spawns a Tokio task that pushes the current state once (so the UI starts
    /// correct) and then every `StateChanged` signal. The task ends when `tx`
    /// is dropped (window gone) or the proxy is absent. Cheap to call once at
    /// window construction.
    pub fn spawn_state_listener(&self, tx: mpsc::UnboundedSender<VoiceState>) {
        let Some(proxy) = self.proxy.clone() else {
            return;
        };
        spawn_on_runtime(async move {
            run_state_listener(proxy, tx).await;
        });
    }
}

/// Push the initial state, then forward every `StateChanged` until `tx`
/// closes. Errors (daemon absent / bus drop) end the loop quietly: the controls
/// stay at their last state and the availability probe handles the rest.
async fn run_state_listener(proxy: VoiceProxy<'static>, tx: mpsc::UnboundedSender<VoiceState>) {
    // Seed with the current state if the daemon is up; ignore failure (absent).
    if let Ok(state) = proxy.get_state().await
        && tx.send(VoiceState::from_dbus(&state)).is_err()
    {
        return;
    }

    let changes = match proxy.receive_state_changed().await {
        Ok(changes) => changes,
        Err(error) => {
            tracing::debug!(%error, "voice state_changed subscribe failed (service likely absent)");
            return;
        }
    };
    let mut changes = pin!(changes);
    while let Some(change) =
        std::future::poll_fn(|cx| Stream::poll_next(changes.as_mut(), cx)).await
    {
        let Ok(args) = change.args() else { continue };
        if tx.send(VoiceState::from_dbus(args.state)).is_err() {
            // Receiver gone (window closed) — stop listening.
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_states() {
        assert_eq!(VoiceState::from_dbus("Idle"), VoiceState::Idle);
        assert_eq!(VoiceState::from_dbus("Listening"), VoiceState::Listening);
        assert_eq!(VoiceState::from_dbus("Processing"), VoiceState::Processing);
        assert_eq!(VoiceState::from_dbus("Speaking"), VoiceState::Speaking);
    }

    #[test]
    fn unknown_state_falls_back_to_idle() {
        // A future/garbled state must not break the listener — rest at Idle.
        assert_eq!(VoiceState::from_dbus("Dreaming"), VoiceState::Idle);
        assert_eq!(VoiceState::from_dbus(""), VoiceState::Idle);
    }

    #[test]
    fn default_state_is_idle() {
        assert_eq!(VoiceState::default(), VoiceState::Idle);
    }

    #[test]
    fn every_state_has_a_label() {
        // Labels are user-facing; make sure none is empty and Listening/
        // Processing/Speaking read as in-progress.
        assert_eq!(VoiceState::Idle.label(), "Idle");
        assert!(VoiceState::Listening.label().ends_with('…'));
        assert!(VoiceState::Processing.label().ends_with('…'));
        assert!(VoiceState::Speaking.label().ends_with('…'));
    }

    #[test]
    fn ptt_routes_into_the_active_conversation() {
        // A live conversation id → PushToTalkInConversation(id).
        assert_eq!(
            PttRoute::for_conversation(Some("conv-123")),
            PttRoute::InConversation("conv-123".to_string())
        );
    }

    #[test]
    fn ptt_falls_back_to_daemon_session_with_no_conversation() {
        // Nothing open → plain PushToTalk (daemon's own session).
        assert_eq!(PttRoute::for_conversation(None), PttRoute::DaemonSession);
    }

    #[test]
    fn ptt_treats_empty_conversation_id_as_no_conversation() {
        // An empty/whitespace id must not be sent as a "real" conversation; it
        // normalises to the daemon session (which is also how the daemon reads
        // an empty id), so the mic button issues the explicit PushToTalk().
        assert_eq!(
            PttRoute::for_conversation(Some("")),
            PttRoute::DaemonSession
        );
        assert_eq!(
            PttRoute::for_conversation(Some("   ")),
            PttRoute::DaemonSession
        );
    }
}
