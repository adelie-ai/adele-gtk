use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use desktop_assistant_api_model as api;
use desktop_assistant_client_common::{
    AssistantClient, ChatMessage, ConnectionConfig, Connector, ConversationDetail,
    ConversationSummary,
};
use gtk4::prelude::*;
use gtk4::{
    Align, Application, ApplicationWindow, Box as GtkBox, Button, CheckButton, Entry, Label,
    MenuButton, Orientation, Paned, Popover, Revealer, RevealerTransitionType, Separator, Window,
    gdk, glib,
};
use tokio::sync::mpsc;

use crate::async_bridge::{AsyncBridge, UiMessage, connection_manager};
use crate::management_client;
use crate::voice_client::{VoiceController, VoiceState};
use crate::widgets::chat_view::ChatView;
use crate::widgets::conversation_side_pane::{ConversationSidePane, SidePaneAction};
use crate::widgets::input_bar::InputBar;
use crate::widgets::model_picker::ModelPicker;
use crate::widgets::sidebar::Sidebar;
use crate::widgets::tasks_panel::TasksPanel;

/// Shared mutable state for the window.
#[derive(Default)]
struct WindowState {
    conversations: Vec<ConversationSummary>,
    current_conversation_id: Option<String>,
    current_conversation: Option<ConversationDetail>,
    pending_request_id: Option<String>,
    streaming_buffer: String,
    debug_enabled: bool,
    /// Per-conversation "read aloud" (accessibility) toggle (issue #76, reframed
    /// in #78), keyed by conversation id. Default (absent key) is **OFF**. When
    /// ON, every reply auto-routes to the Speaker (LLM-unaware). State is
    /// per-conversation, so enabling it in one conversation never leaks audio
    /// into another.
    conversation_speech_enabled: HashMap<String, bool>,
    /// Per-conversation soft-sticky "voice mode" toggle (issue #78), keyed by
    /// conversation id. Default (absent key) is **OFF**. Entered by the user
    /// (UI) or the model (`request_voice`), left via UI or `stop_voice`. When
    /// ON: replies are narrated (same routing as read-aloud) AND a concise
    /// read-aloud `system_refinement` is attached on send. Independent of
    /// read-aloud — either being ON narrates.
    conversation_voice_mode: HashMap<String, bool>,
}

/// Concise read-aloud system refinement attached on send while voice mode is ON
/// (issue #78). Mirrors the *essence* of the voice daemon's
/// `spoken_response_hint` (brief, conversational, no markdown, symbols/acronyms
/// spelled out, written for the ear) without duplicating its full paragraph.
/// Deliberately free of markdown markers so it can't itself leak formatting.
const VOICE_SYSTEM_REFINEMENT: &str = "This reply will be read aloud, so write it to be spoken, not read. Keep it brief and \
     conversational, a few short sentences at most. Use no markdown or formatting of any kind, \
     and no emoji. Spell out acronyms and abbreviations as full words and avoid symbols that do \
     not read well aloud (say 'and' not an ampersand, 'percent' not a percent sign, 'dollars' not \
     a dollar sign). Do not read out URLs, file paths, or email addresses; describe them in words \
     instead, and write numbers, dates, and times the way you would say them.";

impl WindowState {
    /// Whether read-aloud is enabled for the *currently active* conversation.
    /// `false` when there is no active conversation or the toggle was never set
    /// (default OFF).
    fn speech_enabled_for_current(&self) -> bool {
        self.current_conversation_id
            .as_deref()
            .and_then(|id| self.conversation_speech_enabled.get(id))
            .copied()
            .unwrap_or(false)
    }

    /// Whether voice mode is on for the *currently active* conversation.
    /// `false` when there is no active conversation or it was never set
    /// (default OFF).
    fn voice_mode_for_current(&self) -> bool {
        self.current_conversation_id
            .as_deref()
            .and_then(|id| self.conversation_voice_mode.get(id))
            .copied()
            .unwrap_or(false)
    }

    /// Whether a reply is spoken for the active conversation: read-aloud OR
    /// voice mode (issue #78). The single audio gate both reply narration and
    /// `say_this` consult.
    fn narrate_for_current(&self) -> bool {
        self.speech_enabled_for_current() || self.voice_mode_for_current()
    }
}

/// The read-aloud system refinement to attach on the next send, or `None` when
/// voice mode is off for the active conversation (issue #78). Pure decision the
/// send path consults to choose `send_prompt_with_system_refinement`. Free
/// function (not a method) so the send closure can call it through a snapshot
/// without holding a `WindowState` borrow across the await.
fn refinement_for_send(state: &WindowState) -> Option<&'static str> {
    state
        .voice_mode_for_current()
        .then_some(VOICE_SYSTEM_REFINEMENT)
}

/// The session-scoped client tools this client advertises so the model can
/// enter/leave spoken voice mode (issue #78). Both take no arguments. Registered
/// on connect; the daemon replaces the prior set on each call, so this is the
/// full list, not a delta. (Phase-1's `say_this` is handled defensively without
/// registration — the daemon forwards it regardless — so it is intentionally
/// not advertised here.)
fn voice_mode_client_tools() -> Vec<api::ClientToolRegistration> {
    let no_args = serde_json::json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false,
    });
    vec![
        api::ClientToolRegistration {
            name: "request_voice".to_string(),
            description: "Switch this conversation into spoken voice mode (the user asked to talk \
                 by voice); replies will be read aloud and kept conversational."
                .to_string(),
            input_schema: no_args.clone(),
        },
        api::ClientToolRegistration {
            name: "stop_voice".to_string(),
            description: "Leave voice mode; go back to text-only.".to_string(),
            input_schema: no_args,
        },
    ]
}

/// Extract the `text` argument from a `say_this` client-tool call (issue #76).
///
/// Returns `None` (rather than panicking) when `arguments` is not an object,
/// the `text` field is absent, or it isn't a string — a hostile or buggy
/// payload must resolve to an `Err` result, never crash the turn. An empty
/// string is accepted (the LLM asked to say nothing; the result still resolves).
fn say_this_text(arguments: &serde_json::Value) -> Option<String> {
    arguments
        .get("text")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// A single observable side-effect produced by [`WindowState::apply`].
///
/// `apply` is a pure decision function: it mutates `WindowState` and returns
/// the list of effects to perform, but performs none of them itself (no GTK,
/// no widget refs, no spawns). The thin executor in [`handle_ui_message`]
/// walks the returned `Vec<Effect>` in order and performs each against the
/// real widgets — mirroring the `TasksModel`/`apply` shape already used by
/// `widgets/tasks_panel.rs`. This keeps the entire state-machine decision
/// logic unit-testable without a live GTK context.
///
/// Effects are emitted in the exact order the legacy `handle_ui_message`
/// performed them, so the observable behavior is identical.
enum Effect {
    /// Stash the freshly connected connector in the window's client cell.
    SetClient(Arc<Connector>),
    /// Clear the client cell (on disconnect).
    ClearClient,
    /// Set the bottom status-bar text verbatim.
    SetStatusText(String),
    /// Enable/disable the send button.
    SetSendSensitive(bool),
    /// Repaint the sidebar conversation list.
    SetConversations(Vec<ConversationSummary>),
    /// Run `ensure_active_conversation` (selection sync + auto-load/-create).
    /// Kept as an effect because it needs the live client + ui_tx and spawns
    /// async RPCs; the *decision* to run it lives in `apply`.
    EnsureActiveConversation,
    /// Load an (already debug-filtered) conversation into the chat view.
    LoadConversationIntoChat(ConversationDetail),
    /// Re-fetch a conversation that is already open, to refresh the cached
    /// detail + chat after a reconnect (or a debug/personality refresh) WITHOUT
    /// resetting the model picker. The reply arrives as
    /// `UiMessage::ConversationReloaded`. Unlike a conversation *switch* (which
    /// flows through `ConversationLoaded` and re-applies the picker selection),
    /// a reload must never clobber the user's pick — see issue #72.
    ReloadConversation(String),
    /// Clear the chat view.
    ClearChat,
    /// Set the chat's transient status line.
    SetChatStatus(String),
    /// Clear the chat's transient status line.
    ClearChatStatus,
    /// Append a streaming chunk to the chat view.
    ReceiveChunk(String),
    /// Finalize a streaming response in the chat view.
    CompleteStreaming(String),
    /// Apply (or clear, with `None`) the model-picker selection.
    SetModelSelection(Option<api::ConversationModelSelectionView>),
    /// Replace the model-picker's available models.
    SetModels(Vec<api::ModelListing>),
    /// Set the picker's resolved interactive-purpose default (issue #53). Used
    /// as the fallback selection for conversations with no stored selection so
    /// the button shows a concrete model instead of "(default)".
    SetDefaultModel(Option<crate::selected_models::SelectedModel>),
    /// Show/hide the model picker.
    SetModelPickerVisible(bool),
    /// Reveal a passive toast with the given message.
    ShowToast(String),
    /// Replace the entire background-task list.
    TasksReplaceAll(Vec<api::TaskView>),
    /// A task started.
    TaskStarted(api::TaskView),
    /// A task progress update.
    TaskProgress {
        id: String,
        progress_hint: Option<String>,
    },
    /// A task log line was appended.
    TaskLogAppended {
        id: String,
        entry: api::TaskLogEntry,
    },
    /// A task completed (terminal).
    TaskCompleted { id: String },

    // --- Conversation side pane (issue #60) -------------------------------
    /// Fetch the scratchpad for the given conversation (async RPC + ui_tx),
    /// mirroring `EnsureActiveConversation`. The reply arrives as
    /// `UiMessage::ConversationScratchpadLoaded`.
    FetchScratchpad(String),
    /// Replace the side pane's scratchpad notes (empty clears it).
    SidePaneSetScratchpad(Vec<api::ScratchpadNoteView>),
    /// Recompute the side pane's task list from the authoritative `TasksModel`,
    /// filtered to the active conversation.
    RefreshSidePaneTasks,

    // --- Speech toggle + client tools (issue #76) -------------------------
    /// Speak `text` through the embedded `Speaker`. Emitted only when the
    /// active conversation's speech toggle is ON (the executor still no-ops if
    /// there is no embedded engine, e.g. the daemon path). The master audio
    /// cut-off lives in `apply`: when speech is OFF this effect is never
    /// produced, so no path plays audio while the toggle is off.
    Speak(String),
    /// Render an inline note in the chat transcript (issue #76). Used for the
    /// `(speech mode disabled) …` downgrade when `say_this` arrives with speech
    /// OFF, so the text is shown rather than dropped.
    AddInlineNote(String),
    /// Reflect the active conversation's voice-mode state on the input-bar
    /// toggle (issue #78). Emitted when the model drives voice mode via
    /// `request_voice` / `stop_voice` so the button tracks the model's change
    /// (the user-driven path needs no echo — the button is its own write
    /// source). Suppressed inside `set_voice_mode_active`, so it never loops.
    SetVoiceModeButton(bool),
    /// Resolve a suspended client-tool call back to the daemon via
    /// `submit_client_tool_result` so the parked turn resumes (issue #76). Every
    /// `ClientToolCall` yields exactly one of these — `Ok` on success, `Err`
    /// with a reason otherwise — which is what kills the silent-drop wedge.
    SubmitClientToolResult {
        task_id: String,
        tool_call_id: String,
        result: Result<String, String>,
    },
}

// Manual `Debug` (can't derive: `Connector` is not `Debug`, mirroring
// `UiMessage`). Only `SetClient` needs special handling — it prints a marker
// instead of the opaque connector; every other variant forwards its fields so
// test panic messages stay informative.
impl std::fmt::Debug for Effect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Effect::SetClient(_) => f.debug_tuple("SetClient").field(&"<connector>").finish(),
            Effect::ClearClient => f.write_str("ClearClient"),
            Effect::SetStatusText(t) => f.debug_tuple("SetStatusText").field(t).finish(),
            Effect::SetSendSensitive(b) => f.debug_tuple("SetSendSensitive").field(b).finish(),
            Effect::SetConversations(c) => f.debug_tuple("SetConversations").field(c).finish(),
            Effect::EnsureActiveConversation => f.write_str("EnsureActiveConversation"),
            Effect::LoadConversationIntoChat(d) => {
                f.debug_tuple("LoadConversationIntoChat").field(d).finish()
            }
            Effect::ReloadConversation(id) => {
                f.debug_tuple("ReloadConversation").field(id).finish()
            }
            Effect::ClearChat => f.write_str("ClearChat"),
            Effect::SetChatStatus(m) => f.debug_tuple("SetChatStatus").field(m).finish(),
            Effect::ClearChatStatus => f.write_str("ClearChatStatus"),
            Effect::ReceiveChunk(c) => f.debug_tuple("ReceiveChunk").field(c).finish(),
            Effect::CompleteStreaming(c) => f.debug_tuple("CompleteStreaming").field(c).finish(),
            Effect::SetModelSelection(s) => f.debug_tuple("SetModelSelection").field(s).finish(),
            Effect::SetModels(m) => f.debug_tuple("SetModels").field(m).finish(),
            Effect::SetDefaultModel(m) => f.debug_tuple("SetDefaultModel").field(m).finish(),
            Effect::SetModelPickerVisible(v) => {
                f.debug_tuple("SetModelPickerVisible").field(v).finish()
            }
            Effect::ShowToast(m) => f.debug_tuple("ShowToast").field(m).finish(),
            Effect::TasksReplaceAll(t) => f.debug_tuple("TasksReplaceAll").field(t).finish(),
            Effect::TaskStarted(t) => f.debug_tuple("TaskStarted").field(t).finish(),
            Effect::TaskProgress { id, progress_hint } => f
                .debug_struct("TaskProgress")
                .field("id", id)
                .field("progress_hint", progress_hint)
                .finish(),
            Effect::TaskLogAppended { id, entry } => f
                .debug_struct("TaskLogAppended")
                .field("id", id)
                .field("entry", entry)
                .finish(),
            Effect::TaskCompleted { id } => {
                f.debug_struct("TaskCompleted").field("id", id).finish()
            }
            Effect::FetchScratchpad(c) => f.debug_tuple("FetchScratchpad").field(c).finish(),
            Effect::SidePaneSetScratchpad(n) => {
                f.debug_tuple("SidePaneSetScratchpad").field(n).finish()
            }
            Effect::RefreshSidePaneTasks => f.write_str("RefreshSidePaneTasks"),
            Effect::Speak(t) => f.debug_tuple("Speak").field(t).finish(),
            Effect::AddInlineNote(t) => f.debug_tuple("AddInlineNote").field(t).finish(),
            Effect::SetVoiceModeButton(b) => f.debug_tuple("SetVoiceModeButton").field(b).finish(),
            Effect::SubmitClientToolResult {
                task_id,
                tool_call_id,
                result,
            } => f
                .debug_struct("SubmitClientToolResult")
                .field("task_id", task_id)
                .field("tool_call_id", tool_call_id)
                .field("result", result)
                .finish(),
        }
    }
}

