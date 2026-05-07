use std::cell::RefCell;
use std::rc::Rc;

use desktop_assistant_api_model as api;
use gtk4::prelude::*;
use gtk4::{Align, Box as GtkBox, Button, Label, MenuButton, Orientation, Popover, Separator, Window};

use crate::selected_models::{SelectedModel, SelectedModelsStore};
use crate::widgets::select_models_dialog::show_select_models_dialog;

/// Header-bar widget that shows the current model and lets the user pick a
/// different one for the active conversation. The user curates a subset of
/// available models via the "Select Models…" entry — the dropdown only
/// shows that subset, so noisy connectors (Bedrock) don't drown the picker
/// (#13). Per-conversation selection state still round-trips through the
/// active conversation's `model_selection`.
pub struct ModelPicker {
    pub container: GtkBox,
    menu_button: MenuButton,
    /// Backs the popover content; rebuilt whenever the selected-models list
    /// or the daemon-side available-models list changes.
    popover: Popover,
    /// Mirror of every model the daemon offers (minus embedding-only ones).
    /// Used to populate the Select Models dialog and to render display
    /// labels in the popover.
    available: Rc<RefCell<Vec<api::ModelListing>>>,
    /// User's curated subset, mirrored to `selected_models.json`.
    selected: Rc<RefCell<Vec<SelectedModel>>>,
    /// Currently active selection. `None` means "use the daemon default" —
    /// we leave the override field empty and let the daemon fall back to
    /// the conversation's stored selection or the interactive purpose.
    active: Rc<RefCell<Option<SelectedModel>>>,
    store: Rc<SelectedModelsStore>,
}

impl ModelPicker {
    pub fn new() -> Self {
        let container = GtkBox::new(Orientation::Horizontal, 6);
        container.set_valign(Align::Center);

        let label = Label::new(Some("Model:"));
        label.add_css_class("dim-label");
        container.append(&label);

        let menu_button = MenuButton::new();
        menu_button.set_label("(default)");
        menu_button.set_tooltip_text(Some(
            "Pick a model for this conversation. The choice is remembered \
             until you change it again.",
        ));
        menu_button.set_sensitive(false);

        let popover = Popover::new();
        popover.add_css_class("context-popover");
        menu_button.set_popover(Some(&popover));

        container.append(&menu_button);

        Self {
            container,
            menu_button,
            popover,
            available: Rc::new(RefCell::new(Vec::new())),
            selected: Rc::new(RefCell::new(Vec::new())),
            active: Rc::new(RefCell::new(None)),
            store: Rc::new(SelectedModelsStore::new()),
        }
    }

    /// Replace the daemon's available-models list. Callers should follow
    /// with `set_selection(...)` to apply the active conversation's stored
    /// selection.
    ///
    /// Embedding-only models are filtered out of both the dropdown and the
    /// Select Models dialog — they aren't useful for chat. On first launch
    /// (no persisted selection yet) we seed the user's selected list with
    /// the first non-embedding model so the dropdown isn't empty.
    pub fn set_models(&self, listings: &[api::ModelListing]) {
        let chat_capable: Vec<api::ModelListing> = listings
            .iter()
            .filter(|l| !l.model.capabilities.embedding)
            .cloned()
            .collect();
        *self.available.borrow_mut() = chat_capable;

        // Seed the user's selected list on first launch. We only persist a
        // seed when the file is missing — once the user opens the Select
        // Models dialog and saves (even with nothing checked), we respect
        // their explicit choice.
        if !self.store.is_initialized() {
            let seed = self
                .available
                .borrow()
                .first()
                .map(|l| SelectedModel {
                    connection_id: l.connection_id.clone(),
                    model_id: l.model.id.clone(),
                })
                .into_iter()
                .collect::<Vec<_>>();
            *self.selected.borrow_mut() = seed.clone();
            if let Err(e) = self.store.save(&seed) {
                tracing::warn!("Failed to seed selected_models.json: {e}");
            }
        } else {
            match self.store.load() {
                Ok(loaded) => *self.selected.borrow_mut() = loaded,
                Err(e) => {
                    tracing::warn!("Failed to load selected_models.json: {e}");
                    self.selected.borrow_mut().clear();
                }
            }
        }

        self.menu_button
            .set_sensitive(!self.available.borrow().is_empty());
        self.rebuild_popover();
        self.refresh_button_label();
    }

    /// Apply the active conversation's stored selection (or clear when the
    /// argument is `None`). Updates the visible label without rebuilding
    /// the popover; callers are expected to have already populated the
    /// available list.
    pub fn set_selection(&self, selection: Option<&api::ConversationModelSelectionView>) {
        let active = selection.map(|s| SelectedModel {
            connection_id: s.connection_id.clone(),
            model_id: s.model_id.clone(),
        });
        *self.active.borrow_mut() = active;
        self.refresh_button_label();
    }

    /// The override to attach to the next `SendMessage`, or `None` when no
    /// model is actively selected (the daemon then falls back to the
    /// conversation's stored selection or the interactive purpose).
    pub fn current_override(&self) -> Option<api::SendPromptOverride> {
        self.active.borrow().as_ref().map(|sel| api::SendPromptOverride {
            connection_id: sel.connection_id.clone(),
            model_id: sel.model_id.clone(),
            effort: None,
        })
    }

    /// Hide the entire picker — used when the active transport doesn't
    /// support per-send overrides (D-Bus today).
    pub fn set_visible(&self, visible: bool) {
        self.container.set_visible(visible);
    }

