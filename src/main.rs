mod async_bridge;
mod avatars;
mod credential_store;
mod markdown;
mod oauth;
mod profile;
#[cfg(feature = "linux")]
mod webview;
mod widgets;
mod window;

use anyhow::Result;
use clap::Parser;
use desktop_assistant_client_common::{ConnectionConfig, TransportMode};
use gtk4::Application;
use gtk4::prelude::*;
use gtk4::glib;
use tracing_subscriber::EnvFilter;

use crate::async_bridge::spawn_on_runtime;
use crate::profile::{LastConnectionStore, ProfileStore};
use crate::widgets::login_screen::{LoginScreen, connect_to_profile};

const APP_ID: &str = "org.adelie.DesktopAssistant";
const DEFAULT_WS_URL: &str = desktop_assistant_client_common::config::DEFAULT_WS_URL;
const DEFAULT_WS_SUBJECT: &str = desktop_assistant_client_common::config::DEFAULT_WS_SUBJECT;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[value(rename_all = "lower")]
enum CliTransportMode {
    Ws,
    Dbus,
}

#[derive(Debug, Parser)]
#[command(name = "adele-gtk")]
struct CliArgs {
    #[arg(
        long,
        env = "ADELIE_GTK_TRANSPORT",
        value_enum,
        default_value_t = CliTransportMode::Ws
    )]
    transport: CliTransportMode,
    #[arg(
        long = "ws-url",
        env = "ADELIE_GTK_WS_URL",
        default_value = DEFAULT_WS_URL
    )]
    ws_url: String,
    #[arg(
        long = "ws-subject",
        env = "ADELIE_GTK_WS_SUBJECT",
        default_value = DEFAULT_WS_SUBJECT
    )]
    ws_subject: String,
}

impl From<CliArgs> for ConnectionConfig {
    fn from(cli: CliArgs) -> Self {
        let ws_url = {
            let trimmed = cli.ws_url.trim();
            if trimmed.is_empty() {
                DEFAULT_WS_URL.to_string()
            } else {
                trimmed.to_string()
            }
        };

        let ws_subject = {
            let trimmed = cli.ws_subject.trim();
            if trimmed.is_empty() {
                DEFAULT_WS_SUBJECT.to_string()
            } else {
                trimmed.to_string()
            }
        };

        let transport_mode = match cli.transport {
            CliTransportMode::Ws => TransportMode::Ws,
            CliTransportMode::Dbus => TransportMode::Dbus,
        };

        Self {
            transport_mode,
            ws_url,
            ws_jwt: None,
            ws_login_username: None,
            ws_login_password: None,
            ws_subject,
            ..Default::default()
        }
    }
}

fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install default rustls CryptoProvider");

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let cli = CliArgs::parse();
    let _config = ConnectionConfig::from(cli);

    let app = Application::builder().application_id(APP_ID).build();

    app.connect_activate(move |app| {
        // If a profile was used last time and is still configured, attempt to
        // silently re-establish that connection. On any failure, fall back to
        // the connection picker.
        let app_clone = app.clone();
        glib::spawn_future_local(async move {
            if let Some(profile) = last_active_profile() {
                let profile_id = profile.id.clone();
                let (tx, rx) = tokio::sync::oneshot::channel();
                spawn_on_runtime(async move {
                    let _ = tx.send(connect_to_profile(&profile).await);
                });
                match rx.await {
                    Ok(Ok(config)) => {
                        if let Err(e) = LastConnectionStore::new().set(&profile_id) {
                            tracing::warn!("Failed to refresh last-connection marker: {e}");
                        }
                        let main_win = window::AdelieWindow::new(&app_clone, config);
                        main_win.present();
                        return;
                    }
                    Ok(Err(e)) => {
                        tracing::info!("auto-reconnect failed, showing picker: {e}");
                    }
                    Err(_) => {
                        tracing::info!("auto-reconnect channel dropped, showing picker");
                    }
                }
            }
            let login = LoginScreen::new(&app_clone);
            login.present();
        });
    });

    // GTK expects command-line args but we've already parsed them with clap.
    let empty: Vec<String> = vec![];
    app.run_with_args(&empty);

    Ok(())
}

/// Look up the connection profile recorded as the most recently active.
/// Returns `None` if no marker exists or the profile has since been deleted.
fn last_active_profile() -> Option<profile::ConnectionProfile> {
    let id = LastConnectionStore::new().get()?;
    let profiles = ProfileStore::new().load().ok()?;
    profiles.into_iter().find(|p| p.id == id)
}
