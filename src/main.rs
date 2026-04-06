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
        // Always show the login/connection selection screen.
        // Credentials are obtained interactively and stored in the system keyring.
        let login = widgets::login_screen::LoginScreen::new(app);
        login.present();
    });

    // GTK expects command-line args but we've already parsed them with clap.
    let empty: Vec<String> = vec![];
    app.run_with_args(&empty);

    Ok(())
}
