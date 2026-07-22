//! Top-level Settings dialog (Notebook with Connections + Purposes tabs).
//!
//! Owns the live transport handle and wires tab callbacks to async
//! management RPCs. The dialog is transient to the parent window and
//! refreshes its data every time it is presented (it is cheap: a couple
//! of RPCs).
//!
//! Async wiring mirrors `widgets/knowledge_browser.rs`: the dialog is
//! handed an `Arc<Connector>` (resolved once by the caller) and an
//! `Rc<AsyncBridge>`. Management RPCs go through the connector's transport
//! (`connector.client()`). Each RPC is dispatched on the tokio runtime via
//! `bridge.spawn`, with results routed back to the GTK main thread through
//! a short-lived `tokio::sync::mpsc` channel consumed by
//! `glib::spawn_future_local`. Surface-level errors go to the window status
//! bar via `bridge.ui_sender()` → `UiMessage::Error`. The dialog owns its
//! own `list_available_models` fetch (None connection_id) so the Purposes
//! tab sees *all* models — including embedding models, which the header
//! `ModelPicker` filters out.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use client_ui_common::{BuiltinServerDto, Runner};
use desktop_assistant_api_model as api;
use desktop_assistant_client_common::Connector;
use desktop_assistant_client_common::mcp_host::{ClientMcpConfig, default_client_mcp_path};
use gtk4::prelude::*;
use gtk4::{Align, Box as GtkBox, Button, Label, Notebook, Orientation, Window, glib};
use tokio::sync::mpsc;

use crate::async_bridge::{AsyncBridge, UiMessage};
use crate::management_client;
use crate::mcp_admin::{self, McpBackend};
use crate::preferences::PreferencesStore;
use crate::voice_client::VoiceController;

use super::connection_config_dialog::{ConnectorType, show_configure_dialog};
use super::connections_tab::ConnectionsTab;
use super::general_tab::GeneralTab;
use super::mcp_server_dialog::{BuiltMcpServer, McpForm, McpTransport, show_mcp_server_dialog};
use super::mcp_servers_tab::McpServersTab;
use super::purposes_tab::PurposesTab;
use super::voice_tab::VoiceTab;

/// Minimal yes/no confirmation dialog. We avoid GTK4's `AlertDialog`
/// (gated behind the `v4_10` feature) to keep the feature floor low.
/// `on_confirm` is called when the user clicks the affirmative button.
fn confirm<F>(
    parent: &impl IsA<Window>,
    title: &str,
    detail: &str,
    affirmative: &str,
    destructive: bool,
    on_confirm: F,
) where
    F: Fn() + 'static,
{
    let dialog = Window::builder()
        .title(title)
        .default_width(420)
        .modal(true)
        .resizable(false)
        .transient_for(parent)
        .build();

    let content = GtkBox::new(Orientation::Vertical, 12);
    content.set_margin_start(20);
    content.set_margin_end(20);
    content.set_margin_top(20);
    content.set_margin_bottom(20);

    let message = Label::new(Some(detail));
    message.set_wrap(true);
    message.set_halign(Align::Start);
    content.append(&message);

    let btn_row = GtkBox::new(Orientation::Horizontal, 8);
    btn_row.set_halign(Align::End);
    let cancel = Button::with_label("Cancel");
    let confirm_btn = Button::with_label(affirmative);
    if destructive {
        confirm_btn.add_css_class("destructive-action");
    } else {
        confirm_btn.add_css_class("suggested-action");
    }
    btn_row.append(&cancel);
    btn_row.append(&confirm_btn);
    content.append(&btn_row);

    dialog.set_child(Some(&content));

    cancel.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        move |_| dialog.close()
    ));

    confirm_btn.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        move |_| {
            dialog.close();
            on_confirm();
        }
    ));

    dialog.present();
}

