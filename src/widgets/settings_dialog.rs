//! Top-level Settings dialog (Notebook with Connections + Purposes tabs).
//!
//! Owns the live transport handle and wires tab callbacks to async
//! management RPCs. The dialog is transient to the parent window and
//! refreshes its data every time it is presented (it is cheap: a couple
//! of RPCs).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use desktop_assistant_api_model as api;
use desktop_assistant_client_common::TransportClient;
use gtk4::prelude::*;
use gtk4::{
    Align, Box as GtkBox, Button, Label, Notebook, Orientation, Window, glib,
};
use tokio::sync::mpsc;

use crate::async_bridge::{UiMessage, spawn_on_runtime};
use crate::management_client;

use super::connection_config_dialog::{ConnectorType, show_configure_dialog};
use super::connections_tab::ConnectionsTab;
use super::purposes_tab::PurposesTab;

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

    let dialog_ref = dialog.clone();
    cancel.connect_clicked(move |_| dialog_ref.close());

    let dialog_ref = dialog.clone();
    confirm_btn.connect_clicked(move |_| {
        dialog_ref.close();
        on_confirm();
    });

    dialog.present();
}

/// Present the Settings dialog modally against `parent`, using `client`
/// for all RPC traffic. `ui_tx` is the shared ui-message channel so that
/// errors propagate back to the window status bar.
pub fn show_settings_dialog(
    parent: &impl IsA<Window>,
    client: Rc<RefCell<Option<Arc<TransportClient>>>>,
    ui_tx: mpsc::UnboundedSender<UiMessage>,
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

    notebook.append_page(
        &connections_tab.container,
        Some(&Label::new(Some("Connections"))),
    );
    notebook.append_page(
        &purposes_tab.container,
        Some(&Label::new(Some("Purposes"))),
    );

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
    let dialog_ref = dialog.clone();
    close_btn.connect_clicked(move |_| dialog_ref.close());
    footer.append(&close_btn);

    vbox.append(&footer);
    dialog.set_child(Some(&vbox));

    // ---------------------------------------------------------------
    // Data-refresh helper: re-fetches connections + purposes and
    // repopulates the tabs. Called on present and after any mutation.
    // ---------------------------------------------------------------
    let refresh: Rc<dyn Fn()> = {
        let client = Rc::clone(&client);
        let connections_tab = Rc::clone(&connections_tab);
        let purposes_tab = Rc::clone(&purposes_tab);
        let status_label = Rc::clone(&status_label);
        Rc::new(move || {
            let Some(transport) = client.borrow().clone() else {
                status_label.set_text("Not connected to the daemon.");
                return;
            };
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
            spawn_on_runtime(async move {
                let conns = management_client::list_connections(&transport_for_task)
                    .await
                    .map_err(|e| e.to_string());
                let _ = tx_conn.send(conns);

                let purposes = management_client::get_purposes(&transport_for_task)
                    .await
                    .map_err(|e| e.to_string());
                let _ = tx_purposes.send(purposes);

                // Aggregated list (None connection_id) populates model
                // caches for every healthy connection at once.
                let models = management_client::list_available_models(&transport_for_task, None, false)
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
                            // Group by connection id and hand each slice
                            // to the purposes tab so per-connection
                            // dropdowns can populate without firing extra
                            // requests.
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
        })
    };

    // ---------------------------------------------------------------
    // Connections tab wiring.
    // ---------------------------------------------------------------
    {
        let client = Rc::clone(&client);
        let refresh = Rc::clone(&refresh);
        let ui_tx = ui_tx.clone();
        let dialog_weak = dialog.downgrade();
        connections_tab.connect_add(move |connector| {
            let Some(parent) = dialog_weak.upgrade() else {
                return;
            };
            let client = Rc::clone(&client);
            let refresh = Rc::clone(&refresh);
            let ui_tx = ui_tx.clone();
            show_configure_dialog(
                &parent,
                connector,
                None,
                move |id, config| {
                    let Some(transport) = client.borrow().clone() else {
                        let _ = ui_tx.send(UiMessage::Error("Not connected".into()));
                        return;
                    };
                    let refresh = Rc::clone(&refresh);
                    let ui_tx = ui_tx.clone();
                    let (tx, mut rx) = mpsc::unbounded_channel::<Result<(), String>>();
                    let transport_for_task = Arc::clone(&transport);
                    let id_for_task = id.clone();
                    spawn_on_runtime(async move {
                        let r = management_client::create_connection(
                            &transport_for_task,
                            id_for_task,
                            config,
                        )
                        .await
                        .map_err(|e| e.to_string());
                        let _ = tx.send(r);
                    });
                    glib::spawn_future_local(async move {
                        if let Some(result) = rx.recv().await {
                            match result {
                                Ok(()) => refresh(),
                                Err(e) => {
                                    let _ =
                                        ui_tx.send(UiMessage::Error(format!("Create connection: {e}")));
                                }
                            }
                        }
                    });
                },
                // Add-flow never has a connection to refresh yet.
                |_id| {},
            );
        });
    }

    {
        let client_cfg = Rc::clone(&client);
        let connections_tab_cfg = Rc::clone(&connections_tab);
        let refresh = Rc::clone(&refresh);
        let ui_tx = ui_tx.clone();
        let dialog_weak = dialog.downgrade();
        connections_tab.connect_configure(move |id| {
            let Some(parent) = dialog_weak.upgrade() else {
                return;
            };
            let Some(existing) = connections_tab_cfg.find(&id) else {
                return;
            };
            let connector = match ConnectorType::from_slug(&existing.connector_type) {
                Some(c) => c,
                None => {
                    let _ = ui_tx.send(UiMessage::Error(format!(
                        "Unknown connector type: {}",
                        existing.connector_type
                    )));
                    return;
                }
            };
            // We don't have the raw config from the daemon in ConnectionView.
            // Pass an empty config of the right variant; the user re-enters
            // fields on edit. (ConnectionConfigView fields aren't echoed
            // back from the server — credentials are never surfaced.)
            let existing_pair = Some((existing.id.clone(), connector.empty_config()));

            let client = Rc::clone(&client_cfg);
            let refresh_inner = Rc::clone(&refresh);
            let ui_tx_inner = ui_tx.clone();
            let client_for_refresh = Rc::clone(&client);
            let ui_tx_for_refresh = ui_tx.clone();

            show_configure_dialog(
                &parent,
                connector,
                existing_pair,
                move |id, config| {
                    let Some(transport) = client.borrow().clone() else {
                        let _ = ui_tx_inner.send(UiMessage::Error("Not connected".into()));
                        return;
                    };
                    let refresh = Rc::clone(&refresh_inner);
                    let ui_tx = ui_tx_inner.clone();
                    let (tx, mut rx) = mpsc::unbounded_channel::<Result<(), String>>();
                    let transport_for_task = Arc::clone(&transport);
                    let id_for_task = id.clone();
                    spawn_on_runtime(async move {
                        let r = management_client::update_connection(
                            &transport_for_task,
                            id_for_task,
                            config,
                        )
                        .await
                        .map_err(|e| e.to_string());
                        let _ = tx.send(r);
                    });
                    glib::spawn_future_local(async move {
                        if let Some(result) = rx.recv().await {
                            match result {
                                Ok(()) => refresh(),
                                Err(e) => {
                                    let _ = ui_tx.send(UiMessage::Error(format!(
                                        "Update connection: {e}"
                                    )));
                                }
                            }
                        }
                    });
                },
                move |id_for_refresh| {
                    // Refresh Bedrock models cache.
                    let Some(transport) = client_for_refresh.borrow().clone() else {
                        return;
                    };
                    let ui_tx = ui_tx_for_refresh.clone();
                    spawn_on_runtime(async move {
                        let r = management_client::list_available_models(
                            &transport,
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
        });
    }

    {
        let client = Rc::clone(&client);
        let refresh = Rc::clone(&refresh);
        let ui_tx = ui_tx.clone();
        let dialog_weak = dialog.downgrade();
        connections_tab.connect_remove(move |id| {
            let Some(parent) = dialog_weak.upgrade() else {
                return;
            };
            let client_for_confirm = Rc::clone(&client);
            let refresh_for_confirm = Rc::clone(&refresh);
            let ui_tx_for_confirm = ui_tx.clone();
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
                        client_for_confirm.clone(),
                        refresh_for_confirm.clone(),
                        ui_tx_for_confirm.clone(),
                        id_for_confirm.clone(),
                        false,
                    );
                },
            );
        });
    }

    // ---------------------------------------------------------------
    // Purposes tab wiring.
    // ---------------------------------------------------------------
    {
        let client = Rc::clone(&client);
        let refresh = Rc::clone(&refresh);
        let ui_tx = ui_tx.clone();
        purposes_tab.connect_set_purpose(move |purpose, config| {
            let Some(transport) = client.borrow().clone() else {
                let _ = ui_tx.send(UiMessage::Error("Not connected".into()));
                return;
            };
            let refresh = Rc::clone(&refresh);
            let ui_tx = ui_tx.clone();
            let (tx, mut rx) = mpsc::unbounded_channel::<Result<(), String>>();
            let transport_for_task = Arc::clone(&transport);
            spawn_on_runtime(async move {
                let r =
                    management_client::set_purpose(&transport_for_task, purpose, config).await;
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
        });
    }

    {
        let client = Rc::clone(&client);
        let purposes_tab_for_cb = Rc::clone(&purposes_tab);
        let ui_tx = ui_tx.clone();
        purposes_tab.connect_request_models(move |connection_id| {
            let Some(transport) = client.borrow().clone() else {
                return;
            };
            let ui_tx = ui_tx.clone();
            let purposes_tab = Rc::clone(&purposes_tab_for_cb);
            let (tx, mut rx) =
                mpsc::unbounded_channel::<Result<Vec<api::ModelListing>, String>>();
            let transport_for_task = Arc::clone(&transport);
            let connection_id_for_task = connection_id.clone();
            spawn_on_runtime(async move {
                let r = management_client::list_available_models(
                    &transport_for_task,
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
                            let _ =
                                ui_tx.send(UiMessage::Error(format!("List models: {e}")));
                        }
                    }
                }
            });
        });
    }

    // First refresh.
    refresh();

    dialog.present();
}

/// Issue a `DeleteConnection` call. On refusal (purposes still reference
/// it), prompt the user to retry with `force: true`. On force success,
/// the daemon rewires referencing purposes to fall back to interactive.
fn do_remove_connection(
    parent: &impl IsA<Window>,
    client: Rc<RefCell<Option<Arc<TransportClient>>>>,
    refresh: Rc<dyn Fn()>,
    ui_tx: mpsc::UnboundedSender<UiMessage>,
    id: String,
    force: bool,
) {
    let Some(transport) = client.borrow().clone() else {
        let _ = ui_tx.send(UiMessage::Error("Not connected".into()));
        return;
    };
    let (tx, mut rx) = mpsc::unbounded_channel::<Result<(), String>>();
    let transport_for_task = Arc::clone(&transport);
    let id_for_task = id.clone();
    spawn_on_runtime(async move {
        let r =
            management_client::delete_connection(&transport_for_task, id_for_task, force).await;
        let _ = tx.send(r.map_err(|e| e.to_string()));
    });

    let parent = parent.clone();
    let client_for_retry = client.clone();
    let refresh_outer = Rc::clone(&refresh);
    let ui_tx_outer = ui_tx.clone();
    glib::spawn_future_local(async move {
        if let Some(result) = rx.recv().await {
            match result {
                Ok(()) => refresh_outer(),
                Err(e) if !force => {
                    let id_for_retry = id.clone();
                    let parent_inner = parent.clone();
                    let client_inner = client_for_retry.clone();
                    let refresh_inner = Rc::clone(&refresh_outer);
                    let ui_tx_inner = ui_tx_outer.clone();
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
                                client_inner.clone(),
                                refresh_inner.clone(),
                                ui_tx_inner.clone(),
                                id_for_retry.clone(),
                                true,
                            );
                        },
                    );
                }
                Err(e) => {
                    let _ = ui_tx_outer.send(UiMessage::Error(format!("Delete connection: {e}")));
                }
            }
        }
    });
}
