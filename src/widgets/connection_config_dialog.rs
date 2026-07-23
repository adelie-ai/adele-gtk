//! Per-connector-type Configure dialog.
//!
//! Each connector variant has its own field set (Anthropic / OpenAI use an
//! API-key-env-var + base_url; Bedrock uses AWS profile/region and has a
//! "Refresh models" escape hatch; Ollama is just base_url). The dialog
//! produces a `(id, ConnectionConfigView)` pair that the caller submits via
//! `CreateConnection` or `UpdateConnection`.
//!
//! Credentials: the API model carries only the *name* of the env var that
//! holds the API key. Storing the actual secret is out of scope for the
//! dialog — the user is expected to set the env var externally (or have
//! their OS keyring forward it). The field is labelled accordingly.

use std::cell::RefCell;
use std::rc::Rc;

use desktop_assistant_api_model as api;
use gtk4::prelude::*;
use gtk4::{Align, Box as GtkBox, Button, Entry, Label, Orientation, Separator, Window, glib};

/// Which connector type this dialog is configuring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectorType {
    Anthropic,
    OpenAi,
    OpenRouter,
    Azure,
    Google,
    Bedrock,
    Ollama,
}

impl ConnectorType {
    pub fn label(self) -> &'static str {
        match self {
            Self::Anthropic => "Anthropic",
            Self::OpenAi => "OpenAI",
            Self::OpenRouter => "OpenRouter",
            Self::Azure => "Azure",
            Self::Google => "Google",
            Self::Bedrock => "Bedrock",
            Self::Ollama => "Ollama",
        }
    }

    /// Inverse of [`Self::from_slug`]. Only exercised by the round-trip
    /// test today (the daemon supplies connector-type strings directly),
    /// so it's gated to test builds to avoid a dead-code warning while
    /// keeping the round-trip assertion meaningful.
    #[cfg(test)]
    pub fn slug(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
            Self::OpenRouter => "openrouter",
            Self::Azure => "azure",
            Self::Google => "google",
            Self::Bedrock => "bedrock",
            Self::Ollama => "ollama",
        }
    }

    pub fn from_slug(s: &str) -> Option<Self> {
        match s {
            "anthropic" => Some(Self::Anthropic),
            "openai" => Some(Self::OpenAi),
            "openrouter" => Some(Self::OpenRouter),
            "azure" => Some(Self::Azure),
            "google" => Some(Self::Google),
            "bedrock" => Some(Self::Bedrock),
            "ollama" => Some(Self::Ollama),
            _ => None,
        }
    }

    pub fn empty_config(self) -> api::ConnectionConfigView {
        match self {
            Self::Anthropic => api::ConnectionConfigView::Anthropic {
                base_url: None,
                api_key_env: None,
                connect_timeout_secs: None,
                stream_timeout_secs: None,
                max_context_tokens: None,
            },
            Self::OpenAi => api::ConnectionConfigView::OpenAi {
                base_url: None,
                api_key_env: None,
                connect_timeout_secs: None,
                stream_timeout_secs: None,
                max_context_tokens: None,
            },
            Self::OpenRouter => api::ConnectionConfigView::OpenRouter {
                base_url: None,
                api_key_env: None,
                connect_timeout_secs: None,
                stream_timeout_secs: None,
                max_context_tokens: None,
            },
            Self::Azure => api::ConnectionConfigView::Azure {
                base_url: None,
                api_key_env: None,
                api_surface: None,
                auth_mode: None,
                api_version: None,
                connect_timeout_secs: None,
                stream_timeout_secs: None,
                max_context_tokens: None,
            },
            Self::Google => api::ConnectionConfigView::Google {
                base_url: None,
                api_key_env: None,
                project: None,
                location: None,
                auth_mode: None,
                credentials_path: None,
                connect_timeout_secs: None,
                stream_timeout_secs: None,
                max_context_tokens: None,
            },
            Self::Bedrock => api::ConnectionConfigView::Bedrock {
                aws_profile: None,
                region: None,
                base_url: None,
                connect_timeout_secs: None,
                stream_timeout_secs: None,
                max_context_tokens: None,
            },
            Self::Ollama => api::ConnectionConfigView::Ollama {
                base_url: None,
                connect_timeout_secs: None,
                stream_timeout_secs: None,
                keep_warm: None,
                max_context_tokens: None,
            },
        }
    }
}

/// Sanitize a text entry into `Option<String>`, trimming and treating the
/// empty string as `None`.
fn text_opt(entry: &Entry) -> Option<String> {
    let t = entry.text().trim().to_string();
    if t.is_empty() { None } else { Some(t) }
}

/// Config fields the dialog doesn't surface as form inputs (timeouts, the
/// context ceiling, and Ollama's keep-warm flag). We carry them through an
/// edit round-trip so saving the dialog preserves whatever the daemon had
/// stored instead of silently resetting them to `None`. `keep_warm` is
/// Ollama-only and `None` for every other variant; a `None` config (the
/// create path) yields all `None`.
#[derive(Clone, Copy, Default)]
struct PreservedFields {
    connect_timeout_secs: Option<u64>,
    stream_timeout_secs: Option<u64>,
    max_context_tokens: Option<u64>,
    keep_warm: Option<bool>,
}