impl WindowState {
    /// Apply a `UiMessage` to the window state, returning the side-effects to
    /// perform. PURE: mutates `self` and returns effects; performs no GTK
    /// work and holds no widget refs.
    ///
    /// Every `UiMessage` variant is handled here; the executor in
    /// `handle_ui_message` is a mechanical translation of the returned
    /// effects into widget calls.
    fn apply(&mut self, msg: UiMessage) -> Vec<Effect> {
        match msg {
            UiMessage::ClientReady(connector) => {
                // The connection_manager handed us a freshly connected
                // connector. Stash it so the rest of the UI can issue RPCs;
                // this arrives before `ConversationsLoaded`, which relies on
                // the client cell.
                vec![Effect::SetClient(connector)]
            }
            UiMessage::ConversationsLoaded(convs) => {
                self.conversations = convs.clone();
                let mut effects = vec![
                    Effect::SetConversations(convs),
                    Effect::EnsureActiveConversation,
                ];
                // On *reconnect* the window still has an active conversation
                // that is still present. `EnsureActiveConversation` only
                // re-syncs the sidebar selection in that case (it does not
                // reload the messages), so re-fetch the conversation to refresh
                // the cached detail + chat (it may have changed while we were
                // disconnected) — via `ReloadConversation`, which keeps the
                // model picker intact. On the first connect there is no active
                // conversation yet, so the initial load happens through
                // `EnsureActiveConversation -> ConversationLoaded` (which does
                // set the picker). See issue #72.
                if let Some(id) = self.current_conversation_id.clone()
                    && self.conversations.iter().any(|c| c.id == id)
                {
                    effects.push(Effect::ReloadConversation(id));
                }
                effects
            }
            UiMessage::ConversationLoaded(detail) => {
                let id = detail.id.clone();
                let filtered = filter_messages(&detail, self.debug_enabled);
                let selection = detail.model_selection.clone();
                self.current_conversation = Some(detail);
                self.current_conversation_id = Some(id.clone());
                vec![
                    Effect::SetModelSelection(selection),
                    Effect::LoadConversationIntoChat(filtered),
                    // Rebind the side pane to the new conversation: clear stale
                    // notes until the fetch returns, refresh the filtered task
                    // list, and fetch this conversation's scratchpad.
                    Effect::SidePaneSetScratchpad(Vec::new()),
                    Effect::RefreshSidePaneTasks,
                    Effect::FetchScratchpad(id),
                ]
            }
            UiMessage::ConversationReloaded(detail) => {
                // A conversation already open was re-fetched (reconnect / debug /
                // personality refresh). Refresh the cached detail + chat (and
                // side pane) but deliberately do NOT emit `SetModelSelection`:
                // the model picker must keep the user's current selection across
                // a reconnect (issue #72). Drop the reply if the user switched
                // conversations while the fetch was in flight.
                if self.current_conversation_id.as_deref() != Some(detail.id.as_str()) {
                    vec![]
                } else {
                    let id = detail.id.clone();
                    let filtered = filter_messages(&detail, self.debug_enabled);
                    self.current_conversation = Some(detail);
                    vec![
                        Effect::LoadConversationIntoChat(filtered),
                        Effect::SidePaneSetScratchpad(Vec::new()),
                        Effect::RefreshSidePaneTasks,
                        Effect::FetchScratchpad(id),
                    ]
                }
            }
            UiMessage::ConversationCreated { id } => {
                self.current_conversation_id = Some(id);
                vec![]
            }
            UiMessage::ConversationDeleted { id } => {
                self.conversations.retain(|c| c.id != id);
                let is_active = self.current_conversation_id.as_deref() == Some(&id);
                if is_active {
                    self.current_conversation_id = None;
                    self.current_conversation = None;
                }
                let convs = self.conversations.clone();
                let mut effects = vec![Effect::SetConversations(convs)];
                if is_active {
                    effects.push(Effect::ClearChat);
                    effects.push(Effect::SidePaneSetScratchpad(Vec::new()));
                    effects.push(Effect::RefreshSidePaneTasks);
                    effects.push(Effect::EnsureActiveConversation);
                }
                effects
            }
            UiMessage::ConversationRenamed { id, title } => {
                for conv in &mut self.conversations {
                    if conv.id == id {
                        conv.title = title.clone();
                    }
                }
                vec![Effect::SetConversations(self.conversations.clone())]
            }
            UiMessage::PromptSent { task_id: _ } => {
                // The wire ack carries either a `task_id` (post-#114
                // `SendMessageAck`) or an empty string (legacy `Ack`). Neither
                // is the chunk-stream `request_id` — that is daemon-generated
                // and arrives inside the first `AssistantDelta`. Use the
                // sentinel until then; `StreamChunk` claims it on first frame.
                // See issue #31.
                self.pending_request_id = Some("__pending__".to_string());
                self.streaming_buffer.clear();
                vec![]
            }
            UiMessage::AssistantStatus {
                request_id,
                message,
            } => {
                if self.pending_request_id.as_deref() == Some(&request_id)
                    || self.pending_request_id.as_deref() == Some("__pending__")
                {
                    vec![Effect::SetChatStatus(message)]
                } else {
                    vec![]
                }
            }
            UiMessage::StreamChunk { request_id, chunk } => {
                // Claim request ID if pending
                if self.pending_request_id.as_deref() == Some("__pending__") {
                    self.pending_request_id = Some(request_id.clone());
                }
                if self.pending_request_id.as_deref() == Some(&request_id) {
                    let first_chunk = self.streaming_buffer.is_empty();
                    self.streaming_buffer.push_str(&chunk);
                    let mut effects = Vec::new();
                    if first_chunk {
                        effects.push(Effect::ClearChatStatus);
                    }
                    effects.push(Effect::ReceiveChunk(chunk));
                    effects
                } else {
                    vec![]
                }
            }
            UiMessage::StreamComplete {
                request_id,
                full_response,
            } => {
                if self.pending_request_id.as_deref() == Some("__pending__") {
                    self.pending_request_id = Some(request_id.clone());
                }
                if self.pending_request_id.as_deref() == Some(&request_id) {
                    self.pending_request_id = None;
                    self.streaming_buffer.clear();
                    if let Some(ref mut conv) = self.current_conversation {
                        conv.messages.push(ChatMessage {
                            role: "assistant".to_string(),
                            content: full_response.clone(),
                        });
                    }
                    // Reply narration (issue #76/#78): narrate the finalized
                    // reply via the embedded `Speaker` when the active
                    // conversation has read-aloud ON *or* voice mode ON. Gated
                    // entirely here so the cut-off holds — with both OFF no
                    // `Speak` effect exists, so no path plays audio. (The
                    // executor additionally no-ops when there is no embedded
                    // engine, e.g. the daemon path, which narrates its own
                    // replies.)
                    let narrate = self.narrate_for_current();
                    let mut effects = vec![Effect::ClearChatStatus];
                    if narrate {
                        effects.push(Effect::Speak(full_response.clone()));
                    }
                    effects.push(Effect::CompleteStreaming(full_response));
                    // The turn may have changed the scratchpad (Adele's todos);
                    // refresh the pane. (The live `ScratchpadChanged` event also
                    // covers this, but a turn-boundary refetch is a cheap
                    // backstop if the event was missed.)
                    if let Some(id) = self.current_conversation_id.clone() {
                        effects.push(Effect::FetchScratchpad(id));
                    }
                    effects
                } else {
                    vec![]
                }
            }
            UiMessage::StreamError { request_id, error } => {
                if self.pending_request_id.as_deref() == Some("__pending__") {
                    self.pending_request_id = Some(request_id.clone());
                }
                if self.pending_request_id.as_deref() == Some(&request_id) {
                    self.pending_request_id = None;
                    self.streaming_buffer.clear();
                    vec![
                        Effect::ClearChatStatus,
                        Effect::SetStatusText(format!("Error: {error}")),
                    ]
                } else {
                    vec![]
                }
            }
            UiMessage::TitleChanged {
                conversation_id,
                title,
            } => {
                for conv in &mut self.conversations {
                    if conv.id == conversation_id {
                        conv.title = title.clone();
                    }
                }
                vec![Effect::SetConversations(self.conversations.clone())]
            }
            UiMessage::ConversationWarning {
                conversation_id,
                warning,
            } => {
                // Single variant today — DanglingModelSelection. The daemon has
                // already cleared its side and fell back; if this is the
                // currently-open conversation, clear the header picker so it
                // doesn't show a stale "stuck" model, then surface a passive
                // toast explaining the fallback.
                match &warning {
                    api::ConversationWarning::DanglingModelSelection {
                        previous_selection,
                        fallback_to,
                    } => {
                        let is_current = self.current_conversation_id.as_deref()
                            == Some(conversation_id.as_str());
                        let mut effects = Vec::new();
                        if is_current {
                            effects.push(Effect::SetModelSelection(None));
                            // Also clear the cached detail's selection so a
                            // later `ModelsLoaded` doesn't re-apply the stale
                            // dangling selection, contradicting this toast.
                            if let Some(ref mut conv) = self.current_conversation {
                                conv.model_selection = None;
                            }
                        }
                        let message = format!(
                            "The model \"{}\" on connection \"{}\" is no longer available — falling back to \"{}\" on \"{}\".",
                            previous_selection.model_id,
                            previous_selection.connection_id,
                            fallback_to.model_id,
                            fallback_to.connection_id,
                        );
                        effects.push(Effect::ShowToast(message));
                        effects
                    }
                }
            }
            UiMessage::StatusUpdate(text) => vec![Effect::SetStatusText(text)],
            UiMessage::Error(text) => vec![Effect::SetStatusText(format!("Error: {text}"))],
            UiMessage::ModelsLoaded(listings) => {
                // A models refresh fires on every (re)connect (the UDS link
                // drops on idle / the daemon restarts) and when Settings is
                // opened. It must NOT re-apply the conversation's stored
                // selection: `set_models` already preserves the picker's active
                // selection, and re-applying the *cached* `model_selection`
                // (which is `None`/default for most conversations and is never
                // refreshed after a send) clobbered the user's in-memory pick
                // back to stored-or-default on each reconnect. The picker's
                // selection is owned by `ConversationLoaded` (an explicit
                // switch) and `set_default_model` (connect). See issue #72.
                let visible = !listings.is_empty();
                vec![
                    Effect::SetModels(listings),
                    Effect::SetModelPickerVisible(visible),
                ]
            }
            UiMessage::DefaultModelLoaded(default) => {
                // The picker uses this as the fallback selection for
                // conversations with no stored selection. Set it independently
                // of `set_selection`; the picker re-resolves
                // stored-or-default on every conversation load, so ordering
                // between the two only requires both to have run.
                vec![Effect::SetDefaultModel(default)]
            }
            UiMessage::Connected { label } => {
                vec![Effect::SetStatusText(label), Effect::SetSendSensitive(true)]
            }
            UiMessage::TasksLoaded(tasks) => {
                vec![Effect::TasksReplaceAll(tasks), Effect::RefreshSidePaneTasks]
            }
            UiMessage::TaskStarted(task) => {
                vec![Effect::TaskStarted(task), Effect::RefreshSidePaneTasks]
            }
            UiMessage::TaskProgress { id, progress_hint } => {
                vec![
                    Effect::TaskProgress { id, progress_hint },
                    Effect::RefreshSidePaneTasks,
                ]
            }
            UiMessage::TaskLogAppended { id, entry } => {
                // Log lines don't change the row set, so the side pane (which
                // shows no logs) needs no refresh here.
                vec![Effect::TaskLogAppended { id, entry }]
            }
            UiMessage::TaskCompleted { id } => {
                vec![Effect::TaskCompleted { id }, Effect::RefreshSidePaneTasks]
            }
            UiMessage::ConversationScratchpadLoaded {
                conversation_id,
                notes,
            } => {
                // Apply only if it's still the active conversation (a fetch may
                // race a conversation switch).
                if self.current_conversation_id.as_deref() == Some(conversation_id.as_str()) {
                    vec![Effect::SidePaneSetScratchpad(notes)]
                } else {
                    vec![]
                }
            }
            UiMessage::ScratchpadChanged { conversation_id } => {
                if self.current_conversation_id.as_deref() == Some(conversation_id.as_str()) {
                    vec![Effect::FetchScratchpad(conversation_id)]
                } else {
                    vec![]
                }
            }
            UiMessage::SetSpeechEnabled {
                conversation_id,
                enabled,
            } => {
                // Record the per-conversation read-aloud toggle. Pure state
                // change; the button already reflects itself (it is the write
                // source). Keyed by conversation so it never bleeds across them.
                self.conversation_speech_enabled
                    .insert(conversation_id, enabled);
                vec![]
            }
            UiMessage::SetVoiceMode {
                conversation_id,
                enabled,
            } => {
                // Record the per-conversation voice-mode toggle (issue #78).
                // Pure state change; the button is the write source here (the
                // user clicked it), so no UI reflection is needed. Keyed by
                // conversation so it never bleeds across them.
                self.conversation_voice_mode
                    .insert(conversation_id, enabled);
                vec![]
            }
            UiMessage::ClientToolCall {
                task_id,
                conversation_id: _,
                tool_call_id,
                tool_name,
                arguments,
            } => {
                // ALWAYS resolve the call (issue #76) so the suspended turn
                // resumes — the previous code dropped it and wedged the turn.
                //
                // We gate/apply against the *active* conversation, not the
                // call's `conversation_id`: a tool call for some other (e.g. a
                // concurrent voice session's) conversation must never borrow a
                // different conversation's state on this client. With #261's
                // session-scoped registration the daemon already scopes tools
                // per session; this is the belt-and-braces local guard.
                match tool_name.as_str() {
                    "say_this" => match say_this_text(&arguments) {
                        // Audio gate broadened in #78 to (read-aloud OR voice
                        // mode): speak the aside when either is on for the
                        // active conversation.
                        Some(text) if self.narrate_for_current() => vec![
                            Effect::Speak(text),
                            Effect::SubmitClientToolResult {
                                task_id,
                                tool_call_id,
                                result: Ok("spoken".to_string()),
                            },
                        ],
                        Some(text) => {
                            // Both controls OFF → show, don't speak. The turn
                            // still completes; no audio on any path.
                            let note = format!("(speech mode disabled) {text}");
                            vec![
                                Effect::AddInlineNote(note),
                                Effect::SubmitClientToolResult {
                                    task_id,
                                    tool_call_id,
                                    result: Ok("speech mode disabled; shown to the user as text \
                                         instead of spoken"
                                        .to_string()),
                                },
                            ]
                        }
                        None => {
                            // Malformed arguments (missing/!string `text`):
                            // never panic, resolve an Err so the turn completes.
                            vec![Effect::SubmitClientToolResult {
                                task_id,
                                tool_call_id,
                                result: Err(
                                    "say_this requires a string `text` argument".to_string()
                                ),
                            }]
                        }
                    },
                    // The model asks to switch this conversation into spoken
                    // voice mode (issue #78). Applies to the ACTIVE conversation
                    // (consistent with say_this's active-conversation gating);
                    // sticks until left. Reflect it on the button so the UI
                    // tracks the model's change, and always resolve a result.
                    // `request_voice` / `stop_voice` take no arguments, so a junk
                    // payload is simply ignored — never a panic.
                    "request_voice" => {
                        if let Some(id) = self.current_conversation_id.clone() {
                            self.conversation_voice_mode.insert(id, true);
                        }
                        vec![
                            Effect::SetVoiceModeButton(true),
                            Effect::SubmitClientToolResult {
                                task_id,
                                tool_call_id,
                                result: Ok("voice mode on; replies will be read aloud and kept \
                                     conversational"
                                    .to_string()),
                            },
                        ]
                    }
                    "stop_voice" => {
                        if let Some(id) = self.current_conversation_id.clone() {
                            self.conversation_voice_mode.insert(id, false);
                        }
                        vec![
                            Effect::SetVoiceModeButton(false),
                            Effect::SubmitClientToolResult {
                                task_id,
                                tool_call_id,
                                result: Ok("voice mode off; back to text-only".to_string()),
                            },
                        ]
                    }
                    _ => {
                        // Any other client tool: this client has no runtime for
                        // it, but it must still be resolved or the turn wedges.
                        vec![Effect::SubmitClientToolResult {
                            task_id,
                            tool_call_id,
                            result: Err(format!("this client cannot run the tool \"{tool_name}\"")),
                        }]
                    }
                }
            }
            UiMessage::Disconnected { reason } => {
                let mut effects = vec![
                    Effect::ClearClient,
                    Effect::SetSendSensitive(false),
                    Effect::SetStatusText(format!("Disconnected: {reason}")),
                ];

                // Finalize any in-progress streaming buffer
                if self.pending_request_id.is_some() {
                    self.pending_request_id = None;
                    if !self.streaming_buffer.is_empty() {
                        self.streaming_buffer.push_str("\n\n[Connection lost]");
                        let full = self.streaming_buffer.clone();
                        self.streaming_buffer.clear();
                        if let Some(ref mut conv) = self.current_conversation {
                            conv.messages.push(ChatMessage {
                                role: "assistant".to_string(),
                                content: full.clone(),
                            });
                        }
                        effects.push(Effect::CompleteStreaming(full));
                    }
                }
                effects
            }
        }
    }
}

pub struct AdelieWindow {
    pub window: ApplicationWindow,
}

