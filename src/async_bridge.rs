use std::future::Future;
use std::sync::Arc;
use std::sync::OnceLock;

use desktop_assistant_api_model as api;
use desktop_assistant_client_common::SignalEvent;
use desktop_assistant_client_common::{AssistantClient, ConnectionConfig, Connector};
use gtk4::glib;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

fn runtime() -> &'static Runtime {
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to create tokio runtime")
    })
}

/// Messages sent from async tasks back to the GTK main thread.
pub enum UiMessage {
    /// A freshly connected [`Connector`] handed off to the GTK main thread.
    /// `Arc<Connector>` is `Send`, so this rides the same channel as
    /// every other UI message (replacing the former dedicated `InternalMsg`
    /// channel). Sent by `connection_manager` immediately after `Connected`
    /// and before `ConversationsLoaded`, so the window's client cell is
    /// populated before any handler that needs it runs. The `Connector` owns
    /// the transport (`connector.client()`) and the signal stream; the window
    /// shares this same handle wherever it previously held an
    /// `Arc<TransportClient>`.
    ClientReady(Arc<Connector>),
    ConversationsLoaded(Vec<desktop_assistant_client_common::ConversationSummary>),
    ConversationLoaded(desktop_assistant_client_common::ConversationDetail),
    /// A conversation that is *already open* was re-fetched (on reconnect, or
    /// after a debug/personality refresh). The window refreshes the cached
    /// detail + chat but, unlike [`ConversationLoaded`], does NOT reset the
    /// model picker — the user's selection (sent or unsent) must survive a
    /// reconnect (issue #72).
    ConversationReloaded(desktop_assistant_client_common::ConversationDetail),
    ConversationCreated {
        id: String,
    },
    ConversationDeleted {
        id: String,
    },
    ConversationRenamed {
        id: String,
        title: String,
    },
    StreamChunk {
        request_id: String,
        chunk: String,
    },
    StreamComplete {
        request_id: String,
        full_response: String,
    },
    StreamError {
        request_id: String,
        error: String,
    },
    AssistantStatus {
        request_id: String,
        message: String,
    },
    TitleChanged {
        conversation_id: String,
        title: String,
    },
    /// A one-time advisory for a conversation emitted as a live signal
    /// (today only `DanglingModelSelection`: the stored model selection no
    /// longer resolves and was cleared server-side). Drives a passive toast
    /// in the window. Replaces the earlier lossy `StatusUpdate`-string
    /// mapping so the handler can act on the typed warning.
    ConversationWarning {
        conversation_id: String,
        warning: api::ConversationWarning,
    },
    /// The wire ack carries a `task_id` (post-#114 `SendMessageAck`) or an
    /// empty string (legacy `Ack`). It is NOT the chunk-stream
    /// `request_id` — that is server-generated and arrives embedded in
    /// the first `AssistantDelta`. See issue #31.
    PromptSent {
        // Staged for streaming-chunk correlation (#31): consumer currently
        // ignores it (`PromptSent { task_id: _ }`), but the ack value is kept
        // on the message so the streaming work can correlate without a wire
        // change. See the variant doc above and issues #114/#31.
        #[allow(dead_code)]
        task_id: String,
    },
    /// Available (connection, model) pairs, fetched once on connect.
    /// Empty list means the picker should hide (e.g. D-Bus transport).
    ModelsLoaded(Vec<api::ModelListing>),
    /// The resolved interactive-purpose default model, fetched via
    /// `GetPurposes` on connect (and re-fetched after Settings edits). The
    /// picker uses it as the fallback selection for conversations with no
    /// stored selection, so the button always shows a concrete model instead
    /// of a "(default)" placeholder. `None` when it can't be resolved (the
    /// command failed, the interactive purpose is unset, or it uses the
    /// "primary"/inherit sentinel) — the picker then degrades to "Model".
    DefaultModelLoaded(Option<crate::selected_models::SelectedModel>),
    Connected {
        label: String,
    },
    Disconnected {
        reason: String,
    },
    StatusUpdate(String),
    Error(String),

