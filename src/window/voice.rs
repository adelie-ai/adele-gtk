//! Voice wiring for the window: the daemon mic button, the embedded mic, and
//! the shared daemon-first speak path. Split out of the window module so the
//! root file stays focused on layout + the UI-message executor.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::glib;
use gtk4::prelude::*;
use tokio::sync::mpsc;

use crate::async_bridge::{AsyncBridge, UiMessage};
use crate::voice_client::{VoiceController, VoiceState};
use crate::widgets::input_bar::InputBar;

use super::WindowState;

/// Connect to the voice daemon and wire the input bar's mic button + state
/// reflection (issues #59, #63).
///
/// Connecting is async (session bus + proxy build), so it runs on the Tokio
/// runtime; the resulting [`VoiceController`] is delivered back to the GTK main
/// thread, stored in `voice` (shared with the Settings → Voice tab), and used
/// to:
/// - show the mic button only when the daemon owns its bus name
///   (graceful degradation when it's absent), and
/// - keep the button's state in sync with the daemon's `StateChanged` signal.
///
/// Clicking the mic button dictates **into the active conversation**: it reads
/// the window's `current_conversation_id` and calls
/// `PushToTalkInConversation(<id>)` so the spoken prompt and reply land in the
/// conversation the user is viewing (mirrors voice#24); with no conversation
/// open it falls back to plain `PushToTalk()` (the daemon's own session). If a
/// reply is currently playing (`Speaking`), the click barges in with
/// `StopSpeaking()` instead — matching the plasmoid.
pub(super) fn wire_voice_controls(
    voice: &Rc<RefCell<Option<VoiceController>>>,
    input_bar: &Rc<InputBar>,
    bridge: &Rc<AsyncBridge>,
    state: &Rc<RefCell<WindowState>>,
) {
    // Mic button click. The controller may not be connected yet; a click
    // before then is a harmless no-op (the button is hidden until the daemon is
    // confirmed present anyway).
    input_bar.mic_button.connect_clicked(glib::clone!(
        #[strong]
        voice,
        #[strong]
        bridge,
        #[strong]
        state,
        #[strong]
        input_bar,
        move |_| {
            let Some(controller) = voice.borrow().clone() else {
                return;
            };
            let ui_tx = bridge.ui_sender();

            // Barge-in: while a reply is playing, the click stops it rather than
            // starting a new turn (mirrors the plasmoid's mic button).
            if matches!(input_bar.current_state(), VoiceState::Speaking) {
                crate::async_bridge::spawn_on_runtime(async move {
                    if let Err(e) = controller.stop_speaking().await {
                        let _ = ui_tx.send(UiMessage::Error(format!("Voice: {e}")));
                    }
                });
                return;
            }

            // Otherwise start a dictation turn routed into the conversation the
            // user is viewing (or the daemon's own session when none is open).
            let active = state.borrow().current_conversation_id.clone();
            crate::async_bridge::spawn_on_runtime(async move {
                if let Err(e) = controller.push_to_talk_routed(active.as_deref()).await {
                    let _ = ui_tx.send(UiMessage::Error(format!("Voice: {e}")));
                }
            });
        }
    ));

    // Connect + probe + subscribe. The controller and the initial availability
    // are delivered to the main thread; the state listener then streams
    // `VoiceState` updates over its own channel.
    let (ready_tx, mut ready_rx) = mpsc::unbounded_channel::<(VoiceController, bool)>();
    let (state_tx, mut state_rx) = mpsc::unbounded_channel::<VoiceState>();
    crate::async_bridge::spawn_on_runtime(async move {
        let controller = VoiceController::connect().await;
        let available = controller.is_available().await;
        // Subscribe to state changes regardless of the initial probe: the
        // daemon may be activated on demand after we connect.
        controller.spawn_state_listener(state_tx);
        let _ = ready_tx.send((controller, available));
    });

    // Apply the connected controller + initial availability on the main thread.
    glib::spawn_future_local(glib::clone!(
        #[strong]
        voice,
        #[strong]
        input_bar,
        async move {
            if let Some((controller, available)) = ready_rx.recv().await {
                *voice.borrow_mut() = Some(controller);
                input_bar.set_voice_available(available);
            }
        }
    ));

    // Reflect every pipeline-state change on the mic button. A non-Idle state
    // also implies the daemon is present, so reveal the button if a state
    // arrives before (or instead of) the initial availability probe.
    glib::spawn_future_local(glib::clone!(
        #[strong]
        input_bar,
        async move {
            while let Some(state) = state_rx.recv().await {
                input_bar.set_voice_available(true);
                input_bar.reflect_voice_state(state);
            }
        }
    ));
}