impl AdelieWindow {
    pub fn new(app: &Application, config: ConnectionConfig) -> Self {
        let window = ApplicationWindow::builder()
            .application(app)
            .title("Adelie Desktop Assistant")
            .default_width(1100)
            .default_height(700)
            .build();

        // Set application icon for taskbar
        install_app_icon();

        // Apply theme-aware CSS: the dark palette is always installed; the
        // light overrides (see `crate::theme`) are layered on only when the
        // system/GTK preference is light, and re-applied on preference change.
        crate::theme::install_for_display(&gdk::Display::default().expect("display"));

        // Layout: resizable paned split between sidebar and chat. The left pane
        // holds the conversation list directly. Background tasks (issue #19)
        // used to be a second `Stack` page here, but they now live in a
        // dedicated popup window (`tasks_window`, built below) so the task list
        // and per-task log have room to show real detail — what each task is
        // doing right now, its MCP/tool calls, etc.
        let paned = Paned::new(Orientation::Horizontal);

        let sidebar = Sidebar::new();
        let tasks_panel = TasksPanel::new();

        let left_box = GtkBox::new(Orientation::Vertical, 0);
        left_box.set_size_request(280, -1);
        sidebar.container.set_vexpand(true);
        left_box.append(&sidebar.container);

        paned.set_start_child(Some(&left_box));

        // Background-tasks popup. Built once and hidden (not destroyed) on close
        // so the long-lived `TasksPanel` — which keeps receiving live `Task*`
        // events whether or not the window is visible — survives reopen with its
        // model and selection intact.
        //
        // Deliberately *not* `transient_for` the main window and *not* added to
        // the `Application`: a transient would stay stacked above the chat and
        // share its taskbar entry, and an app-owned window would keep the
        // process alive after the chat window closed. As an independent toplevel
        // it gets its own taskbar entry and can sit behind the chat; we tie only
        // its teardown to the client below so it can't outlive it.
        let tasks_window = Window::builder()
            .title("Background Tasks")
            .modal(false)
            .default_width(820)
            .default_height(560)
            .build();
        tasks_window.set_child(Some(&tasks_panel.container));
        tasks_window.set_hide_on_close(true);
        let tasks_window = Rc::new(tasks_window);

        // The popup isn't owned by the `Application`, so closing the main window
        // won't reap it on its own. Destroy it when the chat window closes so it
        // never lingers past client exit (`hide_on_close` is bypassed by an
        // explicit `destroy`).
        window.connect_close_request(glib::clone!(
            #[strong]
            tasks_window,
            move |_| {
                tasks_window.destroy();
                glib::Propagation::Proceed
            }
        ));
        paned.set_resize_start_child(false);
        paned.set_shrink_start_child(false);
        paned.set_position(280);

        let right_box = GtkBox::new(Orientation::Vertical, 0);
        right_box.set_hexpand(true);
        right_box.set_vexpand(true);

        // Header bar with hamburger menu
        let header_bar = GtkBox::new(Orientation::Horizontal, 8);
        header_bar.set_margin_start(8);
        header_bar.set_margin_end(8);
        header_bar.set_margin_top(4);
        header_bar.set_margin_bottom(4);

        // Per-conversation model picker — populated on connect, selection
        // tracks `ConversationView.model_selection` after each load.
        let model_picker = ModelPicker::new();
        header_bar.append(&model_picker.container);

        // Spacer to push menu button to the right
        let spacer = GtkBox::new(Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        header_bar.append(&spacer);

        // Toggle for the conversation side pane (issue #60). Wired to the
        // revealer once it's constructed below.
        //
        // Themed-icon fallback chain so the toggle renders across icon themes:
        // Breeze (KDE) lacks the GNOME `sidebar-show-*-symbolic` names (it shows
        // a broken glyph), so prefer Breeze's `tools-symbolic` (a wrench), then
        // fall back to `applications-utilities-symbolic` /
        // `applications-engineering-symbolic`, which exist in both Breeze and
        // Adwaita.
        let toggle_icon = gtk4::gio::ThemedIcon::from_names(&[
            "tools-symbolic",
            "applications-utilities-symbolic",
            "applications-engineering-symbolic",
        ]);
        let side_pane_toggle = Button::new();
        side_pane_toggle.set_child(Some(&gtk4::Image::from_gicon(&toggle_icon)));
        side_pane_toggle.add_css_class("flat");
        side_pane_toggle.set_tooltip_text(Some("Tasks & scratchpad for this conversation"));
        header_bar.append(&side_pane_toggle);

        // Hamburger menu button
        let menu_button = MenuButton::new();
        menu_button.set_icon_name("open-menu-symbolic");
        menu_button.add_css_class("flat");

        let menu_popover = Popover::new();
        menu_popover.add_css_class("context-popover");
        let menu_box = GtkBox::new(Orientation::Vertical, 0);

        let settings_btn = Button::with_label("Settings");
        settings_btn.add_css_class("context-button");
        settings_btn.set_halign(Align::Fill);
        menu_box.append(&settings_btn);

        let switch_conn_btn = Button::with_label("Switch Connection…");
        switch_conn_btn.add_css_class("context-button");
        switch_conn_btn.set_halign(Align::Fill);
        menu_box.append(&switch_conn_btn);

        let knowledge_btn = Button::with_label("Knowledge Base");
        knowledge_btn.add_css_class("context-button");
        knowledge_btn.set_halign(Align::Fill);
        menu_box.append(&knowledge_btn);

        let tasks_btn = Button::with_label("Background Tasks");
        tasks_btn.add_css_class("context-button");
        tasks_btn.set_halign(Align::Fill);
        menu_box.append(&tasks_btn);

        // Per-conversation personality override (#70). Opens a modal picker
        // pre-filled from the active conversation's stored override.
        let personality_btn = Button::with_label("Personality…");
        personality_btn.add_css_class("context-button");
        personality_btn.set_halign(Align::Fill);
        menu_box.append(&personality_btn);

        let disconnect_btn = Button::with_label("Disconnect");
        disconnect_btn.add_css_class("context-button");
        disconnect_btn.set_halign(Align::Fill);
        menu_box.append(&disconnect_btn);

        menu_popover.set_child(Some(&menu_box));
        menu_button.set_popover(Some(&menu_popover));
        header_bar.append(&menu_button);

        right_box.append(&header_bar);

        let header_sep = Separator::new(Orientation::Horizontal);
        right_box.append(&header_sep);

        // The chat column and the conversation side pane sit side by side below
        // the (full-width) header, so the pane stays visible while chatting.
        let body = GtkBox::new(Orientation::Horizontal, 0);
        body.set_hexpand(true);
        body.set_vexpand(true);

        let chat_column = GtkBox::new(Orientation::Vertical, 0);
        chat_column.set_hexpand(true);
        chat_column.set_vexpand(true);

        let chat_view = ChatView::new();
        chat_column.append(&chat_view.container);

        // Passive toast for advisory warnings (e.g. a dangling model
        // selection cleared by the daemon). The revealer is always in the
        // layout; we reveal it with a message when something needs
        // attention, and the user can dismiss it.
        let toast_revealer = Revealer::new();
        toast_revealer.set_transition_type(RevealerTransitionType::SlideUp);
        toast_revealer.set_reveal_child(false);
        let toast_row = GtkBox::new(Orientation::Horizontal, 8);
        toast_row.add_css_class("toast-row");
        toast_row.set_margin_start(12);
        toast_row.set_margin_end(12);
        toast_row.set_margin_top(6);
        toast_row.set_margin_bottom(6);
        let toast_label = Label::new(None);
        toast_label.set_halign(Align::Start);
        toast_label.set_hexpand(true);
        toast_label.set_wrap(true);
        toast_row.append(&toast_label);
        let toast_dismiss = Button::from_icon_name("window-close-symbolic");
        toast_dismiss.add_css_class("flat");
        toast_dismiss.connect_clicked(glib::clone!(
            #[weak]
            toast_revealer,
            move |_| toast_revealer.set_reveal_child(false)
        ));
        toast_row.append(&toast_dismiss);
        toast_revealer.set_child(Some(&toast_row));
        chat_column.append(&toast_revealer);

        let input_sep = Separator::new(Orientation::Horizontal);
        chat_column.append(&input_sep);

        let input_bar = InputBar::new();
        input_bar.send_button.set_sensitive(false); // disabled until connected
        chat_column.append(&input_bar.container);

        let status_bar = GtkBox::new(Orientation::Horizontal, 0);
        status_bar.set_margin_top(4);
        status_bar.set_margin_bottom(4);

        let status_label = Label::new(Some("Connecting..."));
        status_label.set_halign(gtk4::Align::Start);
        status_label.set_hexpand(true);
        status_label.set_margin_start(12);
        status_label.add_css_class("status-bar");
        status_bar.append(&status_label);

        let debug_check = CheckButton::with_label("Debug");
        debug_check.set_halign(gtk4::Align::End);
        debug_check.set_margin_end(12);
        debug_check.add_css_class("debug-check");
        status_bar.append(&debug_check);

        chat_column.append(&status_bar);

        // Conversation side pane (issue #60): tasks + scratchpad for the active
        // conversation, revealed from the right via the header toggle. The
        // divider lives inside the revealer so it only shows when revealed.
        let side_pane = ConversationSidePane::new();
        let side_revealer = Revealer::new();
        side_revealer.set_transition_type(RevealerTransitionType::SlideLeft);
        side_revealer.set_reveal_child(false);
        let side_box = GtkBox::new(Orientation::Horizontal, 0);
        side_box.append(&Separator::new(Orientation::Vertical));
        side_box.append(&side_pane.container);
        side_revealer.set_child(Some(&side_box));

        body.append(&chat_column);
        body.append(&side_revealer);
        right_box.append(&body);

        side_pane_toggle.connect_clicked(glib::clone!(
            #[weak]
            side_revealer,
            move |_| side_revealer.set_reveal_child(!side_revealer.reveals_child())
        ));

        paned.set_end_child(Some(&right_box));
        paned.set_resize_end_child(true);
        paned.set_shrink_end_child(false);
        window.set_child(Some(&paned));

        // Shared state
        let state = Rc::new(RefCell::new(WindowState {
            conversations: Vec::new(),
            current_conversation_id: None,
            current_conversation: None,
            pending_request_id: None,
            streaming_buffer: String::new(),
            debug_enabled: false,
            conversation_speech_enabled: HashMap::new(),
            conversation_voice_mode: HashMap::new(),
        }));

        // Wrap widgets in Rc for closures
        let sidebar = Rc::new(sidebar);
        let chat_view = Rc::new(RefCell::new(chat_view));
        let input_bar = Rc::new(input_bar);
        let status_label = Rc::new(status_label);
        let model_picker = Rc::new(model_picker);
        let tasks_panel = Rc::new(tasks_panel);
        let side_pane = Rc::new(side_pane);
        let toast_revealer = Rc::new(toast_revealer);
        let toast_label = Rc::new(toast_label);

        // Voice config (issue #65): pick between the standalone voice daemon
        // (default; `org.desktopAssistant.Voice` over D-Bus) and an in-process
        // embedded engine. Loaded once from `~/.config/adele-gtk/voice.toml`; an
        // absent/partial file resolves to the daemon mode, so existing users see
        // no change. When embedded, `embedded_voice` is the in-process engine
        // (lazily initialized on first mic use — no models load, no mic opens,
        // at startup); `None` on the daemon path.
        let voice_config = crate::voice_config::VoiceConfig::load();
        let embedded_voice: Rc<Option<crate::voice_embedded::EmbeddedVoice>> =
            Rc::new(if voice_config.is_embedded() {
                Some(crate::voice_embedded::EmbeddedVoice::new(
                    crate::voice_embedded::EmbeddedConfig {
                        audio: voice_config.audio.clone(),
                        vad: voice_config.vad.clone(),
                        stt: voice_config.stt.clone(),
                        tts: voice_config.tts.clone(),
                    },
                ))
            } else {
                None
            });
        // Set when a turn was started by the embedded mic button, so only
        // voice-initiated replies are spoken (a typed message stays silent).
        // Lives outside `WindowState` to keep `apply()` pure; the reply-playback
        // hook reads + clears it in the effect executor.
        let voice_reply_pending: Rc<Cell<bool>> = Rc::new(Cell::new(false));

        // Connector wrapped in Arc for async tasks, Rc<RefCell<>> for GTK
        // thread. The `Connector` owns the transport; call `.client()` for the
        // `&TransportClient` surface (`as_commands()`, `AssistantClient` RPCs).
        let client: Rc<RefCell<Option<Arc<Connector>>>> = Rc::new(RefCell::new(None));

        // Set up async bridge with UI message handler
        let bridge = AsyncBridge::new(glib::clone!(
            #[strong]
            state,
            #[strong]
            sidebar,
            #[strong]
            chat_view,
            #[strong]
            status_label,
            #[strong]
            client,
            #[strong]
            input_bar,
            #[strong]
            model_picker,
            #[strong]
            tasks_panel,
            #[strong]
            side_pane,
            #[strong]
            toast_revealer,
            #[strong]
            toast_label,
            #[strong]
            embedded_voice,
            #[strong]
            voice_reply_pending,
            move |msg, ui_tx| {
                handle_ui_message(
                    msg,
                    &state,
                    &sidebar,
                    &chat_view,
                    &status_label,
                    &client,
                    &input_bar,
                    &model_picker,
                    &tasks_panel,
                    &side_pane,
                    &toast_revealer,
                    &toast_label,
                    &embedded_voice,
                    &voice_reply_pending,
                    ui_tx,
                );
            }
        ));
        let bridge = Rc::new(bridge);

        // Voice controls (issue #59). The voice daemon is a *separate* D-Bus
        // service (`org.desktopAssistant.Voice`); it has no relationship to the
        // orchestrator transport above. We connect once, share the handle so
        // both the mic button and the Settings → Voice tab can drive it, and
        // gate the UI on the daemon actually owning its bus name (graceful
        // degradation when it isn't running / models aren't provisioned).
        let voice: Rc<RefCell<Option<VoiceController>>> = Rc::new(RefCell::new(None));
        // The daemon controls are wired only on the daemon path. In embedded
        // mode the mic button is driven in-process (wired in the send block
        // below, where it can reuse the send action) and shown immediately,
        // since there is no daemon to probe for.
        if embedded_voice.is_some() {
            input_bar.set_voice_available(true);
        } else {
            wire_voice_controls(&voice, &input_bar, &bridge, &state);
        }

        // Wire the side pane's interactions to daemon commands. Edits/toggles/
        // deletes issue scratchpad commands; the daemon's `ScratchpadChanged`
        // event then refreshes the pane (issue #60).
        side_pane.set_on_action(glib::clone!(
            #[strong]
            state,
            #[strong]
            client,
            move |action: SidePaneAction| {
                let Some(conv) = state.borrow().current_conversation_id.clone() else {
                    return;
                };
                let Some(connector) = client.borrow().clone() else {
                    return;
                };
                crate::async_bridge::spawn_on_runtime(async move {
                    let Some(cmds) = connector.client().as_commands() else {
                        return;
                    };
                    let result = match action {
                        SidePaneAction::SetNote {
                            key,
                            content,
                            note_type,
                            sequence,
                            done,
                        } => cmds
                            .set_scratchpad_note(&conv, &key, &content, &note_type, sequence, done)
                            .await
                            .map(|_| ()),
                        SidePaneAction::DeleteNote { key } => {
                            cmds.delete_scratchpad_notes(&conv, vec![key], false).await
                        }
                    };
                    if let Err(e) = result {
                        tracing::warn!("scratchpad action failed: {e}");
                    }
                });
            }
        ));
        side_pane.set_on_cancel_task(glib::clone!(
            #[strong]
            client,
            move |id: String| {
                let Some(connector) = client.borrow().clone() else {
                    return;
                };
                crate::async_bridge::spawn_on_runtime(async move {
                    if let Some(cmds) = connector.client().as_commands() {
                        let _ = cmds
                            .send_command(api::Command::CancelBackgroundTask { id })
                            .await;
                    }
                });
            }
        ));

        // Spawn persistent connection manager (connect → forward → reconnect).
        // It now delivers the freshly connected `Connector` to the main thread
        // via `UiMessage::ClientReady` on the same channel as every other UI
        // message (handled in `handle_ui_message`).
        {
            let ui_tx = bridge.ui_sender();
            bridge.spawn(connection_manager(config.clone(), ui_tx));
        }

        // Sidebar row activation → load conversation
        sidebar.list_box.connect_row_activated(glib::clone!(
            #[strong]
            client,
            #[strong]
            state,
            #[strong]
            bridge,
            move |_, row| {
                let idx = row.index() as usize;
                let state_borrow = state.borrow();
                if let Some(conv) = state_borrow.conversations.get(idx) {
                    let conv_id = conv.id.clone();
                    drop(state_borrow);

                    if let Some(connector) = client.borrow().clone() {
                        let tx = bridge.ui_sender();
                        bridge.spawn(async move {
                            match connector.client().get_conversation(&conv_id).await {
                                Ok(detail) => {
                                    let _ = tx.send(UiMessage::ConversationLoaded(detail));
                                }
                                Err(e) => {
                                    let _ = tx
                                        .send(UiMessage::Error(format!("Load conversation: {e}")));
                                }
                            }
                        });
                    }
                }
            }
        ));

        // New conversation button
        sidebar.new_button.connect_clicked(glib::clone!(
            #[strong]
            client,
            #[strong]
            bridge,
            move |_| {
                if let Some(connector) = client.borrow().clone() {
                    let tx = bridge.ui_sender();
                    bridge.spawn(async move {
                        let client = connector.client();
                        match client.create_conversation("New Conversation").await {
                            Ok(id) => {
                                let _ = tx.send(UiMessage::ConversationCreated { id: id.clone() });
                                // Refresh conversation list
                                if let Ok(convs) = client.list_conversations().await {
                                    let _ = tx.send(UiMessage::ConversationsLoaded(convs));
                                }
                                // Load the new conversation
                                if let Ok(detail) = client.get_conversation(&id).await {
                                    let _ = tx.send(UiMessage::ConversationLoaded(detail));
                                }
                            }
                            Err(e) => {
                                let _ =
                                    tx.send(UiMessage::Error(format!("Create conversation: {e}")));
                            }
                        }
                    });
                }
            }
        ));

        // Context menu: Delete conversation
        sidebar.connect_delete(glib::clone!(
            #[strong]
            client,
            #[strong]
            bridge,
            #[strong]
            state,
            move |idx| {
                let id = {
                    let s = state.borrow();
                    match s.conversations.get(idx) {
                        Some(conv) => conv.id.clone(),
                        None => return,
                    }
                };
                if let Some(connector) = client.borrow().clone() {
                    let tx = bridge.ui_sender();
                    let id = id.clone();
                    bridge.spawn(async move {
                        match connector.client().delete_conversation(&id).await {
                            Ok(()) => {
                                let _ = tx.send(UiMessage::ConversationDeleted { id });
                            }
                            Err(e) => {
                                let _ =
                                    tx.send(UiMessage::Error(format!("Delete conversation: {e}")));
                            }
                        }
                    });
                }
            }
        ));

        // Context menu: Rename conversation
        sidebar.connect_rename(glib::clone!(
            #[strong]
            client,
            #[strong]
            bridge,
            #[strong]
            state,
            #[weak]
            window,
            move |idx| {
                let (id, current_title) = {
                    let s = state.borrow();
                    match s.conversations.get(idx) {
                        Some(conv) => (conv.id.clone(), conv.title.clone()),
                        None => return,
                    }
                };

                let dialog = Window::builder()
                    .title("Rename Conversation")
                    .transient_for(&window)
                    .modal(true)
                    .default_width(360)
                    .default_height(10)
                    .resizable(false)
                    .build();

                let vbox = GtkBox::new(Orientation::Vertical, 8);
                vbox.set_margin_start(16);
                vbox.set_margin_end(16);
                vbox.set_margin_top(16);
                vbox.set_margin_bottom(16);

                let entry = Entry::new();
                entry.set_text(&current_title);
                entry.set_activates_default(true);
                vbox.append(&entry);

                let btn_box = GtkBox::new(Orientation::Horizontal, 8);
                btn_box.set_halign(gtk4::Align::End);

                let cancel_btn = Button::with_label("Cancel");
                cancel_btn.connect_clicked(glib::clone!(
                    #[weak]
                    dialog,
                    move |_| {
                        dialog.close();
                    }
                ));
                btn_box.append(&cancel_btn);

                let confirm_btn = Button::with_label("Rename");
                confirm_btn.add_css_class("suggested-action");
                confirm_btn.connect_clicked(glib::clone!(
                    #[strong]
                    client,
                    #[strong]
                    bridge,
                    #[weak]
                    dialog,
                    #[weak]
                    entry,
                    move |_| {
                        let new_title = entry.text().trim().to_string();
                        if new_title.is_empty() {
                            return;
                        }
                        dialog.close();
                        if let Some(connector) = client.borrow().clone() {
                            let tx = bridge.ui_sender();
                            let id = id.clone();
                            let title = new_title.clone();
                            bridge.spawn(async move {
                                match connector.client().rename_conversation(&id, &title).await {
                                    Ok(()) => {
                                        let _ =
                                            tx.send(UiMessage::ConversationRenamed { id, title });
                                    }
                                    Err(e) => {
                                        let _ = tx.send(UiMessage::Error(format!(
                                            "Rename conversation: {e}"
                                        )));
                                    }
                                }
                            });
                        }
                    }
                ));
                btn_box.append(&confirm_btn);

                // Enter key in entry confirms
                entry.connect_activate(glib::clone!(
                    #[weak]
                    confirm_btn,
                    move |_| {
                        confirm_btn.emit_clicked();
                    }
                ));

                vbox.append(&btn_box);
                dialog.set_child(Some(&vbox));
                dialog.present();
            }
        ));

        // Context menu: Archive/unarchive conversation
        sidebar.connect_archive(glib::clone!(
            #[strong]
            client,
            #[strong]
            bridge,
            #[strong]
            state,
            move |idx| {
                let (id, archived) = {
                    let s = state.borrow();
                    match s.conversations.get(idx) {
                        Some(conv) => (conv.id.clone(), conv.archived),
                        None => return,
                    }
                };
                if let Some(connector) = client.borrow().clone() {
                    let tx = bridge.ui_sender();
                    let id = id.clone();
                    bridge.spawn(async move {
                        let client = connector.client();
                        let result = if archived {
                            client.unarchive_conversation(&id).await
                        } else {
                            client.archive_conversation(&id).await
                        };
                        match result {
                            Ok(()) => {
                                // Refresh conversation list
                                if let Ok(convs) = client.list_conversations().await {
                                    let _ = tx.send(UiMessage::ConversationsLoaded(convs));
                                }
                            }
                            Err(e) => {
                                let _ =
                                    tx.send(UiMessage::Error(format!("Archive conversation: {e}")));
                            }
                        }
                    });
                }
            }
        ));

        // Show archived checkbox toggle
        sidebar.connect_show_archived_toggled(glib::clone!(
            #[strong]
            client,
            #[strong]
            bridge,
            move |include_archived| {
                if let Some(connector) = client.borrow().clone() {
                    let tx = bridge.ui_sender();
                    bridge.spawn(async move {
                        let client = connector.client();
                        let result = if include_archived {
                            client.list_conversations_with_archived().await
                        } else {
                            client.list_conversations().await
                        };
                        match result {
                            Ok(convs) => {
                                let _ = tx.send(UiMessage::ConversationsLoaded(convs));
                            }
                            Err(e) => {
                                let _ =
                                    tx.send(UiMessage::Error(format!("Load conversations: {e}")));
                            }
                        }
                    });
                }
            }
        ));

        // Send button / Enter key → send prompt
        {
            let send_action = Rc::new(glib::clone!(
                #[strong]
                client,
                #[strong(rename_to = bridge_ref)]
                bridge,
                #[strong]
                state,
                #[strong]
                input_bar,
                #[strong]
                chat_view,
                #[strong]
                model_picker,
                move || {
                    let text = input_bar.take_text();
                    let text = text.trim().to_string();
                    if text.is_empty() {
                        return;
                    }
                    let state_borrow = state.borrow();
                    let conv_id = match &state_borrow.current_conversation_id {
                        Some(id) => id.clone(),
                        None => return,
                    };
                    drop(state_borrow);

                    // Show user message immediately
                    chat_view.borrow_mut().add_user_message(&text);

                    // Track in local conversation copy
                    {
                        let mut s = state.borrow_mut();
                        if let Some(ref mut conv) = s.current_conversation {
                            conv.messages.push(ChatMessage {
                                role: "user".to_string(),
                                content: text.clone(),
                            });
                        }
                    }

                    let override_selection = model_picker.current_override();
                    // Voice-mode send shaping (issue #78): when the active
                    // conversation is in voice mode, attach the read-aloud
                    // system refinement so the reply is shaped for speech. This
                    // travels in the system prompt for the turn only — never the
                    // visible chat text. `None` when voice mode is off → an
                    // empty refinement, i.e. the unchanged phase-1 send.
                    let refinement = refinement_for_send(&state.borrow())
                        .unwrap_or_default()
                        .to_string();

                    if let Some(connector) = client.borrow().clone() {
                        let tx = bridge_ref.ui_sender();
                        let text = text.clone();
                        bridge_ref.spawn(async move {
                            let client = connector.client();
                            // Socket transports (UDS/WS) carry the model override
                            // AND the system refinement together on the command
                            // channel via `send_prompt_full`. The shared
                            // AssistantClient trait exposes neither, so on D-Bus
                            // we fall back to the Connector's
                            // `send_prompt_with_system_refinement` (which folds
                            // the refinement into the prompt; the override isn't
                            // available over D-Bus regardless).
                            let result = match client.as_commands() {
                                Some(cmds) => {
                                    cmds.send_prompt_full(
                                        &conv_id,
                                        &text,
                                        override_selection,
                                        refinement,
                                    )
                                    .await
                                }
                                None => {
                                    connector
                                        .send_prompt_with_system_refinement(
                                            &conv_id,
                                            &text,
                                            &refinement,
                                        )
                                        .await
                                }
                            };
                            match result {
                                Ok(task_id) => {
                                    let _ = tx.send(UiMessage::PromptSent { task_id });
                                }
                                Err(e) => {
                                    let _ = tx.send(UiMessage::Error(format!("Send error: {e}")));
                                }
                            }
                        });
                    }
                }
            ));

            // Send button click
            input_bar.send_button.connect_clicked(glib::clone!(
                #[strong]
                send_action,
                move |_| {
                    send_action();
                }
            ));

            // Enter key in text view (Shift+Enter for newline)
            let key_controller = gtk4::EventControllerKey::new();
            key_controller.connect_key_pressed(glib::clone!(
                #[strong]
                send_action,
                move |_, key, _, modifiers| {
                    if key == gdk::Key::Return
                        && !modifiers.contains(gdk::ModifierType::SHIFT_MASK)
                        && !modifiers.contains(gdk::ModifierType::CONTROL_MASK)
                    {
                        send_action();
                        glib::Propagation::Stop
                    } else {
                        glib::Propagation::Proceed
                    }
                }
            ));
            input_bar.text_view.add_controller(key_controller);

            // Embedded voice (issue #65): when the in-process engine is active,
            // the mic button dictates locally and sends via the same
            // `send_action` as a typed message. Wired here (inside the send
            // block) so it can reuse `send_action`; the daemon path wires the
            // mic separately in `wire_voice_controls`.
            if let Some(engine) = (*embedded_voice).clone() {
                wire_embedded_mic(
                    engine,
                    &input_bar,
                    &send_action,
                    &state,
                    &bridge,
                    &voice_reply_pending,
                );
            }
        }

        // Per-conversation speech toggle (issue #76). A user flip routes a
        // `SetSpeechEnabled` for the *current* conversation through the same
        // handler as everything else, so the pure `apply` records it. The
        // `set_speech_active` reflection on conversation switch (below, in the
        // `LoadConversationIntoChat` effect) is suppressed, so it never echoes
        // back here. With no active conversation the flip is dropped (the
        // button is only meaningful against a conversation).
        input_bar.connect_speech_toggled(glib::clone!(
            #[strong]
            state,
            #[strong]
            bridge,
            move |enabled| {
                let Some(conv_id) = state.borrow().current_conversation_id.clone() else {
                    return;
                };
                let _ = bridge.ui_sender().send(UiMessage::SetSpeechEnabled {
                    conversation_id: conv_id,
                    enabled,
                });
            }
        ));

        // Per-conversation voice-mode toggle (issue #78). A user flip routes a
        // `SetVoiceMode` for the *current* conversation through the same handler
        // so the pure `apply` records it. The `set_voice_mode_active` reflection
        // on conversation switch / model drive is suppressed, so it never echoes
        // back here. With no active conversation the flip is dropped.
        input_bar.connect_voice_mode_toggled(glib::clone!(
            #[strong]
            state,
            #[strong]
            bridge,
            move |enabled| {
                let Some(conv_id) = state.borrow().current_conversation_id.clone() else {
                    return;
                };
                let _ = bridge.ui_sender().send(UiMessage::SetVoiceMode {
                    conversation_id: conv_id,
                    enabled,
                });
            }
        ));

        // Hamburger menu: Switch Connection → open the picker in a new
        // window. The current connection's window is intentionally left open;
        // selecting a profile spawns a fresh AdelieWindow alongside it.
        switch_conn_btn.connect_clicked(glib::clone!(
            #[weak]
            app,
            #[weak]
            menu_popover,
            move |_| {
                menu_popover.popdown();
                let login = crate::widgets::login_screen::LoginScreen::new(&app);
                login.present();
            }
        ));

        // Hamburger menu: Settings → Connections / Purposes tabs (#1). The
        // dialog is WS-only (named-connection management isn't exposed over
        // D-Bus); on a D-Bus transport we status-message and no-op — the
        // header model picker is already hidden there.
        settings_btn.connect_clicked(glib::clone!(
            #[weak]
            menu_popover,
            #[weak]
            window,
            #[strong]
            client,
            #[strong]
            bridge,
            #[strong]
            status_label,
            #[strong]
            voice,
            move |_| {
                menu_popover.popdown();
                let Some(connector) = client.borrow().clone() else {
                    status_label.set_text("Not connected — settings unavailable");
                    return;
                };
                if connector.client().as_commands().is_none() {
                    status_label.set_text(
                        "Settings require a local-socket or WebSocket connection (not available over D-Bus)",
                    );
                    return;
                }
                // The Voice tab talks to its own daemon, so hand the dialog a
                // controller regardless of which orchestrator transport we're
                // on. When voice hasn't connected yet (or has no session bus),
                // fall back to an inert controller — the tab then shows its
                // "unavailable" state.
                let voice_handle = match voice.borrow().clone() {
                    Some(v) => v,
                    None => VoiceController::unavailable(),
                };
                crate::widgets::settings_dialog::show_settings_dialog(
                    &window,
                    Arc::clone(&connector),
                    Rc::clone(&bridge),
                    voice_handle,
                );
                // The user may have added/removed connections; re-query the
                // aggregated model list so the header picker reflects the new
                // set. Fire-and-forget — errors are non-fatal. Runs once when
                // Settings is opened (so it picks up the previous session's
                // changes); the dialog itself keeps its own tabs in sync.
                let tx = bridge.ui_sender();
                bridge.spawn(async move {
                    let transport = connector.client();
                    match management_client::list_available_models(transport, None, false).await {
                        Ok(listings) => {
                            let _ = tx.send(UiMessage::ModelsLoaded(listings));
                        }
                        Err(e) => {
                            tracing::warn!("Failed to refresh models after settings: {e}");
                        }
                    }
                    // The user may have changed the interactive purpose; re-fetch
                    // purposes so the picker's default updates for conversations
                    // still on the default (issue #53). Graceful: on failure we
                    // emit `None` (picker degrades to "Model").
                    let default_model =
                        match management_client::get_purposes(transport).await {
                            Ok(purposes) => {
                                crate::async_bridge::interactive_default_from_purposes(&purposes)
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "Failed to refresh purposes after settings: {e}"
                                );
                                None
                            }
                        };
                    let _ = tx.send(UiMessage::DefaultModelLoaded(default_model));
                });
            }
        ));

        // Hamburger menu: Knowledge Base → open the KB browser/editor (#74)
        knowledge_btn.connect_clicked(glib::clone!(
            #[weak]
            menu_popover,
            #[weak]
            window,
            #[strong]
            client,
            #[strong]
            bridge,
            #[strong]
            status_label,
            move |_| {
                menu_popover.popdown();
                let Some(connector) = client.borrow().clone() else {
                    status_label.set_text("Not connected — knowledge base unavailable");
                    return;
                };
                let browser = crate::widgets::knowledge_browser::KnowledgeBrowser::new(
                    &window,
                    connector,
                    Rc::clone(&bridge),
                );
                browser.present();
            }
        ));

        // Hamburger menu: Background Tasks → present the tasks popup. The window
        // and its `TasksPanel` already exist and stay current via the live
        // `Task*` event stream, so opening is just a `present()`.
        tasks_btn.connect_clicked(glib::clone!(
            #[weak]
            menu_popover,
            #[strong]
            tasks_window,
            move |_| {
                menu_popover.popdown();
                tasks_window.present();
            }
        ));

        // Hamburger menu: Personality… → open the per-conversation personality
        // picker (#70). Mirrors the model picker's gating: the override travels
        // on the command channel (UDS/WS), which D-Bus doesn't expose. Pre-fills
        // from the active conversation's stored override and, on Save, dispatches
        // `set_conversation_personality` through the async bridge, then reloads
        // the conversation so `current_conversation.conversation_personality`
        // (the next pre-fill source) stays in sync with the daemon.
        personality_btn.connect_clicked(glib::clone!(
            #[weak]
            menu_popover,
            #[weak]
            window,
            #[strong]
            client,
            #[strong]
            bridge,
            #[strong]
            status_label,
            #[strong]
            state,
            move |_| {
                menu_popover.popdown();
                let Some(connector) = client.borrow().clone() else {
                    status_label.set_text("Not connected — personality unavailable");
                    return;
                };
                if connector.client().as_commands().is_none() {
                    status_label.set_text(
                        "Personality settings require a local-socket or WebSocket connection (not available over D-Bus)",
                    );
                    return;
                }
                let (conv_id, prefill) = {
                    let s = state.borrow();
                    let id = s.current_conversation_id.clone();
                    let prefill = s
                        .current_conversation
                        .as_ref()
                        .and_then(|c| c.conversation_personality);
                    (id, prefill)
                };
                let Some(conv_id) = conv_id else {
                    status_label.set_text("Open a conversation first to set its personality");
                    return;
                };

                let bridge_for_save = Rc::clone(&bridge);
                let connector_for_save = Arc::clone(&connector);
                let status_for_save = status_label.clone();
                crate::widgets::personality_dialog::show_personality_dialog(
                    &window,
                    prefill.as_ref(),
                    move |personality| {
                        let tx = bridge_for_save.ui_sender();
                        let connector = Arc::clone(&connector_for_save);
                        let conv_id = conv_id.clone();
                        let status = status_for_save.clone();
                        status.set_text("Saving personality…");
                        bridge_for_save.spawn(async move {
                            let transport = connector.client();
                            match management_client::set_conversation_personality(
                                transport,
                                &conv_id,
                                personality,
                            )
                            .await
                            {
                                Ok(_stored) => {
                                    // Reload so the next pre-fill reflects the
                                    // stored personality override (and any
                                    // all-None clear). Use `ConversationReloaded`
                                    // so refreshing the personality cache doesn't
                                    // reset the model picker (issue #72).
                                    match transport.get_conversation(&conv_id).await {
                                        Ok(detail) => {
                                            let _ =
                                                tx.send(UiMessage::ConversationReloaded(detail));
                                        }
                                        Err(e) => {
                                            let _ = tx.send(UiMessage::Error(format!(
                                                "Reload after personality save: {e}"
                                            )));
                                        }
                                    }
                                }
                                Err(e) => {
                                    let _ = tx.send(UiMessage::Error(format!(
                                        "Save personality: {e}"
                                    )));
                                }
                            }
                        });
                    },
                );
            }
        ));

        // Hamburger menu: Disconnect → close this window, show login screen
        disconnect_btn.connect_clicked(glib::clone!(
            #[weak]
            app,
            #[weak]
            window,
            #[weak]
            menu_popover,
            move |_| {
                menu_popover.popdown();
                let login = crate::widgets::login_screen::LoginScreen::new(&app);
                login.present();
                window.close();
            }
        ));

        // Debug checkbox toggle → re-fetch conversation with filtering
        debug_check.connect_toggled(glib::clone!(
            #[strong]
            client,
            #[strong]
            bridge,
            #[strong]
            state,
            move |btn| {
                state.borrow_mut().debug_enabled = btn.is_active();
                let conv_id = state.borrow().current_conversation_id.clone();
                if let Some(conv_id) = conv_id
                    && let Some(connector) = client.borrow().clone()
                {
                    let tx = bridge.ui_sender();
                    bridge.spawn(async move {
                        match connector.client().get_conversation(&conv_id).await {
                            Ok(detail) => {
                                // Reload (re-filter) the same conversation — keep
                                // the model picker (issue #72).
                                let _ = tx.send(UiMessage::ConversationReloaded(detail));
                            }
                            Err(e) => {
                                let _ =
                                    tx.send(UiMessage::Error(format!("Reload conversation: {e}")));
                            }
                        }
                    });
                }
            }
        ));

        // Tasks panel: toolbar wiring (#19).
        //
        // `Cancel` sends `CancelBackgroundTask` over the command channel;
        // `Open Conversation` hides the tasks popup and loads the task's
        // conversation into the main window so the streaming output keeps
        // flowing into the chat view.
        tasks_panel.connect_cancel(glib::clone!(
            #[strong]
            client,
            #[strong]
            bridge,
            move |task_id| {
                let Some(connector) = client.borrow().clone() else {
                    return;
                };
                let tx = bridge.ui_sender();
                bridge.spawn(async move {
                    let Some(cmds) = connector.client().as_commands() else {
                        let _ = tx.send(UiMessage::Error(
                            "Background tasks require a local-socket or WebSocket connection \
                             (not available over D-Bus)"
                                .to_string(),
                        ));
                        return;
                    };
                    if let Err(e) = cmds
                        .send_command(api::Command::CancelBackgroundTask { id: task_id })
                        .await
                    {
                        let _ = tx.send(UiMessage::Error(format!("Cancel task: {e}")));
                    }
                });
            }
        ));

        tasks_panel.connect_open_conversation(glib::clone!(
            #[strong]
            client,
            #[strong]
            bridge,
            #[strong]
            tasks_window,
            move |conv_id| {
                tasks_window.set_visible(false);
                let Some(connector) = client.borrow().clone() else {
                    return;
                };
                let tx = bridge.ui_sender();
                bridge.spawn(async move {
                    match connector.client().get_conversation(&conv_id).await {
                        Ok(detail) => {
                            let _ = tx.send(UiMessage::ConversationLoaded(detail));
                        }
                        Err(e) => {
                            let _ = tx.send(UiMessage::Error(format!("Load conversation: {e}")));
                        }
                    }
                });
            }
        ));

        Self { window }
    }