    // --- Background tasks (issue #19) -------------------------------------
    //
    // The connection manager forwards `Event::Task*` frames into these
    // variants so the GTK main thread can update the process-manager panel
    // without touching tokio or the WebSocket directly. `TasksLoaded` is
    // produced by the initial `ListBackgroundTasks` snapshot taken on
    // connect (and on reconnect — see `connection_manager`).
    TasksLoaded(Vec<api::TaskView>),
    // The four streaming variants below carry the daemon's
    // `Event::Task*` frames into the GTK main thread via the
    // `SignalEvent::Task*` family on `client-common` (issue #22).
    TaskStarted(api::TaskView),
    TaskProgress {
        id: String,
        progress_hint: Option<String>,
    },
    TaskLogAppended {
        id: String,
        entry: api::TaskLogEntry,
    },
    TaskCompleted {
        id: String,
    },

    // --- Conversation scratchpad (issue #60) ------------------------------
    /// The scratchpad notes for a conversation, fetched via
    /// `GetConversationScratchpad` after a load / turn-complete / change event.
    /// The window applies it to the side pane only when it matches the active
    /// conversation.
    ConversationScratchpadLoaded {
        conversation_id: String,
        notes: Vec<api::ScratchpadNoteView>,
    },
    /// A conversation's scratchpad changed (the LLM's tools or a client command
    /// mutated it). Carried from `SignalEvent::ScratchpadChanged`; the window
    /// re-fetches when it matches the active conversation.
    ScratchpadChanged {
        conversation_id: String,
    },
}

// Manual `Debug` (can't derive: `Connector` is not `Debug`). Only
// `ClientReady` needs special handling — it prints a marker instead of the
// opaque connector; every other variant forwards its fields as before so
// the test panic messages stay informative.
impl std::fmt::Debug for UiMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UiMessage::ClientReady(_) => {
                f.debug_tuple("ClientReady").field(&"<connector>").finish()
            }
            UiMessage::ConversationsLoaded(v) => {
                f.debug_tuple("ConversationsLoaded").field(v).finish()
            }
            UiMessage::ConversationLoaded(v) => {
                f.debug_tuple("ConversationLoaded").field(v).finish()
            }
            UiMessage::ConversationReloaded(v) => {
                f.debug_tuple("ConversationReloaded").field(v).finish()
            }
            UiMessage::ConversationCreated { id } => f
                .debug_struct("ConversationCreated")
                .field("id", id)
                .finish(),
            UiMessage::ConversationDeleted { id } => f
                .debug_struct("ConversationDeleted")
                .field("id", id)
                .finish(),
            UiMessage::ConversationRenamed { id, title } => f
                .debug_struct("ConversationRenamed")
                .field("id", id)
                .field("title", title)
                .finish(),
            UiMessage::StreamChunk { request_id, chunk } => f
                .debug_struct("StreamChunk")
                .field("request_id", request_id)
                .field("chunk", chunk)
                .finish(),
            UiMessage::StreamComplete {
                request_id,
                full_response,
            } => f
                .debug_struct("StreamComplete")
                .field("request_id", request_id)
                .field("full_response", full_response)
                .finish(),
            UiMessage::StreamError { request_id, error } => f
                .debug_struct("StreamError")
                .field("request_id", request_id)
                .field("error", error)
                .finish(),
            UiMessage::AssistantStatus {
                request_id,
                message,
            } => f
                .debug_struct("AssistantStatus")
                .field("request_id", request_id)
                .field("message", message)
                .finish(),
            UiMessage::TitleChanged {
                conversation_id,
                title,
            } => f
                .debug_struct("TitleChanged")
                .field("conversation_id", conversation_id)
                .field("title", title)
                .finish(),
            UiMessage::ConversationWarning {
                conversation_id,
                warning,
            } => f
                .debug_struct("ConversationWarning")
                .field("conversation_id", conversation_id)
                .field("warning", warning)
                .finish(),
            UiMessage::PromptSent { task_id } => f
                .debug_struct("PromptSent")
                .field("task_id", task_id)
                .finish(),
            UiMessage::ModelsLoaded(v) => f.debug_tuple("ModelsLoaded").field(v).finish(),
            UiMessage::DefaultModelLoaded(v) => {
                f.debug_tuple("DefaultModelLoaded").field(v).finish()
            }
            UiMessage::Connected { label } => {
                f.debug_struct("Connected").field("label", label).finish()
            }
            UiMessage::Disconnected { reason } => f
                .debug_struct("Disconnected")
                .field("reason", reason)
                .finish(),
            UiMessage::StatusUpdate(s) => f.debug_tuple("StatusUpdate").field(s).finish(),
            UiMessage::Error(s) => f.debug_tuple("Error").field(s).finish(),
            UiMessage::TasksLoaded(v) => f.debug_tuple("TasksLoaded").field(v).finish(),
            UiMessage::TaskStarted(v) => f.debug_tuple("TaskStarted").field(v).finish(),
            UiMessage::TaskProgress { id, progress_hint } => f
                .debug_struct("TaskProgress")
                .field("id", id)
                .field("progress_hint", progress_hint)
                .finish(),
            UiMessage::TaskLogAppended { id, entry } => f
                .debug_struct("TaskLogAppended")
                .field("id", id)
                .field("entry", entry)
                .finish(),
            UiMessage::TaskCompleted { id } => {
                f.debug_struct("TaskCompleted").field("id", id).finish()
            }
            UiMessage::ConversationScratchpadLoaded {
                conversation_id,
                notes,
            } => f
                .debug_struct("ConversationScratchpadLoaded")
                .field("conversation_id", conversation_id)
                .field("notes", notes)
                .finish(),
            UiMessage::ScratchpadChanged { conversation_id } => f
                .debug_struct("ScratchpadChanged")
                .field("conversation_id", conversation_id)
                .finish(),
        }
    }
}

