use std::cell::RefCell;
use std::rc::Rc;

use desktop_assistant_client_common::ConversationSummary;
use gtk4::prelude::*;
use gtk4::{
    Box as GtkBox, Button, CheckButton, GestureClick, Image, Label, ListBox, ListBoxRow,
    Orientation, ScrolledWindow, SelectionMode, glib,
};

use crate::widgets::context_menu;

/// Context-menu callback carrying the **conversation id** the menu was opened
/// on — never a row index. The gesture captures the id at paint time, so a
/// repaint that reorders or removes rows while the menu is open can't make the
/// action target the wrong conversation (GTK-7).
type IdCallback = Box<dyn Fn(&str)>;
type ToggleCallback = Box<dyn Fn(bool)>;

/// Sidebar widget displaying the conversation list and a "New" button.
pub struct Sidebar {
    pub container: GtkBox,
    pub list_box: ListBox,
    pub new_button: Button,
    on_rename: Rc<RefCell<Option<IdCallback>>>,
    on_delete: Rc<RefCell<Option<IdCallback>>>,
    on_archive: Rc<RefCell<Option<IdCallback>>>,
    on_show_archived_toggled: Rc<RefCell<Option<ToggleCallback>>>,
}

impl Sidebar {
    pub fn new() -> Self {
        let container = GtkBox::new(Orientation::Vertical, 0);
        container.set_width_request(280);

        // Adele branding icon
        let brand_box = GtkBox::new(Orientation::Horizontal, 8);
        brand_box.set_margin_start(12);
        brand_box.set_margin_top(10);
        brand_box.set_margin_bottom(4);

        const ICON_BYTES: &[u8] = include_bytes!("../../assets/adele_communicating.png");
        let icon_path =
            match crate::assets::extract_to_cache(ICON_BYTES, "adele-gtk-brand-icon.png") {
                Ok(path) => path,
                Err(e) => {
                    tracing::warn!("Failed to write brand icon: {e}");
                    dirs::cache_dir()
                        .unwrap_or_else(std::env::temp_dir)
                        .join("adele-gtk-brand-icon.png")
                }
            };
        let icon = Image::from_file(icon_path.to_str().unwrap_or_default());
        icon.set_pixel_size(44);
        brand_box.append(&icon);

        let title_label = Label::new(Some("Adele Desktop Assistant"));
        title_label.add_css_class("brand-title");
        title_label.set_valign(gtk4::Align::Center);
        brand_box.append(&title_label);

        container.append(&brand_box);

        let header = Label::new(Some("Conversations"));
        header.add_css_class("sidebar-header");
        header.set_halign(gtk4::Align::Start);
        header.set_margin_start(12);
        header.set_margin_top(8);
        header.set_margin_bottom(8);
        container.append(&header);

        let scrolled_window = ScrolledWindow::new();
        scrolled_window.set_vexpand(true);

        let list_box = ListBox::new();
        list_box.set_selection_mode(SelectionMode::Single);
        list_box.add_css_class("conversation-list");
        scrolled_window.set_child(Some(&list_box));
        container.append(&scrolled_window);

        let show_archived_check = CheckButton::with_label("Show archived");
        show_archived_check.set_margin_start(12);
        show_archived_check.set_margin_top(4);
        show_archived_check.set_margin_bottom(4);
        container.append(&show_archived_check);

        let new_button = Button::with_label("+ New Conversation");
        new_button.add_css_class("new-conversation-button");
        new_button.set_margin_start(8);
        new_button.set_margin_end(8);
        new_button.set_margin_top(8);
        new_button.set_margin_bottom(8);
        container.append(&new_button);

        let on_show_archived_toggled: Rc<RefCell<Option<ToggleCallback>>> =
            Rc::new(RefCell::new(None));

        show_archived_check.connect_toggled(glib::clone!(
            #[strong(rename_to = cb)]
            on_show_archived_toggled,
            move |check| {
                let active = check.is_active();
                if let Some(ref f) = *cb.borrow() {
                    f(active);
                }
            }
        ));

        Self {
            container,
            list_box,
            new_button,
            on_rename: Rc::new(RefCell::new(None)),
            on_delete: Rc::new(RefCell::new(None)),
            on_archive: Rc::new(RefCell::new(None)),
            on_show_archived_toggled,
        }
    }

