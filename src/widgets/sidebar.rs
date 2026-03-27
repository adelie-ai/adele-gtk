use std::cell::RefCell;
use std::rc::Rc;

use desktop_assistant_client_common::ConversationSummary;
use gtk4::prelude::*;
use gtk4::{
    Box as GtkBox, Button, CheckButton, GestureClick, Image, Label, ListBox, ListBoxRow,
    Orientation, Popover, ScrolledWindow, SelectionMode,
};

type IndexCallback = Box<dyn Fn(usize)>;
type ToggleCallback = Box<dyn Fn(bool)>;

/// Sidebar widget displaying the conversation list and a "New" button.
pub struct Sidebar {
    pub container: GtkBox,
    pub list_box: ListBox,
    pub new_button: Button,
    pub show_archived_check: CheckButton,
    pub scrolled_window: ScrolledWindow,
    on_rename: Rc<RefCell<Option<IndexCallback>>>,
    on_delete: Rc<RefCell<Option<IndexCallback>>>,
    on_archive: Rc<RefCell<Option<IndexCallback>>>,
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
        let icon_path = std::env::temp_dir().join("adelie-gtk-brand-icon.png");
        if !icon_path.exists() {
            let _ = std::fs::write(&icon_path, ICON_BYTES);
        }
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

        {
            let cb = Rc::clone(&on_show_archived_toggled);
            show_archived_check.connect_toggled(move |check| {
                let active = check.is_active();
                if let Some(ref f) = *cb.borrow() {
                    f(active);
                }
            });
        }

        Self {
            container,
            list_box,
            new_button,
            show_archived_check,
            scrolled_window,
            on_rename: Rc::new(RefCell::new(None)),
            on_delete: Rc::new(RefCell::new(None)),
            on_archive: Rc::new(RefCell::new(None)),
            on_show_archived_toggled,
        }
    }

    /// Register a callback for when the user chooses "Rename" from the context menu.
    pub fn connect_rename<F: Fn(usize) + 'static>(&self, f: F) {
        *self.on_rename.borrow_mut() = Some(Box::new(f));
    }

    /// Register a callback for when the user chooses "Delete" from the context menu.
    pub fn connect_delete<F: Fn(usize) + 'static>(&self, f: F) {
        *self.on_delete.borrow_mut() = Some(Box::new(f));
    }

    /// Register a callback for when the user chooses "Archive"/"Unarchive" from the context menu.
    pub fn connect_archive<F: Fn(usize) + 'static>(&self, f: F) {
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

        for (idx, conv) in conversations.iter().enumerate() {
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
            let on_rename = Rc::clone(&self.on_rename);
            let on_delete = Rc::clone(&self.on_delete);
            let on_archive = Rc::clone(&self.on_archive);
            let is_archived = conv.archived;
            gesture.connect_pressed(move |gesture, _n_press, x, y| {
                let Some(widget) = gesture.widget() else {
                    return;
                };

                let popover = Popover::new();
                popover.add_css_class("context-popover");
                popover.set_parent(&widget);
                popover.set_pointing_to(Some(&gtk4::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
                popover.set_has_arrow(false);

                let menu_box = GtkBox::new(Orientation::Vertical, 0);

                let rename_btn = Button::with_label("Rename");
                rename_btn.add_css_class("context-button");
                let on_rename_inner = Rc::clone(&on_rename);
                let popover_ref = popover.clone();
                rename_btn.connect_clicked(move |_| {
                    popover_ref.popdown();
                    if let Some(ref cb) = *on_rename_inner.borrow() {
                        cb(idx);
                    }
                });
                menu_box.append(&rename_btn);

                let archive_label = if is_archived { "Unarchive" } else { "Archive" };
                let archive_btn = Button::with_label(archive_label);
                archive_btn.add_css_class("context-button");
                let on_archive_inner = Rc::clone(&on_archive);
                let popover_ref = popover.clone();
                archive_btn.connect_clicked(move |_| {
                    popover_ref.popdown();
                    if let Some(ref cb) = *on_archive_inner.borrow() {
                        cb(idx);
                    }
                });
                menu_box.append(&archive_btn);

                let delete_btn = Button::with_label("Delete");
                delete_btn.add_css_class("context-button");
                delete_btn.add_css_class("destructive-action");
                let on_delete_inner = Rc::clone(&on_delete);
                let popover_ref = popover.clone();
                delete_btn.connect_clicked(move |_| {
                    popover_ref.popdown();
                    if let Some(ref cb) = *on_delete_inner.borrow() {
                        cb(idx);
                    }
                });
                menu_box.append(&delete_btn);

                popover.set_child(Some(&menu_box));
                popover.popup();
            });
            row.add_controller(gesture);

            self.list_box.append(&row);
        }
    }

    /// Get the index of the currently selected row.
    pub fn selected_index(&self) -> Option<usize> {
        let row = self.list_box.selected_row()?;
        Some(row.index() as usize)
    }

    /// Select a row by index.
    pub fn select_index(&self, index: usize) {
        if let Some(row) = self.list_box.row_at_index(index as i32) {
            self.list_box.select_row(Some(&row));
        }
    }
}
