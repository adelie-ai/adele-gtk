use std::collections::HashSet;

use desktop_assistant_api_model as api;
use gtk4::prelude::*;
use gtk4::{
    Align, Box as GtkBox, Button, CheckButton, Label, Orientation, ScrolledWindow, Separator,
    Window,
};

use crate::selected_models::SelectedModel;

/// Show a modal dialog letting the user pick which (connection, model) pairs
/// should appear in the per-conversation model dropdown. Embedding-only
/// models are filtered out — they aren't usable for chat. Selection is
/// returned to `on_save` in the order the connectors and models appeared in
/// `available`, which preserves whatever ordering the daemon chose.
pub fn show_select_models_dialog<F>(
    parent: &Window,
    available: &[api::ModelListing],
    currently_selected: &[SelectedModel],
    on_save: F,
) where
    F: Fn(Vec<SelectedModel>) + 'static,
{
    let dialog = Window::builder()
        .title("Select Models")
        .transient_for(parent)
        .modal(true)
        .default_width(440)
        .default_height(520)
        .build();

    let outer = GtkBox::new(Orientation::Vertical, 0);

    let intro = Label::new(Some(
        "Pick which models appear in the model dropdown. \
         Embedding-only models are excluded.",
    ));
    intro.set_wrap(true);
    intro.set_xalign(0.0);
    intro.set_margin_start(16);
    intro.set_margin_end(16);
    intro.set_margin_top(16);
    intro.set_margin_bottom(8);
    outer.append(&intro);

    let scrolled = ScrolledWindow::new();
    scrolled.set_vexpand(true);
    scrolled.set_hexpand(true);
    scrolled.set_margin_start(16);
    scrolled.set_margin_end(16);

    let list_box = GtkBox::new(Orientation::Vertical, 8);

    let selected_set: HashSet<(String, String)> = currently_selected
        .iter()
        .map(|m| (m.connection_id.clone(), m.model_id.clone()))
        .collect();

    // Group by connection while preserving the connector order from the
    // daemon's listing. `BTreeMap` would re-sort by id; we want the daemon's
    // order so a Bedrock connector with hundreds of models still flows in
    // the same shape it returned.
    let mut groups: Vec<(String, String, Vec<&api::ModelListing>)> = Vec::new();
    for listing in available {
        if listing.model.capabilities.embedding {
            continue;
        }
        match groups
            .iter_mut()
            .find(|(id, _, _)| id == &listing.connection_id)
        {
            Some((_, _, items)) => items.push(listing),
            None => groups.push((
                listing.connection_id.clone(),
                listing.connection_label.clone(),
                vec![listing],
            )),
        }
    }

    let checks: std::rc::Rc<std::cell::RefCell<Vec<(SelectedModel, CheckButton)>>> =
        std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));

    if groups.is_empty() {
        let empty = Label::new(Some(
            "No chat-capable models are available from the connected daemon.",
        ));
        empty.set_wrap(true);
        empty.add_css_class("dim-label");
        list_box.append(&empty);
    } else {
        for (idx, (conn_id, conn_label, models)) in groups.iter().enumerate() {
            if idx > 0 {
                list_box.append(&Separator::new(Orientation::Horizontal));
            }

            let header = Label::new(Some(conn_label));
            header.add_css_class("sidebar-header");
            header.set_halign(Align::Start);
            list_box.append(&header);

            for listing in models {
                let label_text = display_label(listing);
                let check = CheckButton::with_label(&label_text);
                let key = (conn_id.clone(), listing.model.id.clone());
                if selected_set.contains(&key) {
                    check.set_active(true);
                }
                list_box.append(&check);
                checks.borrow_mut().push((
                    SelectedModel {
                        connection_id: conn_id.clone(),
                        model_id: listing.model.id.clone(),
                    },
                    check,
                ));
            }
        }
    }

    scrolled.set_child(Some(&list_box));
    outer.append(&scrolled);

    let btn_row = GtkBox::new(Orientation::Horizontal, 8);
    btn_row.set_halign(Align::End);
    btn_row.set_margin_top(12);
    btn_row.set_margin_bottom(16);
    btn_row.set_margin_start(16);
    btn_row.set_margin_end(16);

    let cancel = Button::with_label("Cancel");
    let dialog_ref = dialog.clone();
    cancel.connect_clicked(move |_| {
        dialog_ref.close();
    });
    btn_row.append(&cancel);

    let save = Button::with_label("Save");
    save.add_css_class("suggested-action");
    let dialog_ref = dialog.clone();
    let checks_ref = std::rc::Rc::clone(&checks);
    save.connect_clicked(move |_| {
        let chosen: Vec<SelectedModel> = checks_ref
            .borrow()
            .iter()
            .filter_map(|(model, check)| check.is_active().then(|| model.clone()))
            .collect();
        dialog_ref.close();
        on_save(chosen);
    });
    btn_row.append(&save);

    outer.append(&btn_row);

    dialog.set_child(Some(&outer));
    dialog.present();
}

fn display_label(listing: &api::ModelListing) -> String {
    if listing.model.display_name.is_empty() {
        listing.model.id.clone()
    } else {
        format!("{} ({})", listing.model.display_name, listing.model.id)
    }
}
