use std::future::Future;
use std::sync::Arc;
use std::sync::OnceLock;

use desktop_assistant_client_common::SignalEvent;
use desktop_assistant_client_common::{
    AssistantClient, ConnectionConfig, TransportClient, connect_transport,
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
    PromptSent {
        request_id: String,
    },
    Connected {
        label: String,
    },
    Disconnected {
        reason: String,
    },
    StatusUpdate(String),
    Error(String),
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
    /// Create a new bridge. `handler` is called on the GTK main thread for each UiMessage.
    pub fn new<F>(handler: F) -> Self
    where
        F: Fn(UiMessage) + 'static,
    {
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiMessage>();

        // Spawn a local future on the GLib main context to receive messages
        glib::spawn_future_local(async move {
            while let Some(msg) = ui_rx.recv().await {
                handler(msg);
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

                let label = transport_label(config.transport_mode).to_string();
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

                // Forward signals until disconnect
                while let Some(signal) = signal_rx.recv().await {
                    let msg = match signal {
                        SignalEvent::Chunk { request_id, chunk } => {
                            UiMessage::StreamChunk { request_id, chunk }
                        }
                        SignalEvent::Complete {
                            request_id,
                            full_response,
                        } => UiMessage::StreamComplete {
                            request_id,
                            full_response,
                        },
                        SignalEvent::Error { request_id, error } => {
                            UiMessage::StreamError { request_id, error }
                        }
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
                        SignalEvent::Disconnected { reason } => {
                            UiMessage::Disconnected { reason }
                        }
                    };
                    if ui_tx.send(msg).is_err() {
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
