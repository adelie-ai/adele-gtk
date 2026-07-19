//! Pure, GTK-free logic for the merged daemon/client MCP admin panel (#122).
//!
//! Phase 2 of epic desktop-assistant#531 lets the GTK Settings dialog administer
//! **both** populations of MCP servers in one list: the ones the daemon hosts and
//! the ones this client hosts on the edge (`client-mcp.toml`). The view-model
//! merge/sort/filter/label lives in `client-ui-common`; what lives *here* is the
//! GTK-client-specific glue that stays pure so it unit-tests without a display:
//!
//! - [`backend_for`] — the runner fork that decides whether a save/toggle/remove
//!   goes to the daemon RPC path or the local `client-mcp.toml`.
//! - [`daemon_link`] — derives `is_remote` + an optional host label for
//!   `runner_label` from the client's [`ConnectionConfig`].
//! - [`client_server_dtos`] — projects a loaded [`ClientMcpConfig`] into the
//!   [`ClientServerDto`] rows the panel merges with the daemon's.
//! - [`apply_client_save`] / [`apply_client_toggle`] / [`apply_client_remove`] —
//!   the definition + gtk-surface mutations a client-row edit performs before
//!   the config is written back atomically.
//!
//! The gtk surface exposes a client server iff its definition is `enabled` **and**
//! the `[surfaces.gtk]` list names it, so "enabled" in the UI means "gtk actually
//! hosts this". The disable toggle is deliberately **asymmetric**: enabling sets
//! both grains, but disabling touches only the `[surfaces.gtk]` membership and
//! leaves the shared definition enabled, so turning a server off in gtk never
//! disables it for another surface sharing the same `client-mcp.toml`.

use std::collections::HashMap;

use client_ui_common::{ClientServerDto, Runner};
use desktop_assistant_client_common::mcp_host::{ClientMcpConfig, McpServerConfig};
use desktop_assistant_client_common::{ConnectionConfig, TransportMode};

/// The client surface this GTK client administers (`[surfaces.gtk]`).
pub const GTK_SURFACE: &str = "gtk";

/// Which backend administers a server of a given [`Runner`].
///
/// The panel's save/toggle/remove callbacks fork on this: [`Daemon`](Self::Daemon)
/// rows go through the daemon management RPCs; [`Client`](Self::Client) rows edit
/// the local `client-mcp.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpBackend {
    /// Administer via the daemon's `UpsertMcpServer` / `SetMcpServerEnabled` /
    /// `RemoveMcpServer` command surface.
    Daemon,
    /// Administer via the local [`ClientMcpConfig`] on disk.
    Client,
}

/// The runner fork: map a [`Runner`] to the [`McpBackend`] that administers it.
pub fn backend_for(runner: Runner) -> McpBackend {
    match runner {
        Runner::Daemon => McpBackend::Daemon,
        Runner::Client => McpBackend::Client,
    }
}

/// Derive `(is_remote, host)` for `runner_label` from the client's connection.
///
/// A WebSocket link is remote (the daemon may run on another host), so the label
/// gains a `daemon · <host>` suffix when the URL yields a host. A UDS or D-Bus
/// link is co-located, so it is never remote and carries no host.
pub fn daemon_link(config: &ConnectionConfig) -> (bool, Option<String>) {
    match config.transport_mode {
        TransportMode::Ws => (true, ws_host(&config.ws_url)),
        TransportMode::Uds | TransportMode::Dbus => (false, None),
    }
}

/// The host component of a `ws://` / `wss://` URL, or `None` when it does not
/// parse or carries no host. A missing host degrades to a plain "daemon" label
/// rather than an error.
fn ws_host(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
}

/// True when the gtk surface hosts `server`: its definition is enabled **and**
/// the `[surfaces.gtk]` list names it. Both grains must agree.
fn hosted_by_gtk(server: &McpServerConfig, gtk_enabled: &[String]) -> bool {
    server.enabled && gtk_enabled.iter().any(|n| n == &server.name)
}

