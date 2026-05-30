use gtk4::prelude::*;
use gtk4::{Align, Box as GtkBox, Button, DropDown, Entry, Label, Orientation, StringList, Window};

use crate::credential_store::CredentialStore;
use crate::profile::{ConnectionProfile, ProtocolConfig};

// DropDown indices for the protocol selector.
const PROTO_LOCAL: u32 = 0;
const PROTO_WEBSOCKET: u32 = 1;

/// Show a modal dialog for adding or editing a connection profile.
///
/// `existing` is `Some` when editing, `None` when creating. New profiles
/// default to the local Unix socket. `on_save` is called with the completed
/// profile when the user clicks Save.
pub fn show_setup_dialog<F: Fn(ConnectionProfile) + 'static>(
    parent: &impl IsA<Window>,
    existing: Option<&ConnectionProfile>,
    on_save: F,
) {
    let is_edit = existing.is_some();
    let dialog_title = if is_edit {
        "Edit Connection"
    } else {
        "Add Connection"
    };

    let dialog = Window::builder()
        .title(dialog_title)
        .default_width(400)
        .default_height(360)
        .modal(true)
        .transient_for(parent)
        .build();

    let content = GtkBox::new(Orientation::Vertical, 12);
    content.set_margin_start(20);
    content.set_margin_end(20);
    content.set_margin_top(20);
    content.set_margin_bottom(20);

    // Connection name
    let name_label = Label::new(Some("Connection Name"));
    name_label.set_halign(Align::Start);
    content.append(&name_label);

    let name_entry = Entry::new();
    name_entry.set_placeholder_text(Some("My Server"));
    if let Some(profile) = existing {
        name_entry.set_text(&profile.name);
    }
    content.append(&name_entry);

    // Protocol selector
    let proto_label = Label::new(Some("Protocol"));
    proto_label.set_halign(Align::Start);
    content.append(&proto_label);

    let proto_model = StringList::new(&["Local socket", "WebSocket"]);
    let proto_dropdown = DropDown::builder().model(&proto_model).build();
    content.append(&proto_dropdown);

    // --- Local group: an optional socket path ---
    let local_group = GtkBox::new(Orientation::Vertical, 12);

    let sock_label = Label::new(Some("Socket path (optional — blank uses the default)"));
    sock_label.set_halign(Align::Start);
    local_group.append(&sock_label);

    let sock_entry = Entry::new();
    sock_entry.set_placeholder_text(Some("$XDG_RUNTIME_DIR/adelie/sock"));
    local_group.append(&sock_entry);

    content.append(&local_group);

    // --- WebSocket group: URL + password-auth credentials ---
    let ws_group = GtkBox::new(Orientation::Vertical, 12);

    let url_label = Label::new(Some("Server URL"));
    url_label.set_halign(Align::Start);
    ws_group.append(&url_label);

    let url_entry = Entry::new();
    url_entry.set_placeholder_text(Some("wss://127.0.0.1:11339/ws"));
    url_entry.set_text("wss://127.0.0.1:11339/ws");
    ws_group.append(&url_entry);

    let user_label = Label::new(Some("Username (for password auth, optional)"));
    user_label.set_halign(Align::Start);
    ws_group.append(&user_label);

    let user_entry = Entry::new();
    user_entry.set_placeholder_text(Some("Leave blank to skip"));
    ws_group.append(&user_entry);

    let pass_label = Label::new(Some("Password (optional)"));
    pass_label.set_halign(Align::Start);
    ws_group.append(&pass_label);

    let pass_entry = Entry::new();
    pass_entry.set_visibility(false);
    pass_entry.set_placeholder_text(Some("Leave blank to skip"));
    ws_group.append(&pass_entry);

    content.append(&ws_group);

    // Populate fields from an existing profile, or default a new one to Local.
    match existing.map(|p| &p.protocol) {
        Some(ProtocolConfig::Local { path }) => {
            proto_dropdown.set_selected(PROTO_LOCAL);
            if let Some(p) = path {
                sock_entry.set_text(&p.display().to_string());
            }
        }
        Some(ProtocolConfig::Websocket { url, .. }) => {
            proto_dropdown.set_selected(PROTO_WEBSOCKET);
            url_entry.set_text(url);
        }
        None => proto_dropdown.set_selected(PROTO_LOCAL),
    }
    if let Some(profile) = existing
        && let Ok(Some(username)) =
            CredentialStore::get_password(&format!("{}-username", profile.id))
    {
        user_entry.set_text(&username);
    }

    // Show only the group matching the selected protocol, now and on change.
    let apply_visibility = |sel: u32, local: &GtkBox, ws: &GtkBox| {
        local.set_visible(sel == PROTO_LOCAL);
        ws.set_visible(sel == PROTO_WEBSOCKET);
    };
    apply_visibility(proto_dropdown.selected(), &local_group, &ws_group);
    {
        let local_group = local_group.clone();
        let ws_group = ws_group.clone();
        proto_dropdown.connect_selected_notify(move |dd| {
            apply_visibility(dd.selected(), &local_group, &ws_group);
        });
    }

    // Buttons
    let button_box = GtkBox::new(Orientation::Horizontal, 8);
    button_box.set_halign(Align::End);
    button_box.set_margin_top(8);

    let cancel_btn = Button::with_label("Cancel");
    button_box.append(&cancel_btn);

    let save_btn = Button::with_label("Save");
    save_btn.add_css_class("send-button");
    button_box.append(&save_btn);

    content.append(&button_box);

    // Status label for validation errors
    let status = Label::new(None);
    status.add_css_class("status-bar");
    status.set_halign(Align::Start);
    content.append(&status);

    dialog.set_child(Some(&content));

    // Cancel
    {
        let dialog_ref = dialog.clone();
        cancel_btn.connect_clicked(move |_| {
            dialog_ref.close();
        });
    }

    // Save
    {
        let dialog_ref = dialog.clone();
        let existing_id = existing.map(|p| p.id.clone());
        save_btn.connect_clicked(move |_| {
            let name = name_entry.text().trim().to_string();
            if name.is_empty() {
                status.set_text("Connection name is required");
                return;
            }

            let id = existing_id
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

            let protocol = if proto_dropdown.selected() == PROTO_WEBSOCKET {
                let url = url_entry.text().trim().to_string();
                if url.is_empty() {
                    status.set_text("Server URL is required for WebSocket");
                    return;
                }
                // Store credentials in the keyring (never in the profile).
                let username = user_entry.text().trim().to_string();
                let password = pass_entry.text().to_string();
                if !username.is_empty() {
                    let _ = CredentialStore::store_password(&format!("{id}-username"), &username);
                }
                if !password.is_empty() {
                    let _ = CredentialStore::store_password(&id, &password);
                }
                ProtocolConfig::Websocket {
                    url,
                    subject: "desktop-tui".to_string(),
                }
            } else {
                let raw = sock_entry.text().trim().to_string();
                let path = if raw.is_empty() {
                    None
                } else {
                    Some(std::path::PathBuf::from(raw))
                };
                ProtocolConfig::Local { path }
            };

            on_save(ConnectionProfile { id, name, protocol });
            dialog_ref.close();
        });
    }

    dialog.present();
}
