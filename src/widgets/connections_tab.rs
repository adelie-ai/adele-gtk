//! Connections tab of the Settings dialog.
//!
//! Shows the live list of configured connections (`ListConnections`) with
//! Add / Configure / Remove. The tab is a passive view — it asks the
//! parent to perform the actual RPC work via callbacks. The parent (the
//! Settings dialog) owns the `TransportClient` and the async bridge.

use std::cell::RefCell;
use std::rc::Rc;

use desktop_assistant_api_model as api;
use gtk4::prelude::*;
use gtk4::{
    Align, Box as GtkBox, Button, Label, ListBox, ListBoxRow, MenuButton, Orientation, Popover,
    ScrolledWindow, SelectionMode, Separator, glib,
};

use super::connection_config_dialog::ConnectorType;

type AddConnectionCb = Box<dyn Fn(ConnectorType)>;
type ConnectionActionCb = Box<dyn Fn(String)>;

pub struct ConnectionsTab {
    pub container: GtkBox,
    list_box: ListBox,
    connections: Rc<RefCell<Vec<api::ConnectionView>>>,
    on_add: Rc<RefCell<Option<AddConnectionCb>>>,
    on_configure: Rc<RefCell<Option<ConnectionActionCb>>>,
    on_remove: Rc<RefCell<Option<ConnectionActionCb>>>,
}

impl ConnectionsTab {
    pub fn new() -> Self {
        let container = GtkBox::new(Orientation::Vertical, 8);
        container.set_margin_start(12);
        container.set_margin_end(12);
        container.set_margin_top(12);
        container.set_margin_bottom(12);

        // Header row: title + Add menu.
        let header = GtkBox::new(Orientation::Horizontal, 6);
        let title = Label::new(Some("Connections"));
        title.add_css_class("heading");
        title.set_halign(Align::Start);
        title.set_hexpand(true);
        header.append(&title);

        let add_button = MenuButton::new();
        add_button.set_label("Add");
        add_button.add_css_class("suggested-action");

        let popover = Popover::new();
        popover.add_css_class("context-popover");
        let popover_box = GtkBox::new(Orientation::Vertical, 0);

        let on_add: Rc<RefCell<Option<AddConnectionCb>>> = Rc::new(RefCell::new(None));
        for connector in [
            ConnectorType::Anthropic,
            ConnectorType::OpenAi,
            ConnectorType::OpenRouter,
            ConnectorType::Azure,
            ConnectorType::Google,
            ConnectorType::Bedrock,
            ConnectorType::Ollama,
        ] {
            let btn = Button::with_label(connector.label());
            btn.add_css_class("context-button");
            btn.set_halign(Align::Fill);
            btn.connect_clicked(glib::clone!(
                #[strong(rename_to = on_add_inner)]
                on_add,
                #[weak(rename_to = popover_ref)]
                popover,
                move |_| {
                    popover_ref.popdown();
                    if let Some(ref cb) = *on_add_inner.borrow() {
                        cb(connector);
                    }
                }
            ));
            popover_box.append(&btn);
        }
        popover.set_child(Some(&popover_box));
        add_button.set_popover(Some(&popover));
        header.append(&add_button);

        container.append(&header);
        container.append(&Separator::new(Orientation::Horizontal));

        let scrolled = ScrolledWindow::new();
        scrolled.set_vexpand(true);

        let list_box = ListBox::new();
        list_box.set_selection_mode(SelectionMode::None);
        list_box.add_css_class("connections-list");
        scrolled.set_child(Some(&list_box));
        container.append(&scrolled);

        Self {
            container,
            list_box,
            connections: Rc::new(RefCell::new(Vec::new())),
            on_add,
            on_configure: Rc::new(RefCell::new(None)),
            on_remove: Rc::new(RefCell::new(None)),
        }
    }

    pub fn connect_add<F>(&self, f: F)
    where
        F: Fn(ConnectorType) + 'static,
    {
        *self.on_add.borrow_mut() = Some(Box::new(f));
    }

    pub fn connect_configure<F>(&self, f: F)
    where
        F: Fn(String) + 'static,
    {
        *self.on_configure.borrow_mut() = Some(Box::new(f));
    }

    pub fn connect_remove<F>(&self, f: F)
    where
        F: Fn(String) + 'static,
    {
        *self.on_remove.borrow_mut() = Some(Box::new(f));
    }

    /// Fetch the live view for a given id, if currently loaded.
    pub fn find(&self, id: &str) -> Option<api::ConnectionView> {
        self.connections
            .borrow()
            .iter()
            .find(|c| c.id == id)
            .cloned()
    }

    /// Replace the list contents.
    pub fn set_connections(&self, connections: &[api::ConnectionView]) {
        while let Some(child) = self.list_box.first_child() {
            self.list_box.remove(&child);
        }
        *self.connections.borrow_mut() = connections.to_vec();

        if connections.is_empty() {
            let row = ListBoxRow::new();
            row.set_selectable(false);
            let placeholder =
                Label::new(Some("No connections configured. Click Add to create one."));
            placeholder.add_css_class("dim-label");
            placeholder.set_margin_start(12);
            placeholder.set_margin_end(12);
            placeholder.set_margin_top(12);
            placeholder.set_margin_bottom(12);
            row.set_child(Some(&placeholder));
            self.list_box.append(&row);
            return;
        }

        for conn in connections {
            let row = self.build_row(conn);
            self.list_box.append(&row);
        }
    }

    fn build_row(&self, conn: &api::ConnectionView) -> ListBoxRow {
        let row = ListBoxRow::new();
        row.set_selectable(false);

        let hbox = GtkBox::new(Orientation::Horizontal, 12);
        hbox.set_margin_start(10);
        hbox.set_margin_end(10);
        hbox.set_margin_top(8);
        hbox.set_margin_bottom(8);

        let text_col = GtkBox::new(Orientation::Vertical, 2);
        text_col.set_hexpand(true);

        let title = Label::new(Some(&format!("{}  ({})", conn.id, conn.connector_type)));
        title.set_halign(Align::Start);
        title.add_css_class("heading");
        text_col.append(&title);

        let availability_text = match &conn.availability {
            api::ConnectionAvailability::Ok => "Available".to_string(),
            api::ConnectionAvailability::Unavailable { reason } => {
                format!("Unavailable: {reason}")
            }
        };
        let creds_text = if conn.has_credentials {
            "credentials: present"
        } else {
            "credentials: missing"
        };
        let subtitle = Label::new(Some(&format!("{availability_text} · {creds_text}")));
        subtitle.set_halign(Align::Start);
        subtitle.add_css_class("dim-label");
        text_col.append(&subtitle);

        hbox.append(&text_col);

        let configure_btn = Button::with_label("Configure");
        let id_for_configure = conn.id.clone();
        configure_btn.connect_clicked(glib::clone!(
            #[strong(rename_to = on_configure)]
            self.on_configure,
            move |_| {
                if let Some(ref cb) = *on_configure.borrow() {
                    cb(id_for_configure.clone());
                }
            }
        ));
        hbox.append(&configure_btn);

        let remove_btn = Button::with_label("Remove");
        remove_btn.add_css_class("destructive-action");
        let id_for_remove = conn.id.clone();
        remove_btn.connect_clicked(glib::clone!(
            #[strong(rename_to = on_remove)]
            self.on_remove,
            move |_| {
                if let Some(ref cb) = *on_remove.borrow() {
                    cb(id_for_remove.clone());
                }
            }
        ));
        hbox.append(&remove_btn);

        row.set_child(Some(&hbox));
        row
    }
}