/// Project a loaded [`ClientMcpConfig`] into the panel's client rows.
///
/// Transport is taken honestly from the definition (`http` table present ⇒
/// `"http"`, else `"stdio"`); status is the coarse `"enabled"`/`"disabled"` the
/// gtk surface sees. `tool_count` is the live per-server total from the running
/// [`McpHost`](desktop_assistant_client_common::mcp_host::McpHost), looked up in
/// `counts` by the server's **namespace** (`namespace`, or the name when unset) —
/// the same key the host reports against. It is `0` when the host has not
/// reported yet (e.g. the panel is opened before a connection) or hosts no tools.
pub fn client_server_dtos(
    cfg: &ClientMcpConfig,
    counts: &HashMap<String, u32>,
) -> Vec<ClientServerDto> {
    let gtk_enabled = cfg.surface_enabled_names(GTK_SURFACE);
    cfg.list_defined_servers()
        .iter()
        .map(|s| {
            let transport = if s.http.is_some() { "http" } else { "stdio" };
            let on = hosted_by_gtk(s, gtk_enabled);
            let namespace = s.namespace.clone().unwrap_or_else(|| s.name.clone());
            ClientServerDto {
                name: s.name.clone(),
                transport: transport.to_string(),
                status: if on { "enabled" } else { "disabled" }.to_string(),
                tool_count: counts.get(&namespace).copied().unwrap_or(0),
            }
        })
        .collect()
}

/// Whether the gtk surface hosts the client server named `name` right now: its
/// definition is enabled and `[surfaces.gtk]` names it. Drives the "Enabled" box
/// when pre-filling a client-row edit. `false` when no such definition exists.
pub fn client_row_enabled(cfg: &ClientMcpConfig, name: &str) -> bool {
    let gtk_enabled = cfg.surface_enabled_names(GTK_SURFACE);
    cfg.list_defined_servers()
        .iter()
        .any(|s| s.name == name && hosted_by_gtk(s, gtk_enabled))
}

/// Apply a client-server save (add or edit) to the loaded config.
///
/// Upserts the definition and sets its gtk-surface membership to match `enabled`
/// so both grains agree (see the module note). When an existing definition of the
/// same name carries `env_secrets` or a `description` the form does not surface,
/// they are preserved rather than blanked by the round-trip.
pub fn apply_client_save(cfg: &mut ClientMcpConfig, mut server: McpServerConfig, enabled: bool) {
    let name = server.name.clone();
    if let Some(existing) = cfg.list_defined_servers().iter().find(|s| s.name == name) {
        if server.env_secrets.is_empty() {
            server.env_secrets = existing.env_secrets.clone();
        }
        if server.description.is_none() {
            server.description = existing.description.clone();
        }
    }
    cfg.upsert_server(server);
    cfg.set_surface_enabled(GTK_SURFACE, &name, enabled);
}

/// Enable/disable a client server **for the gtk surface**, asymmetrically, so one
/// surface's choice never disturbs another sharing the same `client-mcp.toml`:
///
/// - **On:** join the `[surfaces.gtk]` list **and** ensure the definition's own
///   `enabled` flag is set, so enabling actually results in gtk hosting the
///   server even if the shared definition had been globally disabled.
/// - **Off:** drop it from `[surfaces.gtk]` **only**, leaving the definition
///   enabled so every other surface that lists it keeps hosting it.
///
/// Errors (fail-closed) if no definition by that name exists, in either
/// direction, rather than materializing a gtk surface entry for a phantom server
/// (the error is surfaced to the status bar).
pub fn apply_client_toggle(cfg: &mut ClientMcpConfig, name: &str, on: bool) -> Result<(), String> {
    if on {
        cfg.set_server_enabled(name, true)?;
        cfg.set_surface_enabled(GTK_SURFACE, name, true);
    } else {
        // Surface-scoped disable: `set_surface_enabled` never errors and would
        // create a gtk entry for an unknown name, so validate existence first to
        // preserve the fail-closed contract, then touch only the gtk membership.
        if !cfg.list_defined_servers().iter().any(|s| s.name == name) {
            return Err(format!("no such server: {name}"));
        }
        cfg.set_surface_enabled(GTK_SURFACE, name, false);
    }
    Ok(())
}

/// Remove a client-server definition (and its membership in every surface).
pub fn apply_client_remove(cfg: &mut ClientMcpConfig, name: &str) -> Result<(), String> {
    cfg.remove_server(name)
}