impl PreservedFields {
    fn from_config(config: Option<&api::ConnectionConfigView>) -> Self {
        match config {
            Some(
                api::ConnectionConfigView::Anthropic {
                    connect_timeout_secs,
                    stream_timeout_secs,
                    max_context_tokens,
                    ..
                }
                | api::ConnectionConfigView::OpenAi {
                    connect_timeout_secs,
                    stream_timeout_secs,
                    max_context_tokens,
                    ..
                }
                | api::ConnectionConfigView::OpenRouter {
                    connect_timeout_secs,
                    stream_timeout_secs,
                    max_context_tokens,
                    ..
                }
                | api::ConnectionConfigView::Bedrock {
                    connect_timeout_secs,
                    stream_timeout_secs,
                    max_context_tokens,
                    ..
                },
            ) => Self {
                connect_timeout_secs: *connect_timeout_secs,
                stream_timeout_secs: *stream_timeout_secs,
                max_context_tokens: *max_context_tokens,
                keep_warm: None,
            },
            Some(api::ConnectionConfigView::Ollama {
                connect_timeout_secs,
                stream_timeout_secs,
                max_context_tokens,
                keep_warm,
                ..
            }) => Self {
                connect_timeout_secs: *connect_timeout_secs,
                stream_timeout_secs: *stream_timeout_secs,
                max_context_tokens: *max_context_tokens,
                keep_warm: *keep_warm,
            },
            // Azure and Google are first-class creatable/editable connectors, so
            // their unsurfaced timeouts and context ceiling must round-trip
            // through an edit the same way the other API-key connectors do.
            // Neither carries `keep_warm` (Ollama-only).
            Some(
                api::ConnectionConfigView::Azure {
                    connect_timeout_secs,
                    stream_timeout_secs,
                    max_context_tokens,
                    ..
                }
                | api::ConnectionConfigView::Google {
                    connect_timeout_secs,
                    stream_timeout_secs,
                    max_context_tokens,
                    ..
                },
            ) => Self {
                connect_timeout_secs: *connect_timeout_secs,
                stream_timeout_secs: *stream_timeout_secs,
                max_context_tokens: *max_context_tokens,
                keep_warm: None,
            },
            // Create path (no stored config): nothing to preserve.
            None => Self::default(),
        }
    }
}

/// Declarative description of one form field for a connector. `initial` holds
/// the value to pre-fill from echoed config (always `None` for secret fields —
/// raw secrets are never round-tripped through the API). Pure data so the
/// pre-fill mapping can be unit-tested without constructing GTK widgets.
struct FieldSpec {
    label: &'static str,
    name: &'static str,
    placeholder: Option<&'static str>,
    initial: Option<String>,
    secret: bool,
}