    /// Register a callback for when the user chooses "Rename" from the context
    /// menu. The callback receives the conversation id (GTK-7), not a row index.
    pub fn connect_rename<F: Fn(&str) + 'static>(&self, f: F) {
        *self.on_rename.borrow_mut() = Some(Box::new(f));
    }

    /// Register a callback for when the user chooses "Delete" from the context
    /// menu. The callback receives the conversation id (GTK-7), not a row index.
    pub fn connect_delete<F: Fn(&str) + 'static>(&self, f: F) {
        *self.on_delete.borrow_mut() = Some(Box::new(f));
    }

    /// Register a callback for when the user chooses "Archive"/"Unarchive" from
    /// the context menu. The callback receives the conversation id (GTK-7).
    pub fn connect_archive<F: Fn(&str) + 'static>(&self, f: F) {
        *self.on_archive.borrow_mut() = Some(Box::new(f));
    }

    /// Register a callback for when the "Show archived" checkbox is toggled.
    pub fn connect_show_archived_toggled<F: Fn(bool) + 'static>(&self, f: F) {
        *self.on_show_archived_toggled.borrow_mut() = Some(Box::new(f));
    }

    /// Replace the conversation list contents.
    pub fn set_conversations(&self, conversations: &[ConversationSummary]) {
        // Remove all existing rows
        while let Some(child) = self.list_box.first_child() {
            self.list_box.remove(&child);
        }

        for conv in conversations.iter() {
            let row = ListBoxRow::new();
            let hbox = GtkBox::new(Orientation::Horizontal, 8);
            hbox.set_margin_start(12);
            hbox.set_margin_end(12);
            hbox.set_margin_top(6);
            hbox.set_margin_bottom(6);

            let title_label = Label::new(Some(&conv.title));
            title_label.set_halign(gtk4::Align::Start);
            title_label.set_hexpand(true);
            title_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            if conv.archived {
                title_label.add_css_class("dim-label");
            }
            hbox.append(&title_label);

            let count_label = Label::new(Some(&format!("({})", conv.message_count)));
            count_label.add_css_class("dim-label");
            hbox.append(&count_label);

            row.set_child(Some(&hbox));

            // Right-click context menu
            let gesture = GestureClick::new();
            gesture.set_button(3); // secondary (right) click
            let is_archived = conv.archived;
            // Capture the conversation id at paint time (GTK-7): the menu acts
            // on THIS conversation regardless of any repaint that reorders or
            // drops rows while it's open.
            let conv_id = conv.id.clone();
            gesture.connect_pressed(glib::clone!(
                #[strong(rename_to = on_rename)]
                self.on_rename,
                #[strong(rename_to = on_delete)]
                self.on_delete,
                #[strong(rename_to = on_archive)]
                self.on_archive,
                #[strong]
                conv_id,
                move |gesture, _n_press, x, y| {
                    let Some(widget) = gesture.widget() else {
                        return;
                    };

                    let archive_label = if is_archived { "Unarchive" } else { "Archive" };
                    let items = vec![
                        context_menu::MenuItem::new(
                            "Rename",
                            glib::clone!(
                                #[strong]
                                on_rename,
                                #[strong]
                                conv_id,
                                move || {
                                    if let Some(ref cb) = *on_rename.borrow() {
                                        cb(&conv_id);
                                    }
                                }
                            ),
                        ),
                        context_menu::MenuItem::new(
                            archive_label,
                            glib::clone!(
                                #[strong]
                                on_archive,
                                #[strong]
                                conv_id,
                                move || {
                                    if let Some(ref cb) = *on_archive.borrow() {
                                        cb(&conv_id);
                                    }
                                }
                            ),
                        ),
                        context_menu::MenuItem::destructive(
                            "Delete",
                            glib::clone!(
                                #[strong]
                                on_delete,
                                #[strong]
                                conv_id,
                                move || {
                                    if let Some(ref cb) = *on_delete.borrow() {
                                        cb(&conv_id);
                                    }
                                }
                            ),
                        ),
                    ];
                    context_menu::show(&widget, x, y, items);
                }
            ));
            row.add_controller(gesture);

            self.list_box.append(&row);
        }
    }

    /// Select a row by index.
    pub fn select_index(&self, index: usize) {
        if let Some(row) = self.list_box.row_at_index(index as i32) {
            self.list_box.select_row(Some(&row));
        }
    }
}
