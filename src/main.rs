//! adele-gtk — the GTK4 desktop client for the Adele assistant.
//!
//! Renders the chat UI and talks to the `desktop-assistant` daemon over the
//! transport provided by `desktop-assistant-client-common` (D-Bus on Linux,
//! or a direct WebSocket/UDS connection). The GTK widget tree lives in
//! [`widgets`] and [`window`]; daemon I/O runs on a Tokio runtime and is
//! marshalled back onto the GTK main loop via [`async_bridge`]. Credentials
//! and OAuth/OIDC login are handled by [`credential_store`] and [`oauth`].

mod async_bridge;
mod avatars;
mod credential_store;
mod markdown;
mod oauth;
mod profile;
mod selected_models;
#[cfg(feature = "linux")]
mod webview;
mod widgets;
mod window;

use anyhow::Result;
use clap::Parser;
use desktop_assistant_client_common::{ConnectionConfig, TransportMode};
use gtk4::Application;
use gtk4::glib;
use gtk4::prelude::*;
use tracing_subscriber::EnvFilter;

use crate::async_bridge::spawn_on_runtime;
use crate::credential_store::CredentialStore;
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

/// CLI connection overrides; `None` for a field means the flag was not given.
struct CliConnectionOverride {
    transport: Option<CliTransportMode>,
    ws_url: Option<String>,
    ws_subject: Option<String>,
}

/// Resolve the startup connection target. STUB — implemented after the
/// failing-tests commit (#26).
fn resolve_startup_target(
    _cli: &CliConnectionOverride,
    _last_active: Option<profile::ConnectionProfile>,
) -> Option<profile::ConnectionProfile> {
    None
}

fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install default rustls CryptoProvider");

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    CredentialStore::init_store();

    let cli = CliArgs::parse();
    let _config = ConnectionConfig::from(cli);

    let app = Application::builder().application_id(APP_ID).build();

    app.connect_activate(move |app| {
        // If a profile was used last time and is still configured, attempt to
        // silently re-establish that connection. On any failure, fall back to
        // the connection picker.
        let app_clone = app.clone();
        // Hold the application active across the async reconnect so it does
        // not shut down before the future creates its first window.
        let hold = app.hold();
        glib::spawn_future_local(async move {
            let _hold = hold;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{ConnectionProfile, ProtocolConfig, default_ws_subject};

    fn ws_profile(id: &str, url: &str) -> ConnectionProfile {
        ConnectionProfile {
            id: id.to_string(),
            name: format!("name-{id}"),
            protocol: ProtocolConfig::Websocket {
                url: url.to_string(),
                subject: default_ws_subject(),
            },
        }
    }

    #[test]
    fn cli_ws_url_override_changes_default_profile_target() {
        let cli = CliConnectionOverride {
            transport: None,
            ws_url: Some("wss://remote/ws".to_string()),
            ws_subject: None,
        };
        let last_active = Some(ws_profile("saved", "ws://127.0.0.1:11339/ws"));
        let target = resolve_startup_target(&cli, last_active).expect("expected override target");
        match target.protocol {
            ProtocolConfig::Websocket { url, subject } => {
                assert_eq!(url, "wss://remote/ws");
                assert_eq!(subject, default_ws_subject());
            }
            other => panic!("expected websocket override, got {other:?}"),
        }
    }

    #[test]
    fn cli_ws_url_override_applies_custom_subject() {
        let cli = CliConnectionOverride {
            transport: None,
            ws_url: Some("wss://remote/ws".to_string()),
            ws_subject: Some("custom".to_string()),
        };
        let target = resolve_startup_target(&cli, None).expect("expected override target");
        match target.protocol {
            ProtocolConfig::Websocket { url, subject } => {
                assert_eq!(url, "wss://remote/ws");
                assert_eq!(subject, "custom");
            }
            other => panic!("expected websocket override, got {other:?}"),
        }
    }

    #[test]
    fn no_override_returns_last_active() {
        let cli = CliConnectionOverride {
            transport: None,
            ws_url: None,
            ws_subject: None,
        };
        let p = ws_profile("saved", "ws://127.0.0.1:11339/ws");
        let target = resolve_startup_target(&cli, Some(p.clone()));
        assert_eq!(target, Some(p));
    }

    #[test]
    fn no_override_and_no_last_active_returns_none() {
        let cli = CliConnectionOverride {
            transport: None,
            ws_url: None,
            ws_subject: None,
        };
        assert_eq!(resolve_startup_target(&cli, None), None);
    }

    #[test]
    fn dbus_transport_override_forces_local() {
        let cli = CliConnectionOverride {
            transport: Some(CliTransportMode::Dbus),
            ws_url: None,
            ws_subject: None,
        };
        let last_active = Some(ws_profile("saved", "ws://127.0.0.1:11339/ws"));
        let target = resolve_startup_target(&cli, last_active).expect("expected local override");
        assert_eq!(target.protocol, ProtocolConfig::Local { path: None });
    }
}
