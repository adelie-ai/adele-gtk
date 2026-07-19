//! Transport-aware MCP-server add/edit dialog (issue #495) and the pure form
//! logic behind it.
//!
//! The daemon's `McpServerConfig` spans two transports with divergent field
//! sets, so - mirroring [`super::connection_config_dialog`] - the dialog picks
//! its form from a Transport selector:
//!
//! - **Local (stdio):** `command` + `args` + `namespace` + `env`.
//! - **Remote (HTTP):** `url` + an Authentication sub-selector (None / Bearer /
//!   OAuth). Bearer stores a token value via `Command::SetMcpSecret` under the
//!   `{name}_token` ref (the config carries only the ref); OAuth references a
//!   reusable service account (epic #477) by id + scopes and stores no secret.
//!
//! **Pure logic, host-testable.** The form <-> `config_json` mapping, the
//! env/args/scope parsers, and the `{name}_token` ref are transport-/GTK-free
//! ([`McpForm`], [`parse_env`], ...) so they unit-test without a display -
//! exactly as the web panel (`adele-web-ui`'s `crate::mcp`) does. The GTK
//! [`show_mcp_server_dialog`] is the thin shell over that logic.
//!
//! **Bearer secrets are write-only.** A bearer token is never echoed by the
//! daemon (the view carries only refs/kinds), never pre-filled on edit
//! ([`McpForm::from_view`] leaves it blank), and only sent - via
//! `Command::SetMcpSecret` under `{name}_token`, *before* the `UpsertMcpServer`
//! that references it - when the user actually types one.

use std::collections::BTreeMap;
use std::rc::Rc;

use client_ui_common::Runner;
use desktop_assistant_api_model::{McpServerView, ServiceAccountView};
use desktop_assistant_client_common::mcp_host::McpServerConfig;
use gtk4::prelude::*;
use gtk4::{
    Align, Box as GtkBox, Button, CheckButton, DropDown, Entry, Label, Orientation, ScrolledWindow,
    Separator, StringList, TextView, Window, WrapMode, glib,
};

// ===========================================================================
// Pure logic (host-testable)
// ===========================================================================

/// A minimal `#[derive(Serialize)]` mirror of the daemon's `McpServerConfig`,
/// carrying only the fields the form surfaces. Building `config_json` from this
/// DTO (rather than depending on `desktop-assistant-mcp-client`) keeps this
/// crate free of that crate's process-spawn transport. The daemon's
/// `McpServerConfig` uses serde defaults for every field this omits, so
/// omit-empty is safe; `env` is a `BTreeMap` so its JSON is key-sorted and the
/// wire form is deterministic (a `HashMap` would reorder between builds).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
struct McpConfigDto {
    name: String,
    enabled: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    command: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    args: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    namespace: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    env: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    http: Option<HttpDto>,
}

/// The `http` sub-table of [`McpConfigDto`] - mirrors the daemon's
/// `HttpTransportConfig` for the two auth modes the form drives: a static bearer
/// token (by secret ref) or a reference to an OAuth service account (epic #477).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
struct HttpDto {
    url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    auth_bearer_secret: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    oauth_account: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    scopes: Vec<String>,
}

/// The transport a server speaks. Selects which set of form fields is shown and
/// which shape [`McpForm::build`] emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpTransport {
    /// Local process spawned over stdio (`command`/`args`/`env`).
    Stdio,
    /// Remote streamable-HTTP endpoint (`url` + auth).
    Http,
}

/// How a remote (HTTP) server authenticates. Mirrors the daemon's `auth_kind`
/// (`"none"` | `"bearer"` | `"oauth"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpAuthKind {
    /// No authentication.
    None,
    /// A static `Authorization: Bearer` token, stored write-only under the
    /// `{name}_token` secret ref.
    Bearer,
    /// OAuth 2.0 via a reusable service account (epic #477) referenced by id.
    OAuth,
}

/// Parse an env textarea into ordered `(KEY, value)` pairs. Each non-blank line
/// is `KEY=value`; the key is trimmed and the value is everything after the
/// first `=` (values may themselves contain `=`), also trimmed. Lines without a
/// `=`, or with a blank key, are skipped - a malformed line is dropped, never
/// turned into a half-entry.
pub fn parse_env(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let (key, value) = line.split_once('=')?;
            let key = key.trim();
            if key.is_empty() {
                return None;
            }
            Some((key.to_string(), value.trim().to_string()))
        })
        .collect()
}

/// Split a space-separated args string into argv tokens. Any run of whitespace
/// separates; empty tokens are dropped. Deliberately simple - a server needing
/// shell-quoted args with embedded spaces is a rare case v1 leaves to a direct
/// config edit.
pub fn split_args(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_string).collect()
}

