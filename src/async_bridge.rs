use std::collections::HashMap;
use std::future::Future;
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};

use desktop_assistant_api_model as api;
use desktop_assistant_client_common::mcp_host::{
    ClientMcpConfig, McpHost, default_client_mcp_path, dispatch_client_tool_call,
    merge_registrations,
};
use desktop_assistant_client_common::{AssistantClient, ConnectionConfig, Connector, SignalEvent};
use gtk4::glib;
use tokio::runtime::Runtime;
use tokio::sync::{mpsc, watch};

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

/// The three-way voice-**output** level for a conversation (issue #80), exposed
/// by the `Adele:` dropdown (`Disabled` / `OnDemand` / `Always`; default
/// `Disabled`). Now owned by the shared `adele-voice-client-common` crate
/// (desktop-assistant#274) so the GTK and TUI clients share one definition + the
/// narration gate; re-exported here so existing `crate::async_bridge::AdeleOutput`
/// paths keep resolving unchanged.
pub use adele_voice_client_common::AdeleOutput;
use client_ui_common::BuiltinServerDto;
pub use client_ui_common::{
    UiMessage, interactive_default_from_purposes, signal_to_ui_message, voice_mode_client_tools,
};

fn runtime() -> &'static Runtime {
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to create tokio runtime")
    })
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
        // The loop must NOT hold a strong sender to its own channel (GTK-1):
        // that made the channel permanently unclosable, so the loop — and the handler's
        // captured widget Rcs — lived forever after the window closed. A weak
        // sender is upgraded per message instead; once every strong sender
        // (the bridge + spawned tasks' clones) is gone, `recv()` returns
        // `None`, the loop exits, and the handler (with its widgets) drops.
        let weak_tx = ui_tx.downgrade();

        // Spawn a local future on the GLib main context to receive messages
        glib::spawn_future_local(async move {
            while let Some(msg) = ui_rx.recv().await {
                // All strong senders gone: nothing can act on follow-ups the
                // handler would send, so stop draining and exit.
                let Some(tx) = weak_tx.upgrade() else { break };
                handler(msg, &tx);
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
/// Exits when `ui_tx` is closed (GTK window gone) or when `shutdown` is
/// signalled (the window's `close-request` fired, or the window dropped its
/// shutdown handle). Exiting drops this task's `Arc<Connector>` clone and
/// `ui_tx` clone, which is what lets the daemon session end and the bridge
/// channel close (issue: GTK-1).
///
/// Owns the client-side MCP host for the whole session (desktop-assistant#464):
/// it is started once, *before* the connect loop, so its local MCP server
/// processes survive reconnects (their tools are re-advertised on every
/// (re)connect inside [`drive_connection`]); and it is shut down when the manager
/// exits — i.e. when the window closes — so no orphaned server processes linger.
pub async fn connection_manager(
    config: ConnectionConfig,
    ui_tx: mpsc::UnboundedSender<UiMessage>,
    // Where each freshly connected `Connector` is handed to the GTK main thread
    // (#106): `drive_connection` stores it here just before `Connected`. The core
    // can't carry the handle (wasm), so this gtk-local cell is the seam.
    pending_connector: Arc<Mutex<Option<Arc<Connector>>>>,
    // Live per-server client-tool counts by namespace, refreshed from the running
    // `McpHost` here and after each (re)registration, and read by the Settings MCP
    // panel so client rows show a real tool count (adele-gtk#125).
    client_tool_counts: Arc<Mutex<HashMap<String, u32>>>,
    // The client's compiled-in built-in MCP servers, projected to the panel's
    // view-model DTOs (da#538 Phase D). Snapshotted once from the running
    // `McpHost::builtin_status()` after the host starts, and read by the Settings
    // MCP panel so built-in rows (including any shadowed by an external server of
    // the same name) render alongside the daemon/client rows. Empty when built-ins
    // are compiled out or no host is running.
    mcp_builtin_dtos: Arc<Mutex<Vec<BuiltinServerDto>>>,
    shutdown: watch::Receiver<bool>,
) {
    // Start the client-side MCP host for the `gtk` surface once (mirroring the
    // TUI, adele-tui#113): the servers named by `[surfaces.gtk]` in
    // `~/.config/adele/client-mcp.toml` run on this machine and expose their
    // tools to the daemon as client-side tools. `None` when nothing is configured
    // (an absent/malformed config resolves to an empty selection).
    let host = start_mcp_host().await;
    // Seed the panel's tool counts as soon as the host is up (before the first
    // connect); `drive_connection` refreshes them after each (re)registration.
    snapshot_tool_counts(host.as_ref(), &client_tool_counts);
    // The built-in set is fixed once the host starts (the override decision is
    // made in `start_with`), so a single snapshot here is enough — unlike the
    // per-server tool counts, it never changes across reconnects (da#538 Phase D).
    snapshot_builtin_status(host.as_ref(), &mcp_builtin_dtos);
    connection_loop(
        config,
        ui_tx,
        pending_connector,
        client_tool_counts,
        shutdown,
        host.as_ref(),
    )
    .await;
    // Session over (window closed): stop every hosted MCP server process. The
    // servers also kill-on-drop, but shutting down explicitly is deterministic.
    if let Some(host) = host {
        host.shutdown().await;
    }
}

/// Overwrite the shared per-server tool-count cell from the running host
/// (adele-gtk#125). Keyed by namespace, matching
/// [`crate::mcp_admin::client_server_dtos`]. A quick lock + overwrite; the host's
/// `usize` totals are clamped into `u32` for the wire-free DTO, and a `None` host
/// clears the cell. The Settings MCP panel reads a snapshot of this cell when it
/// builds its client rows.
fn snapshot_tool_counts(
    host: Option<&McpHost>,
    client_tool_counts: &Arc<Mutex<HashMap<String, u32>>>,
) {
    let fresh: HashMap<String, u32> = host
        .map(McpHost::tool_counts)
        .unwrap_or_default()
        .into_iter()
        .map(|(namespace, n)| (namespace, u32::try_from(n).unwrap_or(u32::MAX)))
        .collect();
    *client_tool_counts.lock().unwrap() = fresh;
}

/// Overwrite the shared built-in-DTO cell from the running host's
/// `builtin_status()` (da#538 Phase D), projected via
/// [`crate::builtins::builtin_dtos`]. A `None` host clears the cell. The Settings
/// MCP panel reads a snapshot of this cell when it builds its rows so the client's
/// compiled-in built-ins (and any shadowed by an external server of the same name)
/// render alongside the daemon/client rows. A quick lock + overwrite; the guard is
/// never held across an await.
fn snapshot_builtin_status(
    host: Option<&McpHost>,
    mcp_builtin_dtos: &Arc<Mutex<Vec<BuiltinServerDto>>>,
) {
    let fresh = host
        .map(|h| crate::builtins::builtin_dtos(h.builtin_status()))
        .unwrap_or_default();
    *mcp_builtin_dtos.lock().unwrap() = fresh;
}

/// Start the client-side MCP host for the `gtk` surface, or `None` when no
/// servers are configured for it. A missing/malformed `client-mcp.toml` resolves
/// to an empty selection (logged), so this never fails the connection.
async fn start_mcp_host() -> Option<McpHost> {
    let cfg = ClientMcpConfig::load(&default_client_mcp_path());
    let servers: Vec<_> = cfg.resolved_servers("gtk").into_iter().cloned().collect();
    // Compiled-in built-ins (da#538 Phase C/D): host the full core MCP set
    // in-process. `McpHost::start_with_disabled` centralizes the override, skipping
    // (and logging) any built-in whose name a configured client-mcp server already
    // provides, AND any built-in the user turned off for this surface via the
    // config's `disabled_builtins` (da#538 slice 4). It reports each built-in's
    // status (with the override / config-disable flags) for the F5 panel. Named
    // `mcp_builtins` to avoid colliding with the voice-mode client-tool `builtins`
    // used elsewhere.
    let mcp_builtins = crate::builtins::builtin_servers();
    // Host if there is anything to host (configured servers OR built-ins).
    if servers.is_empty() && mcp_builtins.is_empty() {
        None
    } else {
        Some(
            McpHost::start_with_disabled(
                &servers,
                mcp_builtins,
                cfg.surface_disabled_builtins("gtk"),
            )
            .await,
        )
    }
}

/// The connect → forward → reconnect loop. Split out of [`connection_manager`]
/// so the MCP host it owns can be shut down at the single exit point *after* this
/// returns (the loop has several early `return`s on the shutdown/teardown paths).
/// `host` is borrowed for the per-(re)connect client-tool registration and the
/// incoming-`ClientToolCall` dispatch inside [`drive_connection`].
async fn connection_loop(
    config: ConnectionConfig,
    ui_tx: mpsc::UnboundedSender<UiMessage>,
    pending_connector: Arc<Mutex<Option<Arc<Connector>>>>,
    client_tool_counts: Arc<Mutex<HashMap<String, u32>>>,
    mut shutdown: watch::Receiver<bool>,
    host: Option<&McpHost>,
) {
    const INITIAL_BACKOFF_SECS: u64 = 2;
    const MAX_BACKOFF_SECS: u64 = 30;

    let mut backoff_secs = INITIAL_BACKOFF_SECS;

    loop {
        // Every await in the cycle (connect, the connected session, the
        // backoff sleep) races the shutdown signal, so a closing window can
        // interrupt the manager wherever it is.
        let connect_result = tokio::select! {
            result = Connector::connect(&config) => result,
            _ = shutdown_requested(&mut shutdown) => return,
        };

        match connect_result {
            Ok(connector) => {
                backoff_secs = INITIAL_BACKOFF_SECS;
                // Run the connected session; on shutdown the whole session
                // future (connector, signal stream) is dropped here, which
                // closes the transport and ends the daemon session.
                let flow = tokio::select! {
                    flow = drive_connection(connector, &ui_tx, &pending_connector, &client_tool_counts, host) => flow,
                    _ = shutdown_requested(&mut shutdown) => return,
                };
                if flow.is_break() {
                    return;
                }
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

        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)) => {}
            _ = shutdown_requested(&mut shutdown) => return,
        }
        backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECS);
    }
}

/// Resolves when window shutdown is requested: the value flipped to `true`
/// (close-request fired) or the sender dropped (window torn down without an
/// explicit signal). Never resolves while the window is alive and open.
async fn shutdown_requested(shutdown: &mut watch::Receiver<bool>) {
    loop {
        if *shutdown.borrow_and_update() {
            return;
        }
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

/// One connected session: hand the connector to the GTK thread, run the
/// initial fetches, then forward signals until the connection ends.
///
/// Returns [`ControlFlow::Break`] when `ui_tx` is closed (the window's receive
/// loop is gone — the manager must exit) and [`ControlFlow::Continue`] when
/// the connection dropped (the manager should reconnect).
async fn drive_connection(
    connector: Connector,
    ui_tx: &mpsc::UnboundedSender<UiMessage>,
    pending_connector: &Arc<Mutex<Option<Arc<Connector>>>>,
    client_tool_counts: &Arc<Mutex<HashMap<String, u32>>>,
    host: Option<&McpHost>,
) -> std::ops::ControlFlow<()> {
    use std::ops::ControlFlow::{Break, Continue};

    // Subscribe before issuing any prompt so no early chunk is
    // lost; the fanout pump inside the `Connector` keeps running as
    // long as the `Arc<Connector>` is alive (held by the window).
    let mut signal_rx = connector.subscribe();
    let connector = Arc::new(connector);
    // `client()` borrows the transport owned by the `Connector`;
    // the connector outlives every use below (and the shared
    // `Arc` clone handed to the GTK thread keeps the pump alive).
    let transport = connector.client();

    // Hand the connector to the GTK main thread (#106): stash it where the
    // `Connected` handler drains it, THEN send `Connected`. Storing before the
    // send preserves the ordering the chat relies on — the connector is in place
    // before `Connected` (and the later `ConversationsLoaded`, which needs it) is
    // handled — without routing the handle through the core, which can't name
    // `Connector` on wasm. Replaces the old `UiMessage::ClientReady` round-trip.
    *pending_connector.lock().unwrap() = Some(Arc::clone(&connector));

    let label = connector.label().to_string();
    if ui_tx.send(UiMessage::Connected { label }).is_err() {
        return Break(());
    }

    // Advertise gtk's client tools as the single set the daemon expects: its
    // built-in voice-mode tools (issue #78) MERGED with any client-hosted MCP
    // host tools (desktop-assistant#464). The daemon replaces the whole set per
    // call, so hosted tools must be registered together with the built-ins;
    // `merge_registrations` lets a built-in win a name clash. Runs on every
    // (re)connect (the daemon scopes client tools per session, #261/#231), and is
    // best-effort: socket-only (UDS/WS), a logged no-op on D-Bus, and a failure
    // never blocks the chat. (Registration was moved here from the GTK thread's
    // `connector_arrived` branch so the merge with the host's tools happens once,
    // where the host lives — mirroring the TUI's `finish_connection_init`.)
    let host_tools = host.map(McpHost::registrations).unwrap_or_default();
    if let Err(e) = connector
        .register_client_tools(merge_registrations(voice_mode_client_tools(), host_tools))
        .await
    {
        tracing::debug!("client tool registration skipped: {e}");
    }
    // Refresh the panel's per-server tool counts after each (re)registration
    // (adele-gtk#125). The host's tools are fixed after start, so this is
    // idempotent; it keeps the shared cell current across reconnects.
    snapshot_tool_counts(host, client_tool_counts);

    // Refresh conversation list on connect
    match transport.list_conversations().await {
        Ok(convs) => {
            if ui_tx.send(UiMessage::ConversationsLoaded(convs)).is_err() {
                return Break(());
            }
        }
        Err(e) => {
            if ui_tx
                .send(UiMessage::Error(format!("Load conversations: {e}")))
                .is_err()
            {
                return Break(());
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
        return Break(());
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
        return Break(());
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
                    return Break(());
                }
            }
            Ok(other) => {
                tracing::warn!("unexpected response for ListBackgroundTasks: {other:?}");
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
        // Client-hosted MCP tools take precedence over the core's built-in
        // client-tool dispatch (desktop-assistant#464): if the local host serves
        // this tool it runs it and submits the result itself, so the call must
        // NOT also be forwarded to the core (that would double-dispatch and
        // double-submit). A `ClientToolCall` for a tool the host does NOT serve
        // (say_this / the voice-mode tools) falls through to the core exactly as
        // before, as does every non-tool signal.
        if let (
            Some(host),
            SignalEvent::ClientToolCall {
                task_id,
                tool_call_id,
                tool_name,
                arguments,
                ..
            },
        ) = (host, &signal)
            && dispatch_client_tool_call(
                host,
                connector.as_ref(),
                task_id,
                tool_call_id,
                tool_name,
                arguments.clone(),
            )
            .await
        {
            continue;
        }
        if ui_tx.send(signal_to_ui_message(signal)).is_err() {
            return Break(());
        }
    }

    // signal_rx closed without a Disconnected event (shouldn't
    // happen normally, but handle it defensively)
    let _ = ui_tx.send(UiMessage::Disconnected {
        reason: "Connection lost".to_string(),
    });
    Continue(())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;
    use std::time::Duration;

    use super::*;

    /// desktop-assistant#464: gtk advertises its built-in voice-mode client
    /// tools MERGED with any client-hosted MCP host tools — it must never replace
    /// one set with the other (the daemon takes the whole set per call, so the
    /// hosted tools have to be registered together with the built-ins). Mirrors
    /// the `mcp_host::bridge` merge tests, but with gtk's real built-ins, guarding
    /// the wiring in `drive_connection`.
    #[test]
    fn client_tool_registration_merges_builtins_with_hosted_tools() {
        use desktop_assistant_client_common::mcp_host::merge_registrations;

        let builtins = voice_mode_client_tools();
        assert!(
            !builtins.is_empty(),
            "gtk must advertise its own voice-mode client tools"
        );
        let hosted = vec![api::ClientToolRegistration {
            name: "fs__read_file".to_string(),
            description: "read a file".to_string(),
            input_schema: serde_json::json!({ "type": "object" }),
        }];

        let merged = merge_registrations(builtins.clone(), hosted);

        // Every built-in survives the merge …
        for tool in &builtins {
            assert!(
                merged.iter().any(|t| t.name == tool.name),
                "built-in '{}' must remain advertised after merging host tools",
                tool.name
            );
        }
        // … and the hosted tool is advertised alongside them.
        assert!(
            merged.iter().any(|t| t.name == "fs__read_file"),
            "hosted MCP tool must be advertised"
        );
        assert_eq!(merged.len(), builtins.len() + 1);
    }

    /// GTK-1 acceptance: the bridge's receive loop must NOT keep its own
    /// channel alive. Once the `AsyncBridge` (and every sender clone) is
    /// dropped, the loop has to observe the close, exit, and release the
    /// handler — which is what releases the widget `Rc`s a real window's
    /// handler captures. The old code cloned `ui_tx` into the loop itself, so
    /// the channel could never close and every closed window leaked its whole
    /// widget tree.
    #[test]
    fn bridge_handler_loop_exits_and_releases_handler_when_senders_drop() {
        // A dedicated context so parallel tests don't fight over the default.
        let ctx = glib::MainContext::new();
        ctx.with_thread_default(|| {
            /// Sets the flag when the handler closure (its owner) is dropped.
            struct DropFlag(Rc<Cell<bool>>);
            impl Drop for DropFlag {
                fn drop(&mut self) {
                    self.0.set(true);
                }
            }

            let handled = Rc::new(Cell::new(0u32));
            let dropped = Rc::new(Cell::new(false));
            let guard = DropFlag(Rc::clone(&dropped));
            let handled_in = Rc::clone(&handled);
            let bridge = AsyncBridge::new(move |_msg, _tx| {
                let _ = &guard; // the closure owns the drop guard
                handled_in.set(handled_in.get() + 1);
            });

            // Sanity: a message still flows through the loop to the handler.
            bridge
                .ui_sender()
                .send(UiMessage::StatusUpdate("ping".to_string()))
                .expect("send on live bridge");
            for _ in 0..100 {
                if handled.get() > 0 {
                    break;
                }
                ctx.iteration(false);
            }
            assert_eq!(handled.get(), 1, "message must reach the handler");
            assert!(!dropped.get(), "handler must be alive while bridge lives");

            // Drop the last sender; the loop must exit and drop the handler.
            drop(bridge);
            for _ in 0..100 {
                if dropped.get() {
                    break;
                }
                ctx.iteration(false);
            }
            assert!(
                dropped.get(),
                "receive loop must exit (and release the handler) once all senders drop"
            );
        })
        .expect("acquire test main context");
    }

    /// GTK-1 acceptance: signalling shutdown makes `connection_manager` return
    /// promptly — even from its reconnect backoff sleep — instead of
    /// reconnecting forever after the window closed.
    #[tokio::test]
    async fn connection_manager_exits_promptly_when_shutdown_signalled() {
        // A UDS path that cannot exist → connect fails fast → the manager sits
        // in its backoff sleep, which shutdown must interrupt.
        let config = ConnectionConfig {
            transport_mode: desktop_assistant_client_common::TransportMode::Uds,
            socket_path: Some(std::path::PathBuf::from(
                "/nonexistent/adele-gtk-test/never.sock",
            )),
            ..Default::default()
        };
        let (ui_tx, _ui_rx) = mpsc::unbounded_channel::<UiMessage>();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let manager = tokio::spawn(connection_manager(
            config,
            ui_tx,
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(Vec::new())),
            shutdown_rx,
        ));
        // Let it fail its first connect and enter backoff.
        tokio::time::sleep(Duration::from_millis(100)).await;
        shutdown_tx.send(true).expect("manager subscribed");

        tokio::time::timeout(Duration::from_secs(1), manager)
            .await
            .expect("connection_manager must exit promptly on shutdown")
            .expect("connection_manager task must not panic");
    }

    /// GTK-1 unhappy path: the shutdown *sender* being dropped (window torn
    /// down without an explicit signal) must also stop the manager.
    #[tokio::test]
    async fn connection_manager_exits_when_shutdown_sender_dropped() {
        let config = ConnectionConfig {
            transport_mode: desktop_assistant_client_common::TransportMode::Uds,
            socket_path: Some(std::path::PathBuf::from(
                "/nonexistent/adele-gtk-test/never.sock",
            )),
            ..Default::default()
        };
        let (ui_tx, _ui_rx) = mpsc::unbounded_channel::<UiMessage>();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let manager = tokio::spawn(connection_manager(
            config,
            ui_tx,
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(Vec::new())),
            shutdown_rx,
        ));
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(shutdown_tx);

        tokio::time::timeout(Duration::from_secs(1), manager)
            .await
            .expect("connection_manager must exit when the shutdown sender drops")
            .expect("connection_manager task must not panic");
    }

    /// GTK-1 regression guard: when the UI channel itself closes (receiver
    /// gone), the manager must still exit on its own — shutdown is an
    /// *additional* exit, not a replacement for the existing one.
    #[tokio::test]
    async fn connection_manager_exits_when_ui_channel_closes() {
        let config = ConnectionConfig {
            transport_mode: desktop_assistant_client_common::TransportMode::Uds,
            socket_path: Some(std::path::PathBuf::from(
                "/nonexistent/adele-gtk-test/never.sock",
            )),
            ..Default::default()
        };
        let (ui_tx, ui_rx) = mpsc::unbounded_channel::<UiMessage>();
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        drop(ui_rx); // window's receive loop already gone
        let manager = tokio::spawn(connection_manager(
            config,
            ui_tx,
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(Vec::new())),
            shutdown_rx,
        ));

        tokio::time::timeout(Duration::from_secs(1), manager)
            .await
            .expect("connection_manager must exit when the UI channel closes")
            .expect("connection_manager task must not panic");
    }

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
            owner_todo: String::new(),
            spawn_marker: None,
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
            conversation_id: "c1".to_string(),
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
    fn signal_user_message_added_maps_to_ui_message() {
        let msg = signal_to_ui_message(SignalEvent::UserMessageAdded {
            conversation_id: "c1".to_string(),
            request_id: "r1".to_string(),
            content: "what's the weather?".to_string(),
            idempotency_key: Some("turn-key-1".to_string()),
        });
        match msg {
            UiMessage::UserMessageAdded {
                conversation_id,
                request_id,
                content,
                idempotency_key,
            } => {
                assert_eq!(conversation_id, "c1");
                assert_eq!(request_id, "r1");
                assert_eq!(content, "what's the weather?");
                // The echoed send key (#570) rides through the signal→UI map so
                // the reducer can dedupe our own optimistic bubble by exact key.
                assert_eq!(idempotency_key, Some("turn-key-1".to_string()));
            }
            other => panic!("expected UserMessageAdded, got {other:?}"),
        }
    }

    #[test]
    fn signal_context_usage_maps_to_ui_message() {
        let msg = signal_to_ui_message(SignalEvent::ContextUsage {
            conversation_id: "c1".to_string(),
            request_id: "r1".to_string(),
            used_tokens: 12_000,
            budget_tokens: 32_000,
            compaction_active: true,
        });
        match msg {
            UiMessage::ContextUsage {
                conversation_id,
                used_tokens,
                budget_tokens,
                compaction_active,
            } => {
                assert_eq!(conversation_id, "c1");
                assert_eq!(used_tokens, 12_000);
                assert_eq!(budget_tokens, 32_000);
                assert!(compaction_active);
            }
            other => panic!("expected ContextUsage, got {other:?}"),
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

    /// Issue #1: a `ConversationListChanged` signal (a sibling client or the
    /// voice daemon mutated the user's conversation list) must map to the typed
    /// `UiMessage::ConversationListChanged`, carrying the affected id, so the
    /// reducer can trigger a sidebar-only re-fetch.
    #[test]
    fn signal_conversation_list_changed_maps_to_ui_message() {
        let msg = signal_to_ui_message(SignalEvent::ConversationListChanged {
            conversation_id: "conv-42".to_string(),
        });
        match msg {
            UiMessage::ConversationListChanged { conversation_id } => {
                assert_eq!(conversation_id, "conv-42");
            }
            other => panic!("expected ConversationListChanged, got {other:?}"),
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

    /// Issue #76: a `ClientToolCall` signal must map to the typed
    /// `UiMessage::ClientToolCall` carrying every field verbatim — not the old
    /// lossy `StatusUpdate` string, which dropped the ids and wedged the turn
    /// (the window could never post a result without them).
    #[test]
    fn signal_client_tool_call_routes_to_typed_ui_message_with_all_fields() {
        let msg = signal_to_ui_message(SignalEvent::ClientToolCall {
            task_id: "task-9".to_string(),
            conversation_id: "conv-7".to_string(),
            tool_call_id: "call-3".to_string(),
            tool_name: "say_this".to_string(),
            arguments: serde_json::json!({ "text": "hi there" }),
        });
        match msg {
            UiMessage::ClientToolCall {
                task_id,
                conversation_id,
                tool_call_id,
                tool_name,
                arguments,
            } => {
                assert_eq!(task_id, "task-9");
                assert_eq!(conversation_id, "conv-7");
                assert_eq!(tool_call_id, "call-3");
                assert_eq!(tool_name, "say_this");
                assert_eq!(arguments, serde_json::json!({ "text": "hi there" }));
            }
            other => panic!("expected ClientToolCall, got {other:?}"),
        }
    }

    /// Issue #80: the user-driven `You:` (voice input) dropdown is its own
    /// `UiMessage`; its `Debug` must surface both fields so a test panic stays
    /// informative.
    #[test]
    fn set_voice_in_debug_includes_conversation_and_enabled() {
        let dbg = format!(
            "{:?}",
            UiMessage::SetVoiceIn {
                conversation_id: "conv-1".to_string(),
                enabled: true,
            }
        );
        assert!(dbg.contains("SetVoiceIn"), "got {dbg}");
        assert!(dbg.contains("conv-1"), "got {dbg}");
        assert!(dbg.contains("true"), "got {dbg}");
    }

    /// Issue #80: the user-driven `Adele:` (voice output) dropdown is its own
    /// `UiMessage` carrying the three-way level; its `Debug` must surface both
    /// the conversation and the level variant.
    #[test]
    fn set_adele_output_debug_includes_conversation_and_level() {
        let dbg = format!(
            "{:?}",
            UiMessage::SetAdeleOutput {
                conversation_id: "conv-2".to_string(),
                level: AdeleOutput::Always,
            }
        );
        assert!(dbg.contains("SetAdeleOutput"), "got {dbg}");
        assert!(dbg.contains("conv-2"), "got {dbg}");
        assert!(dbg.contains("Always"), "got {dbg}");
    }
}