/// Bridge between the GTK main loop and tokio async tasks.
///
/// Uses a tokio mpsc channel + `glib::spawn_future_local` to dispatch
/// messages onto the GTK main thread.
pub struct AsyncBridge {
    ui_tx: mpsc::UnboundedSender<UiMessage>,
}

impl AsyncBridge {
    /// Create a new bridge. `handler` is called on the GTK main thread for
    /// each UiMessage and receives a clone of the sender so it can fire
    /// follow-up async work (e.g. auto-loading a conversation when the list
    /// arrives) and feed the results back through the same channel.
    pub fn new<F>(handler: F) -> Self
    where
        F: Fn(UiMessage, &mpsc::UnboundedSender<UiMessage>) + 'static,
    {
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiMessage>();
        let handler_tx = ui_tx.clone();

        // Spawn a local future on the GLib main context to receive messages
        glib::spawn_future_local(async move {
            while let Some(msg) = ui_rx.recv().await {
                handler(msg, &handler_tx);
            }
        });

        Self { ui_tx }
    }

    /// Spawn an async task on the tokio runtime.
    pub fn spawn<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        runtime().spawn(future);
    }

    /// Get a clone of the UI sender for passing into async tasks.
    pub fn ui_sender(&self) -> mpsc::UnboundedSender<UiMessage> {
        self.ui_tx.clone()
    }
}

/// Spawn an async task on the shared tokio runtime (usable without an AsyncBridge instance).
pub fn spawn_on_runtime<F>(future: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    runtime().spawn(future);
}