    fn refresh_button_label(&self) {
        let active = self.active.borrow();
        let label_text = match active.as_ref() {
            Some(sel) => self.label_for(sel),
            None => "(default)".to_string(),
        };
        self.menu_button.set_label(&label_text);
    }

    fn label_for(&self, sel: &SelectedModel) -> String {
        label_for(sel, &self.available)
    }

    fn rebuild_popover(&self) {
        rebuild_popover_into(
            &self.popover,
            &self.menu_button,
            &self.available,
            &self.selected,
            &self.active,
            &self.store,
        );
    }
}

/// Rebuild the popover's children. Stand-alone (rather than `&self`) so it
/// can be invoked from the Select Models save callback without juggling a
/// borrow of the outer `ModelPicker`.
fn rebuild_popover_into(
    popover: &Popover,
    menu_button: &MenuButton,
    available: &Rc<RefCell<Vec<api::ModelListing>>>,
    selected: &Rc<RefCell<Vec<SelectedModel>>>,
    active: &Rc<RefCell<Option<SelectedModel>>>,
    store: &Rc<SelectedModelsStore>,
) {
    let menu_box = GtkBox::new(Orientation::Vertical, 0);

    let select_btn = Button::with_label("Select Models…");
    select_btn.add_css_class("context-button");
    select_btn.set_halign(Align::Fill);
    let popover_ref = popover.clone();
    let popover_to_rebuild = popover.clone();
    let menu_button_ref = menu_button.clone();
    let available_ref = Rc::clone(available);
    let selected_ref = Rc::clone(selected);
    let active_ref = Rc::clone(active);
    let store_ref = Rc::clone(store);
    select_btn.connect_clicked(move |btn| {
        popover_ref.popdown();
        let Some(parent) = btn.root().and_then(|r| r.downcast::<Window>().ok()) else {
            return;
        };
        let available_snapshot = available_ref.borrow().clone();
        let currently_selected = selected_ref.borrow().clone();
        let selected_for_save = Rc::clone(&selected_ref);
        let active_for_save = Rc::clone(&active_ref);
        let store_for_save = Rc::clone(&store_ref);
        let popover_for_save = popover_to_rebuild.clone();
        let menu_button_for_save = menu_button_ref.clone();
        let available_for_save = Rc::clone(&available_ref);
        show_select_models_dialog(
            &parent,
            &available_snapshot,
            &currently_selected,
            move |chosen| {
                if let Err(e) = store_for_save.save(&chosen) {
                    tracing::warn!("Failed to save selected_models.json: {e}");
                }
                // Drop the active selection if it's no longer in the
                // user's curated list — keeping it would mean the
                // button reflects a row that isn't shown in the popover.
                // The conversation's stored selection on the daemon is
                // untouched: we just stop overriding it from the client.
                {
                    let mut active = active_for_save.borrow_mut();
                    if let Some(sel) = active.as_ref()
                        && !chosen.iter().any(|c| c == sel)
                    {
                        *active = None;
                    }
                }
                *selected_for_save.borrow_mut() = chosen;
                rebuild_popover_into(
                    &popover_for_save,
                    &menu_button_for_save,
                    &available_for_save,
                    &selected_for_save,
                    &active_for_save,
                    &store_for_save,
                );
                refresh_menu_button_label(
                    &menu_button_for_save,
                    &available_for_save,
                    &active_for_save,
                );
            },
        );
    });
    menu_box.append(&select_btn);

    menu_box.append(&Separator::new(Orientation::Horizontal));

    let selected_list = selected.borrow();
    if selected_list.is_empty() {
        let empty = Label::new(Some("No models selected"));
        empty.add_css_class("dim-label");
        empty.set_halign(Align::Start);
        empty.set_margin_start(8);
        empty.set_margin_end(8);
        empty.set_margin_top(6);
        empty.set_margin_bottom(6);
        menu_box.append(&empty);
    } else {
        for sel in selected_list.iter() {
            let label_text = label_for(sel, available);
            let btn = Button::with_label(&label_text);
            btn.add_css_class("context-button");
            btn.set_halign(Align::Fill);

            let active_ref = Rc::clone(active);
            let popover_ref = popover.clone();
            let menu_button_ref = menu_button.clone();
            let available_ref = Rc::clone(available);
            let sel_owned = sel.clone();
            btn.connect_clicked(move |_| {
                *active_ref.borrow_mut() = Some(sel_owned.clone());
                refresh_menu_button_label(&menu_button_ref, &available_ref, &active_ref);
                popover_ref.popdown();
            });
            menu_box.append(&btn);
        }
    }

    popover.set_child(Some(&menu_box));
}

fn refresh_menu_button_label(
    menu_button: &MenuButton,
    available: &Rc<RefCell<Vec<api::ModelListing>>>,
    active: &Rc<RefCell<Option<SelectedModel>>>,
) {
    let text = match active.borrow().as_ref() {
        Some(sel) => label_for(sel, available),
        None => "(default)".to_string(),
    };
    menu_button.set_label(&text);
}

/// Render the popover row label for a selected model. Falls back to a raw
/// `model_id · connection_id` when the daemon no longer enumerates this
/// pair (e.g. the connection was removed) — better than hiding the row,
/// since the daemon may still resolve it.
fn label_for(sel: &SelectedModel, available: &Rc<RefCell<Vec<api::ModelListing>>>) -> String {
    let available = available.borrow();
    match available
        .iter()
        .find(|l| l.connection_id == sel.connection_id && l.model.id == sel.model_id)
    {
        Some(listing) => format_label(listing),
        None => format!("{} · {}", sel.model_id, sel.connection_id),
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