/// Present the Settings dialog modally against `parent`, using `connector`
/// for all RPC traffic (management commands go through `connector.client()`).
/// `bridge` provides the tokio runtime + the shared ui-message channel so
/// errors propagate back to the window status bar.
///
/// `voice` is the handle to the standalone voice daemon (a *separate* D-Bus
/// service from the connector's orchestrator); it backs the Voice tab's
/// wake-word toggle and voice picker. The tab degrades gracefully when the
/// daemon is absent.
///
/// `daemon_is_remote` / `daemon_host` describe the client's link to the daemon
/// (derived from the connection config by [`mcp_admin::daemon_link`], which the
/// caller owns): they drive the MCP panel's runner chip so a daemon row on a
/// remote WebSocket link reads `daemon · <host>` and a co-located one reads
/// `daemon`.
///
/// `client_tool_counts` is the window-owned cell the connection manager fills
/// from the running `McpHost` (per namespace); the MCP panel reads a snapshot of
/// it when it builds its client rows so each shows a live tool count
/// (adele-gtk#125). It is empty (rows show 0) until the host has started.
///
/// `mcp_builtin_dtos` is the window-owned cell the connection manager fills once
/// from the running `McpHost::builtin_status()` (da#538 Phase D); the MCP panel
/// reads a snapshot when it builds its rows so the client's compiled-in built-in
/// servers (and any shadowed by an external server of the same name) render
/// alongside the daemon/client rows. It is empty until the host has started, or
/// when built-ins are compiled out.
#[allow(clippy::too_many_arguments)]
pub fn show_settings_dialog(
    parent: &impl IsA<Window>,
    transport: Arc<Connector>,
    bridge: Rc<AsyncBridge>,
    voice: VoiceController,
    daemon_is_remote: bool,
    daemon_host: Option<String>,
    client_tool_counts: Arc<Mutex<HashMap<String, u32>>>,
    mcp_builtin_dtos: Arc<Mutex<Vec<BuiltinServerDto>>>,
) {
    let dialog = Window::builder()
        .title("Settings")
        .default_width(720)
        .default_height(520)
        .modal(true)
        .transient_for(parent)
        .build();

    let vbox = GtkBox::new(Orientation::Vertical, 0);
    vbox.set_vexpand(true);

    let notebook = Notebook::new();
    notebook.set_vexpand(true);

    let connections_tab = Rc::new(ConnectionsTab::new());
    let purposes_tab = Rc::new(PurposesTab::new());
    let mcp_tab = Rc::new(McpServersTab::new());
    let voice_tab = Rc::new(VoiceTab::new());
    let general_tab = Rc::new(GeneralTab::new());

    // Snapshot of the OAuth service accounts (epic #477), refreshed alongside
    // the MCP server list and handed to the editor's account picker.
    let service_accounts: Rc<RefCell<Vec<api::ServiceAccountView>>> =
        Rc::new(RefCell::new(Vec::new()));

    // Snapshot of the on-disk client MCP config (`client-mcp.toml`, #122),
    // refreshed alongside the daemon list. Cached so a client-row edit can
    // pre-fill from the local definition without another file read on the main
    // loop; all writes to it happen off the main loop via the async bridge.
    let client_config: Rc<RefCell<ClientMcpConfig>> =
        Rc::new(RefCell::new(ClientMcpConfig::default()));
    let client_mcp_path: Rc<PathBuf> = Rc::new(default_client_mcp_path());

    notebook.append_page(
        &connections_tab.container,
        Some(&Label::new(Some("Connections"))),
    );
    notebook.append_page(&purposes_tab.container, Some(&Label::new(Some("Purposes"))));
    notebook.append_page(&mcp_tab.container, Some(&Label::new(Some("MCP Servers"))));
    notebook.append_page(&voice_tab.container, Some(&Label::new(Some("Voice"))));
    notebook.append_page(&general_tab.container, Some(&Label::new(Some("General"))));

    vbox.append(&notebook);

    // Status row + close.
    let footer = GtkBox::new(Orientation::Horizontal, 8);
    footer.set_margin_start(12);
    footer.set_margin_end(12);
    footer.set_margin_top(6);
    footer.set_margin_bottom(8);

    let status_label = Rc::new(Label::new(None));
    status_label.set_halign(Align::Start);
    status_label.set_hexpand(true);
    status_label.add_css_class("dim-label");
    footer.append(&*status_label);

    let close_btn = Button::with_label("Close");
    close_btn.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        move |_| dialog.close()
    ));
    footer.append(&close_btn);

    vbox.append(&footer);
    dialog.set_child(Some(&vbox));

    // ---------------------------------------------------------------
    // Data-refresh helper: re-fetches connections + purposes + the
    // aggregated model list, then repopulates the tabs. Called on present
    // and after any mutation. Owns its own `list_available_models(None)`
    // fetch so embedding models reach the Purposes tab.
    // ---------------------------------------------------------------
    let refresh: Rc<dyn Fn()> = Rc::new(glib::clone!(
        #[strong]
        transport,
        #[strong]
        bridge,
        #[strong]
        connections_tab,
        #[strong]
        purposes_tab,
        #[strong]
        status_label,
        move || {
            let connections_tab = Rc::clone(&connections_tab);
            let purposes_tab = Rc::clone(&purposes_tab);
            let status_label = Rc::clone(&status_label);

            let (tx_conn, mut rx_conn) =
                mpsc::unbounded_channel::<Result<Vec<api::ConnectionView>, String>>();
            let (tx_purposes, mut rx_purposes) =
                mpsc::unbounded_channel::<Result<api::PurposesView, String>>();
            let (tx_models, mut rx_models) =
                mpsc::unbounded_channel::<Result<Vec<api::ModelListing>, String>>();

            let transport_for_task = Arc::clone(&transport);
            bridge.spawn(async move {
                let client = transport_for_task.client();
                let conns = management_client::list_connections(client)
                    .await
                    .map_err(|e| e.to_string());
                let _ = tx_conn.send(conns);

                let purposes = management_client::get_purposes(client)
                    .await
                    .map_err(|e| e.to_string());
                let _ = tx_purposes.send(purposes);

                // Aggregated list (None connection_id) populates model
                // caches for every healthy connection at once.
                let models = management_client::list_available_models(client, None, false)
                    .await
                    .map_err(|e| e.to_string());
                let _ = tx_models.send(models);
            });

            let connections_tab_a = Rc::clone(&connections_tab);
            let purposes_tab_a = Rc::clone(&purposes_tab);
            let status_a = Rc::clone(&status_label);
            glib::spawn_future_local(async move {
                if let Some(result) = rx_conn.recv().await {
                    match result {
                        Ok(list) => {
                            connections_tab_a.set_connections(&list);
                            purposes_tab_a.set_connections(&list);
                        }
                        Err(e) => status_a.set_text(&format!("List connections: {e}")),
                    }
                }
                if let Some(result) = rx_purposes.recv().await {
                    match result {
                        Ok(p) => purposes_tab_a.set_purposes(p),
                        Err(e) => status_a.set_text(&format!("Get purposes: {e}")),
                    }
                }
                if let Some(result) = rx_models.recv().await {
                    match result {
                        Ok(listings) => {
                            // Group by connection id and hand each slice to
                            // the purposes tab so per-connection dropdowns
                            // populate without firing extra requests.
                            let mut by_conn: std::collections::BTreeMap<
                                String,
                                Vec<api::ModelListing>,
                            > = std::collections::BTreeMap::new();
                            for listing in listings {
                                by_conn
                                    .entry(listing.connection_id.clone())
                                    .or_default()
                                    .push(listing);
                            }
                            for (id, list) in by_conn {
                                purposes_tab_a.set_models(&id, list);
                            }
                        }
                        Err(e) => status_a.set_text(&format!("List models: {e}")),
                    }
                }
            });
        }
    ));

    // ---------------------------------------------------------------
    // Connections tab wiring.
    // ---------------------------------------------------------------
    connections_tab.connect_add(glib::clone!(
        #[strong]
        transport,
        #[strong]
        bridge,
        #[strong]
        refresh,
        #[weak(rename_to = parent)]
        dialog,
        move |connector| {
            let transport = Arc::clone(&transport);
            let bridge = Rc::clone(&bridge);
            let refresh = Rc::clone(&refresh);
            show_configure_dialog(
                &parent,
                connector,
                None,
                move |id, config| {
                    let transport = Arc::clone(&transport);
                    let refresh = Rc::clone(&refresh);
                    let ui_tx = bridge.ui_sender();
                    let (tx, mut rx) = mpsc::unbounded_channel::<Result<(), String>>();
                    bridge.spawn(async move {
                        let r =
                            management_client::create_connection(transport.client(), id, config)
                                .await
                                .map_err(|e| e.to_string());
                        let _ = tx.send(r);
                    });
                    glib::spawn_future_local(async move {
                        if let Some(result) = rx.recv().await {
                            match result {
                                Ok(()) => refresh(),
                                Err(e) => {
                                    let _ = ui_tx
                                        .send(UiMessage::Error(format!("Create connection: {e}")));
                                }
                            }
                        }
                    });
                },
                // Add-flow never has a connection to refresh yet.
                |_id| {},
            );
        }
    ));

    connections_tab.connect_configure(glib::clone!(
        #[strong]
        transport,
        #[strong]
        bridge,
        #[strong(rename_to = connections_tab_cfg)]
        connections_tab,
        #[strong]
        refresh,
        #[weak(rename_to = parent)]
        dialog,
        move |id| {
            let Some(existing) = connections_tab_cfg.find(&id) else {
                return;
            };
            let connector = match ConnectorType::from_slug(&existing.connector_type) {
                Some(c) => c,
                None => {
                    let _ = bridge.ui_sender().send(UiMessage::Error(format!(
                        "Unknown connector type: {}",
                        existing.connector_type
                    )));
                    return;
                }
            };
            // The daemon echoes the non-secret config (base_url, env-var
            // *name*, aws_profile, region) on `ConnectionView::config`; pre-fill
            // the dialog from it so edits don't wipe stored fields. Raw secrets
            // are never echoed, so the api-key entry stays blank. Fall back to
            // an empty config of the right variant when an older daemon omits
            // `config`.
            let existing_config = existing
                .config
                .clone()
                .unwrap_or_else(|| connector.empty_config());
            let existing_pair = Some((existing.id.clone(), existing_config));

            let transport_save = Arc::clone(&transport);
            let bridge_save = Rc::clone(&bridge);
            let refresh_inner = Rc::clone(&refresh);
            let transport_refresh = Arc::clone(&transport);
            let bridge_refresh = Rc::clone(&bridge);

            show_configure_dialog(
                &parent,
                connector,
                existing_pair,
                move |id, config| {
                    let transport = Arc::clone(&transport_save);
                    let refresh = Rc::clone(&refresh_inner);
                    let ui_tx = bridge_save.ui_sender();
                    let (tx, mut rx) = mpsc::unbounded_channel::<Result<(), String>>();
                    bridge_save.spawn(async move {
                        let r =
                            management_client::update_connection(transport.client(), id, config)
                                .await
                                .map_err(|e| e.to_string());
                        let _ = tx.send(r);
                    });
                    glib::spawn_future_local(async move {
                        if let Some(result) = rx.recv().await {
                            match result {
                                Ok(()) => refresh(),
                                Err(e) => {
                                    let _ = ui_tx
                                        .send(UiMessage::Error(format!("Update connection: {e}")));
                                }
                            }
                        }
                    });
                },
                move |id_for_refresh| {
                    // Refresh Bedrock models cache (refresh: true).
                    let transport = Arc::clone(&transport_refresh);
                    let ui_tx = bridge_refresh.ui_sender();
                    bridge_refresh.spawn(async move {
                        let r = management_client::list_available_models(
                            transport.client(),
                            Some(id_for_refresh),
                            true,
                        )
                        .await;
                        if let Err(e) = r {
                            let _ = ui_tx.send(UiMessage::Error(format!("Refresh models: {e}")));
                        }
                    });
                },
            );
        }
    ));

    connections_tab.connect_remove(glib::clone!(
        #[strong]
        transport,
        #[strong]
        bridge,
        #[strong]
        refresh,
        #[weak(rename_to = parent)]
        dialog,
        move |id| {
            let transport_for_confirm = Arc::clone(&transport);
            let bridge_for_confirm = Rc::clone(&bridge);
            let refresh_for_confirm = Rc::clone(&refresh);
            let parent_for_retry = parent.clone();
            let id_for_confirm = id.clone();
            confirm(
                &parent,
                &format!("Remove connection \"{id}\"?"),
                "This will fail if any purpose still references the connection. You will be offered a force-remove on the retry dialog, which re-assigns referencing purposes to the interactive purpose.",
                "Remove",
                true,
                move || {
                    do_remove_connection(
                        &parent_for_retry,
                        Arc::clone(&transport_for_confirm),
                        Rc::clone(&bridge_for_confirm),
                        Rc::clone(&refresh_for_confirm),
                        id_for_confirm.clone(),
                        false,
                    );
                },
            );
        }
    ));

    // ---------------------------------------------------------------
    // Purposes tab wiring.
    // ---------------------------------------------------------------
    purposes_tab.connect_set_purpose(glib::clone!(
        #[strong]
        transport,
        #[strong]
        bridge,
        #[strong]
        refresh,
        move |purpose, config| {
            let transport = Arc::clone(&transport);
            let refresh = Rc::clone(&refresh);
            let ui_tx = bridge.ui_sender();
            let (tx, mut rx) = mpsc::unbounded_channel::<Result<(), String>>();
            bridge.spawn(async move {
                let r = management_client::set_purpose(transport.client(), purpose, config).await;
                let _ = tx.send(r.map_err(|e| e.to_string()));
            });
            glib::spawn_future_local(async move {
                if let Some(result) = rx.recv().await {
                    match result {
                        Ok(()) => refresh(),
                        Err(e) => {
                            let _ = ui_tx.send(UiMessage::Error(format!("Set purpose: {e}")));
                        }
                    }
                }
            });
        }
    ));

    purposes_tab.connect_request_models(glib::clone!(
        #[strong]
        transport,
        #[strong]
        bridge,
        #[strong(rename_to = purposes_tab_for_cb)]
        purposes_tab,
        move |connection_id| {
            let transport = Arc::clone(&transport);
            let ui_tx = bridge.ui_sender();
            let purposes_tab = Rc::clone(&purposes_tab_for_cb);
            let (tx, mut rx) = mpsc::unbounded_channel::<Result<Vec<api::ModelListing>, String>>();
            let connection_id_for_task = connection_id.clone();
            bridge.spawn(async move {
                let r = management_client::list_available_models(
                    transport.client(),
                    Some(connection_id_for_task),
                    false,
                )
                .await
                .map_err(|e| e.to_string());
                let _ = tx.send(r);
            });
            glib::spawn_future_local(async move {
                if let Some(result) = rx.recv().await {
                    match result {
                        Ok(listings) => {
                            purposes_tab.set_models(&connection_id, listings);
                        }
                        Err(e) => {
                            let _ = ui_tx.send(UiMessage::Error(format!("List models: {e}")));
                        }
                    }
                }
            });
        }
    ));

    // ---------------------------------------------------------------
    // Voice tab wiring (separate D-Bus service — see `voice_client`).
    // ---------------------------------------------------------------
    wire_voice_tab(&voice_tab, &voice, &bridge);

    // ---------------------------------------------------------------
    // General tab wiring (client-local preferences, da#549).
    //
    // The share-device-info toggle is backed by the client's own
    // `PreferencesStore` (not the daemon): hydrate it from the persisted value
    // (default on) and persist every change. The `preferences.json` read/write
    // is a tiny local file op, matching the existing sync store usage
    // (profiles, selected models); the new value is applied on the next
    // (re)connect, so the toggle notes that rather than reconnecting live.
    let prefs_store = Rc::new(PreferencesStore::new());
    general_tab.set_share_client_context(prefs_store.load().share_client_context);
    general_tab.connect_toggled(glib::clone!(
        #[strong]
        prefs_store,
        #[strong]
        status_label,
        move |on| {
            match prefs_store.set_share_client_context(on) {
                Ok(()) => status_label.set_text(
                    "Device-info sharing preference saved - applies on the next connect.",
                ),
                Err(e) => status_label.set_text(&format!("Save preference: {e}")),
            }
        }
    ));

    // ---------------------------------------------------------------
    // MCP-servers tab wiring (issue #495).
    // ---------------------------------------------------------------
    let refresh_mcp = mcp_refresh_closure(
        Arc::clone(&transport),
        Rc::clone(&bridge),
        Rc::clone(&mcp_tab),
        Rc::clone(&service_accounts),
        Rc::clone(&status_label),
        Rc::clone(&client_config),
        Rc::clone(&client_mcp_path),
        Arc::clone(&client_tool_counts),
        Arc::clone(&mcp_builtin_dtos),
        daemon_is_remote,
        daemon_host.clone(),
    );

    // Add: open a blank editor (defaulting to a daemon server); the "Runs on"
    // selector chooses the runner, and save forks to the daemon RPC path or the
    // local client-mcp.toml on it.
    mcp_tab.connect_add(glib::clone!(
        #[strong]
        transport,
        #[strong]
        bridge,
        #[strong]
        refresh_mcp,
        #[strong]
        service_accounts,
        #[strong]
        client_config,
        #[strong]
        client_mcp_path,
        #[strong(rename_to = mcp_tab_add)]
        mcp_tab,
        #[weak]
        dialog,
        move || {
            let accounts = service_accounts.borrow().clone();
            // The create-path uniqueness guard checks a typed name against the
            // currently-loaded servers of the SELECTED runner so an add can't
            // silently overwrite one (a client "files" and a daemon "files" are
            // distinct, so the lists are kept per-runner).
            let daemon_names = mcp_tab_add.daemon_names();
            let client_names: Vec<String> = client_config
                .borrow()
                .list_defined_servers()
                .iter()
                .map(|s| s.name.clone())
                .collect();
            let transport = Arc::clone(&transport);
            let bridge = Rc::clone(&bridge);
            let refresh = Rc::clone(&refresh_mcp);
            let path = (*client_mcp_path).clone();
            show_mcp_server_dialog(
                &dialog,
                McpForm::blank(McpTransport::Stdio),
                accounts,
                daemon_names,
                client_names,
                move |built| {
                    save_mcp_built(
                        Arc::clone(&transport),
                        Rc::clone(&bridge),
                        Rc::clone(&refresh),
                        path.clone(),
                        built,
                    );
                },
            );
        }
    ));

    // Edit: pre-fill the editor from the runner's own source (a daemon view, or
    // the cached client-mcp.toml definition), then save through the runner fork.
    // The runner is locked in the editor, so a save can't change sides.
    mcp_tab.connect_edit(glib::clone!(
        #[strong]
        transport,
        #[strong]
        bridge,
        #[strong]
        refresh_mcp,
        #[strong]
        service_accounts,
        #[strong]
        client_config,
        #[strong]
        client_mcp_path,
        #[strong(rename_to = mcp_tab_edit)]
        mcp_tab,
        #[weak]
        dialog,
        move |name, runner| {
            let accounts = service_accounts.borrow().clone();
            let form = match runner {
                Runner::Daemon => {
                    let Some(view) = mcp_tab_edit.find(&name) else {
                        return;
                    };
                    McpForm::from_view(&view)
                }
                Runner::Client => {
                    let cfg = client_config.borrow();
                    let Some(server) = cfg.list_defined_servers().iter().find(|s| s.name == name)
                    else {
                        return;
                    };
                    let enabled = mcp_admin::client_row_enabled(&cfg, &name);
                    McpForm::from_client_config(server, enabled)
                }
            };
            let transport = Arc::clone(&transport);
            let bridge = Rc::clone(&bridge);
            let refresh = Rc::clone(&refresh_mcp);
            let path = (*client_mcp_path).clone();
            // Edit targets an existing name by design; the dup-name guard is
            // create-only, so no existing-names lists are needed here.
            show_mcp_server_dialog(
                &dialog,
                form,
                accounts,
                Vec::new(),
                Vec::new(),
                move |built| {
                    save_mcp_built(
                        Arc::clone(&transport),
                        Rc::clone(&bridge),
                        Rc::clone(&refresh),
                        path.clone(),
                        built,
                    );
                },
            );
        }
    ));

    // Enable/disable toggle - forked by runner.
    mcp_tab.connect_toggle(glib::clone!(
        #[strong]
        transport,
        #[strong]
        bridge,
        #[strong]
        refresh_mcp,
        #[strong]
        client_mcp_path,
        move |name, runner, enabled| match mcp_admin::backend_for(runner) {
            McpBackend::Daemon => do_toggle_mcp(
                Arc::clone(&transport),
                Rc::clone(&bridge),
                Rc::clone(&refresh_mcp),
                name,
                enabled,
            ),
            McpBackend::Client => do_client_toggle(
                Rc::clone(&bridge),
                Rc::clone(&refresh_mcp),
                (*client_mcp_path).clone(),
                name,
                enabled,
            ),
        }
    ));

    // Built-in enable/disable toggle (da#538 slice 4): writes the client config's
    // per-surface `disabled_builtins` set. The running host keeps its built-in set
    // until the client restarts, so this persists the config + refreshes the display
    // (the refresh overlays the new disabled set) and notes it applies on restart.
    mcp_tab.connect_builtin_toggle(glib::clone!(
        #[strong]
        bridge,
        #[strong]
        refresh_mcp,
        #[strong]
        status_label,
        #[strong]
        client_mcp_path,
        move |name, disabled| {
            do_builtin_toggle(
                Rc::clone(&bridge),
                Rc::clone(&refresh_mcp),
                Rc::clone(&status_label),
                (*client_mcp_path).clone(),
                name,
                disabled,
            );
        }
    ));

    // Remove (with confirmation) - forked by runner.
    mcp_tab.connect_remove(glib::clone!(
        #[strong]
        transport,
        #[strong]
        bridge,
        #[strong]
        refresh_mcp,
        #[strong]
        client_mcp_path,
        #[weak]
        dialog,
        move |name, runner| {
            let transport = Arc::clone(&transport);
            let bridge = Rc::clone(&bridge);
            let refresh = Rc::clone(&refresh_mcp);
            let path = (*client_mcp_path).clone();
            let name_for_confirm = name.clone();
            let detail = match runner {
                Runner::Daemon => "Its tools will no longer be available to the assistant.",
                Runner::Client => {
                    "This removes the local server definition and drops it from every client surface."
                }
            };
            confirm(
                &dialog,
                &format!("Remove MCP server \"{name}\"?"),
                detail,
                "Remove",
                true,
                move || match mcp_admin::backend_for(runner) {
                    McpBackend::Daemon => do_remove_mcp(
                        Arc::clone(&transport),
                        Rc::clone(&bridge),
                        Rc::clone(&refresh),
                        name_for_confirm.clone(),
                    ),
                    McpBackend::Client => do_client_remove(
                        Rc::clone(&bridge),
                        Rc::clone(&refresh),
                        path.clone(),
                        name_for_confirm.clone(),
                    ),
                },
            );
        }
    ));

    // Sign in / configure - spawn the daemon's configure command on this host
    // (the GTK client runs on the daemon host, so it can drive the browser
    // flow), then refresh when it completes.
    mcp_tab.connect_signin(glib::clone!(
        #[strong]
        bridge,
        #[strong]
        refresh_mcp,
        #[strong]
        status_label,
        move |argv| {
            do_signin_mcp(
                Rc::clone(&bridge),
                Rc::clone(&refresh_mcp),
                Rc::clone(&status_label),
                argv,
            );
        }
    ));

    // First refresh.
    refresh();
    refresh_mcp();

    dialog.present();
}