/// Persistent connection lifecycle: connect → forward signals → detect
/// disconnect → reconnect with exponential backoff.
///
/// Exits when `ui_tx` is closed (GTK window gone).
pub async fn connection_manager(config: ConnectionConfig, ui_tx: mpsc::UnboundedSender<UiMessage>) {
    const INITIAL_BACKOFF_SECS: u64 = 2;
    const MAX_BACKOFF_SECS: u64 = 30;

    let mut backoff_secs = INITIAL_BACKOFF_SECS;

    loop {
        match Connector::connect(&config).await {
            Ok(connector) => {
                backoff_secs = INITIAL_BACKOFF_SECS;
                // Subscribe before issuing any prompt so no early chunk is
                // lost; the fanout pump inside the `Connector` keeps running as
                // long as the `Arc<Connector>` is alive (held by the window).
                let mut signal_rx = connector.subscribe();
                let connector = Arc::new(connector);
                // `client()` borrows the transport owned by the `Connector`;
                // the connector outlives every use below (and the shared
                // `Arc` clone handed to the GTK thread keeps the pump alive).
                let transport = connector.client();

                let label = connector.label().to_string();
                if ui_tx.send(UiMessage::Connected { label }).is_err() {
                    return;
                }

                // Hand the connector to the GTK main thread before the
                // conversation list (which needs it). Same channel, so
                // delivery stays ordered.
                if ui_tx
                    .send(UiMessage::ClientReady(Arc::clone(&connector)))
                    .is_err()
                {
                    return;
                }

                // Refresh conversation list on connect
                match transport.list_conversations().await {
                    Ok(convs) => {
                        if ui_tx.send(UiMessage::ConversationsLoaded(convs)).is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        if ui_tx
                            .send(UiMessage::Error(format!("Load conversations: {e}")))
                            .is_err()
                        {
                            return;
                        }
                    }
                }

                // Fetch available models when the transport supports it
                // (the command channel — Uds and Ws; the D-Bus interface
                // doesn't expose this command).
                let listings = match transport.as_commands() {
                    Some(cmds) => cmds
                        .list_available_models(None, false)
                        .await
                        .unwrap_or_else(|e| {
                            tracing::warn!("list_available_models failed: {e}");
                            Vec::new()
                        }),
                    None => Vec::new(),
                };
                if ui_tx.send(UiMessage::ModelsLoaded(listings)).is_err() {
                    return;
                }

                // Resolve the interactive-purpose default model so the picker
                // can show a concrete fallback for conversations with no stored
                // selection (issue #53). Graceful: on failure (or a D-Bus
                // transport that doesn't carry GetPurposes) we send `None` and
                // the picker degrades to its last-resort "Model" label.
                let default_model = match crate::management_client::get_purposes(transport).await {
                    Ok(purposes) => interactive_default_from_purposes(&purposes),
                    Err(e) => {
                        tracing::warn!("get_purposes failed; default model unresolved: {e}");
                        None
                    }
                };
                if ui_tx
                    .send(UiMessage::DefaultModelLoaded(default_model))
                    .is_err()
                {
                    return;
                }

                // Subscribe to background-task events and fetch the initial
                // snapshot over the command channel (Uds and Ws). The D-Bus
                // surface does not expose background tasks (issue #116 covers
                // that path).
                if let Some(cmds) = transport.as_commands() {
                    if let Err(e) = cmds
                        .send_command(api::Command::SubscribeBackgroundTasks)
                        .await
                    {
                        tracing::warn!("SubscribeBackgroundTasks failed: {e}");
                    }
                    match cmds
                        .send_command(api::Command::ListBackgroundTasks {
                            include_finished: false,
                            limit: None,
                        })
                        .await
                    {
                        Ok(api::CommandResult::BackgroundTasks(tasks)) => {
                            if ui_tx.send(UiMessage::TasksLoaded(tasks)).is_err() {
                                return;
                            }
                        }
                        Ok(other) => {
                            tracing::warn!(
                                "unexpected response for ListBackgroundTasks: {other:?}"
                            );
                        }
                        Err(e) => {
                            tracing::warn!("ListBackgroundTasks failed: {e}");
                        }
                    }
                }

                // Background-task updates now arrive via the streaming
                // `SignalEvent::Task*` family below (issue #22 — replaces the
                // earlier 5 s `ListBackgroundTasks` poll). The initial
                // `ListBackgroundTasks` snapshot above seeds the panel; the
                // streaming arms in `match signal` keep it live.

                // Forward signals until disconnect. The `Connector`'s fanout
                // emits a terminal `SignalEvent::Disconnected` and then closes
                // this receiver when the underlying stream ends, so the normal
                // path already delivers a `UiMessage::Disconnected` here.
                while let Some(signal) = signal_rx.recv().await {
                    if ui_tx.send(signal_to_ui_message(signal)).is_err() {
                        return;
                    }
                }

                // signal_rx closed without a Disconnected event (shouldn't
                // happen normally, but handle it defensively)
                let _ = ui_tx.send(UiMessage::Disconnected {
                    reason: "Connection lost".to_string(),
                });
            }
            Err(e) => {
                if ui_tx
                    .send(UiMessage::Disconnected {
                        reason: format!("Connection failed: {e}"),
                    })
                    .is_err()
                {
                    return;
                }
            }
        }

        // Backoff before reconnect
        if ui_tx
            .send(UiMessage::StatusUpdate(format!(
                "Reconnecting in {backoff_secs}s..."
            )))
            .is_err()
        {
            return;
        }

        tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
        backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECS);
    }
}

/// The daemon sentinel meaning "inherit from the interactive purpose"; it
/// never appears for the *interactive* purpose itself, but we guard against it
/// (and empty fields) defensively so a malformed config degrades to "no
/// default" rather than pinning a non-resolvable model.
const PRIMARY_SENTINEL: &str = "primary";

