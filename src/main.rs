mod async_bridge;
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
use gtk4::prelude::*;
use gtk4::Application;
use tracing_subscriber::EnvFilter;

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
#[command(name = "adelie-gtk")]
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
    #[arg(long = "ws-jwt", env = "ADELIE_GTK_WS_JWT")]
    ws_jwt: Option<String>,
    #[arg(
        long = "ws-login-username",
        env = "ADELIE_GTK_WS_LOGIN_USERNAME"
    )]
    ws_login_username: Option<String>,
    #[arg(
        long = "ws-login-password",
        env = "ADELIE_GTK_WS_LOGIN_PASSWORD"
    )]
    ws_login_password: Option<String>,
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

        let ws_jwt = cli
            .ws_jwt
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());

        let ws_login_username = cli
            .ws_login_username
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());

        let ws_login_password = cli
            .ws_login_password
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());

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
            ws_jwt,
            ws_login_username,
            ws_login_password,
            ws_subject,
        }
    }
}

/// Returns true if the user explicitly provided auth credentials via CLI/env,
/// meaning we should skip the login screen and connect directly.
fn cli_has_explicit_auth(config: &ConnectionConfig) -> bool {
    config.ws_jwt.is_some()
        || config.ws_login_username.is_some()
        || config.ws_login_password.is_some()
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let cli = CliArgs::parse();
    let config = ConnectionConfig::from(cli);

    let app = Application::builder().application_id(APP_ID).build();

    app.connect_activate(move |app| {
        if cli_has_explicit_auth(&config) {
            // Direct connection — backward compatible with CLI args / env vars
            let win = window::AdelieWindow::new(app, config.clone());
            win.present();
        } else {
            // Show login/connection selection screen
            let login = widgets::login_screen::LoginScreen::new(app);
            login.present();
        }
    });

    // GTK expects command-line args but we've already parsed them with clap.
    let empty: Vec<String> = vec![];
    app.run_with_args(&empty);

    Ok(())
}