/// Build the MCP-tab refresh closure: fetch the daemon server list + the OAuth
/// service accounts over the transport **and** load this client's on-disk MCP
/// config, all off the GTK main loop, then merge them into the panel on the main
/// thread. Called on present and after every MCP mutation.
///
/// The client config is loaded on the runtime (a file read must not block the
/// main loop), cached for client-row edits, and projected to the panel's client
/// rows — with a snapshot of `client_tool_counts` (namespace -> live tool total)
/// so each client row shows a real count. The client's compiled-in built-ins are
/// merged in from a snapshot of `mcp_builtin_dtos` (da#538 Phase D).
/// `daemon_is_remote`/`daemon_host` drive the daemon rows' runner chip.
#[allow(clippy::too_many_arguments)]
fn mcp_refresh_closure(
    transport: Arc<Connector>,
    bridge: Rc<AsyncBridge>,
    mcp_tab: Rc<McpServersTab>,
    service_accounts: Rc<RefCell<Vec<api::ServiceAccountView>>>,
    status_label: Rc<Label>,
    client_config: Rc<RefCell<ClientMcpConfig>>,
    client_mcp_path: Rc<PathBuf>,
    client_tool_counts: Arc<Mutex<HashMap<String, u32>>>,
    mcp_builtin_dtos: Arc<Mutex<Vec<BuiltinServerDto>>>,
    daemon_is_remote: bool,
    daemon_host: Option<String>,
) -> Rc<dyn Fn()> {
    Rc::new(move || {
        let transport = Arc::clone(&transport);
        let mcp_tab = Rc::clone(&mcp_tab);
        let service_accounts = Rc::clone(&service_accounts);
        let status_label = Rc::clone(&status_label);
        let client_config = Rc::clone(&client_config);
        let client_tool_counts = Arc::clone(&client_tool_counts);
        let mcp_builtin_dtos = Arc::clone(&mcp_builtin_dtos);
        let path = (*client_mcp_path).clone();
        let daemon_host = daemon_host.clone();
        #[allow(clippy::type_complexity)]
        let (tx, mut rx) = mpsc::unbounded_channel::<(
            Result<Vec<api::McpServerView>, String>,
            Result<Vec<api::ServiceAccountView>, String>,
            ClientMcpConfig,
        )>();
        bridge.spawn(async move {
            let client = transport.client();
            let servers = management_client::list_mcp_servers(client)
                .await
                .map_err(|e| e.to_string());
            let accounts = management_client::list_service_accounts(client)
                .await
                .map_err(|e| e.to_string());
            // File read: on the runtime, never the GTK main loop.
            let cfg = ClientMcpConfig::load(&path);
            let _ = tx.send((servers, accounts, cfg));
        });
        glib::spawn_future_local(async move {
            if let Some((servers, accounts, cfg)) = rx.recv().await {
                match accounts {
                    Ok(list) => *service_accounts.borrow_mut() = list,
                    Err(e) => status_label.set_text(&format!("List service accounts: {e}")),
                }
                // Daemon rows: keep the client-side view even if the daemon list
                // fails, so client-hosted servers still render.
                let daemon_views = match servers {
                    Ok(list) => list,
                    Err(e) => {
                        status_label.set_text(&format!("List MCP servers: {e}"));
                        Vec::new()
                    }
                };
                // Snapshot the live per-server tool counts (a quick lock+clone on
                // the main thread; never held across an await) so client rows show
                // a real count instead of 0.
                let counts = client_tool_counts.lock().unwrap().clone();
                let client_rows = mcp_admin::client_server_dtos(&cfg, &counts);
                // Snapshot the client's compiled-in built-ins the same way, so they
                // merge into the panel (da#538 Phase D). The host snapshots its
                // disabled set once at start (built-ins are fixed until relaunch), so
                // overlay the *current* config's `disabled_builtins` for this surface
                // to reflect a live toggle's pending state (da#538 slice 4).
                let mut builtins = mcp_builtin_dtos.lock().unwrap().clone();
                mcp_admin::apply_builtin_disabled_overlay(
                    &cfg,
                    mcp_admin::GTK_SURFACE,
                    &mut builtins,
                );
                mcp_tab.set_data(
                    &daemon_views,
                    &client_rows,
                    &builtins,
                    daemon_is_remote,
                    daemon_host.as_deref(),
                );
                *client_config.borrow_mut() = cfg;
            }
        });
    })
}