/// Split an OAuth scopes string on whitespace and/or commas into individual
/// scopes, dropping empties.
pub fn split_scopes(text: &str) -> Vec<String> {
    text.split([',', ' ', '\t', '\n', '\r'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// The `secrets.toml` ref a server's bearer token is stored under. Convention:
/// `{name}_token`, so a server's config can reference its token by a stable id
/// the user never has to hand-edit.
pub fn bearer_secret_ref(name: &str) -> String {
    format!("{name}_token")
}

/// `true` when a freshly-typed create name collides with an already-configured
/// server. `UpsertMcpServer` is add-or-replace, so creating a server whose name
/// already exists would silently overwrite that server's config *and* clobber
/// its `{name}_token` secret - a footgun the create path uses this to refuse
/// (the user should edit the existing server instead). `name` is trimmed to
/// match the save-time normalization; the comparison is case-sensitive because
/// a server name is an exact config-table key (a case-variant is a distinct
/// server the upsert would not overwrite). Edit is unaffected: it targets its
/// own already-stored name, which is expected to be present.
pub fn is_duplicate_new_name(name: &str, existing: &[String]) -> bool {
    let name = name.trim();
    existing.iter().any(|e| e == name)
}

/// Validate a server name on create: non-empty and only letters, digits, `-`,
/// `_` (mirrors [`super::connection_config_dialog`]'s slug contract - the name
/// is a config table key and a tool-namespace prefix).
fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Server name is required.".to_string());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("Name may only contain letters, digits, '-', and '_'.".to_string());
    }
    Ok(())
}

/// Trim `s`; `None` when the trimmed result is empty (so an empty optional is
/// omitted from the JSON rather than sent as `""`).
fn opt(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// The reactive-free model of the add/edit form. The flat DTO is splatted into
/// the widgets on open and read back on submit, keeping the validation/mapping
/// here (tested) rather than in the view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpForm {
    /// `true` when editing an existing server - the name is immutable.
    pub editing: bool,
    /// Where the server runs: the daemon fleet or this client's local host. It is
    /// a separate axis from [`Self::transport`] (a stdio *or* http server can run
    /// on either side) and is immutable on edit - you cannot move a server between
    /// runners in place.
    pub runner: Runner,
    pub transport: McpTransport,
    pub name: String,
    pub enabled: bool,
    // --- stdio ---
    pub command: String,
    /// Space-separated argv (split on save).
    pub args: String,
    pub namespace: String,
    /// `KEY=value` lines (parsed on save).
    pub env: String,
    // --- http ---
    pub url: String,
    pub auth: McpAuthKind,
    /// Write-only bearer token; never populated from a view.
    pub bearer_token: String,
    /// Referenced service-account id (OAuth).
    pub oauth_account: String,
    /// Space/comma-separated OAuth scopes.
    pub scopes: String,
}

impl McpForm {
    /// A blank create form for `transport`, defaulting to a daemon-run server
    /// (the historical behavior; the create dialog's "Runs on" selector changes
    /// it).
    pub fn blank(transport: McpTransport) -> Self {
        Self {
            editing: false,
            runner: Runner::Daemon,
            transport,
            name: String::new(),
            enabled: true,
            command: String::new(),
            args: String::new(),
            namespace: String::new(),
            env: String::new(),
            url: String::new(),
            auth: McpAuthKind::None,
            bearer_token: String::new(),
            oauth_account: String::new(),
            scopes: String::new(),
        }
    }

    /// Pre-fill an edit form from a server view: name + transport, the surfaced
    /// non-secret config fields, and (for http) the auth kind + oauth
    /// ref/scopes. Secret material (the bearer token) stays blank - the daemon
    /// never echoes it. The `env` box also stays blank: the view does not carry
    /// env, so editing a stdio server cannot pre-fill it (see the form note).
    pub fn from_view(view: &McpServerView) -> Self {
        let transport = if view.transport == "http" {
            McpTransport::Http
        } else {
            McpTransport::Stdio
        };
        let auth = match view.auth_kind.as_deref() {
            Some("bearer") => McpAuthKind::Bearer,
            Some("oauth") => McpAuthKind::OAuth,
            _ => McpAuthKind::None,
        };
        // For http the target is the url; for stdio the command is authoritative.
        let url = if transport == McpTransport::Http {
            view.target.clone()
        } else {
            String::new()
        };
        Self {
            editing: true,
            // Daemon-hosted: the view type only ever describes the daemon's fleet.
            runner: Runner::Daemon,
            transport,
            name: view.name.clone(),
            enabled: view.enabled,
            command: view.command.clone(),
            args: view.args.join(" "),
            namespace: view.namespace.clone().unwrap_or_default(),
            // The view carries no env - it can't be pre-filled (see the form note).
            env: String::new(),
            url,
            auth,
            // Write-only: the bearer token is never echoed / pre-filled.
            bearer_token: String::new(),
            oauth_account: view.oauth_account_ref.clone().unwrap_or_default(),
            scopes: view.oauth_scopes.join(" "),
        }
    }

    /// Pre-fill an edit form from a **client-hosted** server's on-disk config
    /// (`client-mcp.toml`). Unlike the daemon path this config lives locally, so
    /// non-secret fields - including `env` - are echoed. `enabled` is the gtk
    /// surface's view of the server (definition enabled *and* named by
    /// `[surfaces.gtk]`); the bearer token stays blank (there is no client secret
    /// store). Env lines are key-sorted for a deterministic display.
    pub fn from_client_config(cfg: &McpServerConfig, enabled: bool) -> Self {
        let transport = if cfg.http.is_some() {
            McpTransport::Http
        } else {
            McpTransport::Stdio
        };
        let (url, auth, oauth_account, scopes) = match cfg.http.as_ref() {
            Some(http) => {
                let auth = if http.oauth_account.is_some() {
                    McpAuthKind::OAuth
                } else if http.auth_bearer_secret.is_some() {
                    McpAuthKind::Bearer
                } else {
                    McpAuthKind::None
                };
                (
                    http.url.clone(),
                    auth,
                    http.oauth_account.clone().unwrap_or_default(),
                    http.scopes.join(" "),
                )
            }
            None => (
                String::new(),
                McpAuthKind::None,
                String::new(),
                String::new(),
            ),
        };
        let mut env_lines: Vec<String> = cfg.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
        env_lines.sort();
        Self {
            editing: true,
            runner: Runner::Client,
            transport,
            name: cfg.name.clone(),
            enabled,
            command: cfg.command.clone(),
            args: cfg.args.join(" "),
            namespace: cfg.namespace.clone().unwrap_or_default(),
            env: env_lines.join("\n"),
            url,
            auth,
            bearer_token: String::new(),
            oauth_account,
            scopes,
        }
    }

    /// Validate + assemble the form into the command inputs: the target name
    /// (typed + validated on create, immutable on edit), the `config_json`
    /// string `Command::UpsertMcpServer` receives, and the optional bearer
    /// secret `(ref, value)` to write *first*. `Err` carries a human-readable
    /// reason.
    pub fn build(&self) -> Result<BuiltMcpServer, String> {
        let name = self.name.trim().to_string();
        // The name is immutable on edit (already daemon-validated); only a
        // freshly-typed create name is checked.
        if !self.editing {
            validate_name(&name)?;
        }

        // A client-run server is stdio-only: there is no client-side secret store
        // to hold an http bearer token, so honor the runner over any (stale) http
        // selection rather than silently dropping the token (adele-gtk#125). The
        // dialog also forces this in the UI; this keeps the invariant in the pure,
        // tested path. A daemon server keeps both transports.
        let transport = match self.runner {
            Runner::Client => McpTransport::Stdio,
            Runner::Daemon => self.transport,
        };

        let (dto, secret) = match transport {
            McpTransport::Stdio => {
                let command = self.command.trim().to_string();
                if command.is_empty() {
                    return Err("Command is required for a stdio server.".to_string());
                }
                let dto = McpConfigDto {
                    name: name.clone(),
                    enabled: self.enabled,
                    command,
                    args: split_args(&self.args),
                    namespace: opt(&self.namespace),
                    env: parse_env(&self.env).into_iter().collect(),
                    http: None,
                };
                (dto, None)
            }
            McpTransport::Http => {
                let url = self.url.trim().to_string();
                if url.is_empty() {
                    return Err("URL is required for an HTTP server.".to_string());
                }
                let (auth_bearer_secret, oauth_account, scopes, secret) = match self.auth {
                    McpAuthKind::None => (None, None, Vec::new(), None),
                    McpAuthKind::Bearer => {
                        let secret_ref = bearer_secret_ref(&name);
                        let token = self.bearer_token.trim();
                        // Write-only: only write a secret when the user typed one;
                        // a blank field leaves any stored token untouched. The
                        // config still references the ref so the server stays
                        // "bearer" rather than silently going unauthenticated.
                        let secret = if token.is_empty() {
                            None
                        } else {
                            Some((secret_ref.clone(), token.to_string()))
                        };
                        (Some(secret_ref), None, Vec::new(), secret)
                    }
                    McpAuthKind::OAuth => {
                        let account = self.oauth_account.trim().to_string();
                        if account.is_empty() {
                            return Err(
                                "Choose a service account for OAuth authentication.".to_string()
                            );
                        }
                        (None, Some(account), split_scopes(&self.scopes), None)
                    }
                };
                let dto = McpConfigDto {
                    name: name.clone(),
                    enabled: self.enabled,
                    command: String::new(),
                    args: Vec::new(),
                    namespace: None,
                    env: BTreeMap::new(),
                    http: Some(HttpDto {
                        url,
                        auth_bearer_secret,
                        oauth_account,
                        scopes,
                    }),
                };
                (dto, secret)
            }
        };

        let config_json = serde_json::to_string(&dto)
            .map_err(|e| format!("Failed to encode the server config: {e}"))?;
        Ok(BuiltMcpServer {
            editing: self.editing,
            runner: self.runner,
            name,
            config_json,
            secret,
        })
    }
}

/// The assembled inputs for an upsert (+ optional bearer secret) round-trip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltMcpServer {
    /// `true` ⇒ the name already existed (edit); `false` ⇒ create. Both go
    /// through `UpsertMcpServer`, which is add-or-replace.
    pub editing: bool,
    /// Where the server runs. The Settings dialog forks the save on this: a
    /// [`Runner::Daemon`] server goes through the daemon RPC path; a
    /// [`Runner::Client`] server is written to the local `client-mcp.toml`.
    pub runner: Runner,
    /// The target server name (immutable on edit, validated on create).
    pub name: String,
    /// The JSON `McpServerConfig` string for `UpsertMcpServer { config_json }`.
    pub config_json: String,
    /// `(secret_ref, value)` to store via `SetMcpSecret` *before* the upsert,
    /// when the user typed a bearer token. `None` leaves any stored secret
    /// untouched (write-only: a blank field never wipes a token).
    pub secret: Option<(String, String)>,
}

