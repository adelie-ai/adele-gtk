use std::cell::RefCell;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{
    Align, Application, ApplicationWindow, Box as GtkBox, Button, GestureClick, Image, Label,
    ListBox, ListBoxRow, Orientation, Popover, ScrolledWindow, SelectionMode, gdk, glib,
};

use crate::async_bridge;
use crate::credential_store::CredentialStore;
use crate::oauth;
use crate::profile::{ConnectionProfile, ProfileStore};
use crate::widgets::setup_dialog;
use crate::window;

/// Login/connection selection screen shown at startup.
pub struct LoginScreen {
    pub window: ApplicationWindow,
}

impl LoginScreen {
    pub fn new(app: &Application) -> Self {
        let window = ApplicationWindow::builder()
            .application(app)
            .title("Adelie — Connect")
            .default_width(480)
            .default_height(520)
            .build();

        // Install app icon
        window::install_app_icon();

        // Apply CSS
        let provider = gtk4::CssProvider::new();
        provider.load_from_data(include_str!("../style.css"));
        gtk4::style_context_add_provider_for_display(
            &gdk::Display::default().expect("display"),
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );

        let outer = GtkBox::new(Orientation::Vertical, 0);
        outer.set_margin_start(24);
        outer.set_margin_end(24);
        outer.set_margin_top(20);
        outer.set_margin_bottom(20);

        // Branding
        let brand_box = GtkBox::new(Orientation::Horizontal, 12);
        brand_box.set_halign(Align::Center);
        brand_box.set_margin_bottom(16);

        const ICON_BYTES: &[u8] = include_bytes!("../../assets/adele_communicating.png");
        let icon_path = dirs::cache_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("adele-gtk-brand-icon.png");
        if let Err(e) = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&icon_path)
            .and_then(|mut f| std::io::Write::write_all(&mut f, ICON_BYTES))
        {
            if e.kind() != std::io::ErrorKind::AlreadyExists {
                tracing::warn!("Failed to write brand icon: {e}");
            }
        }
        let icon = Image::from_file(icon_path.to_str().unwrap_or_default());
        icon.set_pixel_size(56);
        brand_box.append(&icon);

        let title = Label::new(Some("Adele Desktop Assistant"));
        title.add_css_class("brand-title");
        brand_box.append(&title);
        outer.append(&brand_box);

        let store = ProfileStore::new();
        let profiles = store.load().unwrap_or_default();
        let profiles = Rc::new(RefCell::new(profiles));
        let status_label = Rc::new(Label::new(None));
        status_label.add_css_class("status-bar");
        status_label.set_halign(Align::Start);
        status_label.set_margin_top(8);

        let list_box = ListBox::new();
        list_box.set_selection_mode(SelectionMode::Single);
        list_box.add_css_class("conversation-list");

        let scrolled = ScrolledWindow::new();
        scrolled.set_vexpand(true);
        scrolled.set_child(Some(&list_box));

        let empty_label = Rc::new(Label::new(Some("No saved connections")));
        empty_label.add_css_class("dim-label");
        empty_label.set_margin_top(24);
        empty_label.set_margin_bottom(24);

        outer.append(&*empty_label);
        outer.append(&scrolled);

        // Buttons
        let button_box = GtkBox::new(Orientation::Horizontal, 8);
        button_box.set_margin_top(12);
        button_box.set_halign(Align::End);

        let add_btn = Button::with_label("Add Connection");
        add_btn.add_css_class("new-conversation-button");
        button_box.append(&add_btn);

        let connect_btn = Button::with_label("Connect");
        connect_btn.add_css_class("send-button");
        connect_btn.set_sensitive(false);
        button_box.append(&connect_btn);

        outer.append(&button_box);
        outer.append(&*status_label);
        window.set_child(Some(&outer));

        let list_box = Rc::new(list_box);
        let connect_btn = Rc::new(connect_btn);
        let window_ref = window.clone();

