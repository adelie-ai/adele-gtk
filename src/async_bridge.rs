use std::future::Future;
use std::sync::Arc;
use std::sync::OnceLock;

use desktop_assistant_api_model as api;
use desktop_assistant_client_common::SignalEvent;
use desktop_assistant_client_common::{
    AssistantClient, AssistantCommands, ConnectionConfig, TransportClient, connect_transport,
    transport::transport_label,
};
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
#[derive(Debug)]
pub enum UiMessage {
    ConversationsLoaded(Vec<desktop_assistant_client_common::ConversationSummary>),
    ConversationLoaded(desktop_assistant_client_common::ConversationDetail),
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
}

/// Internal message for delivering a new client to the GTK main thread.
pub enum InternalMsg {
    ClientReady(Arc<TransportClient>),
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
pub async fn connection_manager(
    config: ConnectionConfig,
    ui_tx: mpsc::UnboundedSender<UiMessage>,
    internal_tx: mpsc::UnboundedSender<InternalMsg>,
) {
    const INITIAL_BACKOFF_SECS: u64 = 2;
    const MAX_BACKOFF_SECS: u64 = 30;

    let mut backoff_secs = INITIAL_BACKOFF_SECS;

    loop {
        match connect_transport(&config).await {
            Ok((transport, mut signal_rx)) => {
                backoff_secs = INITIAL_BACKOFF_SECS;
                let transport = Arc::new(transport);

                let label = transport_label(&config);
                if ui_tx.send(UiMessage::Connected { label }).is_err() {
                    return;
                }

                if internal_tx
                    .send(InternalMsg::ClientReady(Arc::clone(&transport)))
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
                // (WS only — the D-Bus interface doesn't expose this command).
                let listings = match transport.as_ws() {
                    Some(ws) => ws
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

                // Subscribe to background-task events and fetch the initial
                // snapshot. WS-only: the D-Bus surface does not expose
                // background tasks (issue #116 covers that path).
                if let Some(ws) = transport.as_ws() {
                    if let Err(e) = ws
                        .send_command(api::Command::SubscribeBackgroundTasks)
                        .await
                    {
                        tracing::warn!("SubscribeBackgroundTasks failed: {e}");
                    }
                    match ws
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

                // Forward signals until disconnect
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
        } => UiMessage::StatusUpdate(format!("Conversation {conversation_id}: {warning:?}")),
        SignalEvent::TaskStarted { task } => UiMessage::TaskStarted(task),
        SignalEvent::TaskProgress { id, progress_hint } => {
            UiMessage::TaskProgress { id, progress_hint }
        }
        SignalEvent::TaskLogAppended { id, entry } => UiMessage::TaskLogAppended { id, entry },
        SignalEvent::TaskCompleted { id, .. } => UiMessage::TaskCompleted { id },
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