// ===========================================================================
// GTK dialog (thin shell over the pure logic above)
// ===========================================================================

/// Convert an [`McpTransport`] to the transport DropDown index (0 = stdio,
/// 1 = http) and back.
fn transport_from_index(i: u32) -> McpTransport {
    if i == 1 {
        McpTransport::Http
    } else {
        McpTransport::Stdio
    }
}

/// Convert a "Runs on" DropDown index (0 = daemon, 1 = client) to the enum.
fn runner_from_index(i: u32) -> Runner {
    if i == 1 {
        Runner::Client
    } else {
        Runner::Daemon
    }
}

/// Convert a [`Runner`] to its "Runs on" DropDown index.
fn runner_to_index(r: Runner) -> u32 {
    match r {
        Runner::Daemon => 0,
        Runner::Client => 1,
    }
}

/// Convert an auth DropDown index (0 = none, 1 = bearer, 2 = oauth) to the enum.
fn auth_from_index(i: u32) -> McpAuthKind {
    match i {
        1 => McpAuthKind::Bearer,
        2 => McpAuthKind::OAuth,
        _ => McpAuthKind::None,
    }
}

fn auth_to_index(a: McpAuthKind) -> u32 {
    match a {
        McpAuthKind::None => 0,
        McpAuthKind::Bearer => 1,
        McpAuthKind::OAuth => 2,
    }
}

/// A labelled single-line entry, appended to `parent`. Returns the entry so the
/// save handler can read it back.
fn labelled_entry(
    parent: &GtkBox,
    label_text: &str,
    placeholder: Option<&str>,
    initial: &str,
    password: bool,
) -> Entry {
    let label = Label::new(Some(label_text));
    label.set_halign(Align::Start);
    parent.append(&label);
    let entry = Entry::new();
    if let Some(p) = placeholder {
        entry.set_placeholder_text(Some(p));
    }
    if password {
        entry.set_visibility(false);
    }
    entry.set_text(initial);
    parent.append(&entry);
    entry
}