/// Write any bearer secret (before the upsert), then upsert the server config,
/// then refresh. A blank bearer field carries no secret and leaves any stored
/// token untouched.
fn do_upsert_mcp(
    transport: Arc<Connector>,
    bridge: Rc<AsyncBridge>,
    refresh: Rc<dyn Fn()>,
    built: BuiltMcpServer,
) {
    let ui_tx = bridge.ui_sender();
    let (tx, mut rx) = mpsc::unbounded_channel::<Result<(), String>>();
    bridge.spawn(async move {
        let client = transport.client();
        // Secret first: the daemon reloads its secrets so the upsert that
        // references the ref resolves it.
        if let Some((id, value)) = built.secret
            && let Err(e) = management_client::set_mcp_secret(client, id, api::Secret(value)).await
        {
            let _ = tx.send(Err(format!("store token: {e}")));
            return;
        }
        let r = management_client::upsert_mcp_server(client, built.config_json)
            .await
            .map_err(|e| e.to_string());
        let _ = tx.send(r);
    });
    glib::spawn_future_local(async move {
        if let Some(r) = rx.recv().await {
            match r {
                Ok(()) => refresh(),
                Err(e) => {
                    let _ = ui_tx.send(UiMessage::Error(format!("Save MCP server: {e}")));
                }
            }
        }
    });
}