/// Wire the mic button to the **embedded** in-process voice engine (issue #65).
///
/// This is the no-daemon path: a click runs [`EmbeddedVoice::dictate`] locally
/// (mic → Silero VAD endpoint → Whisper), drops the transcript into the input
/// box, and fires the same `send_action` a typed message uses — so the spoken
/// prompt lands in the active conversation through the app's normal assistant
/// path. The reply is narrated only if the conversation's `AdeleOutput` gate
/// holds (#80): dictation itself no longer forces narration (GTK-3).
///
/// A click **while a reply is playing barges in** (stops playback) instead of
/// starting a new turn, mirroring the daemon mic button. The button reflects
/// `Listening` for the duration of the capture, then returns to `Idle`.
///
/// All voice work runs on the Tokio runtime; only the transcript crosses back
/// to the GTK thread (via a oneshot) to touch widgets and call `send_action`.
pub(super) fn wire_embedded_mic(
    engine: crate::voice_embedded::EmbeddedVoice,
    input_bar: &Rc<InputBar>,
    send_action: &Rc<impl Fn() + 'static>,
    state: &Rc<RefCell<WindowState>>,
    bridge: &Rc<AsyncBridge>,
) {
    // Guards against a second click starting an overlapping dictation while one
    // is already in flight (the mic stream + Whisper are single-shot per turn).
    let dictating = Rc::new(Cell::new(false));

    input_bar.mic_button.connect_clicked(glib::clone!(
        #[strong]
        engine,
        #[strong]
        input_bar,
        #[strong]
        send_action,
        #[strong]
        state,
        #[strong]
        bridge,
        #[strong]
        dictating,
        move |_| {
            if dictating.get() {
                return; // a capture is already running
            }

            // Require an open conversation before dictating, so the spoken
            // prompt has somewhere to go (matches the typed-send guard).
            if state.borrow().current_conversation_id.is_none() {
                return;
            }

            let ui_tx = bridge.ui_sender();

            // Barge-in: if a reply is currently playing, the click stops it
            // rather than starting a new turn. `is_playing` is async (the engine
            // lives on the runtime), so probe there; if not playing, dictate.
            let (decision_tx, decision_rx) = mpsc::unbounded_channel::<bool>();
            crate::async_bridge::spawn_on_runtime(glib::clone!(
                #[strong]
                engine,
                async move {
                    if engine.is_playing().await {
                        if let Err(e) = engine.stop_speaking().await {
                            let _ = ui_tx.send(UiMessage::Error(format!("Voice: {e}")));
                        }
                        let _ = decision_tx.send(false); // barged in; don't dictate
                    } else {
                        let _ = decision_tx.send(true); // proceed to dictate
                    }
                }
            ));

            // Back on the GTK thread: if we should dictate, run the capture and
            // feed the transcript into the send path.
            glib::spawn_future_local(glib::clone!(
                #[strong]
                engine,
                #[strong]
                input_bar,
                #[strong]
                send_action,
                #[strong]
                dictating,
                #[strong]
                bridge,
                async move {
                    let mut decision_rx = decision_rx;
                    let Some(true) = decision_rx.recv().await else {
                        return; // barged in (or channel dropped) — no capture
                    };

                    dictating.set(true);
                    input_bar.reflect_voice_state(VoiceState::Listening);

                    // Run the (blocking-ish) capture on the runtime; the
                    // transcript comes back over a oneshot.
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    let ui_tx = bridge.ui_sender();
                    crate::async_bridge::spawn_on_runtime(glib::clone!(
                        #[strong]
                        engine,
                        async move {
                            let result = engine.dictate().await;
                            let _ = tx.send(result);
                        }
                    ));

                    let result = rx.await;
                    dictating.set(false);
                    input_bar.reflect_voice_state(VoiceState::Idle);

                    match result {
                        Ok(Ok(Some(text))) => {
                            // Drop the transcript into the input box and send it
                            // like a typed message. Reply narration is decided by
                            // the conversation's `AdeleOutput` gate (#80, GTK-3),
                            // not by the fact that this turn was dictated.
                            input_bar.set_text(&text);
                            send_action();
                        }
                        // No speech / near-silent capture — nothing to send.
                        Ok(Ok(None)) => {}
                        Ok(Err(e)) => {
                            let _ = ui_tx.send(UiMessage::Error(format!("Voice: {e}")));
                        }
                        // The capture task was dropped before replying.
                        Err(_) => {}
                    }
                }
            ));
        }
    ));
}

/// Speak `text` aloud, daemon-first and chunked (issue #80).
///
/// Single entry point shared by every spoken-output site (reply narration and
/// `say_this` asides) so routing + chunking live in one place:
///
/// 1. **Chunk.** `text` is split into one-short-sentence-per-call pieces via
///    [`into_speakable_sentences`]. Both backends' synth is one-shot with a
///    ~20s per-synth timeout, so feeding a long reply whole would blow it — the
///    live bug this fixes.
/// 2. **Route, daemon-first.** When a connected voice daemon is available, each
///    sentence goes to its warm `SayText`; otherwise, if the embedded engine is
///    present, to `EmbeddedVoice::say`; otherwise nothing is spoken. The backend
///    is chosen **once** for the whole utterance (not per sentence) so playback
///    never splits across engines.
/// 3. **Order.** Sentences are awaited **sequentially**, so the daemon/embedded
///    sink receives — and plays — them in order; they are never fired unordered.
///
/// Errors are reported once (the first failing sentence) via `ui_tx` and the
/// rest of the utterance is abandoned, matching the prior single-shot behaviour.
pub(super) async fn speak_text(
    voice: Option<VoiceController>,
    embedded: Option<crate::voice_embedded::EmbeddedVoice>,
    ui_tx: mpsc::UnboundedSender<UiMessage>,
    text: String,
) {
    let sentences = crate::voice_embedded::into_speakable_sentences(&text);
    if sentences.is_empty() {
        return;
    }

    // Choose the backend once for the whole utterance: a daemon that has
    // actually connected wins (warm models), else the in-process engine. Probing
    // availability also avoids handing sentences to a daemon that vanished.
    let daemon = match voice {
        Some(controller) if controller.is_available().await => Some(controller),
        _ => None,
    };

    for sentence in sentences {
        let result = if let Some(controller) = &daemon {
            controller.say(sentence).await
        } else if let Some(engine) = &embedded {
            engine.say(&sentence).await
        } else {
            // Neither backend present (daemon absent + no embedded engine):
            // nothing to speak, and nothing more will become available mid-loop.
            return;
        };
        if let Err(e) = result {
            let _ = ui_tx.send(UiMessage::Error(format!("Voice: {e}")));
            return;
        }
    }
}