/// Show the runner- and transport-aware MCP add/edit dialog. `initial` is a
/// blank form (create) or a prefilled edit ([`McpForm::from_view`] for a daemon
/// server, [`McpForm::from_client_config`] for a client one); `service_accounts`
/// populates the OAuth account picker. `daemon_names` / `client_names` are the
/// currently-configured server names on each side: on the create path a typed
/// name that collides with one on the *selected runner's* side is refused inline
/// (see [`is_duplicate_new_name`]) rather than silently overwriting it - a client
/// "files" and a daemon "files" are distinct, so the check is per-runner. Both
/// lists are ignored when editing. `on_save` receives the validated
/// [`BuiltMcpServer`] when the user clicks Save; the dialog closes itself on
/// success and keeps itself open (showing the error) on a validation failure.
pub fn show_mcp_server_dialog<FSave>(
    parent: &impl IsA<Window>,
    initial: McpForm,
    service_accounts: Vec<ServiceAccountView>,
    daemon_names: Vec<String>,
    client_names: Vec<String>,
    on_save: FSave,
) where
    FSave: Fn(BuiltMcpServer) + 'static,
{
    let editing = initial.editing;
    let title = if editing {
        format!("Edit MCP server: {}", initial.name)
    } else {
        "Add MCP server".to_string()
    };

    let dialog = Window::builder()
        .title(&title)
        .default_width(480)
        .default_height(460)
        .modal(true)
        .transient_for(parent)
        .build();

    let content = GtkBox::new(Orientation::Vertical, 10);
    content.set_margin_start(20);
    content.set_margin_end(20);
    content.set_margin_top(20);
    content.set_margin_bottom(20);

    let blurb = Label::new(Some(
        "A server runs on the daemon (the shared fleet) or on this client (local tools on this machine). Its transport is separate: a stdio server spawns a command, an HTTP server connects to an endpoint with optional bearer or OAuth authentication. Secret values are stored securely and never kept in the server config.",
    ));
    blurb.set_wrap(true);
    blurb.set_halign(Align::Start);
    blurb.add_css_class("dim-label");
    content.append(&blurb);

    // Runs on: daemon vs this client. Locked on edit - a server cannot move
    // between runners in place (they are administered through different backends).
    let runner_label = Label::new(Some(if editing {
        "Runs on (locked)"
    } else {
        "Runs on"
    }));
    runner_label.set_halign(Align::Start);
    content.append(&runner_label);
    let runner_list = StringList::new(&["Daemon", "This client"]);
    let runner_dd = DropDown::new(Some(runner_list), gtk4::Expression::NONE);
    runner_dd.set_selected(runner_to_index(initial.runner));
    runner_dd.set_sensitive(!editing);
    content.append(&runner_dd);

    // Name.
    let name_label = Label::new(Some(if editing { "Name (locked)" } else { "Name" }));
    name_label.set_halign(Align::Start);
    content.append(&name_label);
    let name_entry = Entry::new();
    name_entry.set_placeholder_text(Some("e.g. files, gmail, github"));
    name_entry.set_text(&initial.name);
    name_entry.set_sensitive(!editing);
    content.append(&name_entry);

    // Enabled.
    let enabled_check = CheckButton::with_label("Enabled");
    enabled_check.set_active(initial.enabled);
    content.append(&enabled_check);

    // Transport (stdio vs http) - a separate axis from the runner above.
    let transport_label = Label::new(Some("Transport"));
    transport_label.set_halign(Align::Start);
    content.append(&transport_label);
    let transport_list = StringList::new(&["Stdio (spawns a command)", "HTTP (remote endpoint)"]);
    let transport_dd = DropDown::new(Some(transport_list), gtk4::Expression::NONE);
    transport_dd.set_selected(match initial.transport {
        McpTransport::Stdio => 0,
        McpTransport::Http => 1,
    });
    // Locked on edit: the transport is part of the stored identity.
    transport_dd.set_sensitive(!editing);
    content.append(&transport_dd);

    // Client servers are stdio-only (no client-side secret store for an http
    // bearer token), so for a client runner the transport picker + http fields
    // are hidden and this hint is shown in their place (adele-gtk#125). Toggled by
    // `update_visibility` below.
    let client_transport_hint = Label::new(Some("Client servers run locally over stdio."));
    client_transport_hint.set_wrap(true);
    client_transport_hint.set_halign(Align::Start);
    client_transport_hint.add_css_class("dim-label");
    content.append(&client_transport_hint);

    content.append(&Separator::new(Orientation::Horizontal));

    // --- stdio fields ---
    let stdio_box = GtkBox::new(Orientation::Vertical, 8);
    let command_entry = labelled_entry(
        &stdio_box,
        "Command",
        Some("e.g. fileio-mcp"),
        &initial.command,
        false,
    );
    let args_entry = labelled_entry(
        &stdio_box,
        "Arguments (space-separated)",
        Some("e.g. serve --root /data"),
        &initial.args,
        false,
    );
    let namespace_entry = labelled_entry(
        &stdio_box,
        "Namespace (optional)",
        Some("prefixes this server's tool names"),
        &initial.namespace,
        false,
    );
    let env_label = Label::new(Some("Environment (KEY=value per line)"));
    env_label.set_halign(Align::Start);
    stdio_box.append(&env_label);
    let env_view = TextView::new();
    env_view.set_wrap_mode(WrapMode::None);
    env_view.set_monospace(true);
    env_view.buffer().set_text(&initial.env);
    let env_scroll = ScrolledWindow::new();
    env_scroll.set_min_content_height(72);
    env_scroll.set_child(Some(&env_view));
    stdio_box.append(&env_scroll);
    if editing {
        let env_note = Label::new(Some(
            "Environment variables aren't shown when editing - re-enter any to keep, or leave blank to clear them.",
        ));
        env_note.set_wrap(true);
        env_note.set_halign(Align::Start);
        env_note.add_css_class("dim-label");
        stdio_box.append(&env_note);
    }
    content.append(&stdio_box);

    // --- http fields ---
    let http_box = GtkBox::new(Orientation::Vertical, 8);
    let url_entry = labelled_entry(
        &http_box,
        "URL",
        Some("https://example.com/mcp/v1"),
        &initial.url,
        false,
    );

    let auth_label = Label::new(Some("Authentication"));
    auth_label.set_halign(Align::Start);
    http_box.append(&auth_label);
    let auth_list = StringList::new(&["None", "Bearer token", "OAuth account"]);
    let auth_dd = DropDown::new(Some(auth_list), gtk4::Expression::NONE);
    auth_dd.set_selected(auth_to_index(initial.auth));
    http_box.append(&auth_dd);

    // Bearer sub-fields.
    let bearer_box = GtkBox::new(Orientation::Vertical, 8);
    let bearer_entry = labelled_entry(
        &bearer_box,
        "Bearer token",
        Some(if editing {
            "Leave blank to keep the stored token"
        } else {
            "Stored write-only; never shown here"
        }),
        &initial.bearer_token,
        true,
    );
    let bearer_note = Label::new(Some(
        "Sent write-only to the daemon; never shown here. Leave blank to keep the current token.",
    ));
    bearer_note.set_wrap(true);
    bearer_note.set_halign(Align::Start);
    bearer_note.add_css_class("dim-label");
    bearer_box.append(&bearer_note);
    http_box.append(&bearer_box);

    // OAuth sub-fields: a type-constrained service-account picker + scopes. The
    // account list carries only ids/labels - never a secret.
    let oauth_box = GtkBox::new(Orientation::Vertical, 8);
    let account_label = Label::new(Some("Service account"));
    account_label.set_halign(Align::Start);
    oauth_box.append(&account_label);
    let account_ids: Rc<Vec<String>> = Rc::new(
        std::iter::once(String::new())
            .chain(service_accounts.iter().map(|a| a.id.clone()))
            .collect(),
    );
    let account_labels: Vec<String> = std::iter::once("(choose an account)".to_string())
        .chain(service_accounts.iter().map(|a| {
            let base = if a.display_name.is_empty() {
                a.id.clone()
            } else {
                a.display_name.clone()
            };
            if a.authorized {
                base
            } else {
                format!("{base}  (not signed in)")
            }
        }))
        .collect();
    let account_label_refs: Vec<&str> = account_labels.iter().map(String::as_str).collect();
    let account_list = StringList::new(&account_label_refs);
    let account_dd = DropDown::new(Some(account_list), gtk4::Expression::NONE);
    // Preselect the referenced account (index 0 is the placeholder).
    if let Some(idx) = account_ids
        .iter()
        .position(|id| id == &initial.oauth_account)
    {
        account_dd.set_selected(idx as u32);
    }
    oauth_box.append(&account_dd);
    if service_accounts.is_empty() {
        let empty_note = Label::new(Some(
            "No service accounts configured. Add one from the Auth settings, then pick it here.",
        ));
        empty_note.set_wrap(true);
        empty_note.set_halign(Align::Start);
        empty_note.add_css_class("dim-label");
        oauth_box.append(&empty_note);
    }
    let scopes_entry = labelled_entry(
        &oauth_box,
        "Scopes (space or comma-separated)",
        Some("calendar.read calendar.write"),
        &initial.scopes,
        false,
    );
    let oauth_note = Label::new(Some(
        "This server uses the selected account's OAuth client. Sign in from the Auth settings - one sign-in serves every server sharing the account.",
    ));
    oauth_note.set_wrap(true);
    oauth_note.set_halign(Align::Start);
    oauth_note.add_css_class("dim-label");
    oauth_box.append(&oauth_note);
    http_box.append(&oauth_box);

    content.append(&http_box);

    // Error + actions.
    content.append(&Separator::new(Orientation::Horizontal));
    let error_label = Label::new(None);
    error_label.set_halign(Align::Start);
    error_label.set_wrap(true);
    error_label.add_css_class("mcp-error-label");
    content.append(&error_label);

    let btn_box = GtkBox::new(Orientation::Horizontal, 8);
    btn_box.set_halign(Align::End);
    let cancel_btn = Button::with_label("Cancel");
    btn_box.append(&cancel_btn);
    let save_btn = Button::with_label(if editing { "Save" } else { "Create" });
    save_btn.add_css_class("suggested-action");
    btn_box.append(&save_btn);
    content.append(&btn_box);

    // Wrap the tall form in a scroller so small displays can reach the buttons.
    let outer_scroll = ScrolledWindow::new();
    outer_scroll.set_child(Some(&content));
    dialog.set_child(Some(&outer_scroll));

    // --- visibility wiring ---
    // Recompute which field groups are visible from the two selectors. Shared
    // by the initial layout and both `selected_notify` handlers.
    let update_visibility = {
        let stdio_box = stdio_box.clone();
        let http_box = http_box.clone();
        let bearer_box = bearer_box.clone();
        let oauth_box = oauth_box.clone();
        let runner_dd = runner_dd.clone();
        let transport_label = transport_label.clone();
        let transport_dd = transport_dd.clone();
        let client_transport_hint = client_transport_hint.clone();
        let auth_dd = auth_dd.clone();
        Rc::new(move || {
            // A client runner is stdio-only: force stdio and hide the transport
            // picker + http fields, showing the hint in their place. Forcing the
            // selection re-enters this closure via the notify handler, which
            // converges (set_selected is a no-op once the value is already 0).
            let is_client = runner_from_index(runner_dd.selected()) == Runner::Client;
            if is_client && transport_dd.selected() != 0 {
                transport_dd.set_selected(0);
            }
            transport_label.set_visible(!is_client);
            transport_dd.set_visible(!is_client);
            client_transport_hint.set_visible(is_client);

            let is_http =
                !is_client && transport_from_index(transport_dd.selected()) == McpTransport::Http;
            stdio_box.set_visible(!is_http);
            http_box.set_visible(is_http);
            let auth = auth_from_index(auth_dd.selected());
            bearer_box.set_visible(is_http && auth == McpAuthKind::Bearer);
            oauth_box.set_visible(is_http && auth == McpAuthKind::OAuth);
        })
    };
    update_visibility();
    // Switching "Runs on" re-applies the client stdio-only constraint.
    runner_dd.connect_selected_notify(glib::clone!(
        #[strong]
        update_visibility,
        move |_| update_visibility()
    ));
    transport_dd.connect_selected_notify(glib::clone!(
        #[strong]
        update_visibility,
        move |_| update_visibility()
    ));
    auth_dd.connect_selected_notify(glib::clone!(
        #[strong]
        update_visibility,
        move |_| update_visibility()
    ));

    cancel_btn.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        move |_| dialog.close()
    ));

    let on_save = Rc::new(on_save);
    let account_dd_for_save = account_dd.clone();
    let account_ids_for_save = Rc::clone(&account_ids);
    save_btn.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        #[strong]
        error_label,
        move |_| {
            // Read the widgets back into a pure `McpForm`, then validate/build.
            let env_buffer = env_view.buffer();
            let env_text = env_buffer
                .text(&env_buffer.start_iter(), &env_buffer.end_iter(), false)
                .to_string();
            let oauth_account = account_ids_for_save
                .get(account_dd_for_save.selected() as usize)
                .cloned()
                .unwrap_or_default();
            let form = McpForm {
                editing,
                runner: runner_from_index(runner_dd.selected()),
                transport: transport_from_index(transport_dd.selected()),
                name: name_entry.text().to_string(),
                enabled: enabled_check.is_active(),
                command: command_entry.text().to_string(),
                args: args_entry.text().to_string(),
                namespace: namespace_entry.text().to_string(),
                env: env_text,
                url: url_entry.text().to_string(),
                auth: auth_from_index(auth_dd.selected()),
                bearer_token: bearer_entry.text().to_string(),
                oauth_account,
                scopes: scopes_entry.text().to_string(),
            };
            match form.build() {
                Ok(built) => {
                    // Create-path uniqueness guard: refuse a new name that already
                    // exists on the SELECTED runner's side rather than issuing an
                    // add-or-replace that would silently overwrite the server (and,
                    // for a daemon bearer server, clobber its secret). The two
                    // runners have independent name spaces.
                    let existing = match built.runner {
                        Runner::Daemon => &daemon_names,
                        Runner::Client => &client_names,
                    };
                    if !built.editing && is_duplicate_new_name(&built.name, existing) {
                        error_label.set_text(&format!(
                            "A server named \"{}\" already exists on that runner - edit it instead.",
                            built.name
                        ));
                        return;
                    }
                    on_save(built);
                    dialog.close();
                }
                Err(e) => error_label.set_text(&e),
            }
        }
    ));

    dialog.present();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stdio(name: &str) -> McpForm {
        McpForm {
            name: name.into(),
            command: "fileio-mcp".into(),
            ..McpForm::blank(McpTransport::Stdio)
        }
    }

    fn http(name: &str) -> McpForm {
        McpForm {
            name: name.into(),
            url: "https://x.example/mcp".into(),
            ..McpForm::blank(McpTransport::Http)
        }
    }

    // --- parse_env ------------------------------------------------------------

    #[test]
    fn parse_env_reads_key_value_lines_in_order() {
        assert_eq!(
            parse_env("TOKEN=abc\nDEBUG=1"),
            vec![
                ("TOKEN".to_string(), "abc".to_string()),
                ("DEBUG".to_string(), "1".to_string()),
            ]
        );
    }

    #[test]
    fn parse_env_skips_blank_and_malformed_lines() {
        assert_eq!(
            parse_env("\n  \nNOVALUE\n=novalue\nOK=1\n"),
            vec![("OK".to_string(), "1".to_string())]
        );
    }

    #[test]
    fn parse_env_value_may_contain_equals() {
        assert_eq!(
            parse_env("QUERY=a=b=c"),
            vec![("QUERY".to_string(), "a=b=c".to_string())]
        );
    }

    #[test]
    fn parse_env_trims_key_and_value() {
        assert_eq!(
            parse_env("  KEY = val \n"),
            vec![("KEY".to_string(), "val".to_string())]
        );
    }

    // --- split_args / split_scopes -------------------------------------------

    #[test]
    fn split_args_splits_on_whitespace_runs() {
        assert_eq!(
            split_args("serve   --root  /data"),
            vec!["serve", "--root", "/data"]
        );
    }

    #[test]
    fn split_args_empty_is_empty() {
        assert!(split_args("   ").is_empty());
        assert!(split_args("").is_empty());
    }

    #[test]
    fn split_scopes_splits_on_whitespace_and_commas() {
        assert_eq!(split_scopes("a b,c ,  d"), vec!["a", "b", "c", "d"]);
        assert!(split_scopes("").is_empty());
    }

    // --- bearer_secret_ref ----------------------------------------------------

    #[test]
    fn bearer_secret_ref_appends_token_suffix() {
        assert_eq!(bearer_secret_ref("gmail"), "gmail_token");
    }

    // --- is_duplicate_new_name (create-path uniqueness guard) -----------------

    #[test]
    fn is_duplicate_new_name_flags_an_existing_name() {
        // Creating a server whose name already exists would silently overwrite
        // it (UpsertMcpServer is add-or-replace) and clobber its token secret.
        let existing = vec!["files".to_string(), "gmail".to_string()];
        assert!(is_duplicate_new_name("gmail", &existing));
    }

    #[test]
    fn is_duplicate_new_name_allows_a_fresh_name() {
        let existing = vec!["files".to_string(), "gmail".to_string()];
        assert!(!is_duplicate_new_name("github", &existing));
    }

    #[test]
    fn is_duplicate_new_name_trims_before_comparing() {
        // The name is trimmed on save, so a surrounding-whitespace variant still
        // targets the same config-table key and must be caught.
        let existing = vec!["files".to_string()];
        assert!(is_duplicate_new_name("  files  ", &existing));
    }

    #[test]
    fn is_duplicate_new_name_is_case_sensitive() {
        // Server names are exact config-table keys, so "Files" and "files" are
        // distinct - upsert wouldn't overwrite, so the guard must not block it.
        let existing = vec!["files".to_string()];
        assert!(!is_duplicate_new_name("Files", &existing));
    }

    #[test]
    fn is_duplicate_new_name_empty_list_never_duplicates() {
        assert!(!is_duplicate_new_name("files", &[]));
    }

    // --- build: stdio ---------------------------------------------------------

    #[test]
    fn build_stdio_emits_exact_config_json() {
        let form = McpForm {
            args: "serve --root /data".into(),
            namespace: "files".into(),
            env: "TOKEN=abc\nDEBUG=1".into(),
            ..stdio("files")
        };
        let built = form.build().expect("builds");
        assert!(!built.editing);
        assert_eq!(built.name, "files");
        assert_eq!(built.secret, None);
        // env is a BTreeMap in the DTO -> keys sorted (DEBUG before TOKEN),
        // deterministic on the wire.
        assert_eq!(
            built.config_json,
            r#"{"name":"files","enabled":true,"command":"fileio-mcp","args":["serve","--root","/data"],"namespace":"files","env":{"DEBUG":"1","TOKEN":"abc"}}"#
        );
    }

    #[test]
    fn build_stdio_omits_empty_optionals() {
        let built = stdio("bare").build().expect("builds");
        assert_eq!(
            built.config_json,
            r#"{"name":"bare","enabled":true,"command":"fileio-mcp"}"#
        );
    }

    #[test]
    fn build_carries_disabled_flag() {
        let form = McpForm {
            enabled: false,
            ..stdio("x")
        };
        let built = form.build().expect("builds");
        assert!(built.config_json.contains(r#""enabled":false"#));
    }

    // --- build: http bearer ---------------------------------------------------

    #[test]
    fn build_http_bearer_emits_config_and_secret() {
        let form = McpForm {
            url: "https://gmailmcp.googleapis.com/mcp/v1".into(),
            auth: McpAuthKind::Bearer,
            bearer_token: "  ya29.token \n".into(),
            ..http("gmail")
        };
        let built = form.build().expect("builds");
        assert_eq!(
            built.config_json,
            r#"{"name":"gmail","enabled":true,"http":{"url":"https://gmailmcp.googleapis.com/mcp/v1","auth_bearer_secret":"gmail_token"}}"#
        );
        // The token is trimmed and written under the `{name}_token` ref.
        assert_eq!(
            built.secret,
            Some(("gmail_token".to_string(), "ya29.token".to_string()))
        );
    }

    #[test]
    fn build_http_bearer_blank_token_writes_no_secret() {
        // Write-only: a blank token field never wipes a stored token - but the
        // config still references the ref so the server is honestly "bearer,
        // token pending" rather than silently switching to unauthenticated.
        let form = McpForm {
            auth: McpAuthKind::Bearer,
            bearer_token: "   ".into(),
            ..http("gmail")
        };
        let built = form.build().expect("builds");
        assert_eq!(built.secret, None);
        assert!(
            built
                .config_json
                .contains(r#""auth_bearer_secret":"gmail_token""#)
        );
    }

    // --- build: http oauth ----------------------------------------------------

    #[test]
    fn build_http_oauth_emits_account_ref_and_scopes() {
        let form = McpForm {
            url: "https://cal.example/mcp".into(),
            auth: McpAuthKind::OAuth,
            oauth_account: "work-google".into(),
            scopes: "calendar.read, calendar.write".into(),
            ..http("cal")
        };
        let built = form.build().expect("builds");
        // OAuth carries only the account ref + scopes - never a secret value.
        assert_eq!(built.secret, None);
        assert_eq!(
            built.config_json,
            r#"{"name":"cal","enabled":true,"http":{"url":"https://cal.example/mcp","oauth_account":"work-google","scopes":["calendar.read","calendar.write"]}}"#
        );
    }

    // --- build: validation ----------------------------------------------------

    #[test]
    fn build_requires_command_for_stdio() {
        let form = McpForm {
            command: "   ".into(),
            ..stdio("x")
        };
        assert!(form.build().is_err());
    }

    #[test]
    fn build_requires_url_for_http() {
        let form = McpForm {
            url: "".into(),
            ..http("x")
        };
        assert!(form.build().is_err());
    }

    #[test]
    fn build_requires_account_for_oauth() {
        let form = McpForm {
            auth: McpAuthKind::OAuth,
            oauth_account: "  ".into(),
            ..http("x")
        };
        assert!(form.build().is_err());
    }

    #[test]
    fn build_requires_valid_name_on_create() {
        assert!(stdio("").build().is_err());
        assert!(stdio("has space").build().is_err());
        assert!(stdio("ok-name_1").build().is_ok());
    }

    #[test]
    fn build_edit_does_not_revalidate_locked_name() {
        // On edit the name is the already-stored (locked) one, so build trusts
        // it rather than re-running the create-time slug check.
        let form = McpForm {
            editing: true,
            name: "already.there".into(),
            ..stdio("already.there")
        };
        let built = form.build().expect("builds");
        assert!(built.editing);
        assert_eq!(built.name, "already.there");
    }

    // --- from_view (edit prefill) --------------------------------------------

    #[test]
    fn from_view_prefills_stdio_editor() {
        let view = McpServerView {
            name: "files".into(),
            command: "fileio-mcp".into(),
            args: vec!["serve".into(), "--root".into(), "/data".into()],
            namespace: Some("files".into()),
            enabled: true,
            status: "running".into(),
            transport: "stdio".into(),
            target: "fileio-mcp".into(),
            ..Default::default()
        };
        let f = McpForm::from_view(&view);
        assert!(f.editing);
        assert_eq!(f.transport, McpTransport::Stdio);
        assert_eq!(f.name, "files");
        assert_eq!(f.command, "fileio-mcp");
        assert_eq!(f.args, "serve --root /data");
        assert_eq!(f.namespace, "files");
        // The view carries no env - never pre-filled.
        assert_eq!(f.env, "");
    }

    #[test]
    fn from_view_prefills_http_bearer_editor() {
        let view = McpServerView {
            name: "gh".into(),
            enabled: true,
            status: "running".into(),
            transport: "http".into(),
            target: "https://gh.example/mcp".into(),
            auth_kind: Some("bearer".into()),
            ..Default::default()
        };
        let f = McpForm::from_view(&view);
        assert_eq!(f.transport, McpTransport::Http);
        assert_eq!(f.auth, McpAuthKind::Bearer);
        assert_eq!(f.url, "https://gh.example/mcp");
        // Write-only: the token is never echoed / pre-filled.
        assert_eq!(f.bearer_token, "");
    }

    // --- runner threading (#122) ---------------------------------------------

    #[test]
    fn blank_defaults_to_daemon_runner() {
        assert_eq!(McpForm::blank(McpTransport::Stdio).runner, Runner::Daemon);
    }

    #[test]
    fn build_carries_daemon_runner_by_default() {
        let built = stdio("files").build().expect("builds");
        assert_eq!(built.runner, Runner::Daemon);
    }

    #[test]
    fn build_carries_client_runner_when_selected() {
        let form = McpForm {
            runner: Runner::Client,
            ..stdio("files")
        };
        let built = form.build().expect("builds");
        assert_eq!(built.runner, Runner::Client);
        // The config JSON is runner-agnostic; only the routing changes.
        assert!(built.config_json.contains(r#""name":"files""#));
    }

    #[test]
    fn build_client_runner_forces_stdio_never_http() {
        // A client server has no client-side secret store, so an http/bearer
        // config can't be honored; the client runner is stdio-only. Even if the
        // form carries http (a stale selection), build() must emit a stdio config,
        // never http, and drop any bearer secret (adele-gtk#125).
        let form = McpForm {
            runner: Runner::Client,
            command: "fileio-mcp".into(),
            auth: McpAuthKind::Bearer,
            bearer_token: "should-be-ignored".into(),
            ..http("files")
        };
        let built = form.build().expect("client form builds as stdio");
        assert_eq!(built.runner, Runner::Client);
        assert!(
            !built.config_json.contains("http"),
            "client config must never carry http: {}",
            built.config_json
        );
        assert!(
            built.config_json.contains(r#""command":"fileio-mcp""#),
            "client config is the stdio command: {}",
            built.config_json
        );
        assert_eq!(built.secret, None, "no bearer secret for a client server");
    }

    #[test]
    fn from_view_is_always_daemon_runner() {
        let view = McpServerView {
            name: "files".into(),
            command: "fileio-mcp".into(),
            enabled: true,
            status: "running".into(),
            transport: "stdio".into(),
            ..Default::default()
        };
        assert_eq!(McpForm::from_view(&view).runner, Runner::Daemon);
    }

    /// Build an [`McpServerConfig`] from TOML via the client-config parser, so the
    /// test doesn't hand-construct the cross-crate struct's full field set.
    fn client_server(toml: &str) -> McpServerConfig {
        desktop_assistant_client_common::mcp_host::ClientMcpConfig::from_toml(toml)
            .expect("valid client-mcp toml")
            .list_defined_servers()
            .first()
            .expect("one server")
            .clone()
    }

    #[test]
    fn from_client_config_prefills_stdio_including_env() {
        let cfg = client_server(
            r#"
[[servers]]
name = "files"
command = "fileio-mcp"
args = ["serve", "--root", "/data"]
namespace = "fs"
[servers.env]
TOKEN = "abc"
DEBUG = "1"
"#,
        );
        let f = McpForm::from_client_config(&cfg, true);
        assert!(f.editing);
        assert_eq!(f.runner, Runner::Client);
        assert_eq!(f.transport, McpTransport::Stdio);
        assert_eq!(f.name, "files");
        assert_eq!(f.command, "fileio-mcp");
        assert_eq!(f.args, "serve --root /data");
        assert_eq!(f.namespace, "fs");
        assert!(f.enabled);
        // Unlike the daemon path, a client server's env is local and IS echoed,
        // key-sorted for a deterministic display.
        assert_eq!(f.env, "DEBUG=1\nTOKEN=abc");
    }

    #[test]
    fn from_client_config_prefills_http_and_reflects_surface_disabled() {
        let cfg = client_server(
            r#"
[[servers]]
name = "cal"
[servers.http]
url = "https://cal.example/mcp"
oauth_account = "work-google"
scopes = ["calendar.read", "calendar.write"]
"#,
        );
        let f = McpForm::from_client_config(&cfg, false);
        assert_eq!(f.runner, Runner::Client);
        assert_eq!(f.transport, McpTransport::Http);
        assert_eq!(f.auth, McpAuthKind::OAuth);
        assert_eq!(f.url, "https://cal.example/mcp");
        assert_eq!(f.oauth_account, "work-google");
        assert_eq!(f.scopes, "calendar.read calendar.write");
        // The gtk surface does not host it -> the Enabled box is unchecked.
        assert!(!f.enabled);
    }

    #[test]
    fn from_view_prefills_http_oauth_editor() {
        let view = McpServerView {
            name: "cal".into(),
            enabled: true,
            status: "needs_auth".into(),
            transport: "http".into(),
            target: "https://cal.example/mcp".into(),
            auth_kind: Some("oauth".into()),
            oauth_account_ref: Some("work-google".into()),
            oauth_scopes: vec!["calendar.read".into()],
            oauth_authorized: Some(false),
            ..Default::default()
        };
        let f = McpForm::from_view(&view);
        assert_eq!(f.transport, McpTransport::Http);
        assert_eq!(f.auth, McpAuthKind::OAuth);
        assert_eq!(f.url, "https://cal.example/mcp");
        assert_eq!(f.oauth_account, "work-google");
        assert_eq!(f.scopes, "calendar.read");
    }
}