        // Use a shared flag so `populate` can schedule itself after a popover closes.
        // The Rc<dyn Fn()> is built in two steps because the closure references itself.
        let populate: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));

        {
            let profiles = Rc::clone(&profiles);
            let list_box = Rc::clone(&list_box);
            let empty_label = Rc::clone(&empty_label);
            let connect_btn = Rc::clone(&connect_btn);
            let status_label = Rc::clone(&status_label);
            let window_ref = window_ref.clone();
            let populate_self = Rc::clone(&populate);

            let f: Rc<dyn Fn()> = Rc::new(move || {
                // Clear existing rows
                while let Some(child) = list_box.first_child() {
                    list_box.remove(&child);
                }

                let profs = profiles.borrow();
                empty_label.set_visible(profs.is_empty());
                connect_btn.set_sensitive(!profs.is_empty());

                for (idx, profile) in profs.iter().enumerate() {
                    let row = ListBoxRow::new();
                    let hbox = GtkBox::new(Orientation::Horizontal, 8);
                    hbox.set_margin_start(12);
                    hbox.set_margin_end(12);
                    hbox.set_margin_top(8);
                    hbox.set_margin_bottom(8);

                    let name_label = Label::new(Some(&profile.name));
                    name_label.set_halign(Align::Start);
                    name_label.set_hexpand(true);
                    hbox.append(&name_label);

                    let url_label = Label::new(Some(&profile.ws_url));
                    url_label.add_css_class("dim-label");
                    url_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
                    url_label.set_max_width_chars(30);
                    hbox.append(&url_label);

                    row.set_child(Some(&hbox));

                    // Right-click context menu
                    let gesture = GestureClick::new();
                    gesture.set_button(3);
                    let profiles_ref = Rc::clone(&profiles);
                    let status_ref = Rc::clone(&status_label);
                    let window_inner = window_ref.clone();
                    let populate_inner = Rc::clone(&populate_self);
                    gesture.connect_pressed(move |gesture, _n, x, y| {
                        let Some(widget) = gesture.widget() else {
                            return;
                        };

                        let popover = Popover::new();
                        popover.add_css_class("context-popover");
                        popover.set_parent(&widget);
                        popover.set_pointing_to(Some(&gtk4::gdk::Rectangle::new(
                            x as i32, y as i32, 1, 1,
                        )));
                        popover.set_has_arrow(false);

                        let menu_box = GtkBox::new(Orientation::Vertical, 0);

                        // Edit button
                        let edit_btn = Button::with_label("Edit");
                        edit_btn.add_css_class("context-button");
                        let profiles_inner = Rc::clone(&profiles_ref);
                        let popover_ref = popover.clone();
                        let window_edit = window_inner.clone();
                        let populate_edit = Rc::clone(&populate_inner);
                        edit_btn.connect_clicked(move |_| {
                            popover_ref.popdown();
                            let profile = {
                                let profs = profiles_inner.borrow();
                                profs.get(idx).cloned()
                            };
                            if let Some(profile) = profile {
                                let profiles_save = Rc::clone(&profiles_inner);
                                let populate_save = Rc::clone(&populate_edit);
                                setup_dialog::show_setup_dialog(
                                    &window_edit,
                                    Some(&profile),
                                    move |updated| {
                                        let store = ProfileStore::new();
                                        let _ = store.update(&updated);
                                        *profiles_save.borrow_mut() =
                                            store.load().unwrap_or_default();
                                        if let Some(ref f) = *populate_save.borrow() {
                                            f();
                                        }
                                    },
                                );
                            }
                        });
                        menu_box.append(&edit_btn);

                        // Delete button
                        let delete_btn = Button::with_label("Delete");
                        delete_btn.add_css_class("context-button");
                        delete_btn.add_css_class("destructive-action");
                        let profiles_inner = Rc::clone(&profiles_ref);
                        let popover_ref = popover.clone();
                        let status_inner = Rc::clone(&status_ref);
                        let populate_del = Rc::clone(&populate_inner);
                        delete_btn.connect_clicked(move |_| {
                            popover_ref.popdown();
                            let profile_id = {
                                let profs = profiles_inner.borrow();
                                profs.get(idx).map(|p| p.id.clone())
                            };
                            if let Some(id) = profile_id {
                                let _ = CredentialStore::delete_credentials(&id);
                                let store = ProfileStore::new();
                                let _ = store.delete(&id);
                                let new_profiles = store.load().unwrap_or_default();
                                *profiles_inner.borrow_mut() = new_profiles;
                                status_inner.set_text("Connection deleted");
                                if let Some(ref f) = *populate_del.borrow() {
                                    f();
                                }
                            }
                        });
                        menu_box.append(&delete_btn);

                        popover.set_child(Some(&menu_box));
                        popover.popup();
                    });
                    row.add_controller(gesture);

                    list_box.append(&row);
                }

                // Select first row if any
                if !profs.is_empty() {
                    if let Some(first_row) = list_box.row_at_index(0) {
                        list_box.select_row(Some(&first_row));
                    }
                }
            });

            *populate.borrow_mut() = Some(Rc::clone(&f));
        }

        let populate = Rc::clone(&populate);

        // Initial population
        if let Some(ref f) = *populate.borrow() {
            f();
        }

        // Enable connect button when a row is selected
        {
            let connect_btn = Rc::clone(&connect_btn);
            list_box.connect_row_selected(move |_, row| {
                connect_btn.set_sensitive(row.is_some());
            });
        }

        // Add Connection button
        {
            let window_ref = window_ref.clone();
            let profiles = Rc::clone(&profiles);
            let populate = Rc::clone(&populate);
            add_btn.connect_clicked(move |_| {
                let profiles = Rc::clone(&profiles);
                let populate = Rc::clone(&populate);
                setup_dialog::show_setup_dialog(&window_ref, None, move |profile| {
                    let store = ProfileStore::new();
                    let _ = store.add(profile);
                    *profiles.borrow_mut() = store.load().unwrap_or_default();
                    if let Some(ref f) = *populate.borrow() {
                        f();
                    }
                });
            });
        }

        // Connect button
        {
            let profiles = Rc::clone(&profiles);
            let list_box = Rc::clone(&list_box);
            let status_label = Rc::clone(&status_label);
            let app_ref = app.clone();
            let window_ref = window_ref.clone();
            connect_btn.connect_clicked(move |btn| {
                let Some(row) = list_box.selected_row() else {
                    return;
                };
                let idx = row.index() as usize;
                let profs = profiles.borrow();
                let Some(profile) = profs.get(idx).cloned() else {
                    return;
                };
                drop(profs);

                btn.set_sensitive(false);
                status_label.set_text("Connecting...");

                let app = app_ref.clone();
                let win = window_ref.clone();
                let status = Rc::clone(&status_label);
                let btn_ref = btn.clone();

                // Run auth discovery + credential resolution on the tokio runtime
                // (reqwest needs tokio), then handle the result on the GTK main thread.
                let (tx, rx) = tokio::sync::oneshot::channel();
                async_bridge::spawn_on_runtime(async move {
                    let result = connect_to_profile(&profile).await;
                    let _ = tx.send(result);
                });
                glib::spawn_future_local(async move {
                    match rx.await {
                        Ok(Ok(config)) => {
                            let main_win = window::AdelieWindow::new(&app, config);
                            main_win.present();
                            win.close();
                        }
                        Ok(Err(e)) => {
                            status.set_text(&format!("Connection failed: {e}"));
                            btn_ref.set_sensitive(true);
                        }
                        Err(_) => {
                            status.set_text("Connection failed: internal error");
                            btn_ref.set_sensitive(true);
                        }
                    }
                });
            });
        }

        Self { window }
    }

    pub fn present(&self) {
        self.window.present();
    }
}