/// Fork a built server save on its runner: a daemon server goes through the
/// daemon RPC path ([`do_upsert_mcp`]); a client server is written to the local
/// `client-mcp.toml` ([`do_client_save`]).
fn save_mcp_built(
    transport: Arc<Connector>,
    bridge: Rc<AsyncBridge>,
    refresh: Rc<dyn Fn()>,
    client_path: PathBuf,
    built: BuiltMcpServer,
) {
    match mcp_admin::backend_for(built.runner) {
        McpBackend::Daemon => do_upsert_mcp(transport, bridge, refresh, built),
        McpBackend::Client => do_client_save(bridge, refresh, client_path, built),
    }
}

/// Save a client-hosted server to `client-mcp.toml` off the main loop: parse the
/// built config, load the current file, apply the definition + gtk-surface
/// change, and write it back atomically. Then refresh. The dialog's config JSON
/// carries no bearer secret store on the client, so any typed bearer token is
/// dropped (client servers are stdio in practice; http-bearer on the client is a
/// follow-up).
fn do_client_save(
    bridge: Rc<AsyncBridge>,
    refresh: Rc<dyn Fn()>,
    client_path: PathBuf,
    built: BuiltMcpServer,
) {
    let ui_tx = bridge.ui_sender();
    let (tx, mut rx) = mpsc::unbounded_channel::<Result<(), String>>();
    bridge.spawn(async move {
        let result = (|| {
            let server = mcp_admin::parse_server_config(&built.config_json)?;
            let enabled = server.enabled;
            let mut cfg = ClientMcpConfig::load(&client_path);
            mcp_admin::apply_client_save(&mut cfg, server, enabled);
            cfg.save(&client_path)
        })();
        let _ = tx.send(result);
    });
    glib::spawn_future_local(async move {
        if let Some(r) = rx.recv().await {
            match r {
                Ok(()) => refresh(),
                Err(e) => {
                    let _ = ui_tx.send(UiMessage::Error(format!("Save client MCP server: {e}")));
                }
            }
        }
    });
}

