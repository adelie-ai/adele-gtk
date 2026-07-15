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
use std::rc::Rc;
use std::sync::Arc;

use desktop_assistant_api_model as api;
use desktop_assistant_client_common::Connector;
use gtk4::prelude::*;
use gtk4::{Align, Box as GtkBox, Button, Label, Notebook, Orientation, Window, glib};
use tokio::sync::mpsc;

use crate::async_bridge::{AsyncBridge, UiMessage};
use crate::management_client;
use crate::voice_client::VoiceController;

use super::connection_config_dialog::{ConnectorType, show_configure_dialog};
use super::connections_tab::ConnectionsTab;
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
pub fn show_settings_dialog(
    parent: &impl IsA<Window>,
    transport: Arc<Connector>,
    bridge: Rc<AsyncBridge>,
    voice: VoiceController,
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

    // Snapshot of the OAuth service accounts (epic #477), refreshed alongside
    // the MCP server list and handed to the editor's account picker.
    let service_accounts: Rc<RefCell<Vec<api::ServiceAccountView>>> =
        Rc::new(RefCell::new(Vec::new()));

    notebook.append_page(
        &connections_tab.container,
        Some(&Label::new(Some("Connections"))),
    );
    notebook.append_page(&purposes_tab.container, Some(&Label::new(Some("Purposes"))));
    notebook.append_page(&mcp_tab.container, Some(&Label::new(Some("MCP Servers"))));
    notebook.append_page(&voice_tab.container, Some(&Label::new(Some("Voice"))));

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
    // MCP-servers tab wiring (issue #495).
    // ---------------------------------------------------------------
    let refresh_mcp = mcp_refresh_closure(
        Arc::clone(&transport),
        Rc::clone(&bridge),
        Rc::clone(&mcp_tab),
        Rc::clone(&service_accounts),
        Rc::clone(&status_label),
    );

    // Add: open a blank editor; save writes any bearer token then upserts.
    mcp_tab.connect_add(glib::clone!(
        #[strong]
        transport,
        #[strong]
        bridge,
        #[strong]
        refresh_mcp,
        #[strong]
        service_accounts,
        #[strong(rename_to = mcp_tab_add)]
        mcp_tab,
        #[weak]
        dialog,
        move || {
            let accounts = service_accounts.borrow().clone();
            // The create-path uniqueness guard checks the typed name against the
            // currently-loaded servers so an add can't silently overwrite one.
            let existing_names = mcp_tab_add.server_names();
            let transport = Arc::clone(&transport);
            let bridge = Rc::clone(&bridge);
            let refresh = Rc::clone(&refresh_mcp);
            show_mcp_server_dialog(
                &dialog,
                McpForm::blank(McpTransport::Stdio),
                accounts,
                existing_names,
                move |built| {
                    do_upsert_mcp(
                        Arc::clone(&transport),
                        Rc::clone(&bridge),
                        Rc::clone(&refresh),
                        built,
                    );
                },
            );
        }
    ));

    // Edit: look up the loaded view, pre-fill the editor (write-only fields
    // stay blank), save through the same upsert path.
    mcp_tab.connect_edit(glib::clone!(
        #[strong]
        transport,
        #[strong]
        bridge,
        #[strong]
        refresh_mcp,
        #[strong]
        service_accounts,
        #[strong(rename_to = mcp_tab_edit)]
        mcp_tab,
        #[weak]
        dialog,
        move |name| {
            let Some(view) = mcp_tab_edit.find(&name) else {
                return;
            };
            let accounts = service_accounts.borrow().clone();
            let transport = Arc::clone(&transport);
            let bridge = Rc::clone(&bridge);
            let refresh = Rc::clone(&refresh_mcp);
            // Edit targets an existing name by design; the dup-name guard is
            // create-only, so no existing-names list is needed here.
            show_mcp_server_dialog(
                &dialog,
                McpForm::from_view(&view),
                accounts,
                Vec::new(),
                move |built| {
                    do_upsert_mcp(
                        Arc::clone(&transport),
                        Rc::clone(&bridge),
                        Rc::clone(&refresh),
                        built,
                    );
                },
            );
        }
    ));

    // Enable/disable toggle.
    mcp_tab.connect_toggle(glib::clone!(
        #[strong]
        transport,
        #[strong]
        bridge,
        #[strong]
        refresh_mcp,
        move |name, enabled| {
            do_toggle_mcp(
                Arc::clone(&transport),
                Rc::clone(&bridge),
                Rc::clone(&refresh_mcp),
                name,
                enabled,
            );
        }
    ));

    // Remove (with confirmation).
    mcp_tab.connect_remove(glib::clone!(
        #[strong]
        transport,
        #[strong]
        bridge,
        #[strong]
        refresh_mcp,
        #[weak]
        dialog,
        move |name| {
            let transport = Arc::clone(&transport);
            let bridge = Rc::clone(&bridge);
            let refresh = Rc::clone(&refresh_mcp);
            let name_for_confirm = name.clone();
            confirm(
                &dialog,
                &format!("Remove MCP server \"{name}\"?"),
                "Its tools will no longer be available to the assistant.",
                "Remove",
                true,
                move || {
                    do_remove_mcp(
                        Arc::clone(&transport),
                        Rc::clone(&bridge),
                        Rc::clone(&refresh),
                        name_for_confirm.clone(),
                    );
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

/// Build the MCP-tab refresh closure: fetch the server list + the OAuth service
/// accounts on the runtime and apply them on the GTK main thread. Called on
/// present and after every MCP mutation.
fn mcp_refresh_closure(
    transport: Arc<Connector>,
    bridge: Rc<AsyncBridge>,
    mcp_tab: Rc<McpServersTab>,
    service_accounts: Rc<RefCell<Vec<api::ServiceAccountView>>>,
    status_label: Rc<Label>,
) -> Rc<dyn Fn()> {
    Rc::new(move || {
        let transport = Arc::clone(&transport);
        let mcp_tab = Rc::clone(&mcp_tab);
        let service_accounts = Rc::clone(&service_accounts);
        let status_label = Rc::clone(&status_label);
        #[allow(clippy::type_complexity)]
        let (tx, mut rx) = mpsc::unbounded_channel::<(
            Result<Vec<api::McpServerView>, String>,
            Result<Vec<api::ServiceAccountView>, String>,
        )>();
        bridge.spawn(async move {
            let client = transport.client();
            let servers = management_client::list_mcp_servers(client)
                .await
                .map_err(|e| e.to_string());
            let accounts = management_client::list_service_accounts(client)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send((servers, accounts));
        });
        glib::spawn_future_local(async move {
            if let Some((servers, accounts)) = rx.recv().await {
                match accounts {
                    Ok(list) => *service_accounts.borrow_mut() = list,
                    Err(e) => status_label.set_text(&format!("List service accounts: {e}")),
                }
                match servers {
                    Ok(list) => mcp_tab.set_servers(&list),
                    Err(e) => status_label.set_text(&format!("List MCP servers: {e}")),
                }
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