/// Attempt to connect to a profile by discovering auth method and obtaining credentials.
async fn connect_to_profile(
    profile: &ConnectionProfile,
) -> anyhow::Result<desktop_assistant_client_common::ConnectionConfig> {
    use desktop_assistant_client_common::{ConnectionConfig, TransportMode};

    // Try to discover auth config from server
    let ca_cert = desktop_assistant_client_common::config::default_ca_cert_path();
    let ca_cert_ref = ca_cert.as_path();
    let discovery = match oauth::discover_auth_config(&profile.ws_url, Some(ca_cert_ref)).await {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("auth discovery failed, assuming password-only: {e}");
            oauth::AuthDiscovery {
                methods: vec!["password".to_string()],
                oidc: None,
            }
        }
    };

    let has_oidc = discovery.methods.contains(&"oidc".to_string());
    let has_password = discovery.methods.contains(&"password".to_string());

    // Try OIDC first if available
    if has_oidc {
        if let Some(ref oidc) = discovery.oidc {
            // Try silent refresh first
            if let Ok(Some(refresh_token)) = CredentialStore::get_refresh_token(&profile.id) {
                tracing::info!("attempting silent token refresh");
                match oauth::refresh_access_token(oidc, &refresh_token).await {
                    Ok(tokens) => {
                        // Store new refresh token if provided
                        if let Some(ref new_refresh) = tokens.refresh_token {
                            let _ = CredentialStore::store_refresh_token(&profile.id, new_refresh);
                        }
                        return Ok(ConnectionConfig {
                            transport_mode: TransportMode::Ws,
                            ws_url: profile.ws_url.clone(),
                            ws_jwt: Some(tokens.access_token),
                            ws_login_username: None,
                            ws_login_password: None,
                            ws_subject: profile.ws_subject.clone(),
                            ..Default::default()
                        });
                    }
                    Err(e) => {
                        tracing::info!("silent refresh failed, will try browser flow: {e}");
                    }
                }
            }

            // Full browser OAuth flow
            tracing::info!("starting browser OAuth flow");
            let tokens = oauth::run_oauth_flow(oidc).await?;

            // Store refresh token for next time
            if let Some(ref refresh_token) = tokens.refresh_token {
                let _ = CredentialStore::store_refresh_token(&profile.id, refresh_token);
            }

            return Ok(ConnectionConfig {
                transport_mode: TransportMode::Ws,
                ws_url: profile.ws_url.clone(),
                ws_jwt: Some(tokens.access_token),
                ws_login_username: None,
                ws_login_password: None,
                ws_subject: profile.ws_subject.clone(),
                ..Default::default()
            });
        }
    }

    // Fall back to password auth
    if has_password {
        let username = CredentialStore::get_password(&format!("{}-username", profile.id))
            .ok()
            .flatten();
        let password = CredentialStore::get_password(&profile.id).ok().flatten();

        return Ok(ConnectionConfig {
            transport_mode: TransportMode::Ws,
            ws_url: profile.ws_url.clone(),
            ws_jwt: None,
            ws_login_username: username,
            ws_login_password: password,
            ws_subject: profile.ws_subject.clone(),
            ..Default::default()
        });
    }

    anyhow::bail!("server does not offer any supported auth methods")
}