/// Compute the ordered field specs for `connector`, pre-filling `initial` from
/// the echoed non-secret `config` when its variant matches the connector. A
/// `None` config (create path / older daemon that omits `config`) or a
/// mismatched variant yields blank fields. No field carries a secret value.
fn field_specs(
    connector: ConnectorType,
    config: Option<&api::ConnectionConfigView>,
) -> Vec<FieldSpec> {
    match connector {
        ConnectorType::Anthropic => {
            let (base_url, api_key_env) = match config {
                Some(api::ConnectionConfigView::Anthropic {
                    base_url,
                    api_key_env,
                    ..
                }) => (base_url.clone(), api_key_env.clone()),
                _ => (None, None),
            };
            vec![
                FieldSpec {
                    label: "Base URL (optional override)",
                    name: "base_url",
                    placeholder: Some("https://api.anthropic.com"),
                    initial: base_url,
                    secret: false,
                },
                FieldSpec {
                    label: "API key env var (e.g. ANTHROPIC_API_KEY)",
                    name: "api_key_env",
                    placeholder: Some("ANTHROPIC_API_KEY"),
                    initial: api_key_env,
                    secret: false,
                },
            ]
        }
        ConnectorType::OpenAi => {
            let (base_url, api_key_env) = match config {
                Some(api::ConnectionConfigView::OpenAi {
                    base_url,
                    api_key_env,
                    ..
                }) => (base_url.clone(), api_key_env.clone()),
                _ => (None, None),
            };
            vec![
                FieldSpec {
                    label: "Base URL (for OpenAI-compatible providers)",
                    name: "base_url",
                    placeholder: Some("https://api.openai.com/v1"),
                    initial: base_url,
                    secret: false,
                },
                FieldSpec {
                    label: "API key env var (e.g. OPENAI_API_KEY)",
                    name: "api_key_env",
                    placeholder: Some("OPENAI_API_KEY"),
                    initial: api_key_env,
                    secret: false,
                },
            ]
        }
        ConnectorType::OpenRouter => {
            let (base_url, api_key_env) = match config {
                Some(api::ConnectionConfigView::OpenRouter {
                    base_url,
                    api_key_env,
                    ..
                }) => (base_url.clone(), api_key_env.clone()),
                _ => (None, None),
            };
            vec![
                FieldSpec {
                    label: "Base URL (optional override)",
                    name: "base_url",
                    placeholder: Some("https://openrouter.ai/api/v1"),
                    initial: base_url,
                    secret: false,
                },
                FieldSpec {
                    label: "API key env var (e.g. OPENROUTER_API_KEY)",
                    name: "api_key_env",
                    placeholder: Some("OPENROUTER_API_KEY"),
                    initial: api_key_env,
                    secret: false,
                },
            ]
        }
        ConnectorType::Azure => {
            let (base_url, api_key_env, api_surface, auth_mode, api_version) = match config {
                Some(api::ConnectionConfigView::Azure {
                    base_url,
                    api_key_env,
                    api_surface,
                    auth_mode,
                    api_version,
                    ..
                }) => (
                    base_url.clone(),
                    api_key_env.clone(),
                    api_surface.clone(),
                    auth_mode.clone(),
                    api_version.clone(),
                ),
                _ => (None, None, None, None, None),
            };
            vec![
                FieldSpec {
                    label: "Resource endpoint (base URL)",
                    name: "base_url",
                    placeholder: Some("https://<name>.openai.azure.com"),
                    initial: base_url,
                    secret: false,
                },
                FieldSpec {
                    label: "API key env var (e.g. AZURE_OPENAI_API_KEY)",
                    name: "api_key_env",
                    placeholder: Some("AZURE_OPENAI_API_KEY"),
                    initial: api_key_env,
                    secret: false,
                },
                FieldSpec {
                    label: "API surface (v1 or classic)",
                    name: "api_surface",
                    placeholder: Some("v1 (default) or classic"),
                    initial: api_surface,
                    secret: false,
                },
                FieldSpec {
                    label: "Auth mode (api_key or entra)",
                    name: "auth_mode",
                    placeholder: Some("api_key (default) or entra"),
                    initial: auth_mode,
                    secret: false,
                },
                FieldSpec {
                    label: "API version (classic surface only)",
                    name: "api_version",
                    placeholder: Some("e.g. 2024-10-21"),
                    initial: api_version,
                    secret: false,
                },
            ]
        }
        ConnectorType::Google => {
            let (base_url, api_key_env, project, location, auth_mode, credentials_path) =
                match config {
                    Some(api::ConnectionConfigView::Google {
                        base_url,
                        api_key_env,
                        project,
                        location,
                        auth_mode,
                        credentials_path,
                        ..
                    }) => (
                        base_url.clone(),
                        api_key_env.clone(),
                        project.clone(),
                        location.clone(),
                        auth_mode.clone(),
                        credentials_path.clone(),
                    ),
                    _ => (None, None, None, None, None, None),
                };
            vec![
                FieldSpec {
                    label: "Base URL (optional override)",
                    name: "base_url",
                    placeholder: Some("usually blank"),
                    initial: base_url,
                    secret: false,
                },
                FieldSpec {
                    label: "API key env var (e.g. GOOGLE_API_KEY)",
                    name: "api_key_env",
                    placeholder: Some("GOOGLE_API_KEY"),
                    initial: api_key_env,
                    secret: false,
                },
                FieldSpec {
                    label: "GCP project",
                    name: "project",
                    placeholder: Some("my-gcp-project"),
                    initial: project,
                    secret: false,
                },
                FieldSpec {
                    label: "Location / region",
                    name: "location",
                    placeholder: Some("us-central1"),
                    initial: location,
                    secret: false,
                },
                FieldSpec {
                    label: "Auth mode (vertex or api_key)",
                    name: "auth_mode",
                    placeholder: Some("vertex (default) or api_key"),
                    initial: auth_mode,
                    secret: false,
                },
                FieldSpec {
                    label: "Service-account credentials path (Vertex)",
                    name: "credentials_path",
                    placeholder: Some("/path/to/service-account.json"),
                    initial: credentials_path,
                    secret: false,
                },
            ]
        }
        ConnectorType::Bedrock => {
            let (aws_profile, region, base_url) = match config {
                Some(api::ConnectionConfigView::Bedrock {
                    aws_profile,
                    region,
                    base_url,
                    ..
                }) => (aws_profile.clone(), region.clone(), base_url.clone()),
                _ => (None, None, None),
            };
            vec![
                FieldSpec {
                    label: "AWS profile (optional)",
                    name: "aws_profile",
                    placeholder: Some("default"),
                    initial: aws_profile,
                    secret: false,
                },
                FieldSpec {
                    label: "Region",
                    name: "region",
                    placeholder: Some("us-west-2"),
                    initial: region,
                    secret: false,
                },
                FieldSpec {
                    label: "Base URL override (optional)",
                    name: "base_url",
                    placeholder: None,
                    initial: base_url,
                    secret: false,
                },
            ]
        }
        ConnectorType::Ollama => {
            let base_url = match config {
                Some(api::ConnectionConfigView::Ollama { base_url, .. }) => base_url.clone(),
                _ => None,
            };
            vec![FieldSpec {
                label: "Base URL",
                name: "base_url",
                placeholder: Some("http://localhost:11434"),
                initial: base_url,
                secret: false,
            }]
        }
    }
}

