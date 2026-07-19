//! adele-gtk — the GTK4 desktop client for the Adele assistant.
//!
//! Renders the chat UI and talks to the `desktop-assistant` daemon over the
//! transport provided by `desktop-assistant-client-common` (D-Bus on Linux,
//! or a direct WebSocket/UDS connection). The GTK widget tree lives in
//! [`widgets`] and [`window`]; daemon I/O runs on a Tokio runtime and is
//! marshalled back onto the GTK main loop via [`async_bridge`]. Credentials
//! and OAuth/OIDC login are handled by [`credential_store`] and [`oauth`].

mod assets;
mod async_bridge;
// Compiled-in core MCP servers hosted in-process (da#538 Phase C).
mod builtins;
// Avatar data-URI helpers feed the WebView chat renderer; the Label fallback
// (`--no-default-features`) draws plain text and never references them.
#[cfg(feature = "linux")]
mod avatars;
mod context_usage;
mod credential_store;
mod management_client;
mod mcp_admin;
// Markdown→sanitized-HTML rendering + the CSP-pinned WebView template. Only the
// WebView (`linux`) chat path renders HTML.
#[cfg(feature = "linux")]
mod markdown;
// Markdown→`TextView`-tag rendering for the fallback (non-WebView) chat pane,
// mirroring `markdown` for the `--no-default-features` build.
#[cfg(not(feature = "linux"))]
mod markdown_text;
mod oauth;
mod profile;
mod selected_models;
mod theme;
mod voice_client;
mod voice_config;
mod voice_embedded;
#[cfg(feature = "linux")]
mod webview;
mod widgets;
mod window;

use anyhow::Result;
use clap::Parser;
use gtk4::Application;
use gtk4::glib;
use gtk4::prelude::*;
use tracing_subscriber::EnvFilter;

use crate::async_bridge::spawn_on_runtime;
use crate::credential_store::CredentialStore;
use crate::profile::{
    ConnectionProfile, LastConnectionStore, ProfileStore, ProtocolConfig, default_ws_subject,
};
use crate::widgets::login_screen::{LoginScreen, connect_to_profile};

const APP_ID: &str = "org.adelie.DesktopAssistant";

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[value(rename_all = "lower")]
enum CliTransportMode {
    Ws,
    Dbus,
}

#[derive(Debug, Parser)]
#[command(name = "adele-gtk")]
struct CliArgs {
    /// Override the startup transport. With `dbus`, connect to the local
    /// daemon over D-Bus instead of replaying the saved auto-reconnect profile.
    #[arg(long, env = "ADELIE_GTK_TRANSPORT", value_enum)]
    transport: Option<CliTransportMode>,
    /// Override the startup target with this WebSocket URL, bypassing the
    /// saved-profile picker.
    #[arg(long = "ws-url", env = "ADELIE_GTK_WS_URL")]
    ws_url: Option<String>,
    /// JWT subject to use with `--ws-url` (defaults to the standard subject).
    #[arg(long = "ws-subject", env = "ADELIE_GTK_WS_SUBJECT")]
    ws_subject: Option<String>,
}

/// CLI connection overrides; `None` for a field means the flag was not given.
#[derive(Clone)]
struct CliConnectionOverride {
    transport: Option<CliTransportMode>,
    ws_url: Option<String>,
    ws_subject: Option<String>,
}

impl From<CliArgs> for CliConnectionOverride {
    fn from(cli: CliArgs) -> Self {
        Self {
            transport: cli.transport,
            ws_url: cli.ws_url,
            ws_subject: cli.ws_subject,
        }
    }
}

impl CliConnectionOverride {
    /// Whether any flag was supplied that overrides the saved startup target.
    /// When false, the saved auto-reconnect profile is used unchanged.
    fn is_active(&self) -> bool {
        self.ws_url.is_some() || matches!(self.transport, Some(CliTransportMode::Dbus))
    }
}

/// Resolve the startup connection target.
///
/// A CLI override takes precedence over the saved auto-reconnect profile so a
/// headless/scripted/remote launch works without a pre-saved profile:
/// - `--ws-url` produces an ephemeral WebSocket profile (with `--ws-subject`
///   or the default subject), bypassing the picker.
/// - otherwise `--transport dbus` forces the local daemon.
/// - otherwise the saved `last_active` profile is returned unchanged.
fn resolve_startup_target(
    cli: &CliConnectionOverride,
    last_active: Option<ConnectionProfile>,
) -> Option<ConnectionProfile> {
    if let Some(url) = &cli.ws_url {
        return Some(ConnectionProfile {
            id: "cli-override".to_string(),
            name: "CLI override".to_string(),
            protocol: ProtocolConfig::Websocket {
                url: url.clone(),
                subject: cli.ws_subject.clone().unwrap_or_else(default_ws_subject),
            },
        });
    }

    if matches!(cli.transport, Some(CliTransportMode::Dbus)) {
        return Some(ConnectionProfile {
            id: "cli-local".to_string(),
            name: "CLI override (local)".to_string(),
            protocol: ProtocolConfig::Local { path: None },
        });
    }

    last_active
}

fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install default rustls CryptoProvider");

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    CredentialStore::init_store();

    let cli_override = CliConnectionOverride::from(CliArgs::parse());

    let app = Application::builder().application_id(APP_ID).build();

    app.connect_activate(move |app| {
        // Resolve the startup target: a CLI override (--ws-url / --transport
        // dbus) wins over the saved auto-reconnect profile. On any failure,
        // fall back to the connection picker.
        let cli_override = cli_override.clone();
        let app_clone = app.clone();
        // Hold the application active across the async reconnect so it does
        // not shut down before the future creates its first window.
        let hold = app.hold();
        glib::spawn_future_local(async move {
            let _hold = hold;
            // Ephemeral CLI-override profiles must not be persisted as the
            // last connection; only refresh the marker for real saved profiles.
            let override_active = cli_override.is_active();
            if let Some(profile) = resolve_startup_target(&cli_override, last_active_profile()) {
                let profile_id = profile.id.clone();
                let (tx, rx) = tokio::sync::oneshot::channel();
                spawn_on_runtime(async move {
                    let _ = tx.send(connect_to_profile(&profile).await);
                });
                match rx.await {
                    Ok(Ok(config)) => {
                        if !override_active
                            && let Err(e) = LastConnectionStore::new().set(&profile_id)
                        {
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