/// Enable/disable a client-hosted server for the gtk surface (asymmetrically:
/// enabling sets both grains, disabling is surface-scoped) and write the config
/// back off the main loop, then refresh.
fn do_client_toggle(
    bridge: Rc<AsyncBridge>,
    refresh: Rc<dyn Fn()>,
    client_path: PathBuf,
    name: String,
    enabled: bool,
) {
    let ui_tx = bridge.ui_sender();
    let (tx, mut rx) = mpsc::unbounded_channel::<Result<(), String>>();
    bridge.spawn(async move {
        let mut cfg = ClientMcpConfig::load(&client_path);
        let result = mcp_admin::apply_client_toggle(&mut cfg, &name, enabled)
            .and_then(|()| cfg.save(&client_path));
        let _ = tx.send(result);
    });
    glib::spawn_future_local(async move {
        if let Some(r) = rx.recv().await {
            match r {
                Ok(()) => refresh(),
                Err(e) => {
                    let _ = ui_tx.send(UiMessage::Error(format!("Toggle client MCP server: {e}")));
                }
            }
        }
    });
}

/// Enable/disable a compiled-in **built-in** server for the gtk surface (da#538
/// slice 4): load the client config off the main loop, set its per-surface
/// `disabled_builtins` membership via [`ClientMcpConfig::set_builtin_disabled`],
/// write it back, then refresh and note that it applies on restart.
///
/// The running [`McpHost`](desktop_assistant_client_common::mcp_host::McpHost)
/// snapshots its built-in set once at start, so it does NOT drop/add the built-in
/// live; the refresh re-projects the row's disabled state from the just-written
/// config (overlaying the host snapshot), and the status note tells the user the
/// hosting change lands on the next client launch. `disabled == true` turns it off.
fn do_builtin_toggle(
    bridge: Rc<AsyncBridge>,
    refresh: Rc<dyn Fn()>,
    status_label: Rc<Label>,
    client_path: PathBuf,
    name: String,
    disabled: bool,
) {
    let ui_tx = bridge.ui_sender();
    let (tx, mut rx) = mpsc::unbounded_channel::<Result<(), String>>();
    let name_for_task = name.clone();
    bridge.spawn(async move {
        let mut cfg = ClientMcpConfig::load(&client_path);
        cfg.set_builtin_disabled(mcp_admin::GTK_SURFACE, &name_for_task, disabled);
        let _ = tx.send(cfg.save(&client_path));
    });
    glib::spawn_future_local(async move {
        if let Some(r) = rx.recv().await {
            match r {
                Ok(()) => {
                    let verb = if disabled { "disabled" } else { "enabled" };
                    status_label.set_text(&format!(
                        "Built-in \"{name}\" {verb} - applies after the client restarts."
                    ));
                    refresh();
                }
                Err(e) => {
                    let _ =
                        ui_tx.send(UiMessage::Error(format!("Toggle built-in MCP server: {e}")));
                }
            }
        }
    });
}