    pub fn present(&self) {
        self.window.present();
    }
}

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
fn wire_voice_controls(
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
/// path. The reply is spoken by the `CompleteStreaming` hook (gated on
/// `voice_reply_pending`, set here before the send).
///
/// A click **while a reply is playing barges in** (stops playback) instead of
/// starting a new turn, mirroring the daemon mic button. The button reflects
/// `Listening` for the duration of the capture, then returns to `Idle`.
///
/// All voice work runs on the Tokio runtime; only the transcript crosses back
/// to the GTK thread (via a oneshot) to touch widgets and call `send_action`.
fn wire_embedded_mic(
    engine: crate::voice_embedded::EmbeddedVoice,
    input_bar: &Rc<InputBar>,
    send_action: &Rc<impl Fn() + 'static>,
    state: &Rc<RefCell<WindowState>>,
    bridge: &Rc<AsyncBridge>,
    voice_reply_pending: &Rc<Cell<bool>>,
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
        voice_reply_pending,
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
                voice_reply_pending,
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
                            // Mark the turn as voice-initiated so its reply is
                            // spoken, then drop the transcript into the input
                            // box and send it like a typed message.
                            voice_reply_pending.set(true);
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

/// Current wall-clock time in epoch milliseconds. Centralized so the
/// task-panel callers all use the same units as `TaskView.started_at`.
fn now_epoch_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[allow(clippy::too_many_arguments)]
fn handle_ui_message(
    msg: UiMessage,
    state: &Rc<RefCell<WindowState>>,
    sidebar: &Rc<Sidebar>,
    chat_view: &Rc<RefCell<ChatView>>,
    status_label: &Rc<Label>,
    client: &Rc<RefCell<Option<Arc<Connector>>>>,
    input_bar: &Rc<InputBar>,
    model_picker: &Rc<ModelPicker>,
    tasks_panel: &Rc<TasksPanel>,
    side_pane: &Rc<ConversationSidePane>,
    toast_revealer: &Rc<Revealer>,
    toast_label: &Rc<Label>,
    embedded_voice: &Rc<Option<crate::voice_embedded::EmbeddedVoice>>,
    voice_reply_pending: &Rc<Cell<bool>>,
    ui_tx: &mpsc::UnboundedSender<UiMessage>,
) {
    // Embedded voice (issue #65): a voice turn that ends in an error never
    // reaches `CompleteStreaming`, so clear the "speak the reply" flag here on a
    // stream error. Otherwise the flag would leak onto the next (possibly typed)
    // turn and speak a reply the user didn't dictate. The successful path clears
    // it in the `CompleteStreaming` effect below.
    if matches!(msg, UiMessage::StreamError { .. }) {
        voice_reply_pending.set(false);
    }

    // Pure decision: mutate state + compute the effects to perform.
    let effects = state.borrow_mut().apply(msg);

    // Thin executor: perform each effect against the real widgets, in order.
    for effect in effects {
        match effect {
            Effect::SetClient(connector) => {
                // Advertise gtk's session-scoped voice-mode client tools so the
                // model can drive voice mode (issue #78). Registered once on
                // connect; the Connector remembers + replays this set after an
                // auto-reconnect (#246), so a daemon restart won't drop them.
                // Socket-only (UDS/WS) — on a D-Bus transport this is a logged
                // no-op (the daemon has no command channel for client tools),
                // which is fine: voice mode still works as a local UI toggle.
                let registration_connector = Arc::clone(&connector);
                crate::async_bridge::spawn_on_runtime(async move {
                    if let Err(e) = registration_connector
                        .register_client_tools(voice_mode_client_tools())
                        .await
                    {
                        tracing::debug!("voice-mode client tools not registered: {e}");
                    }
                });
                *client.borrow_mut() = Some(connector);
            }
            Effect::ClearClient => {
                *client.borrow_mut() = None;
            }
            Effect::SetStatusText(text) => {
                status_label.set_text(&text);
            }
            Effect::SetSendSensitive(sensitive) => {
                input_bar.send_button.set_sensitive(sensitive);
            }
            Effect::SetConversations(convs) => {
                sidebar.set_conversations(&convs);
            }
            Effect::EnsureActiveConversation => {
                ensure_active_conversation(state, sidebar, client, ui_tx);
            }
            Effect::LoadConversationIntoChat(filtered) => {
                chat_view.borrow_mut().load_conversation(&filtered);
                // Reflect the newly-active conversation's read-aloud + voice-mode
                // toggles on the input bar (issue #76/#78). `apply` has already
                // pointed `current_conversation_id` at this conversation, so
                // these read the right per-conversation state. Suppressed inside
                // each setter, so they don't echo a write back.
                let (read_aloud, voice_mode) = {
                    let s = state.borrow();
                    (s.speech_enabled_for_current(), s.voice_mode_for_current())
                };
                input_bar.set_speech_active(read_aloud);
                input_bar.set_voice_mode_active(voice_mode);
            }
            Effect::ReloadConversation(id) => {
                // Re-fetch an already-open conversation (reconnect / debug /
                // personality refresh) and refresh the cached detail + chat via
                // `ConversationReloaded`, which deliberately leaves the model
                // picker untouched (issue #72).
                if let Some(connector) = client.borrow().clone() {
                    let tx = ui_tx.clone();
                    crate::async_bridge::spawn_on_runtime(async move {
                        match connector.client().get_conversation(&id).await {
                            Ok(detail) => {
                                let _ = tx.send(UiMessage::ConversationReloaded(detail));
                            }
                            Err(e) => {
                                tracing::warn!("reload conversation failed: {e}");
                            }
                        }
                    });
                }
            }
            Effect::ClearChat => {
                chat_view.borrow_mut().clear();
            }
            Effect::SetChatStatus(message) => {
                chat_view.borrow().set_status(&message);
            }
            Effect::ClearChatStatus => {
                chat_view.borrow().clear_status();
            }
            Effect::ReceiveChunk(chunk) => {
                chat_view.borrow_mut().receive_chunk(&chunk);
            }
            Effect::CompleteStreaming(full) => {
                chat_view.borrow_mut().complete_streaming(&full);
                // Embedded voice (issue #65): speak the reply only when this
                // turn was started by the embedded mic button (so typed
                // messages stay silent). The flag is consumed here; on the
                // daemon path `embedded_voice` is `None` and the daemon speaks
                // its own replies, so this is a no-op.
                let engine = if voice_reply_pending.replace(false) {
                    (**embedded_voice).clone()
                } else {
                    None
                };
                if let Some(engine) = engine {
                    let ui_tx = ui_tx.clone();
                    let reply = full.clone();
                    crate::async_bridge::spawn_on_runtime(async move {
                        if let Err(e) = engine.say(&reply).await {
                            let _ = ui_tx.send(UiMessage::Error(format!("Voice: {e}")));
                        }
                    });
                }
            }
            Effect::SetModelSelection(selection) => {
                model_picker.set_selection(selection.as_ref());
            }
            Effect::SetModels(listings) => {
                model_picker.set_models(&listings);
            }
            Effect::SetDefaultModel(default) => {
                model_picker.set_default_model(default);
            }
            Effect::SetModelPickerVisible(visible) => {
                model_picker.set_visible(visible);
            }
            Effect::ShowToast(message) => {
                toast_label.set_text(&message);
                toast_revealer.set_reveal_child(true);
            }
            Effect::TasksReplaceAll(tasks) => {
                tasks_panel.replace_all(tasks, now_epoch_ms());
            }
            Effect::TaskStarted(task) => {
                tasks_panel.handle_task_started(task, now_epoch_ms());
            }
            Effect::TaskProgress { id, progress_hint } => {
                tasks_panel.handle_task_progress(id, progress_hint, now_epoch_ms());
            }
            Effect::TaskLogAppended { id, entry } => {
                tasks_panel.handle_task_log_appended(id, entry);
            }
            Effect::TaskCompleted { id } => {
                tasks_panel.handle_task_completed(id, now_epoch_ms());
            }
            Effect::FetchScratchpad(conversation_id) => {
                if let Some(connector) = client.borrow().clone() {
                    let tx = ui_tx.clone();
                    crate::async_bridge::spawn_on_runtime(async move {
                        let Some(cmds) = connector.client().as_commands() else {
                            return;
                        };
                        match cmds
                            .get_conversation_scratchpad(&conversation_id, None)
                            .await
                        {
                            Ok(notes) => {
                                let _ = tx.send(UiMessage::ConversationScratchpadLoaded {
                                    conversation_id,
                                    notes,
                                });
                            }
                            Err(e) => {
                                tracing::warn!("get_conversation_scratchpad failed: {e}");
                            }
                        }
                    });
                }
            }
            Effect::SidePaneSetScratchpad(notes) => {
                side_pane.set_scratchpad(notes);
            }
            Effect::RefreshSidePaneTasks => {
                let conv = state.borrow().current_conversation_id.clone();
                let rows = tasks_panel.task_view_models_for(conv.as_deref(), now_epoch_ms());
                side_pane.set_tasks(&rows);
            }
            Effect::Speak(text) => {
                // Speak via the embedded engine. `apply` only emits this when
                // the active conversation has speech ON, so this is the spoken
                // path of both reply narration and a `say_this` aside. No-op on
                // the daemon path (`embedded_voice` is `None` there — the daemon
                // narrates its own replies), which keeps the master cut-off:
                // audio is produced by exactly one engine.
                if let Some(engine) = (**embedded_voice).clone() {
                    let ui_tx = ui_tx.clone();
                    crate::async_bridge::spawn_on_runtime(async move {
                        if let Err(e) = engine.say(&text).await {
                            let _ = ui_tx.send(UiMessage::Error(format!("Voice: {e}")));
                        }
                    });
                }
            }
            Effect::AddInlineNote(note) => {
                chat_view.borrow_mut().add_inline_note(&note);
            }
            Effect::SetVoiceModeButton(enabled) => {
                // The model drove voice mode (request_voice / stop_voice);
                // reflect it on the toggle. Suppressed inside, so it doesn't
                // echo a `SetVoiceMode` write back through the user callback.
                input_bar.set_voice_mode_active(enabled);
            }
            Effect::SubmitClientToolResult {
                task_id,
                tool_call_id,
                result,
            } => {
                // Post the outcome back to the daemon so the suspended turn
                // resumes (issue #76). Spawned off the GTK thread; failures to
                // deliver are logged (the daemon's suspension timeout, #262, is
                // the backstop if delivery never lands).
                if let Some(connector) = client.borrow().clone() {
                    crate::async_bridge::spawn_on_runtime(async move {
                        if let Err(e) = connector
                            .submit_client_tool_result(&task_id, &tool_call_id, result)
                            .await
                        {
                            tracing::warn!("submit_client_tool_result failed: {e}");
                        }
                    });
                } else {
                    tracing::warn!("no connector to submit client-tool result for task {task_id}");
                }
            }
        }
    }
}

/// Make sure the window has an active conversation. The daemon returns the
/// conversation list sorted by `updated_at` desc, so picking index 0 yields
/// the most-recently-used conversation. When the list is empty we ask the
/// daemon to create a new one and load it.
///
/// No-op when an active conversation is already set and still present in the
/// list — this lets the function be called freely from `ConversationsLoaded`
/// (which fires on every reconnect) without disturbing in-progress work.
fn ensure_active_conversation(
    state: &Rc<RefCell<WindowState>>,
    sidebar: &Rc<Sidebar>,
    client: &Rc<RefCell<Option<Arc<Connector>>>>,
    ui_tx: &mpsc::UnboundedSender<UiMessage>,
) {
    let (target_id, target_index) = {
        let s = state.borrow();

        // Already-active and still present → just sync the sidebar selection.
        if let Some(active_id) = s.current_conversation_id.as_deref()
            && let Some(idx) = s.conversations.iter().position(|c| c.id == active_id)
        {
            drop(s);
            sidebar.select_index(idx);
            return;
        }

        match s.conversations.first() {
            Some(conv) => (Some(conv.id.clone()), Some(0usize)),
            None => (None, None),
        }
    };

    let Some(connector) = client.borrow().clone() else {
        // Not connected yet — connection_manager will resend
        // ConversationsLoaded once the transport is up, and we'll re-run.
        return;
    };

    let tx = ui_tx.clone();
    match (target_id, target_index) {
        (Some(id), Some(idx)) => {
            sidebar.select_index(idx);
            crate::async_bridge::spawn_on_runtime(async move {
                match connector.client().get_conversation(&id).await {
                    Ok(detail) => {
                        let _ = tx.send(UiMessage::ConversationLoaded(detail));
                    }
                    Err(e) => {
                        let _ = tx.send(UiMessage::Error(format!("Load conversation: {e}")));
                    }
                }
            });
        }
        _ => {
            crate::async_bridge::spawn_on_runtime(async move {
                let client = connector.client();
                match client.create_conversation("New Conversation").await {
                    Ok(id) => {
                        let _ = tx.send(UiMessage::ConversationCreated { id: id.clone() });
                        if let Ok(convs) = client.list_conversations().await {
                            let _ = tx.send(UiMessage::ConversationsLoaded(convs));
                        }
                        if let Ok(detail) = client.get_conversation(&id).await {
                            let _ = tx.send(UiMessage::ConversationLoaded(detail));
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(UiMessage::Error(format!("Auto-create conversation: {e}")));
                    }
                }
            });
        }
    }
}

/// Install the Adele icon into the GTK icon theme so it appears in the taskbar.
///
/// Writes the embedded PNG to a temporary hicolor icon theme directory and adds
/// it to the display's icon search path. Uses the app ID as the icon name so
/// the desktop environment can match it to the window.
pub fn install_app_icon() {
    const ICON_BYTES: &[u8] = include_bytes!("../assets/adele.png");
    const ICON_NAME: &str = "org.adelie.DesktopAssistant";

    let cache_root = dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("adele-gtk-icons");

    // Resolves to <cache_root>/hicolor/512x512/apps/<ICON_NAME>.png; the
    // helper creates the parent dirs and writes idempotently.
    let icon_rel = format!("adele-gtk-icons/hicolor/512x512/apps/{ICON_NAME}.png");
    if let Err(e) = crate::assets::extract_to_cache(ICON_BYTES, &icon_rel) {
        tracing::warn!("Failed to install icon: {e}");
        return;
    }

    let display = gdk::Display::default().expect("display");
    let icon_theme = gtk4::IconTheme::for_display(&display);
    icon_theme.add_search_path(cache_root.to_str().unwrap_or_default());

    gtk4::Window::set_default_icon_name(ICON_NAME);
}

/// Filter a conversation's messages based on debug mode.
///
/// When debug is off, only user and assistant messages are shown.
/// When debug is on, tool messages are included as well.
fn filter_messages(detail: &ConversationDetail, debug: bool) -> ConversationDetail {
    ConversationDetail {
        id: detail.id.clone(),
        title: detail.title.clone(),
        messages: detail
            .messages
            .iter()
            .filter(|m| {
                if debug {
                    return true;
                }
                match m.role.as_str() {
                    "user" => true,
                    // Hide empty assistant messages (tool_calls-only)
                    "assistant" => !m.content.trim().is_empty(),
                    _ => false,
                }
            })
            .cloned()
            .collect(),
        model_selection: detail.model_selection.clone(),
        conversation_personality: detail.conversation_personality,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Fixtures --------------------------------------------------------

    fn summary(id: &str, title: &str, archived: bool) -> ConversationSummary {
        ConversationSummary {
            id: id.to_string(),
            title: title.to_string(),
            message_count: 0,
            archived,
        }
    }

    fn msg(role: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: role.to_string(),
            content: content.to_string(),
        }
    }

    fn detail(id: &str, messages: Vec<ChatMessage>) -> ConversationDetail {
        ConversationDetail {
            id: id.to_string(),
            title: format!("conv {id}"),
            messages,
            model_selection: None,
            conversation_personality: None,
        }
    }

    fn selection(connection_id: &str, model_id: &str) -> api::ConversationModelSelectionView {
        api::ConversationModelSelectionView {
            connection_id: connection_id.to_string(),
            model_id: model_id.to_string(),
            effort: None,
        }
    }

    fn listing(connection_id: &str, model_id: &str) -> api::ModelListing {
        api::ModelListing {
            connection_id: connection_id.to_string(),
            connection_label: connection_id.to_string(),
            model: api::ModelInfoView {
                id: model_id.to_string(),
                display_name: model_id.to_string(),
                context_limit: None,
                capabilities: api::ModelCapabilitiesView::default(),
            },
        }
    }

    // --- __pending__ sentinel handoff (#31) ------------------------------

    #[test]
    fn prompt_sent_sets_pending_sentinel_and_clears_buffer() {
        let mut state = WindowState {
            streaming_buffer: "leftover".to_string(),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::PromptSent {
            task_id: "ack-1".to_string(),
        });
        assert!(effects.is_empty(), "PromptSent performs no widget effects");
        assert_eq!(state.pending_request_id.as_deref(), Some("__pending__"));
        assert!(state.streaming_buffer.is_empty());
    }

    #[test]
    fn first_stream_chunk_claims_real_request_id_from_pending_sentinel() {
        let mut state = WindowState {
            pending_request_id: Some("__pending__".to_string()),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::StreamChunk {
            request_id: "req-real".to_string(),
            chunk: "hello".to_string(),
        });
        // Sentinel is replaced by the daemon's real request id...
        assert_eq!(state.pending_request_id.as_deref(), Some("req-real"));
        assert_eq!(state.streaming_buffer, "hello");
        // ...and because this is the first chunk, the chat status is cleared
        // before the chunk is rendered.
        assert!(
            matches!(effects.as_slice(), [Effect::ClearChatStatus, Effect::ReceiveChunk(c)] if c == "hello"),
            "unexpected effects: {effects:?}"
        );
    }

    #[test]
    fn subsequent_stream_chunk_appends_without_clearing_status() {
        let mut state = WindowState {
            pending_request_id: Some("req-real".to_string()),
            streaming_buffer: "hello".to_string(),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::StreamChunk {
            request_id: "req-real".to_string(),
            chunk: " world".to_string(),
        });
        assert_eq!(state.streaming_buffer, "hello world");
        // Non-first chunk: only the chunk is rendered, no status clear.
        assert!(
            matches!(effects.as_slice(), [Effect::ReceiveChunk(c)] if c == " world"),
            "unexpected effects: {effects:?}"
        );
    }

    #[test]
    fn stream_chunk_for_unrelated_request_id_is_ignored() {
        let mut state = WindowState {
            pending_request_id: Some("req-real".to_string()),
            streaming_buffer: "hello".to_string(),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::StreamChunk {
            request_id: "some-other-req".to_string(),
            chunk: "noise".to_string(),
        });
        assert!(effects.is_empty(), "stray chunk must not render");
        assert_eq!(state.streaming_buffer, "hello", "buffer must be untouched");
    }

    #[test]
    fn assistant_status_matches_pending_sentinel_before_request_id_known() {
        let mut state = WindowState {
            pending_request_id: Some("__pending__".to_string()),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::AssistantStatus {
            request_id: "req-not-yet-claimed".to_string(),
            message: "Searching...".to_string(),
        });
        assert!(
            matches!(effects.as_slice(), [Effect::SetChatStatus(m)] if m == "Searching..."),
            "status during the __pending__ window must reach the chat: {effects:?}"
        );
    }

    #[test]
    fn stream_complete_claims_sentinel_appends_message_and_clears_pending() {
        let mut state = WindowState {
            pending_request_id: Some("__pending__".to_string()),
            streaming_buffer: "partial".to_string(),
            current_conversation: Some(detail("c1", vec![msg("user", "hi")])),
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::StreamComplete {
            request_id: "req-real".to_string(),
            full_response: "the answer".to_string(),
        });
        assert!(state.pending_request_id.is_none());
        assert!(state.streaming_buffer.is_empty());
        let conv = state.current_conversation.as_ref().unwrap();
        assert_eq!(conv.messages.last().unwrap().role, "assistant");
        assert_eq!(conv.messages.last().unwrap().content, "the answer");
        assert!(
            matches!(
                effects.as_slice(),
                [
                    Effect::ClearChatStatus,
                    Effect::CompleteStreaming(c),
                    Effect::FetchScratchpad(conv),
                ] if c == "the answer" && conv == "c1"
            ),
            "unexpected effects: {effects:?}"
        );
    }

    #[test]
    fn stream_error_clears_pending_and_sets_error_status() {
        let mut state = WindowState {
            pending_request_id: Some("req-real".to_string()),
            streaming_buffer: "partial".to_string(),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::StreamError {
            request_id: "req-real".to_string(),
            error: "boom".to_string(),
        });
        assert!(state.pending_request_id.is_none());
        assert!(state.streaming_buffer.is_empty());
        assert!(
            matches!(effects.as_slice(), [Effect::ClearChatStatus, Effect::SetStatusText(t)] if t == "Error: boom"),
            "unexpected effects: {effects:?}"
        );
    }

    #[test]
    fn disconnect_finalizes_in_progress_stream_with_connection_lost_marker() {
        let mut state = WindowState {
            pending_request_id: Some("req-real".to_string()),
            streaming_buffer: "half a thought".to_string(),
            current_conversation: Some(detail("c1", vec![])),
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::Disconnected {
            reason: "socket closed".to_string(),
        });
        assert!(state.pending_request_id.is_none());
        assert!(state.streaming_buffer.is_empty());
        // The partial response is committed to the conversation with the marker.
        let last = state
            .current_conversation
            .as_ref()
            .unwrap()
            .messages
            .last()
            .unwrap();
        assert_eq!(last.content, "half a thought\n\n[Connection lost]");
        // Effects: clear client, desensitize send, status text, then finalize.
        assert!(
            matches!(
                effects.as_slice(),
                [
                    Effect::ClearClient,
                    Effect::SetSendSensitive(false),
                    Effect::SetStatusText(t),
                    Effect::CompleteStreaming(c),
                ] if t == "Disconnected: socket closed" && c == "half a thought\n\n[Connection lost]"
            ),
            "unexpected effects: {effects:?}"
        );
    }

    #[test]
    fn disconnect_without_active_stream_does_not_emit_complete_streaming() {
        let mut state = WindowState::default();
        let effects = state.apply(UiMessage::Disconnected {
            reason: "bye".to_string(),
        });
        assert!(
            matches!(
                effects.as_slice(),
                [
                    Effect::ClearClient,
                    Effect::SetSendSensitive(false),
                    Effect::SetStatusText(_)
                ]
            ),
            "no streaming buffer => no CompleteStreaming: {effects:?}"
        );
    }

    // --- Archived-list refresh -------------------------------------------

    #[test]
    fn conversations_loaded_stores_list_and_refreshes_sidebar_then_ensures_active() {
        // The "show archived" toggle re-fetches and re-delivers the list via
        // ConversationsLoaded; apply must repaint the sidebar with the new
        // (possibly archived-including) set and re-run ensure-active.
        let mut state = WindowState::default();
        let convs = vec![
            summary("c1", "Active one", false),
            summary("c2", "Archived one", true),
        ];
        let effects = state.apply(UiMessage::ConversationsLoaded(convs.clone()));
        assert_eq!(state.conversations.len(), 2);
        assert_eq!(state.conversations[1].id, "c2");
        assert!(state.conversations[1].archived);
        match effects.as_slice() {
            [
                Effect::SetConversations(got),
                Effect::EnsureActiveConversation,
            ] => {
                assert_eq!(got.len(), 2);
                assert_eq!(got[1].id, "c2");
            }
            other => panic!("unexpected effects: {other:?}"),
        }
    }

    #[test]
    fn deleting_active_conversation_clears_chat_and_re_ensures_active() {
        let mut state = WindowState {
            conversations: vec![summary("c1", "one", false), summary("c2", "two", false)],
            current_conversation_id: Some("c1".to_string()),
            current_conversation: Some(detail("c1", vec![])),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::ConversationDeleted {
            id: "c1".to_string(),
        });
        assert_eq!(state.conversations.len(), 1);
        assert_eq!(state.conversations[0].id, "c2");
        assert!(state.current_conversation_id.is_none());
        assert!(state.current_conversation.is_none());
        assert!(
            matches!(
                effects.as_slice(),
                [
                    Effect::SetConversations(_),
                    Effect::ClearChat,
                    Effect::SidePaneSetScratchpad(_),
                    Effect::RefreshSidePaneTasks,
                    Effect::EnsureActiveConversation
                ]
            ),
            "deleting the active conversation must clear chat + side pane + re-ensure: {effects:?}"
        );
    }

    fn note_view(key: &str) -> api::ScratchpadNoteView {
        api::ScratchpadNoteView {
            id: format!("id-{key}"),
            key: key.to_string(),
            content: "x".to_string(),
            note_type: "note".to_string(),
            sequence: None,
            done: false,
            updated_at: "t".to_string(),
        }
    }

    #[test]
    fn scratchpad_loaded_applies_only_for_active_conversation() {
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        // Matching conversation → set the pane.
        let effects = state.apply(UiMessage::ConversationScratchpadLoaded {
            conversation_id: "c1".to_string(),
            notes: vec![note_view("goal")],
        });
        assert!(
            matches!(effects.as_slice(), [Effect::SidePaneSetScratchpad(n)] if n.len() == 1),
            "unexpected: {effects:?}"
        );
        // A fetch that resolves after a conversation switch is ignored.
        let effects = state.apply(UiMessage::ConversationScratchpadLoaded {
            conversation_id: "stale".to_string(),
            notes: vec![note_view("goal")],
        });
        assert!(effects.is_empty(), "stale scratchpad must be dropped");
    }

    #[test]
    fn scratchpad_changed_refetches_only_for_active_conversation() {
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::ScratchpadChanged {
            conversation_id: "c1".to_string(),
        });
        assert!(matches!(effects.as_slice(), [Effect::FetchScratchpad(c)] if c == "c1"));
        let effects = state.apply(UiMessage::ScratchpadChanged {
            conversation_id: "other".to_string(),
        });
        assert!(
            effects.is_empty(),
            "a change to another conversation is ignored"
        );
    }

    #[test]
    fn tasks_loaded_also_refreshes_the_side_pane() {
        let mut state = WindowState::default();
        let effects = state.apply(UiMessage::TasksLoaded(vec![]));
        assert!(matches!(
            effects.as_slice(),
            [Effect::TasksReplaceAll(_), Effect::RefreshSidePaneTasks]
        ));
    }

    #[test]
    fn deleting_inactive_conversation_only_refreshes_sidebar() {
        let mut state = WindowState {
            conversations: vec![summary("c1", "one", false), summary("c2", "two", false)],
            current_conversation_id: Some("c1".to_string()),
            current_conversation: Some(detail("c1", vec![])),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::ConversationDeleted {
            id: "c2".to_string(),
        });
        assert!(state.current_conversation_id.as_deref() == Some("c1"));
        assert!(
            matches!(effects.as_slice(), [Effect::SetConversations(got)] if got.len() == 1),
            "deleting an inactive conversation must not touch the chat: {effects:?}"
        );
    }

    #[test]
    fn rename_updates_matching_conversation_title_and_refreshes_sidebar() {
        let mut state = WindowState {
            conversations: vec![summary("c1", "old", false), summary("c2", "keep", false)],
            ..Default::default()
        };
        let effects = state.apply(UiMessage::ConversationRenamed {
            id: "c1".to_string(),
            title: "new title".to_string(),
        });
        assert_eq!(state.conversations[0].title, "new title");
        assert_eq!(state.conversations[1].title, "keep");
        match effects.as_slice() {
            [Effect::SetConversations(got)] => assert_eq!(got[0].title, "new title"),
            other => panic!("unexpected effects: {other:?}"),
        }
    }

    #[test]
    fn title_changed_signal_updates_matching_conversation_and_refreshes_sidebar() {
        let mut state = WindowState {
            conversations: vec![summary("c1", "untitled", false)],
            ..Default::default()
        };
        let effects = state.apply(UiMessage::TitleChanged {
            conversation_id: "c1".to_string(),
            title: "Auto Title".to_string(),
        });
        assert_eq!(state.conversations[0].title, "Auto Title");
        assert!(matches!(effects.as_slice(), [Effect::SetConversations(_)]));
    }

    // --- Debug filter ----------------------------------------------------

    #[test]
    fn conversation_loaded_hides_tool_messages_when_debug_off() {
        let mut state = WindowState {
            debug_enabled: false,
            ..Default::default()
        };
        let d = detail(
            "c1",
            vec![
                msg("user", "hi"),
                msg("tool", "tool noise"),
                msg("assistant", "answer"),
                msg("assistant", "   "), // empty (tool-calls only) assistant
            ],
        );
        let effects = state.apply(UiMessage::ConversationLoaded(d));
        // The cached (unfiltered) conversation keeps all 4 messages...
        assert_eq!(
            state.current_conversation.as_ref().unwrap().messages.len(),
            4
        );
        // ...but the chat view receives only user + non-empty assistant.
        match effects.as_slice() {
            [
                Effect::SetModelSelection(_),
                Effect::LoadConversationIntoChat(filtered),
                Effect::SidePaneSetScratchpad(_),
                Effect::RefreshSidePaneTasks,
                Effect::FetchScratchpad(_),
            ] => {
                let roles: Vec<&str> = filtered.messages.iter().map(|m| m.role.as_str()).collect();
                assert_eq!(roles, vec!["user", "assistant"]);
                assert_eq!(filtered.messages[1].content, "answer");
            }
            other => panic!("unexpected effects: {other:?}"),
        }
    }

    #[test]
    fn conversation_loaded_shows_tool_messages_when_debug_on() {
        let mut state = WindowState {
            debug_enabled: true,
            ..Default::default()
        };
        let d = detail(
            "c1",
            vec![
                msg("user", "hi"),
                msg("tool", "tool noise"),
                msg("assistant", "   "),
            ],
        );
        let effects = state.apply(UiMessage::ConversationLoaded(d));
        match effects.as_slice() {
            [
                Effect::SetModelSelection(_),
                Effect::LoadConversationIntoChat(filtered),
                Effect::SidePaneSetScratchpad(_),
                Effect::RefreshSidePaneTasks,
                Effect::FetchScratchpad(_),
            ] => {
                // Debug on: nothing is filtered out.
                assert_eq!(filtered.messages.len(), 3);
            }
            other => panic!("unexpected effects: {other:?}"),
        }
    }

    #[test]
    fn conversation_loaded_sets_active_id_and_applies_stored_model_selection() {
        let mut state = WindowState::default();
        let mut d = detail("c9", vec![msg("user", "hi")]);
        d.model_selection = Some(selection("work", "claude"));
        let effects = state.apply(UiMessage::ConversationLoaded(d));
        assert_eq!(state.current_conversation_id.as_deref(), Some("c9"));
        match effects.as_slice() {
            [
                Effect::SetModelSelection(Some(sel)),
                Effect::LoadConversationIntoChat(_),
                Effect::SidePaneSetScratchpad(_),
                Effect::RefreshSidePaneTasks,
                Effect::FetchScratchpad(conv),
            ] => {
                assert_eq!(sel.connection_id, "work");
                assert_eq!(sel.model_id, "claude");
                assert_eq!(conv, "c9");
            }
            other => panic!("unexpected effects: {other:?}"),
        }
    }

    // --- Model-picker re-application -------------------------------------

    #[test]
    fn models_loaded_does_not_touch_picker_selection() {
        // Regression (issue #72): a models refresh fires on every (re)connect.
        // It must NOT re-apply the conversation's stored selection — doing so
        // clobbered the user's in-memory pick back to stored-or-default on each
        // reconnect. `set_models` preserves the picker's `active`; the selection
        // is owned by ConversationLoaded (switch) and set_default_model.
        let mut conv = detail("c1", vec![]);
        conv.model_selection = Some(selection("work", "claude"));
        let mut state = WindowState {
            current_conversation: Some(conv),
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::ModelsLoaded(vec![listing("work", "claude")]));
        match effects.as_slice() {
            [
                Effect::SetModels(models),
                Effect::SetModelPickerVisible(true),
            ] => {
                assert_eq!(models.len(), 1);
            }
            other => panic!("ModelsLoaded must not emit SetModelSelection: {other:?}"),
        }
    }

    #[test]
    fn models_loaded_empty_list_hides_picker_and_skips_reapply_when_no_conversation() {
        let mut state = WindowState::default();
        let effects = state.apply(UiMessage::ModelsLoaded(Vec::new()));
        match effects.as_slice() {
            [
                Effect::SetModels(models),
                Effect::SetModelPickerVisible(false),
            ] => {
                assert!(models.is_empty());
            }
            other => panic!("unexpected effects (no conversation => no reapply): {other:?}"),
        }
    }

    // --- Reconnect: reload the active conversation without resetting picker --

    #[test]
    fn conversations_loaded_on_reconnect_reloads_active_conversation() {
        // Issue #72: on reconnect the (still-present) active conversation is
        // re-fetched via ReloadConversation — which refreshes the cache and
        // keeps the picker — instead of ConversationLoaded (which resets it).
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::ConversationsLoaded(vec![summary(
            "c1", "first", false,
        )]));
        match effects.as_slice() {
            [
                Effect::SetConversations(_),
                Effect::EnsureActiveConversation,
                Effect::ReloadConversation(id),
            ] => assert_eq!(id, "c1"),
            other => panic!("unexpected effects: {other:?}"),
        }
    }

    #[test]
    fn conversations_loaded_on_first_connect_does_not_reload() {
        // First connect: no active conversation yet, so the initial load runs
        // through EnsureActiveConversation -> ConversationLoaded (which sets the
        // picker). No ReloadConversation.
        let mut state = WindowState::default();
        let effects = state.apply(UiMessage::ConversationsLoaded(vec![summary(
            "c1", "first", false,
        )]));
        assert!(
            matches!(
                effects.as_slice(),
                [
                    Effect::SetConversations(_),
                    Effect::EnsureActiveConversation
                ]
            ),
            "first connect must not reload: {effects:?}"
        );
    }

    #[test]
    fn conversations_loaded_skips_reload_when_active_conversation_gone() {
        // The active conversation was deleted while disconnected: don't try to
        // reload it (EnsureActiveConversation switches to another / creates one).
        let mut state = WindowState {
            current_conversation_id: Some("gone".to_string()),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::ConversationsLoaded(vec![summary(
            "c1", "first", false,
        )]));
        assert!(
            matches!(
                effects.as_slice(),
                [
                    Effect::SetConversations(_),
                    Effect::EnsureActiveConversation
                ]
            ),
            "must not reload a conversation that's no longer present: {effects:?}"
        );
    }

    #[test]
    fn conversation_reloaded_refreshes_cache_and_chat_without_touching_picker() {
        // Issue #72: a reload refreshes the cached detail + chat but must NOT
        // emit SetModelSelection (the picker keeps the user's pick).
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        let mut d = detail("c1", vec![msg("user", "hi")]);
        d.model_selection = Some(selection("work", "claude"));
        let effects = state.apply(UiMessage::ConversationReloaded(d));
        assert!(
            state.current_conversation.is_some(),
            "cache must be updated"
        );
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::SetModelSelection(_))),
            "reload must not touch the picker: {effects:?}"
        );
        match effects.as_slice() {
            [
                Effect::LoadConversationIntoChat(_),
                Effect::SidePaneSetScratchpad(_),
                Effect::RefreshSidePaneTasks,
                Effect::FetchScratchpad(conv),
            ] => assert_eq!(conv, "c1"),
            other => panic!("unexpected effects: {other:?}"),
        }
    }

    #[test]
    fn conversation_reloaded_ignored_when_user_switched_away() {
        // A reload reply that arrives after the user switched conversations must
        // be dropped — it would otherwise overwrite the now-current chat.
        let mut state = WindowState {
            current_conversation_id: Some("c2".to_string()),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::ConversationReloaded(detail("c1", vec![])));
        assert!(
            effects.is_empty(),
            "stale reload for a non-active conversation must be a no-op: {effects:?}"
        );
    }

    #[test]
    fn default_model_loaded_emits_set_default_model_effect() {
        let mut state = WindowState::default();
        let default = crate::selected_models::SelectedModel {
            connection_id: "work".to_string(),
            model_id: "claude".to_string(),
        };
        let effects = state.apply(UiMessage::DefaultModelLoaded(Some(default.clone())));
        match effects.as_slice() {
            [Effect::SetDefaultModel(Some(got))] => {
                assert_eq!(got.connection_id, "work");
                assert_eq!(got.model_id, "claude");
            }
            other => panic!("unexpected effects: {other:?}"),
        }
    }

    #[test]
    fn default_model_loaded_none_emits_set_default_model_none() {
        let mut state = WindowState::default();
        let effects = state.apply(UiMessage::DefaultModelLoaded(None));
        assert!(
            matches!(effects.as_slice(), [Effect::SetDefaultModel(None)]),
            "unresolved default must still emit a (None) effect: {effects:?}"
        );
    }

    #[test]
    fn dangling_model_warning_for_current_conversation_clears_picker_and_cached_selection() {
        let mut conv = detail("c1", vec![]);
        conv.model_selection = Some(selection("old", "gone"));
        let mut state = WindowState {
            current_conversation: Some(conv),
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        let warning = api::ConversationWarning::DanglingModelSelection {
            previous_selection: selection("old", "gone"),
            fallback_to: selection("work", "claude"),
        };
        let effects = state.apply(UiMessage::ConversationWarning {
            conversation_id: "c1".to_string(),
            warning,
        });
        // Cached selection must be cleared so a later reload/switch doesn't
        // re-apply the stale dangling selection, contradicting the toast.
        assert!(
            state
                .current_conversation
                .as_ref()
                .unwrap()
                .model_selection
                .is_none()
        );
        match effects.as_slice() {
            [Effect::SetModelSelection(None), Effect::ShowToast(message)] => {
                assert!(message.contains("gone"));
                assert!(message.contains("claude"));
            }
            other => panic!("unexpected effects: {other:?}"),
        }
    }

    #[test]
    fn dangling_model_warning_for_other_conversation_only_toasts() {
        let mut conv = detail("c1", vec![]);
        conv.model_selection = Some(selection("old", "gone"));
        let mut state = WindowState {
            current_conversation: Some(conv),
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        let warning = api::ConversationWarning::DanglingModelSelection {
            previous_selection: selection("old", "gone"),
            fallback_to: selection("work", "claude"),
        };
        let effects = state.apply(UiMessage::ConversationWarning {
            conversation_id: "c2-not-current".to_string(),
            warning,
        });
        // Not the current conversation: don't touch the picker or cached
        // selection — only surface the advisory toast.
        assert!(
            state
                .current_conversation
                .as_ref()
                .unwrap()
                .model_selection
                .is_some()
        );
        assert!(
            matches!(effects.as_slice(), [Effect::ShowToast(_)]),
            "unexpected effects: {effects:?}"
        );
    }

    // --- Simple passthrough variants -------------------------------------

    #[test]
    fn status_update_sets_status_text_verbatim() {
        let mut state = WindowState::default();
        let effects = state.apply(UiMessage::StatusUpdate("Connecting".to_string()));
        assert!(matches!(effects.as_slice(), [Effect::SetStatusText(t)] if t == "Connecting"));
    }

    #[test]
    fn error_message_is_prefixed_in_status_bar() {
        let mut state = WindowState::default();
        let effects = state.apply(UiMessage::Error("nope".to_string()));
        assert!(matches!(effects.as_slice(), [Effect::SetStatusText(t)] if t == "Error: nope"));
    }

    #[test]
    fn connected_sets_label_and_enables_send() {
        let mut state = WindowState::default();
        let effects = state.apply(UiMessage::Connected {
            label: "Local daemon".to_string(),
        });
        assert!(
            matches!(
                effects.as_slice(),
                [Effect::SetStatusText(t), Effect::SetSendSensitive(true)] if t == "Local daemon"
            ),
            "unexpected effects: {effects:?}"
        );
    }

    #[test]
    fn conversation_created_sets_active_id_without_effects() {
        let mut state = WindowState::default();
        let effects = state.apply(UiMessage::ConversationCreated {
            id: "new-c".to_string(),
        });
        assert_eq!(state.current_conversation_id.as_deref(), Some("new-c"));
        assert!(effects.is_empty());
    }

    // --- Voice UI: You/Adele dropdowns + client tools (issue #80) --------

    /// A `say_this` client-tool call (#76, still used in #80). Convenience
    /// constructor for the tests below.
    fn say_this_call(conversation_id: &str, text: &str) -> UiMessage {
        UiMessage::ClientToolCall {
            task_id: "task-1".to_string(),
            conversation_id: conversation_id.to_string(),
            tool_call_id: "call-1".to_string(),
            tool_name: "say_this".to_string(),
            arguments: serde_json::json!({ "text": text }),
        }
    }

    /// A `request_voice` / `stop_voice` client-tool call (#80). Convenience
    /// constructor mirroring `say_this_call`.
    fn voice_tool_call(conversation_id: &str, tool_name: &str) -> UiMessage {
        UiMessage::ClientToolCall {
            task_id: "task-v".to_string(),
            conversation_id: conversation_id.to_string(),
            tool_call_id: "call-v".to_string(),
            tool_name: tool_name.to_string(),
            arguments: serde_json::json!({}),
        }
    }

    /// A `WindowState` pinned to conversation `c1` with the given `You:` and
    /// `Adele:` settings — the common test fixture for the gate tests below.
    fn state_with(voice_in: bool, adele: AdeleOutput) -> WindowState {
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        state.conversation_voice_in.insert("c1".to_string(), voice_in);
        state.conversation_adele_output.insert("c1".to_string(), adele);
        state
    }

    /// A `StreamComplete` for `c1` carrying `full_response`, against a freshly
    /// pinned pending request — the reply-narration trigger.
    fn stream_complete_in(state: &mut WindowState, full_response: &str) -> Vec<Effect> {
        state.pending_request_id = Some("req".to_string());
        state.current_conversation = Some(detail("c1", vec![]));
        state.apply(UiMessage::StreamComplete {
            request_id: "req".to_string(),
            full_response: full_response.to_string(),
        })
    }

    /// Default (You=Disabled, Adele=Disabled): both controls default off for an
    /// untouched conversation, so no audio path can fire.
    #[test]
    fn defaults_are_voice_in_disabled_and_adele_disabled() {
        let state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        assert!(
            !state.voice_in_for_current(),
            "You must default Disabled for an untouched conversation"
        );
        assert_eq!(
            state.adele_output_for_current(),
            AdeleOutput::Disabled,
            "Adele must default Disabled for an untouched conversation"
        );
        assert!(!state.narrate_for_current(), "default gate must be closed");
        assert!(
            !state.say_this_spoken_for_current(),
            "default say_this must downgrade to inline"
        );
    }

    /// Default: a `say_this` produces the inline `(speech mode disabled) …`
    /// downgrade, NO `Speak`, and ALWAYS a `SubmitClientToolResult` (the turn
    /// completes, can't hang).
    #[test]
    fn default_say_this_renders_inline_and_resolves_without_audio() {
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        let effects = state.apply(say_this_call("c1", "the aside"));
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::Speak(_))),
            "Adele Disabled must never produce a Speak effect: {effects:?}"
        );
        let inline = effects.iter().any(
            |e| matches!(e, Effect::AddInlineNote(t) if t == "(speech mode disabled) the aside"),
        );
        assert!(inline, "expected the inline downgrade note: {effects:?}");
        let resolved = effects.iter().any(|e| {
            matches!(
                e,
                Effect::SubmitClientToolResult { task_id, tool_call_id, result: Ok(_) }
                    if task_id == "task-1" && tool_call_id == "call-1"
            )
        });
        assert!(resolved, "say_this must always resolve a result: {effects:?}");
    }

    /// Adele=Always: every reply is spoken (and finalized), independent of You.
    #[test]
    fn adele_always_speaks_every_reply_regardless_of_you() {
        for voice_in in [false, true] {
            let mut state = state_with(voice_in, AdeleOutput::Always);
            assert!(
                state.narrate_for_current(),
                "Always must narrate (You={voice_in})"
            );
            let effects = stream_complete_in(&mut state, "an answer");
            assert!(
                effects
                    .iter()
                    .any(|e| matches!(e, Effect::Speak(t) if t == "an answer")),
                "Always must speak the reply (You={voice_in}): {effects:?}"
            );
            assert!(
                effects
                    .iter()
                    .any(|e| matches!(e, Effect::CompleteStreaming(t) if t == "an answer")),
                "the reply text must still be finalized: {effects:?}"
            );
        }
    }

    /// Adele=Always: a `say_this` aside is spoken (Adele ∈ {OnDemand, Always}).
    #[test]
    fn adele_always_speaks_say_this_aside() {
        let mut state = state_with(false, AdeleOutput::Always);
        let effects = state.apply(say_this_call("c1", "hello aloud"));
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::Speak(t) if t == "hello aloud")),
            "Always must speak a say_this aside: {effects:?}"
        );
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::AddInlineNote(_))),
            "no inline downgrade when spoken: {effects:?}"
        );
        assert!(
            effects.iter().any(|e| matches!(
                e,
                Effect::SubmitClientToolResult { result: Ok(r), .. } if r == "spoken"
            )),
            "result must be \"spoken\": {effects:?}"
        );
    }

    /// Adele=OnDemand + You=Enabled: the reply is spoken (the gate's OnDemand
    /// arm) and finalized.
    #[test]
    fn adele_on_demand_with_you_enabled_speaks_reply() {
        let mut state = state_with(true, AdeleOutput::OnDemand);
        assert!(state.narrate_for_current(), "OnDemand + You=Enabled narrates");
        let effects = stream_complete_in(&mut state, "an answer");
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::Speak(t) if t == "an answer")),
            "OnDemand + You=Enabled must speak the reply: {effects:?}"
        );
    }

    /// Adele=OnDemand + You=Disabled: the reply is NOT spoken (text-only), but a
    /// `say_this` aside still speaks (asides ignore You).
    #[test]
    fn adele_on_demand_with_you_disabled_text_only_but_say_this_speaks() {
        // Reply NOT narrated.
        let mut state = state_with(false, AdeleOutput::OnDemand);
        assert!(
            !state.narrate_for_current(),
            "OnDemand + You=Disabled must not narrate replies"
        );
        let effects = stream_complete_in(&mut state, "an answer");
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::Speak(_))),
            "OnDemand + You=Disabled must not speak the reply: {effects:?}"
        );
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::CompleteStreaming(t) if t == "an answer")),
            "the reply text must still be finalized: {effects:?}"
        );

        // say_this aside STILL speaks (independent of You).
        let mut state = state_with(false, AdeleOutput::OnDemand);
        let effects = state.apply(say_this_call("c1", "an aside"));
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::Speak(t) if t == "an aside")),
            "OnDemand say_this aside must speak even when You=Disabled: {effects:?}"
        );
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::AddInlineNote(_))),
            "no inline downgrade when spoken: {effects:?}"
        );
    }

    /// `request_voice` sets Adele=OnDemand for the active conversation, reflects
    /// the dropdown, and ALWAYS resolves a result (no audio by itself).
    #[test]
    fn request_voice_sets_on_demand_reflects_and_resolves() {
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        let effects = state.apply(voice_tool_call("c1", "request_voice"));
        assert_eq!(
            state.adele_output_for_current(),
            AdeleOutput::OnDemand,
            "request_voice must set Adele=OnDemand for the active conversation"
        );
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::SetAdeleOutputDropdown(AdeleOutput::OnDemand))),
            "request_voice must reflect OnDemand on the dropdown: {effects:?}"
        );
        assert!(
            effects.iter().any(|e| matches!(
                e,
                Effect::SubmitClientToolResult { task_id, tool_call_id, result: Ok(_) }
                    if task_id == "task-v" && tool_call_id == "call-v"
            )),
            "request_voice must resolve an Ok result: {effects:?}"
        );
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::Speak(_))),
            "request_voice itself must not speak: {effects:?}"
        );
    }

    /// `stop_voice` sets Adele=Disabled, reflects the dropdown, and ALWAYS
    /// resolves a result.
    #[test]
    fn stop_voice_sets_disabled_reflects_and_resolves() {
        let mut state = state_with(true, AdeleOutput::Always);
        let effects = state.apply(voice_tool_call("c1", "stop_voice"));
        assert_eq!(
            state.adele_output_for_current(),
            AdeleOutput::Disabled,
            "stop_voice must set Adele=Disabled"
        );
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::SetAdeleOutputDropdown(AdeleOutput::Disabled))),
            "stop_voice must reflect Disabled on the dropdown: {effects:?}"
        );
        assert!(
            effects.iter().any(|e| matches!(
                e,
                Effect::SubmitClientToolResult { task_id, tool_call_id, result: Ok(_) }
                    if task_id == "task-v" && tool_call_id == "call-v"
            )),
            "stop_voice must resolve an Ok result: {effects:?}"
        );
    }

    /// Every client-tool call emits exactly one result (no wedge, no double),
    /// across say_this / request_voice / stop_voice / an unknown tool.
    #[test]
    fn every_client_tool_call_emits_exactly_one_result() {
        let calls = [
            say_this_call("c1", "x"),
            voice_tool_call("c1", "request_voice"),
            voice_tool_call("c1", "stop_voice"),
            UiMessage::ClientToolCall {
                task_id: "t".to_string(),
                conversation_id: "c1".to_string(),
                tool_call_id: "tc".to_string(),
                tool_name: "frobnicate".to_string(),
                arguments: serde_json::json!({}),
            },
        ];
        for call in calls {
            let mut state = WindowState {
                current_conversation_id: Some("c1".to_string()),
                ..Default::default()
            };
            let effects = state.apply(call);
            let results = effects
                .iter()
                .filter(|e| matches!(e, Effect::SubmitClientToolResult { .. }))
                .count();
            assert_eq!(
                results, 1,
                "exactly one result per client-tool call: {effects:?}"
            );
        }
    }

    /// An unknown client tool the GTK client can't run still ALWAYS gets an
    /// `Err` result (no audio), so the suspended turn resumes rather than
    /// wedging.
    #[test]
    fn unknown_client_tool_always_resolves_with_error_result() {
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::ClientToolCall {
            task_id: "task-2".to_string(),
            conversation_id: "c1".to_string(),
            tool_call_id: "call-2".to_string(),
            tool_name: "frobnicate".to_string(),
            arguments: serde_json::json!({}),
        });
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::Speak(_))),
            "an unknown tool must not produce audio: {effects:?}"
        );
        assert!(
            effects.iter().any(|e| matches!(
                e,
                Effect::SubmitClientToolResult { task_id, tool_call_id, result: Err(_) }
                    if task_id == "task-2" && tool_call_id == "call-2"
            )),
            "an unrunnable tool must resolve with an Err result: {effects:?}"
        );
    }

    /// Malformed `say_this` arguments (missing/invalid `text`) must not panic
    /// and must resolve with an `Err` result (never unwrap), even with Adele on.
    #[test]
    fn say_this_with_malformed_arguments_resolves_error_not_panic() {
        let mut state = state_with(true, AdeleOutput::Always);
        let effects = state.apply(UiMessage::ClientToolCall {
            task_id: "task-3".to_string(),
            conversation_id: "c1".to_string(),
            tool_call_id: "call-3".to_string(),
            tool_name: "say_this".to_string(),
            // `text` missing entirely.
            arguments: serde_json::json!({ "wrong": 5 }),
        });
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::Speak(_))),
            "malformed say_this must not speak: {effects:?}"
        );
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::SubmitClientToolResult { result: Err(_), .. })),
            "malformed say_this must resolve an Err result: {effects:?}"
        );
    }

    /// `request_voice` / `stop_voice` with malformed/non-object args still
    /// resolve exactly one result without panicking (they take no arguments).
    #[test]
    fn voice_tools_with_malformed_args_resolve_without_panic() {
        for tool in ["request_voice", "stop_voice"] {
            let mut state = WindowState {
                current_conversation_id: Some("c1".to_string()),
                ..Default::default()
            };
            let effects = state.apply(UiMessage::ClientToolCall {
                task_id: "t".to_string(),
                conversation_id: "c1".to_string(),
                tool_call_id: "tc".to_string(),
                tool_name: tool.to_string(),
                arguments: serde_json::json!("not-an-object"),
            });
            let results = effects
                .iter()
                .filter(|e| matches!(e, Effect::SubmitClientToolResult { .. }))
                .count();
            assert_eq!(results, 1, "{tool} must resolve exactly one result: {effects:?}");
        }
    }

    /// Both controls are per-conversation and isolated: setting them on c1 must
    /// not leak into c2, and they stick when switching back.
    #[test]
    fn both_controls_are_per_conversation_isolated() {
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        state.apply(UiMessage::SetVoiceIn {
            conversation_id: "c1".to_string(),
            enabled: true,
        });
        state.apply(UiMessage::SetAdeleOutput {
            conversation_id: "c1".to_string(),
            level: AdeleOutput::Always,
        });
        assert!(state.voice_in_for_current());
        assert_eq!(state.adele_output_for_current(), AdeleOutput::Always);

        // Switch to c2: both inherit their defaults (no bleed).
        state.current_conversation_id = Some("c2".to_string());
        assert!(!state.voice_in_for_current(), "You must not leak c1 → c2");
        assert_eq!(
            state.adele_output_for_current(),
            AdeleOutput::Disabled,
            "Adele must not leak c1 → c2"
        );

        // Back to c1: both stick.
        state.current_conversation_id = Some("c1".to_string());
        assert!(state.voice_in_for_current());
        assert_eq!(state.adele_output_for_current(), AdeleOutput::Always);
    }

    /// A `request_voice` tagged for a non-active conversation applies to the
    /// ACTIVE conversation (mirrors say_this's gating) and never writes the
    /// foreign conversation's state — but still resolves.
    #[test]
    fn request_voice_for_other_conversation_uses_active_no_bleed() {
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        let effects = state.apply(voice_tool_call("c2", "request_voice"));
        assert_eq!(
            state.adele_output_for_current(),
            AdeleOutput::OnDemand,
            "request_voice applies to the active conversation"
        );
        assert!(
            !state.conversation_adele_output.contains_key("c2"),
            "a foreign-tagged call must not write c2's state: {:?}",
            state.conversation_adele_output
        );
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::SubmitClientToolResult { .. })),
            "still always resolves: {effects:?}"
        );
    }

    /// A `say_this` whose `conversation_id` differs from the active one is judged
    /// against the *active* conversation's Adele level, never another's (no
    /// cross-conversation bleed).
    #[test]
    fn say_this_for_other_conversation_uses_active_level_no_bleed() {
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        // c2 has Adele=Always, but the active conversation is c1 (Disabled). A
        // call tagged c2 must NOT borrow c2's level to play audio on this client.
        state
            .conversation_adele_output
            .insert("c2".to_string(), AdeleOutput::Always);
        let effects = state.apply(say_this_call("c2", "should not play"));
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::Speak(_))),
            "a call for a non-active conversation must not borrow its level: {effects:?}"
        );
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::SubmitClientToolResult { .. })),
            "still always resolves: {effects:?}"
        );
    }

    /// `refinement_for_send` returns the right variant per (Adele level, You):
    /// Disabled → none; OnDemand → the brief/conversational refinement; Always →
    /// the speakable-but-full refinement. `You` does not change the refinement
    /// (it's chosen by the Adele level), and both refinement strings are
    /// non-empty and free of markdown markers so they can't leak formatting.
    #[test]
    fn refinement_for_send_returns_right_variant_per_level() {
        // Disabled → none (independent of You).
        for voice_in in [false, true] {
            let state = state_with(voice_in, AdeleOutput::Disabled);
            assert!(
                refinement_for_send(&state).is_none(),
                "Adele=Disabled must attach no refinement (You={voice_in})"
            );
        }
        // OnDemand → the brief refinement (independent of You).
        for voice_in in [false, true] {
            let state = state_with(voice_in, AdeleOutput::OnDemand);
            assert_eq!(
                refinement_for_send(&state),
                Some(ON_DEMAND_SYSTEM_REFINEMENT),
                "Adele=OnDemand must attach the brief refinement (You={voice_in})"
            );
        }
        // Always → the full refinement (independent of You).
        for voice_in in [false, true] {
            let state = state_with(voice_in, AdeleOutput::Always);
            assert_eq!(
                refinement_for_send(&state),
                Some(ALWAYS_SYSTEM_REFINEMENT),
                "Adele=Always must attach the full refinement (You={voice_in})"
            );
        }
        // The two refinements differ, are non-empty, and carry no markdown.
        assert_ne!(ON_DEMAND_SYSTEM_REFINEMENT, ALWAYS_SYSTEM_REFINEMENT);
        // OnDemand asks for brevity; Always explicitly does not shorten.
        assert!(ON_DEMAND_SYSTEM_REFINEMENT.to_lowercase().contains("brief"));
        assert!(ALWAYS_SYSTEM_REFINEMENT.to_lowercase().contains("do not shorten"));
        for refinement in [ON_DEMAND_SYSTEM_REFINEMENT, ALWAYS_SYSTEM_REFINEMENT] {
            assert!(!refinement.trim().is_empty());
            for marker in ['*', '_', '`', '#'] {
                assert!(
                    !refinement.contains(marker),
                    "the refinement itself must avoid markdown markers ({marker})"
                );
            }
        }
    }

    /// A user-driven `SetVoiceIn` records the per-conversation state and emits
    /// no effects.
    #[test]
    fn set_voice_in_records_state_scoped_to_conversation() {
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::SetVoiceIn {
            conversation_id: "c1".to_string(),
            enabled: true,
        });
        assert!(effects.is_empty(), "setting You emits no effects");
        assert!(state.voice_in_for_current());
        state.current_conversation_id = Some("c2".to_string());
        assert!(
            !state.voice_in_for_current(),
            "You set on c1 must not leak into c2"
        );
    }

    /// A user-driven `SetAdeleOutput` records the per-conversation level and
    /// emits no effects.
    #[test]
    fn set_adele_output_records_state_scoped_to_conversation() {
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::SetAdeleOutput {
            conversation_id: "c1".to_string(),
            level: AdeleOutput::OnDemand,
        });
        assert!(effects.is_empty(), "setting Adele emits no effects");
        assert_eq!(state.adele_output_for_current(), AdeleOutput::OnDemand);
        state.current_conversation_id = Some("c2".to_string());
        assert_eq!(
            state.adele_output_for_current(),
            AdeleOutput::Disabled,
            "Adele set on c1 must not leak into c2"
        );
    }
}
