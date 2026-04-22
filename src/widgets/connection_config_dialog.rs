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
use gtk4::{
    Align, Box as GtkBox, Button, Entry, Label, Orientation, Separator, Window,
};

/// Which connector type this dialog is configuring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectorType {
    Anthropic,
    OpenAi,
    Bedrock,
    Ollama,
}

impl ConnectorType {
    pub fn label(self) -> &'static str {
        match self {
            Self::Anthropic => "Anthropic",
            Self::OpenAi => "OpenAI",
            Self::Bedrock => "Bedrock",
            Self::Ollama => "Ollama",
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
            Self::Bedrock => "bedrock",
            Self::Ollama => "ollama",
        }
    }

    pub fn from_slug(s: &str) -> Option<Self> {
        match s {
            "anthropic" => Some(Self::Anthropic),
            "openai" => Some(Self::OpenAi),
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
            },
            Self::OpenAi => api::ConnectionConfigView::OpenAi {
                base_url: None,
                api_key_env: None,
            },
            Self::Bedrock => api::ConnectionConfigView::Bedrock {
                aws_profile: None,
                region: None,
                base_url: None,
            },
            Self::Ollama => api::ConnectionConfigView::Ollama { base_url: None },
        }
    }
}

/// Sanitize a text entry into `Option<String>`, trimming and treating the
/// empty string as `None`.
fn text_opt(entry: &Entry) -> Option<String> {
    let t = entry.text().trim().to_string();
    if t.is_empty() { None } else { Some(t) }
}

/// Show the Configure dialog. `existing_id` distinguishes edit (Some) from
/// create (None). `on_save` is called with the final `(id, config)` pair
/// when the user clicks Save; the dialog closes on its own.
///
/// `on_refresh_models` is invoked for Bedrock's "Refresh models" button;
/// only passed for Bedrock — it's a best-effort affordance and the dialog
/// doesn't display the returned list.
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

    let existing_config = existing.as_ref().map(|(_, c)| c.clone());
    match connector {
        ConnectorType::Anthropic => {
            let (base_url, api_key_env) = match &existing_config {
                Some(api::ConnectionConfigView::Anthropic {
                    base_url,
                    api_key_env,
                }) => (base_url.clone(), api_key_env.clone()),
                _ => (None, None),
            };
            add_field(
                "Base URL (optional override)",
                "base_url",
                Some("https://api.anthropic.com"),
                base_url.as_deref(),
                false,
            );
            add_field(
                "API key env var (e.g. ANTHROPIC_API_KEY)",
                "api_key_env",
                Some("ANTHROPIC_API_KEY"),
                api_key_env.as_deref(),
                false,
            );
            let hint = Label::new(Some(
                "The daemon reads the API key from the named env var. Set it in your daemon environment (systemd unit, shell, etc.).",
            ));
            hint.set_halign(Align::Start);
            hint.set_wrap(true);
            hint.add_css_class("dim-label");
            content.append(&hint);
        }
        ConnectorType::OpenAi => {
            let (base_url, api_key_env) = match &existing_config {
                Some(api::ConnectionConfigView::OpenAi {
                    base_url,
                    api_key_env,
                }) => (base_url.clone(), api_key_env.clone()),
                _ => (None, None),
            };
            add_field(
                "Base URL (for OpenAI-compatible providers)",
                "base_url",
                Some("https://api.openai.com/v1"),
                base_url.as_deref(),
                false,
            );
            add_field(
                "API key env var (e.g. OPENAI_API_KEY)",
                "api_key_env",
                Some("OPENAI_API_KEY"),
                api_key_env.as_deref(),
                false,
            );
            let hint = Label::new(Some(
                "The daemon reads the API key from the named env var. Set it in your daemon environment.",
            ));
            hint.set_halign(Align::Start);
            hint.set_wrap(true);
            hint.add_css_class("dim-label");
            content.append(&hint);
        }
        ConnectorType::Bedrock => {
            let (aws_profile, region, base_url) = match &existing_config {
                Some(api::ConnectionConfigView::Bedrock {
                    aws_profile,
                    region,
                    base_url,
                }) => (aws_profile.clone(), region.clone(), base_url.clone()),
                _ => (None, None, None),
            };
            add_field(
                "AWS profile (optional)",
                "aws_profile",
                Some("default"),
                aws_profile.as_deref(),
                false,
            );
            add_field(
                "Region",
                "region",
                Some("us-west-2"),
                region.as_deref(),
                false,
            );
            add_field(
                "Base URL override (optional)",
                "base_url",
                None,
                base_url.as_deref(),
                false,
            );
        }
        ConnectorType::Ollama => {
            let base_url = match &existing_config {
                Some(api::ConnectionConfigView::Ollama { base_url }) => base_url.clone(),
                _ => None,
            };
            add_field(
                "Base URL",
                "base_url",
                Some("http://localhost:11434"),
                base_url.as_deref(),
                false,
            );
        }
    }

    // Bedrock-only: "Refresh models" button. Only enabled when editing
    // (we need a valid id) and the handler is supplied.
    if connector == ConnectorType::Bedrock {
        content.append(&Separator::new(Orientation::Horizontal));
        let btn_row = GtkBox::new(Orientation::Horizontal, 8);
        let refresh_btn = Button::with_label("Refresh models");
        refresh_btn.set_tooltip_text(Some(
            "Re-query Bedrock's ListFoundationModels and update the cached model list.",
        ));
        let note = Label::new(Some("(Saves first, then refreshes.)"));
        note.add_css_class("dim-label");
        btn_row.append(&refresh_btn);
        btn_row.append(&note);
        content.append(&btn_row);

        let id_entry_ref = id_entry.clone();
        let refresh_cb = Rc::new(on_refresh_models);
        refresh_btn.connect_clicked(move |_| {
            let id = id_entry_ref.text().trim().to_string();
            if id.is_empty() {
                return;
            }
            refresh_cb(id);
        });
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

    let dialog_ref = dialog.clone();
    cancel_btn.connect_clicked(move |_| dialog_ref.close());

    let save_cb = Rc::new(on_save);
    let dialog_ref = dialog.clone();
    let fields_for_save = Rc::clone(&fields);
    save_btn.connect_clicked(move |_| {
        let id = id_entry.text().trim().to_string();
        if id.is_empty() {
            status.set_text("Connection id is required");
            return;
        }
        if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
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
            },
            ConnectorType::OpenAi => api::ConnectionConfigView::OpenAi {
                base_url: by_name("base_url"),
                api_key_env: by_name("api_key_env"),
            },
            ConnectorType::Bedrock => api::ConnectionConfigView::Bedrock {
                aws_profile: by_name("aws_profile"),
                region: by_name("region"),
                base_url: by_name("base_url"),
            },
            ConnectorType::Ollama => api::ConnectionConfigView::Ollama {
                base_url: by_name("base_url"),
            },
        };

        save_cb(id, config);
        dialog_ref.close();
    });

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
            ConnectorType::Bedrock,
            ConnectorType::Ollama,
        ] {
            assert_eq!(ConnectorType::from_slug(c.slug()), Some(c));
        }
        assert_eq!(ConnectorType::from_slug("unknown"), None);
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
    }
}