/// Trailing dim-label hint shown under the fields for connectors that read an
/// API key from a named env var.
fn connector_hint(connector: ConnectorType) -> Option<&'static str> {
    match connector {
        ConnectorType::Anthropic => Some(
            "The daemon reads the API key from the named env var. Set it in your daemon environment (systemd unit, shell, etc.).",
        ),
        ConnectorType::OpenAi
        | ConnectorType::OpenRouter
        | ConnectorType::Azure
        | ConnectorType::Google => Some(
            "The daemon reads the API key from the named env var (used in api-key auth modes). Set it in your daemon environment.",
        ),
        ConnectorType::Bedrock | ConnectorType::Ollama => None,
    }
}

/// Show the Configure dialog. `existing` distinguishes edit (Some) from
/// create (None). `on_save` is called with the final `(id, config)` pair
/// when the user clicks Save; the dialog closes on its own.
///
/// `on_refresh_models` is invoked for Bedrock's "Refresh models" button;
/// only meaningful for Bedrock — it's a best-effort affordance and the
/// dialog doesn't display the returned list.
pub fn show_configure_dialog<FSave, FRefresh>(
    parent: &impl IsA<Window>,
    connector: ConnectorType,
    existing: Option<(String, api::ConnectionConfigView)>,
    on_save: FSave,
    on_refresh_models: FRefresh,
) where
    FSave: Fn(String, api::ConnectionConfigView) + 'static,
    FRefresh: Fn(String) + 'static,
{
    let is_edit = existing.is_some();
    let title = match &existing {
        Some((id, _)) => format!("Edit {} connection: {id}", connector.label()),
        None => format!("Add {} connection", connector.label()),
    };

    let dialog = Window::builder()
        .title(&title)
        .default_width(440)
        .default_height(320)
        .modal(true)
        .transient_for(parent)
        .build();

    let content = GtkBox::new(Orientation::Vertical, 10);
    content.set_margin_start(20);
    content.set_margin_end(20);
    content.set_margin_top(20);
    content.set_margin_bottom(20);

    // Connection id.
    let id_label = Label::new(Some("Connection id (slug)"));
    id_label.set_halign(Align::Start);
    content.append(&id_label);

    let id_entry = Entry::new();
    id_entry.set_placeholder_text(Some("e.g. work, aws-prod, local"));
    if let Some((id, _)) = &existing {
        id_entry.set_text(id);
        id_entry.set_sensitive(false);
    }
    content.append(&id_entry);

    content.append(&Separator::new(Orientation::Horizontal));

    // Per-connector field map: we track entries by name in a Vec so the
    // save-handler can pick them up regardless of which variant is shown.
    #[derive(Clone)]
    struct Field {
        name: &'static str,
        entry: Entry,
    }
    let fields: Rc<RefCell<Vec<Field>>> = Rc::new(RefCell::new(Vec::new()));

    let add_field = |label_text: &str,
                     name: &'static str,
                     placeholder: Option<&str>,
                     initial: Option<&str>,
                     secret: bool| {
        let lab = Label::new(Some(label_text));
        lab.set_halign(Align::Start);
        content.append(&lab);
        let entry = Entry::new();
        if let Some(p) = placeholder {
            entry.set_placeholder_text(Some(p));
        }
        if secret {
            entry.set_visibility(false);
        }
        if let Some(v) = initial {
            entry.set_text(v);
        }
        content.append(&entry);
        fields.borrow_mut().push(Field {
            name,
            entry: entry.clone(),
        });
    };

    // Pre-fill the per-connector fields from the echoed non-secret config (or
    // leave them blank on the create path / older daemons). Computed by a pure
    // helper so the pre-fill mapping is unit-testable without a GTK display.
    let existing_config = existing.as_ref().map(|(_, c)| c.clone());
    for spec in field_specs(connector, existing_config.as_ref()) {
        add_field(
            spec.label,
            spec.name,
            spec.placeholder,
            spec.initial.as_deref(),
            spec.secret,
        );
    }
    if let Some(hint_text) = connector_hint(connector) {
        let hint = Label::new(Some(hint_text));
        hint.set_halign(Align::Start);
        hint.set_wrap(true);
        hint.add_css_class("dim-label");
        content.append(&hint);
    }

    // Bedrock-only: "Refresh models" button. Only meaningful when editing an
    // existing connection (the Add path has nothing saved to refresh and wires
    // a no-op callback), so only show it there. Refresh calls the refresh
    // callback directly — it does NOT save the dialog's current fields.
    if connector == ConnectorType::Bedrock && is_edit {
        content.append(&Separator::new(Orientation::Horizontal));
        let btn_row = GtkBox::new(Orientation::Horizontal, 8);
        let refresh_btn = Button::with_label("Refresh models");
        refresh_btn.set_tooltip_text(Some(
            "Re-query Bedrock's ListFoundationModels and update the cached model list.",
        ));
        let note = Label::new(Some("(Refreshes the model list for the saved connection.)"));
        note.add_css_class("dim-label");
        btn_row.append(&refresh_btn);
        btn_row.append(&note);
        content.append(&btn_row);

        let refresh_cb = Rc::new(on_refresh_models);
        refresh_btn.connect_clicked(glib::clone!(
            #[weak(rename_to = id_entry_ref)]
            id_entry,
            move |_| {
                let id = id_entry_ref.text().trim().to_string();
                if id.is_empty() {
                    return;
                }
                refresh_cb(id);
            }
        ));
    }

    content.append(&Separator::new(Orientation::Horizontal));

    let status = Label::new(None);
    status.add_css_class("status-bar");
    status.set_halign(Align::Start);
    content.append(&status);

    let btn_box = GtkBox::new(Orientation::Horizontal, 8);
    btn_box.set_halign(Align::End);
    btn_box.set_margin_top(4);

    let cancel_btn = Button::with_label("Cancel");
    btn_box.append(&cancel_btn);

    let save_btn = Button::with_label("Save");
    save_btn.add_css_class("suggested-action");
    btn_box.append(&save_btn);

    content.append(&btn_box);
    dialog.set_child(Some(&content));

    cancel_btn.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        move |_| dialog.close()
    ));

    let save_cb = Rc::new(on_save);
    // Fields the dialog doesn't surface as inputs (timeouts / context ceiling /
    // keep-warm). Carried through so an edit save preserves the daemon's stored
    // values; `None` everywhere on the create path. `Copy`, so it moves into the
    // save closure directly.
    let preserved = PreservedFields::from_config(existing_config.as_ref());

    save_btn.connect_clicked(glib::clone!(
        #[weak(rename_to = dialog_ref)]
        dialog,
        #[strong(rename_to = fields_for_save)]
        fields,
        #[weak]
        id_entry,
        move |_| {
            let id = id_entry.text().trim().to_string();
            if id.is_empty() {
                status.set_text("Connection id is required");
                return;
            }
            if !id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                status.set_text("Id may only contain letters, digits, '-', and '_'");
                return;
            }

            let by_name = |n: &str| -> Option<String> {
                fields_for_save
                    .borrow()
                    .iter()
                    .find(|f| f.name == n)
                    .and_then(|f| text_opt(&f.entry))
            };

            let config = match connector {
                ConnectorType::Anthropic => api::ConnectionConfigView::Anthropic {
                    base_url: by_name("base_url"),
                    api_key_env: by_name("api_key_env"),
                    connect_timeout_secs: preserved.connect_timeout_secs,
                    stream_timeout_secs: preserved.stream_timeout_secs,
                    max_context_tokens: preserved.max_context_tokens,
                },
                ConnectorType::OpenAi => api::ConnectionConfigView::OpenAi {
                    base_url: by_name("base_url"),
                    api_key_env: by_name("api_key_env"),
                    connect_timeout_secs: preserved.connect_timeout_secs,
                    stream_timeout_secs: preserved.stream_timeout_secs,
                    max_context_tokens: preserved.max_context_tokens,
                },
                ConnectorType::OpenRouter => api::ConnectionConfigView::OpenRouter {
                    base_url: by_name("base_url"),
                    api_key_env: by_name("api_key_env"),
                    connect_timeout_secs: preserved.connect_timeout_secs,
                    stream_timeout_secs: preserved.stream_timeout_secs,
                    max_context_tokens: preserved.max_context_tokens,
                },
                ConnectorType::Azure => api::ConnectionConfigView::Azure {
                    base_url: by_name("base_url"),
                    api_key_env: by_name("api_key_env"),
                    api_surface: by_name("api_surface"),
                    auth_mode: by_name("auth_mode"),
                    api_version: by_name("api_version"),
                    connect_timeout_secs: preserved.connect_timeout_secs,
                    stream_timeout_secs: preserved.stream_timeout_secs,
                    max_context_tokens: preserved.max_context_tokens,
                },
                ConnectorType::Google => api::ConnectionConfigView::Google {
                    base_url: by_name("base_url"),
                    api_key_env: by_name("api_key_env"),
                    project: by_name("project"),
                    location: by_name("location"),
                    auth_mode: by_name("auth_mode"),
                    credentials_path: by_name("credentials_path"),
                    connect_timeout_secs: preserved.connect_timeout_secs,
                    stream_timeout_secs: preserved.stream_timeout_secs,
                    max_context_tokens: preserved.max_context_tokens,
                },
                ConnectorType::Bedrock => api::ConnectionConfigView::Bedrock {
                    aws_profile: by_name("aws_profile"),
                    region: by_name("region"),
                    base_url: by_name("base_url"),
                    connect_timeout_secs: preserved.connect_timeout_secs,
                    stream_timeout_secs: preserved.stream_timeout_secs,
                    max_context_tokens: preserved.max_context_tokens,
                },
                ConnectorType::Ollama => api::ConnectionConfigView::Ollama {
                    base_url: by_name("base_url"),
                    connect_timeout_secs: preserved.connect_timeout_secs,
                    stream_timeout_secs: preserved.stream_timeout_secs,
                    keep_warm: preserved.keep_warm,
                    max_context_tokens: preserved.max_context_tokens,
                },
            };

            save_cb(id, config);
            dialog_ref.close();
        }
    ));

    dialog.present();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connector_slug_roundtrip() {
        for c in [
            ConnectorType::Anthropic,
            ConnectorType::OpenAi,
            ConnectorType::OpenRouter,
            ConnectorType::Azure,
            ConnectorType::Google,
            ConnectorType::Bedrock,
            ConnectorType::Ollama,
        ] {
            assert_eq!(ConnectorType::from_slug(c.slug()), Some(c));
        }
        assert_eq!(ConnectorType::from_slug("unknown"), None);
    }

    #[test]
    fn openrouter_slug_maps_to_variant() {
        // OpenRouter is a first-class creatable connector; its slug must round-trip
        // and its empty config must carry the OpenRouter variant.
        assert_eq!(
            ConnectorType::from_slug("openrouter"),
            Some(ConnectorType::OpenRouter)
        );
        assert_eq!(ConnectorType::OpenRouter.slug(), "openrouter");
        assert!(matches!(
            ConnectorType::OpenRouter.empty_config(),
            api::ConnectionConfigView::OpenRouter { .. }
        ));
    }

    #[test]
    fn empty_config_has_correct_variant() {
        assert!(matches!(
            ConnectorType::Anthropic.empty_config(),
            api::ConnectionConfigView::Anthropic { .. }
        ));
        assert!(matches!(
            ConnectorType::Bedrock.empty_config(),
            api::ConnectionConfigView::Bedrock { .. }
        ));
        assert!(matches!(
            ConnectorType::Ollama.empty_config(),
            api::ConnectionConfigView::Ollama { .. }
        ));
        assert!(matches!(
            ConnectorType::OpenAi.empty_config(),
            api::ConnectionConfigView::OpenAi { .. }
        ));
        assert!(matches!(
            ConnectorType::OpenRouter.empty_config(),
            api::ConnectionConfigView::OpenRouter { .. }
        ));
        assert!(matches!(
            ConnectorType::Azure.empty_config(),
            api::ConnectionConfigView::Azure { .. }
        ));
        assert!(matches!(
            ConnectorType::Google.empty_config(),
            api::ConnectionConfigView::Google { .. }
        ));
    }

    #[test]
    fn field_specs_prefill_from_echoed_openrouter_config() {
        // OpenRouter mirrors OpenAI: base_url + api_key_env pre-fill from the
        // echoed config; neither is a secret.
        let config = api::ConnectionConfigView::OpenRouter {
            base_url: Some("https://openrouter.ai/api/v1".to_string()),
            api_key_env: Some("MY_OPENROUTER_KEY".to_string()),
            connect_timeout_secs: None,
            stream_timeout_secs: None,
            max_context_tokens: None,
        };
        let specs = field_specs(ConnectorType::OpenRouter, Some(&config));
        assert_eq!(
            initial_of(&specs, "base_url"),
            Some("https://openrouter.ai/api/v1")
        );
        assert_eq!(initial_of(&specs, "api_key_env"), Some("MY_OPENROUTER_KEY"));
        assert!(specs.iter().all(|f| !f.secret));
    }

    #[test]
    fn preserved_fields_round_trip_openrouter() {
        // OpenRouter is in the api-key preserved-fields group (like OpenAI): its
        // unsurfaced timeouts / context ceiling round-trip; it has no keep_warm.
        let openrouter = api::ConnectionConfigView::OpenRouter {
            base_url: Some("https://openrouter.ai/api/v1".to_string()),
            api_key_env: Some("OPENROUTER_API_KEY".to_string()),
            connect_timeout_secs: Some(7),
            stream_timeout_secs: Some(90),
            max_context_tokens: Some(128_000),
        };
        let p = PreservedFields::from_config(Some(&openrouter));
        assert_eq!(p.connect_timeout_secs, Some(7));
        assert_eq!(p.stream_timeout_secs, Some(90));
        assert_eq!(p.max_context_tokens, Some(128_000));
        assert_eq!(p.keep_warm, None);
    }

    #[test]
    fn field_specs_prefill_from_echoed_azure_config() {
        // Azure surfaces base_url + api_key_env plus the two enum fields
        // (api_surface / auth_mode) and classic-only api_version. Every surfaced
        // field pre-fills from the echoed config on the edit path, and none is a
        // secret (only the env-var *name* travels).
        let config = api::ConnectionConfigView::Azure {
            base_url: Some("https://my-resource.openai.azure.com".to_string()),
            api_key_env: Some("MY_AZURE_KEY".to_string()),
            api_surface: Some("classic".to_string()),
            auth_mode: Some("entra".to_string()),
            api_version: Some("2024-10-21".to_string()),
            connect_timeout_secs: None,
            stream_timeout_secs: None,
            max_context_tokens: None,
        };
        let specs = field_specs(ConnectorType::Azure, Some(&config));
        assert_eq!(
            initial_of(&specs, "base_url"),
            Some("https://my-resource.openai.azure.com")
        );
        assert_eq!(initial_of(&specs, "api_key_env"), Some("MY_AZURE_KEY"));
        assert_eq!(initial_of(&specs, "api_surface"), Some("classic"));
        assert_eq!(initial_of(&specs, "auth_mode"), Some("entra"));
        assert_eq!(initial_of(&specs, "api_version"), Some("2024-10-21"));
        assert!(specs.iter().all(|f| !f.secret));
    }

    #[test]
    fn preserved_fields_round_trip_azure() {
        // Azure joins the API-key preserved-fields group: its unsurfaced
        // timeouts / context ceiling round-trip through an edit, and it has no
        // keep_warm (Ollama-only). This is the guard against a `_ =>` fold
        // silently dropping Azure's stored values on save.
        let azure = api::ConnectionConfigView::Azure {
            base_url: Some("https://my-resource.openai.azure.com".to_string()),
            api_key_env: Some("AZURE_OPENAI_API_KEY".to_string()),
            api_surface: Some("v1".to_string()),
            auth_mode: Some("api_key".to_string()),
            api_version: None,
            connect_timeout_secs: Some(4),
            stream_timeout_secs: Some(75),
            max_context_tokens: Some(200_000),
        };
        let p = PreservedFields::from_config(Some(&azure));
        assert_eq!(p.connect_timeout_secs, Some(4));
        assert_eq!(p.stream_timeout_secs, Some(75));
        assert_eq!(p.max_context_tokens, Some(200_000));
        assert_eq!(p.keep_warm, None);
    }

    #[test]
    fn field_specs_prefill_from_echoed_google_config() {
        // Google (Vertex / Gemini) surfaces base_url, api_key_env, project,
        // location, the auth_mode enum, and the Vertex credentials_path. Every
        // surfaced field pre-fills; none is a secret.
        let config = api::ConnectionConfigView::Google {
            base_url: None,
            api_key_env: Some("MY_GOOGLE_KEY".to_string()),
            project: Some("my-gcp-project".to_string()),
            location: Some("us-central1".to_string()),
            auth_mode: Some("vertex".to_string()),
            credentials_path: Some("/etc/gcp/sa.json".to_string()),
            connect_timeout_secs: None,
            stream_timeout_secs: None,
            max_context_tokens: None,
        };
        let specs = field_specs(ConnectorType::Google, Some(&config));
        assert_eq!(initial_of(&specs, "base_url"), None);
        assert_eq!(initial_of(&specs, "api_key_env"), Some("MY_GOOGLE_KEY"));
        assert_eq!(initial_of(&specs, "project"), Some("my-gcp-project"));
        assert_eq!(initial_of(&specs, "location"), Some("us-central1"));
        assert_eq!(initial_of(&specs, "auth_mode"), Some("vertex"));
        assert_eq!(
            initial_of(&specs, "credentials_path"),
            Some("/etc/gcp/sa.json")
        );
        assert!(specs.iter().all(|f| !f.secret));
    }

    #[test]
    fn preserved_fields_round_trip_google() {
        // Google joins the API-key preserved-fields group: unsurfaced timeouts /
        // context ceiling round-trip; no keep_warm. Guards against a `_ =>` fold
        // dropping Google's stored values on save.
        let google = api::ConnectionConfigView::Google {
            base_url: None,
            api_key_env: Some("GOOGLE_API_KEY".to_string()),
            project: Some("my-gcp-project".to_string()),
            location: Some("us-central1".to_string()),
            auth_mode: Some("vertex".to_string()),
            credentials_path: Some("/etc/gcp/sa.json".to_string()),
            connect_timeout_secs: Some(6),
            stream_timeout_secs: Some(120),
            max_context_tokens: Some(1_000_000),
        };
        let p = PreservedFields::from_config(Some(&google));
        assert_eq!(p.connect_timeout_secs, Some(6));
        assert_eq!(p.stream_timeout_secs, Some(120));
        assert_eq!(p.max_context_tokens, Some(1_000_000));
        assert_eq!(p.keep_warm, None);
    }

    #[test]
    fn field_specs_blank_when_azure_config_on_google_connector() {
        // Variant mismatch must not leak values across the two new connectors.
        let azure = api::ConnectionConfigView::Azure {
            base_url: Some("https://my-resource.openai.azure.com".to_string()),
            api_key_env: Some("AZURE_OPENAI_API_KEY".to_string()),
            api_surface: Some("v1".to_string()),
            auth_mode: Some("api_key".to_string()),
            api_version: None,
            connect_timeout_secs: None,
            stream_timeout_secs: None,
            max_context_tokens: None,
        };
        let specs = field_specs(ConnectorType::Google, Some(&azure));
        assert!(specs.iter().all(|f| f.initial.is_none()));
    }

    /// Look up a field's pre-fill value by name within a spec list.
    fn initial_of<'a>(specs: &'a [FieldSpec], name: &str) -> Option<&'a str> {
        specs
            .iter()
            .find(|f| f.name == name)
            .and_then(|f| f.initial.as_deref())
    }

    #[test]
    fn field_specs_prefill_from_echoed_anthropic_config() {
        let config = api::ConnectionConfigView::Anthropic {
            base_url: Some("https://proxy.example/v1".to_string()),
            api_key_env: Some("MY_ANTHROPIC_KEY".to_string()),
            connect_timeout_secs: None,
            stream_timeout_secs: None,
            max_context_tokens: None,
        };
        let specs = field_specs(ConnectorType::Anthropic, Some(&config));
        assert_eq!(
            initial_of(&specs, "base_url"),
            Some("https://proxy.example/v1")
        );
        // `api_key_env` is the env-var *name*, not the secret — it pre-fills.
        assert_eq!(initial_of(&specs, "api_key_env"), Some("MY_ANTHROPIC_KEY"));
        // No field is ever marked secret, and none carries a raw credential.
        assert!(specs.iter().all(|f| !f.secret));
    }

    #[test]
    fn field_specs_prefill_from_echoed_bedrock_config() {
        let config = api::ConnectionConfigView::Bedrock {
            aws_profile: Some("prod".to_string()),
            region: Some("eu-central-1".to_string()),
            base_url: None,
            connect_timeout_secs: None,
            stream_timeout_secs: None,
            max_context_tokens: None,
        };
        let specs = field_specs(ConnectorType::Bedrock, Some(&config));
        assert_eq!(initial_of(&specs, "aws_profile"), Some("prod"));
        assert_eq!(initial_of(&specs, "region"), Some("eu-central-1"));
        assert_eq!(initial_of(&specs, "base_url"), None);
        assert!(specs.iter().all(|f| !f.secret));
    }

    #[test]
    fn field_specs_blank_when_config_is_none() {
        // Create path / older daemon that omits `config`: every field blank.
        for connector in [
            ConnectorType::Anthropic,
            ConnectorType::OpenAi,
            ConnectorType::OpenRouter,
            ConnectorType::Azure,
            ConnectorType::Google,
            ConnectorType::Bedrock,
            ConnectorType::Ollama,
        ] {
            let specs = field_specs(connector, None);
            assert!(
                specs.iter().all(|f| f.initial.is_none()),
                "{connector:?} should have no pre-filled fields when config is None",
            );
        }
    }

    #[test]
    fn field_specs_blank_on_variant_mismatch() {
        // A config whose variant doesn't match the connector must not leak
        // values across connector types.
        let bedrock = api::ConnectionConfigView::Bedrock {
            aws_profile: Some("prod".to_string()),
            region: Some("us-east-1".to_string()),
            base_url: None,
            connect_timeout_secs: None,
            stream_timeout_secs: None,
            max_context_tokens: None,
        };
        let specs = field_specs(ConnectorType::Anthropic, Some(&bedrock));
        assert!(specs.iter().all(|f| f.initial.is_none()));
    }

    #[test]
    fn preserved_fields_round_trip_unsurfaced_values() {
        // The dialog has no inputs for timeouts / context ceiling / keep-warm,
        // so an edit must carry the daemon's stored values through unchanged.
        let ollama = api::ConnectionConfigView::Ollama {
            base_url: Some("http://localhost:11434".to_string()),
            connect_timeout_secs: Some(5),
            stream_timeout_secs: Some(120),
            keep_warm: Some(true),
            max_context_tokens: Some(8192),
        };
        let p = PreservedFields::from_config(Some(&ollama));
        assert_eq!(p.connect_timeout_secs, Some(5));
        assert_eq!(p.stream_timeout_secs, Some(120));
        assert_eq!(p.keep_warm, Some(true));
        assert_eq!(p.max_context_tokens, Some(8192));

        // Non-Ollama variants have no keep_warm; the rest still round-trip.
        let bedrock = api::ConnectionConfigView::Bedrock {
            aws_profile: None,
            region: Some("us-west-2".to_string()),
            base_url: None,
            connect_timeout_secs: Some(3),
            stream_timeout_secs: None,
            max_context_tokens: Some(200_000),
        };
        let p = PreservedFields::from_config(Some(&bedrock));
        assert_eq!(p.connect_timeout_secs, Some(3));
        assert_eq!(p.max_context_tokens, Some(200_000));
        assert_eq!(p.keep_warm, None);

        // Create path: nothing stored.
        let p = PreservedFields::from_config(None);
        assert_eq!(p.connect_timeout_secs, None);
        assert_eq!(p.max_context_tokens, None);
        assert_eq!(p.keep_warm, None);
    }
}
