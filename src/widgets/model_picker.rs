use std::cell::RefCell;
use std::rc::Rc;

use desktop_assistant_api_model as api;
use gtk4::prelude::*;
use gtk4::{Box as GtkBox, DropDown, Label, Orientation, StringList};

/// Header-bar widget that shows the current model and lets the user pick a
/// different one for the active conversation. The dropdown is populated from
/// `ListAvailableModels`; selection state is per-conversation and round-trips
/// through `ConversationView.model_selection`.
pub struct ModelPicker {
    pub container: GtkBox,
    dropdown: DropDown,
    /// Mirror of the dropdown's StringList contents in order. Each entry is
    /// the (connection_id, model_id) the dropdown's `selected` index resolves
    /// to. Held in an Rc<RefCell<>> so signal handlers can read it without
    /// borrowing the picker itself.
    models: Rc<RefCell<Vec<ModelEntry>>>,
}

#[derive(Debug, Clone)]
struct ModelEntry {
    connection_id: String,
    model_id: String,
}

impl ModelPicker {
    pub fn new() -> Self {
        let container = GtkBox::new(Orientation::Horizontal, 6);
        container.set_valign(gtk4::Align::Center);

        let label = Label::new(Some("Model:"));
        label.add_css_class("dim-label");
        container.append(&label);

        let dropdown = DropDown::new(None::<StringList>, None::<gtk4::Expression>);
        dropdown.set_sensitive(false);
        dropdown.set_tooltip_text(Some(
            "Pick a model for this conversation. The choice is remembered \
             until you change it again.",
        ));
        container.append(&dropdown);

        Self {
            container,
            dropdown,
            models: Rc::new(RefCell::new(Vec::new())),
        }
    }

    /// Replace the available-models list. Resets the dropdown to "no
    /// selection"; callers should follow with `set_selection(...)` once the
    /// active conversation's stored selection is known.
    pub fn set_models(&self, listings: &[api::ModelListing]) {
        let labels: Vec<String> = listings.iter().map(format_label).collect();
        let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let string_list = StringList::new(&label_refs);
        self.dropdown.set_model(Some(&string_list));

        let entries: Vec<ModelEntry> = listings
            .iter()
            .map(|l| ModelEntry {
                connection_id: l.connection_id.clone(),
                model_id: l.model.id.clone(),
            })
            .collect();
        *self.models.borrow_mut() = entries;

        self.dropdown.set_selected(gtk4::INVALID_LIST_POSITION);
        self.dropdown.set_sensitive(!listings.is_empty());
    }

    /// Highlight the dropdown row matching `selection`, or reset to "no
    /// selection" when `None` or when the selection isn't in the current
    /// model list.
    pub fn set_selection(&self, selection: Option<&api::ConversationModelSelectionView>) {
        let Some(sel) = selection else {
            self.dropdown.set_selected(gtk4::INVALID_LIST_POSITION);
            return;
        };
        let idx = self
            .models
            .borrow()
            .iter()
            .position(|m| m.connection_id == sel.connection_id && m.model_id == sel.model_id);
        match idx {
            Some(i) => self.dropdown.set_selected(i as u32),
            None => self.dropdown.set_selected(gtk4::INVALID_LIST_POSITION),
        }
    }

    /// The override to attach to the next `SendMessage`, or `None` when
    /// nothing is selected (the daemon falls back to the conversation's
    /// stored selection or the interactive purpose).
    pub fn current_override(&self) -> Option<api::SendPromptOverride> {
        let idx = self.dropdown.selected();
        if idx == gtk4::INVALID_LIST_POSITION {
            return None;
        }
        let models = self.models.borrow();
        let entry = models.get(idx as usize)?;
        Some(api::SendPromptOverride {
            connection_id: entry.connection_id.clone(),
            model_id: entry.model_id.clone(),
            effort: None,
        })
    }

    /// Hide the entire picker — used when the active transport doesn't
    /// support per-send overrides (D-Bus today).
    pub fn set_visible(&self, visible: bool) {
        self.container.set_visible(visible);
    }
}

fn format_label(listing: &api::ModelListing) -> String {
    let model_label = if listing.model.display_name.is_empty() {
        listing.model.id.as_str()
    } else {
        listing.model.display_name.as_str()
    };
    format!("{} · {}", model_label, listing.connection_label)
}