/// Remove a client-hosted server definition and write the config back off the
/// main loop, then refresh.
fn do_client_remove(
    bridge: Rc<AsyncBridge>,
    refresh: Rc<dyn Fn()>,
    client_path: PathBuf,
    name: String,
) {
    let ui_tx = bridge.ui_sender();
    let (tx, mut rx) = mpsc::unbounded_channel::<Result<(), String>>();
    bridge.spawn(async move {
        let mut cfg = ClientMcpConfig::load(&client_path);
        let result =
            mcp_admin::apply_client_remove(&mut cfg, &name).and_then(|()| cfg.save(&client_path));
        let _ = tx.send(result);
    });
    glib::spawn_future_local(async move {
        if let Some(r) = rx.recv().await {
            match r {
                Ok(()) => refresh(),
                Err(e) => {
                    let _ = ui_tx.send(UiMessage::Error(format!("Remove client MCP server: {e}")));
                }
            }
        }
    });
}

/// Enable/disable an MCP server, then refresh.
fn do_toggle_mcp(
    transport: Arc<Connector>,
    bridge: Rc<AsyncBridge>,
    refresh: Rc<dyn Fn()>,
    name: String,
    enabled: bool,
) {
    let ui_tx = bridge.ui_sender();
    let (tx, mut rx) = mpsc::unbounded_channel::<Result<(), String>>();
    bridge.spawn(async move {
        let r = management_client::set_mcp_server_enabled(transport.client(), name, enabled)
            .await
            .map_err(|e| e.to_string());
        let _ = tx.send(r);
    });
    glib::spawn_future_local(async move {
        if let Some(r) = rx.recv().await {
            match r {
                Ok(()) => refresh(),
                Err(e) => {
                    let _ = ui_tx.send(UiMessage::Error(format!("Toggle MCP server: {e}")));
                }
            }
        }
    });
}

/// Remove an MCP server by name, then refresh.
fn do_remove_mcp(
    transport: Arc<Connector>,
    bridge: Rc<AsyncBridge>,
    refresh: Rc<dyn Fn()>,
    name: String,
) {
    let ui_tx = bridge.ui_sender();
    let (tx, mut rx) = mpsc::unbounded_channel::<Result<(), String>>();
    bridge.spawn(async move {
        let r = management_client::remove_mcp_server(transport.client(), name)
            .await
            .map_err(|e| e.to_string());
        let _ = tx.send(r);
    });
    glib::spawn_future_local(async move {
        if let Some(r) = rx.recv().await {
            match r {
                Ok(()) => refresh(),
                Err(e) => {
                    let _ = ui_tx.send(UiMessage::Error(format!("Remove MCP server: {e}")));
                }
            }
        }
    });
}

/// Spawn the daemon-provided configure/sign-in command (an argv like
/// `[daemon_exe, "--mcp-oauth-login", id]`) on this host and refresh when it
/// finishes. The argv is executed directly (no shell), so there is no shell
/// interpretation of its parts. Empty argv is a no-op.
fn do_signin_mcp(
    bridge: Rc<AsyncBridge>,
    refresh: Rc<dyn Fn()>,
    status_label: Rc<Label>,
    argv: Vec<String>,
) {
    if argv.is_empty() {
        return;
    }
    status_label.set_text(
        "Launched sign-in - finish in the browser; the list refreshes when it completes.",
    );
    let ui_tx = bridge.ui_sender();
    let (tx, mut rx) = mpsc::unbounded_channel::<Result<(), String>>();
    bridge.spawn(async move {
        let mut cmd = tokio::process::Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        let r = match cmd.status().await {
            Ok(status) if status.success() => Ok(()),
            Ok(status) => Err(format!("sign-in exited with {status}")),
            Err(e) => Err(format!("failed to launch sign-in: {e}")),
        };
        let _ = tx.send(r);
    });
    glib::spawn_future_local(async move {
        if let Some(r) = rx.recv().await {
            match r {
                Ok(()) => refresh(),
                Err(e) => {
                    let _ = ui_tx.send(UiMessage::Error(format!("MCP sign-in: {e}")));
                }
            }
        }
    });
}