/// Parse the dialog's `config_json` (a serialized `McpServerConfig` subset) into
/// a real [`McpServerConfig`] for the client path. The omitted fields
/// (`env_secrets`, `description`) fall to their serde defaults; a save preserves
/// any previous values via [`apply_client_save`].
pub fn parse_server_config(config_json: &str) -> Result<McpServerConfig, String> {
    serde_json::from_str(config_json).map_err(|e| format!("invalid server config: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a [`ClientMcpConfig`] from TOML, going through the parser so tests
    /// don't hand-construct the cross-crate `McpServerConfig`.
    fn cfg(toml: &str) -> ClientMcpConfig {
        ClientMcpConfig::from_toml(toml).expect("valid client-mcp toml")
    }

    // --- backend_for (the runner fork) ---------------------------------------

    #[test]
    fn backend_for_daemon_runner_is_daemon() {
        assert_eq!(backend_for(Runner::Daemon), McpBackend::Daemon);
    }

    #[test]
    fn backend_for_client_runner_is_client() {
        assert_eq!(backend_for(Runner::Client), McpBackend::Client);
    }

    // --- daemon_link ----------------------------------------------------------

    #[test]
    fn daemon_link_ws_is_remote_with_host() {
        let config = ConnectionConfig {
            transport_mode: TransportMode::Ws,
            ws_url: "wss://adele.example.lab:8443/ws".to_string(),
            ..Default::default()
        };
        assert_eq!(
            daemon_link(&config),
            (true, Some("adele.example.lab".to_string()))
        );
    }

    #[test]
    fn daemon_link_uds_is_local_no_host() {
        let config = ConnectionConfig {
            transport_mode: TransportMode::Uds,
            ..Default::default()
        };
        assert_eq!(daemon_link(&config), (false, None));
    }

    #[test]
    fn daemon_link_dbus_is_local_no_host() {
        let config = ConnectionConfig {
            transport_mode: TransportMode::Dbus,
            ..Default::default()
        };
        assert_eq!(daemon_link(&config), (false, None));
    }

    #[test]
    fn daemon_link_ws_unparseable_url_degrades_to_no_host() {
        let config = ConnectionConfig {
            transport_mode: TransportMode::Ws,
            ws_url: "not a url".to_string(),
            ..Default::default()
        };
        // Still remote (it is a WS link), just without a host suffix.
        assert_eq!(daemon_link(&config), (true, None));
    }

    // --- client_server_dtos ---------------------------------------------------

    #[test]
    fn client_dto_stdio_vs_http_transport() {
        let c = cfg(r#"
[[servers]]
name = "files"
command = "fileio-mcp"

[[servers]]
name = "remote"
[servers.http]
url = "https://x.example/mcp"

[surfaces.gtk]
enabled = ["files", "remote"]
"#);
        let dtos = client_server_dtos(&c, &HashMap::new());
        let files = dtos.iter().find(|d| d.name == "files").expect("files row");
        let remote = dtos
            .iter()
            .find(|d| d.name == "remote")
            .expect("remote row");
        assert_eq!(files.transport, "stdio");
        assert_eq!(remote.transport, "http");
        // tool_count is a follow-up: 0 for now.
        assert_eq!(files.tool_count, 0);
    }

    #[test]
    fn client_dto_enabled_needs_definition_and_surface() {
        let c = cfg(r#"
[[servers]]
name = "on"
command = "a"

[[servers]]
name = "def-off"
command = "b"
enabled = false

[[servers]]
name = "not-in-gtk"
command = "c"

[surfaces.gtk]
enabled = ["on", "def-off"]
"#);
        let dtos = client_server_dtos(&c, &HashMap::new());
        let status = |name: &str| {
            dtos.iter()
                .find(|d| d.name == name)
                .map(|d| d.status.clone())
                .unwrap_or_default()
        };
        // Enabled definition + named by the gtk surface -> enabled.
        assert_eq!(status("on"), "enabled");
        // In the gtk surface but the definition is disabled -> disabled.
        assert_eq!(status("def-off"), "disabled");
        // Enabled definition but the gtk surface does not name it -> disabled.
        assert_eq!(status("not-in-gtk"), "disabled");
    }

    #[test]
    fn client_dtos_empty_config_is_empty() {
        assert!(client_server_dtos(&ClientMcpConfig::default(), &HashMap::new()).is_empty());
    }

    #[test]
    fn client_dto_tool_count_comes_from_counts_by_namespace() {
        let c = cfg(r#"
[[servers]]
name = "files"
command = "fileio-mcp"
namespace = "fs"

[[servers]]
name = "git"
command = "git-mcp"

[surfaces.gtk]
enabled = ["files", "git"]
"#);
        // The host reports counts keyed by namespace, or by name when a server
        // declares none.
        let mut counts = HashMap::new();
        counts.insert("fs".to_string(), 3u32);
        counts.insert("git".to_string(), 2u32);
        let dtos = client_server_dtos(&c, &counts);
        let files = dtos.iter().find(|d| d.name == "files").expect("files row");
        let git = dtos.iter().find(|d| d.name == "git").expect("git row");
        assert_eq!(files.tool_count, 3, "keyed by the namespace 'fs'");
        assert_eq!(git.tool_count, 2, "keyed by the name when no namespace");
    }

    #[test]
    fn client_dto_tool_count_defaults_zero_when_host_has_not_reported() {
        let c = cfg(r#"
[[servers]]
name = "files"
command = "fileio-mcp"
[surfaces.gtk]
enabled = ["files"]
"#);
        // Empty map (host not connected / no tools) -> 0 rather than a panic.
        assert_eq!(client_server_dtos(&c, &HashMap::new())[0].tool_count, 0);
    }

    // --- client_row_enabled ---------------------------------------------------

    #[test]
    fn client_row_enabled_needs_both_grains() {
        let c = cfg(r#"
[[servers]]
name = "on"
command = "a"

[[servers]]
name = "def-off"
command = "b"
enabled = false

[[servers]]
name = "not-in-gtk"
command = "c"

[surfaces.gtk]
enabled = ["on", "def-off"]
"#);
        assert!(client_row_enabled(&c, "on"));
        assert!(!client_row_enabled(&c, "def-off"));
        assert!(!client_row_enabled(&c, "not-in-gtk"));
        assert!(!client_row_enabled(&c, "ghost"));
    }

    // --- apply_client_save ----------------------------------------------------

    #[test]
    fn apply_client_save_adds_and_enables_for_gtk() {
        let mut c = ClientMcpConfig::default();
        let server =
            parse_server_config(r#"{"name":"files","enabled":true,"command":"fileio-mcp"}"#)
                .expect("parse");
        apply_client_save(&mut c, server, true);

        assert_eq!(c.list_defined_servers().len(), 1);
        assert_eq!(c.surface_enabled_names(GTK_SURFACE), &["files"]);
        assert_eq!(client_server_dtos(&c, &HashMap::new())[0].status, "enabled");
    }

    #[test]
    fn apply_client_save_disabled_removes_from_gtk_surface() {
        let mut c = cfg(r#"
[[servers]]
name = "files"
command = "fileio-mcp"
[surfaces.gtk]
enabled = ["files"]
"#);
        let server =
            parse_server_config(r#"{"name":"files","enabled":false,"command":"fileio-mcp"}"#)
                .expect("parse");
        apply_client_save(&mut c, server, false);
        assert!(
            !c.surface_enabled_names(GTK_SURFACE)
                .iter()
                .any(|n| n == "files")
        );
        assert_eq!(
            client_server_dtos(&c, &HashMap::new())[0].status,
            "disabled"
        );
    }

    #[test]
    fn apply_client_save_preserves_env_secrets_and_description() {
        let mut c = cfg(r#"
[[servers]]
name = "files"
command = "fileio-mcp"
description = "keep me"
[servers.env_secrets]
TOKEN = "files_token"
[surfaces.gtk]
enabled = ["files"]
"#);
        // The form round-trip drops env_secrets/description (not surfaced).
        let edited = parse_server_config(
            r#"{"name":"files","enabled":true,"command":"fileio-mcp","args":["serve"]}"#,
        )
        .expect("parse");
        apply_client_save(&mut c, edited, true);

        let saved = c
            .list_defined_servers()
            .iter()
            .find(|s| s.name == "files")
            .expect("files present");
        assert_eq!(saved.args, vec!["serve"]);
        assert_eq!(saved.description.as_deref(), Some("keep me"));
        assert_eq!(
            saved.env_secrets.get("TOKEN").map(String::as_str),
            Some("files_token")
        );
    }

    // --- apply_client_toggle / remove ----------------------------------------

    #[test]
    fn apply_client_toggle_off_then_on_round_trips_status() {
        let mut c = cfg(r#"
[[servers]]
name = "files"
command = "fileio-mcp"
[surfaces.gtk]
enabled = ["files"]
"#);
        // Off: dropped from the gtk surface, but the DEFINITION stays enabled
        // (surface-scoped disable — see the asymmetric toggle).
        apply_client_toggle(&mut c, "files", false).expect("toggle off");
        assert_eq!(
            client_server_dtos(&c, &HashMap::new())[0].status,
            "disabled"
        );
        assert!(
            c.list_defined_servers()
                .iter()
                .find(|s| s.name == "files")
                .unwrap()
                .enabled,
            "a surface-scoped disable must leave the definition enabled"
        );
        assert!(
            !c.surface_enabled_names(GTK_SURFACE)
                .iter()
                .any(|n| n == "files")
        );

        // On again: back in the gtk surface (and the definition is on).
        apply_client_toggle(&mut c, "files", true).expect("toggle on");
        assert_eq!(client_server_dtos(&c, &HashMap::new())[0].status, "enabled");
        assert!(
            c.surface_enabled_names(GTK_SURFACE)
                .iter()
                .any(|n| n == "files")
        );
    }

    #[test]
    fn toggle_off_is_surface_scoped_leaving_other_surfaces() {
        // A server exposed on BOTH gtk and tui: disabling it in gtk must remove
        // it from the gtk surface ONLY, leaving tui's exposure and the shared
        // definition intact so the other surface keeps hosting it.
        let mut c = cfg(r#"
[[servers]]
name = "files"
command = "fileio-mcp"
[surfaces.gtk]
enabled = ["files"]
[surfaces.tui]
enabled = ["files"]
"#);
        apply_client_toggle(&mut c, "files", false).expect("toggle off");
        // Gone from gtk ...
        assert!(
            !c.surface_enabled_names(GTK_SURFACE)
                .iter()
                .any(|n| n == "files"),
            "must be dropped from the gtk surface"
        );
        // ... still in tui ...
        assert!(
            c.surface_enabled_names("tui").iter().any(|n| n == "files"),
            "another surface's exposure must be untouched"
        );
        // ... and the definition itself is still enabled.
        assert!(
            c.list_defined_servers()
                .iter()
                .find(|s| s.name == "files")
                .unwrap()
                .enabled,
            "the shared definition must stay enabled for other surfaces"
        );
    }

    #[test]
    fn toggle_on_reenables_globally_disabled_definition() {
        // Enabling for gtk must also flip a globally-disabled definition back on,
        // otherwise the surface would list a server the host would never start.
        let mut c = cfg(r#"
[[servers]]
name = "files"
command = "fileio-mcp"
enabled = false
"#);
        apply_client_toggle(&mut c, "files", true).expect("toggle on");
        assert!(
            c.list_defined_servers()
                .iter()
                .find(|s| s.name == "files")
                .unwrap()
                .enabled,
            "enabling in gtk must re-enable the definition"
        );
        assert!(
            c.surface_enabled_names(GTK_SURFACE)
                .iter()
                .any(|n| n == "files")
        );
        assert_eq!(client_server_dtos(&c, &HashMap::new())[0].status, "enabled");
    }

    #[test]
    fn apply_client_toggle_unknown_errors() {
        // Both directions fail closed on an unknown name rather than silently
        // materializing a gtk surface entry for a server that does not exist.
        let mut c = ClientMcpConfig::default();
        assert!(apply_client_toggle(&mut c, "ghost", true).is_err());
        assert!(apply_client_toggle(&mut c, "ghost", false).is_err());
        // The failed off-toggle must not have created a gtk surface entry.
        assert!(c.surface_enabled_names(GTK_SURFACE).is_empty());
    }

    #[test]
    fn apply_client_remove_drops_definition_and_surface() {
        let mut c = cfg(r#"
[[servers]]
name = "files"
command = "fileio-mcp"
[surfaces.gtk]
enabled = ["files"]
"#);
        apply_client_remove(&mut c, "files").expect("remove");
        assert!(c.list_defined_servers().is_empty());
        assert!(c.surface_enabled_names(GTK_SURFACE).is_empty());
        assert!(apply_client_remove(&mut c, "files").is_err());
    }

    // --- parse_server_config --------------------------------------------------

    #[test]
    fn parse_server_config_reads_http_transport() {
        let server = parse_server_config(
            r#"{"name":"cal","enabled":true,"http":{"url":"https://cal.example/mcp"}}"#,
        )
        .expect("parse");
        assert_eq!(server.name, "cal");
        assert!(server.http.is_some());
        assert_eq!(server.http.unwrap().url, "https://cal.example/mcp");
    }

    #[test]
    fn parse_server_config_rejects_malformed_json() {
        assert!(parse_server_config("{not json").is_err());
    }
}
