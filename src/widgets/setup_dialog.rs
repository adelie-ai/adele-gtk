use gtk4::prelude::*;
use gtk4::{Align, Box as GtkBox, Button, Entry, Label, Orientation, Window};

use crate::credential_store::CredentialStore;
use crate::profile::ConnectionProfile;

/// Show a modal dialog for adding or editing a connection profile.
///
/// `existing` is `Some` when editing, `None` when creating.
/// `on_save` is called with the completed profile when the user clicks Save.
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
        .default_height(320)
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

    // Server URL
    let url_label = Label::new(Some("Server URL"));
    url_label.set_halign(Align::Start);
    content.append(&url_label);

    let url_entry = Entry::new();
    url_entry.set_placeholder_text(Some("ws://127.0.0.1:11339/ws"));
    if let Some(profile) = existing {
        url_entry.set_text(&profile.ws_url);
    } else {
        url_entry.set_text("ws://127.0.0.1:11339/ws");
    }
    content.append(&url_entry);

    // Username (for password auth)
    let user_label = Label::new(Some("Username (for password auth, optional)"));
    user_label.set_halign(Align::Start);
    content.append(&user_label);

    let user_entry = Entry::new();
    user_entry.set_placeholder_text(Some("Leave blank to skip"));
    if let Some(profile) = existing {
        if let Ok(Some(username)) =
            CredentialStore::get_password(&format!("{}-username", profile.id))
        {
            user_entry.set_text(&username);
        }
    }
    content.append(&user_entry);

    // Password
    let pass_label = Label::new(Some("Password (optional)"));
    pass_label.set_halign(Align::Start);
    content.append(&pass_label);

    let pass_entry = Entry::new();
    pass_entry.set_visibility(false);
    pass_entry.set_placeholder_text(Some("Leave blank to skip"));
    content.append(&pass_entry);

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
    dialog.set_child(Some(&content));

    // Status label for validation errors
    let status = Label::new(None);
    status.add_css_class("status-bar");
    status.set_halign(Align::Start);
    content.append(&status);

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
            let url = url_entry.text().trim().to_string();
            let username = user_entry.text().trim().to_string();
            let password = pass_entry.text().to_string();

            if name.is_empty() {
                status.set_text("Connection name is required");
                return;
            }
            if url.is_empty() {
                status.set_text("Server URL is required");
                return;
            }

            let id = existing_id
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

            // Store credentials in keyring
            if !username.is_empty() {
                let _ = CredentialStore::store_password(&format!("{id}-username"), &username);
            }
            if !password.is_empty() {
                let _ = CredentialStore::store_password(&id, &password);
            }

            let profile = ConnectionProfile {
                id,
                name,
                ws_url: url,
                ws_subject: "desktop-tui".to_string(),
            };

            on_save(profile);
            dialog_ref.close();
        });
    }

    dialog.present();
}