/// Snapshot of the voice daemon's state used to hydrate the Voice tab in one
/// hop back to the GTK main thread. `available == false` means the daemon has
/// no owner on the bus; the tab then shows its "unavailable" message.
struct VoiceSnapshot {
    available: bool,
    enabled: bool,
    voices: Vec<crate::voice_client::VoiceInfo>,
    current_voice: Option<String>,
}

/// Wire the Voice tab's toggle/dropdown to the [`VoiceController`] and hydrate
/// it from the daemon's current state.
///
/// Writes (`SetEnabled` / `SetVoice`) are dispatched on the Tokio runtime;
/// failures go to the window status bar via the bridge's ui-sender. Hydration
/// fetches availability + enabled + voices in one task and applies the result
/// on the GTK main thread. Graceful: when the daemon is absent the tab is
/// disabled with an explanatory message instead of erroring.
fn wire_voice_tab(voice_tab: &Rc<VoiceTab>, voice: &VoiceController, bridge: &Rc<AsyncBridge>) {
    // Toggle "Hey Adele" → SetEnabled.
    voice_tab.connect_set_enabled(glib::clone!(
        #[strong]
        voice,
        #[strong]
        bridge,
        move |enabled| {
            let voice = voice.clone();
            let ui_tx = bridge.ui_sender();
            crate::async_bridge::spawn_on_runtime(async move {
                if let Err(e) = voice.set_enabled(enabled).await {
                    let _ = ui_tx.send(UiMessage::Error(format!("Set wake word: {e}")));
                }
            });
        }
    ));

    // Pick a voice → SetVoice (default speaker -1).
    voice_tab.connect_set_voice(glib::clone!(
        #[strong]
        voice,
        #[strong]
        bridge,
        move |voice_id| {
            let voice = voice.clone();
            let ui_tx = bridge.ui_sender();
            crate::async_bridge::spawn_on_runtime(async move {
                if let Err(e) = voice.set_voice(voice_id, -1).await {
                    let _ = ui_tx.send(UiMessage::Error(format!("Set voice: {e}")));
                }
            });
        }
    ));

    // Hydrate from the daemon. One task fetches the snapshot; the result is
    // applied on the GTK main thread via a short-lived channel.
    let (tx, mut rx) = mpsc::unbounded_channel::<VoiceSnapshot>();
    let voice_for_fetch = voice.clone();
    bridge.spawn(async move {
        let available = voice_for_fetch.is_available().await;
        let snapshot = if available {
            VoiceSnapshot {
                available: true,
                enabled: voice_for_fetch.get_enabled().await.unwrap_or(false),
                voices: voice_for_fetch.list_voices().await.unwrap_or_default(),
                current_voice: voice_for_fetch.get_voice().await.map(|v| v.id),
            }
        } else {
            VoiceSnapshot {
                available: false,
                enabled: false,
                voices: Vec::new(),
                current_voice: None,
            }
        };
        let _ = tx.send(snapshot);
    });

    let voice_tab = Rc::clone(voice_tab);
    glib::spawn_future_local(async move {
        if let Some(snapshot) = rx.recv().await {
            if !snapshot.available {
                voice_tab.set_unavailable();
                return;
            }
            voice_tab.set_available();
            voice_tab.set_enabled_state(snapshot.enabled);
            voice_tab.set_voices(&snapshot.voices, snapshot.current_voice.as_deref());
        }
    });
}

/// Issue a `DeleteConnection` call. On refusal (purposes still reference
/// it), prompt the user to retry with `force: true`. On force success,
/// the daemon rewires referencing purposes to fall back to interactive.
fn do_remove_connection(
    parent: &impl IsA<Window>,
    transport: Arc<Connector>,
    bridge: Rc<AsyncBridge>,
    refresh: Rc<dyn Fn()>,
    id: String,
    force: bool,
) {
    let ui_tx = bridge.ui_sender();
    let (tx, mut rx) = mpsc::unbounded_channel::<Result<(), String>>();
    let transport_for_task = Arc::clone(&transport);
    let id_for_task = id.clone();
    bridge.spawn(async move {
        let r =
            management_client::delete_connection(transport_for_task.client(), id_for_task, force)
                .await;
        let _ = tx.send(r.map_err(|e| e.to_string()));
    });

    let parent = parent.clone();
    let refresh_outer = Rc::clone(&refresh);
    glib::spawn_future_local(async move {
        if let Some(result) = rx.recv().await {
            match result {
                Ok(()) => refresh_outer(),
                Err(e) if !force => {
                    let id_for_retry = id.clone();
                    let parent_inner = parent.clone();
                    let transport_inner = Arc::clone(&transport);
                    let bridge_inner = Rc::clone(&bridge);
                    let refresh_inner = Rc::clone(&refresh_outer);
                    let parent_for_confirm = parent.clone();
                    confirm(
                        &parent_for_confirm,
                        &format!("Cannot remove \"{id}\""),
                        &format!(
                            "{e}\n\nForce-remove will re-assign any referencing purpose to the interactive purpose."
                        ),
                        "Force remove",
                        true,
                        move || {
                            do_remove_connection(
                                &parent_inner,
                                Arc::clone(&transport_inner),
                                Rc::clone(&bridge_inner),
                                Rc::clone(&refresh_inner),
                                id_for_retry.clone(),
                                true,
                            );
                        },
                    );
                }
                Err(e) => {
                    let _ = ui_tx.send(UiMessage::Error(format!("Delete connection: {e}")));
                }
            }
        }
    });
}