/// Extract the interactive purpose's concrete `(connection, model)` as a
/// [`SelectedModel`]. Returns `None` when the interactive purpose is unset, has
/// an empty connection/model, or uses the `"primary"` inherit sentinel — any of
/// which means there's no concrete model to pin. Pure; unit-tested below.
///
/// Shared with `window.rs`, which re-resolves the default after Settings edits.
pub(crate) fn interactive_default_from_purposes(
    purposes: &api::PurposesView,
) -> Option<crate::selected_models::SelectedModel> {
    let cfg = purposes.interactive.as_ref()?;
    let is_resolvable = |field: &str| !field.is_empty() && field != PRIMARY_SENTINEL;
    if is_resolvable(&cfg.connection) && is_resolvable(&cfg.model) {
        Some(crate::selected_models::SelectedModel {
            connection_id: cfg.connection.clone(),
            model_id: cfg.model.clone(),
        })
    } else {
        None
    }
}

/// Translate a `SignalEvent` from `client-common` into the corresponding
/// `UiMessage` the GTK main thread consumes. Pure mapping; tested below.
fn signal_to_ui_message(signal: SignalEvent) -> UiMessage {
    match signal {
        SignalEvent::Chunk { request_id, chunk } => UiMessage::StreamChunk { request_id, chunk },
        SignalEvent::Complete {
            request_id,
            full_response,
        } => UiMessage::StreamComplete {
            request_id,
            full_response,
        },
        SignalEvent::Error { request_id, error } => UiMessage::StreamError { request_id, error },
        SignalEvent::Status {
            request_id,
            message,
        } => UiMessage::AssistantStatus {
            request_id,
            message,
        },
        SignalEvent::TitleChanged {
            conversation_id,
            title,
        } => UiMessage::TitleChanged {
            conversation_id,
            title,
        },
        SignalEvent::ConversationWarning {
            conversation_id,
            warning,
        } => UiMessage::ConversationWarning {
            conversation_id,
            warning,
        },
        SignalEvent::TaskStarted { task } => UiMessage::TaskStarted(task),
        SignalEvent::TaskProgress { id, progress_hint } => {
            UiMessage::TaskProgress { id, progress_hint }
        }
        SignalEvent::TaskLogAppended { id, entry } => UiMessage::TaskLogAppended { id, entry },
        SignalEvent::TaskCompleted { id, .. } => UiMessage::TaskCompleted { id },
        SignalEvent::ScratchpadChanged { conversation_id } => {
            UiMessage::ScratchpadChanged { conversation_id }
        }
        // Client-local MCP tool execution (#107/#231) is not implemented in the
        // GTK client — it has no local tool runtime to satisfy the call. Surface
        // it as a status string so the parked turn is at least visible; the
        // daemon eventually times the suspended call out. Wiring real
        // client-side tool execution here is a separate feature.
        SignalEvent::ClientToolCall { tool_name, .. } => UiMessage::StatusUpdate(format!(
            "Assistant requested a client-side tool ({tool_name}) this client can't run."
        )),
        SignalEvent::Disconnected { reason } => UiMessage::Disconnected { reason },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_task() -> api::TaskView {
        api::TaskView {
            id: api::TaskId("task-1".to_string()),
            kind: api::TaskKind::Standalone {
                name: "demo".to_string(),
                conversation_id: "conv-1".to_string(),
            },
            status: api::TaskStatus::Running,
            started_at: 0,
            ended_at: None,
            last_error: None,
            parent: None,
            children: Vec::new(),
            title: "demo".to_string(),
            progress_hint: None,
        }
    }

    fn purpose_cfg(connection: &str, model: &str) -> api::PurposeConfigView {
        api::PurposeConfigView {
            connection: connection.to_string(),
            model: model.to_string(),
            effort: None,
            max_context_tokens: None,
        }
    }

    #[test]
    fn interactive_default_resolves_concrete_connection_and_model() {
        let purposes = api::PurposesView {
            interactive: Some(purpose_cfg("work", "claude")),
            ..Default::default()
        };
        let resolved = interactive_default_from_purposes(&purposes).expect("resolvable");
        assert_eq!(resolved.connection_id, "work");
        assert_eq!(resolved.model_id, "claude");
    }

    #[test]
    fn interactive_default_is_none_when_interactive_purpose_unset() {
        let purposes = api::PurposesView::default();
        assert!(interactive_default_from_purposes(&purposes).is_none());
    }

    #[test]
    fn interactive_default_is_none_for_primary_inherit_sentinel() {
        // The interactive purpose shouldn't use the inherit sentinel, but a
        // malformed config must degrade to "no default" rather than pin it.
        let purposes = api::PurposesView {
            interactive: Some(purpose_cfg("primary", "primary")),
            ..Default::default()
        };
        assert!(interactive_default_from_purposes(&purposes).is_none());
    }

    #[test]
    fn interactive_default_is_none_when_model_field_empty() {
        let purposes = api::PurposesView {
            interactive: Some(purpose_cfg("work", "")),
            ..Default::default()
        };
        assert!(interactive_default_from_purposes(&purposes).is_none());
    }

    #[test]
    fn signal_task_started_routes_to_ui_task_started() {
        let task = sample_task();
        let msg = signal_to_ui_message(SignalEvent::TaskStarted { task: task.clone() });
        match msg {
            UiMessage::TaskStarted(got) => assert_eq!(got.id, task.id),
            other => panic!("expected TaskStarted, got {other:?}"),
        }
    }

    #[test]
    fn signal_task_progress_routes_to_ui_task_progress_preserving_hint() {
        let msg = signal_to_ui_message(SignalEvent::TaskProgress {
            id: "task-1".to_string(),
            progress_hint: Some("phase 2".to_string()),
        });
        match msg {
            UiMessage::TaskProgress { id, progress_hint } => {
                assert_eq!(id, "task-1");
                assert_eq!(progress_hint.as_deref(), Some("phase 2"));
            }
            other => panic!("expected TaskProgress, got {other:?}"),
        }
    }

    #[test]
    fn signal_task_log_appended_routes_with_entry_intact() {
        let entry = api::TaskLogEntry {
            seq: 7,
            timestamp: 1234,
            level: api::LogLevel::Info,
            category: api::LogCategory::Status,
            message: "hi".to_string(),
            data: None,
        };
        let msg = signal_to_ui_message(SignalEvent::TaskLogAppended {
            id: "task-1".to_string(),
            entry: entry.clone(),
        });
        match msg {
            UiMessage::TaskLogAppended { id, entry: got } => {
                assert_eq!(id, "task-1");
                assert_eq!(got.seq, entry.seq);
                assert_eq!(got.message, entry.message);
            }
            other => panic!("expected TaskLogAppended, got {other:?}"),
        }
    }

    #[test]
    fn signal_task_completed_routes_id_only() {
        // Terminal-row eviction means the panel only needs the id —
        // status / last_error are dropped at the routing boundary.
        let msg = signal_to_ui_message(SignalEvent::TaskCompleted {
            id: "task-1".to_string(),
            status: api::TaskStatus::Failed,
            last_error: Some("boom".to_string()),
        });
        match msg {
            UiMessage::TaskCompleted { id } => {
                assert_eq!(id, "task-1");
            }
            other => panic!("expected TaskCompleted, got {other:?}"),
        }
    }

    #[test]
    fn signal_chunk_still_routes_to_stream_chunk_regression() {
        let msg = signal_to_ui_message(SignalEvent::Chunk {
            request_id: "r1".to_string(),
            chunk: "hello".to_string(),
        });
        match msg {
            UiMessage::StreamChunk { request_id, chunk } => {
                assert_eq!(request_id, "r1");
                assert_eq!(chunk, "hello");
            }
            other => panic!("expected StreamChunk, got {other:?}"),
        }
    }

    #[test]
    fn signal_conversation_warning_routes_to_typed_ui_warning() {
        let prev = api::ConversationModelSelectionView {
            connection_id: "old".to_string(),
            model_id: "gone".to_string(),
            effort: None,
        };
        let fallback = api::ConversationModelSelectionView {
            connection_id: "work".to_string(),
            model_id: "claude".to_string(),
            effort: None,
        };
        let warning = api::ConversationWarning::DanglingModelSelection {
            previous_selection: prev,
            fallback_to: fallback,
        };
        let msg = signal_to_ui_message(SignalEvent::ConversationWarning {
            conversation_id: "conv-1".to_string(),
            warning: warning.clone(),
        });
        match msg {
            UiMessage::ConversationWarning {
                conversation_id,
                warning: got,
            } => {
                assert_eq!(conversation_id, "conv-1");
                assert_eq!(got, warning);
            }
            other => panic!("expected ConversationWarning, got {other:?}"),
        }
    }

    #[test]
    fn signal_disconnected_still_routes_to_ui_disconnected_regression() {
        let msg = signal_to_ui_message(SignalEvent::Disconnected {
            reason: "lost".to_string(),
        });
        match msg {
            UiMessage::Disconnected { reason } => assert_eq!(reason, "lost"),
            other => panic!("expected Disconnected, got {other:?}"),
        }
    }
}
