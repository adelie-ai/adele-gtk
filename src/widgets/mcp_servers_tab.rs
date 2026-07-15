//! MCP-servers tab of the Settings dialog (issue #495).
//!
//! Shows the live list of the daemon's Model Context Protocol servers
//! (`ListMcpServers`) with an *honest* per-server status, an enable/disable
//! toggle, add/edit (transport-aware), remove, and - for an OAuth server that
//! needs sign-in - a Configure/Sign-in button. Like [`super::connections_tab`]
//! it is a passive view: it renders [`api::McpServerView`]s and asks the parent
//! (the Settings dialog) to perform the actual RPC work via callbacks. The
//! parent owns the `Connector` and the async bridge.
//!
//! The status -> (dot colour, label) and transport -> chip mappings are pure
//! and unit-tested here; the row-building GTK code is the thin shell over them.

use std::cell::RefCell;
use std::rc::Rc;

use desktop_assistant_api_model as api;
use gtk4::prelude::*;
use gtk4::{
    Align, Box as GtkBox, Button, Label, ListBox, ListBoxRow, Orientation, ScrolledWindow,
    SelectionMode, Separator, glib,
};

/// Map the coarse daemon status string to a `(dot CSS modifier class, human
/// label)` pair. Covers the six states the daemon reports; any unrecognized
/// future state renders as a neutral "Unknown" rather than panicking, so an
/// older client degrades honestly against a newer daemon.
///
/// The class is a `mcp-dot-*` modifier applied alongside the base `mcp-dot`
/// class (see `style.css`): `running` -> green, `needs_auth`/`auth_expired` ->
/// amber, `error` -> red, everything else -> neutral grey.
pub fn status_display(status: &str) -> (&'static str, &'static str) {
    match status {
        "running" => ("mcp-dot-running", "Running"),
        "stopped" => ("mcp-dot-neutral", "Stopped"),
        "disabled" => ("mcp-dot-neutral", "Disabled"),
        "needs_auth" => ("mcp-dot-warn", "Sign in required"),
        "auth_expired" => ("mcp-dot-warn", "Sign in expired"),
        "error" => ("mcp-dot-error", "Error"),
        _ => ("mcp-dot-neutral", "Unknown"),
    }
}

/// The transport chip label: an HTTP server is `"remote"`, anything else
/// (stdio) is `"local"`.
pub fn transport_chip(transport: &str) -> &'static str {
    if transport == "http" {
        "remote"
    } else {
        "local"
    }
}

type AddCb = Box<dyn Fn()>;
type NameCb = Box<dyn Fn(String)>;
type ToggleCb = Box<dyn Fn(String, bool)>;
type SignInCb = Box<dyn Fn(Vec<String>)>;

/// Passive MCP-servers list widget. Mirrors [`super::connections_tab`]: it owns
/// the list surface and holds a snapshot of the loaded servers; all mutations
/// are delegated to the parent via callbacks.
pub struct McpServersTab {
    pub container: GtkBox,
    list_box: ListBox,
    servers: Rc<RefCell<Vec<api::McpServerView>>>,
    on_add: Rc<RefCell<Option<AddCb>>>,
    on_edit: Rc<RefCell<Option<NameCb>>>,
    on_toggle: Rc<RefCell<Option<ToggleCb>>>,
    on_remove: Rc<RefCell<Option<NameCb>>>,
    on_signin: Rc<RefCell<Option<SignInCb>>>,
}

impl McpServersTab {
    pub fn new() -> Self {
        let container = GtkBox::new(Orientation::Vertical, 8);
        container.set_margin_start(12);
        container.set_margin_end(12);
        container.set_margin_top(12);
        container.set_margin_bottom(12);

        // Header: title + Add.
        let header = GtkBox::new(Orientation::Horizontal, 6);
        let title = Label::new(Some("MCP Servers"));
        title.add_css_class("heading");
        title.set_halign(Align::Start);
        title.set_hexpand(true);
        header.append(&title);

        let on_add: Rc<RefCell<Option<AddCb>>> = Rc::new(RefCell::new(None));
        let add_button = Button::with_label("Add");
        add_button.add_css_class("suggested-action");
        add_button.connect_clicked(glib::clone!(
            #[strong]
            on_add,
            move |_| {
                if let Some(ref cb) = *on_add.borrow() {
                    cb();
                }
            }
        ));
        header.append(&add_button);

        container.append(&header);

        let blurb = Label::new(Some(
            "Model Context Protocol servers give Adele extra tools. Add, edit, enable, or remove them here.",
        ));
        blurb.set_wrap(true);
        blurb.set_halign(Align::Start);
        blurb.add_css_class("dim-label");
        container.append(&blurb);

        container.append(&Separator::new(Orientation::Horizontal));

        let scrolled = ScrolledWindow::new();
        scrolled.set_vexpand(true);
        let list_box = ListBox::new();
        list_box.set_selection_mode(SelectionMode::None);
        list_box.add_css_class("mcp-servers-list");
        scrolled.set_child(Some(&list_box));
        container.append(&scrolled);

        Self {
            container,
            list_box,
            servers: Rc::new(RefCell::new(Vec::new())),
            on_add,
            on_edit: Rc::new(RefCell::new(None)),
            on_toggle: Rc::new(RefCell::new(None)),
            on_remove: Rc::new(RefCell::new(None)),
            on_signin: Rc::new(RefCell::new(None)),
        }
    }

    pub fn connect_add<F: Fn() + 'static>(&self, f: F) {
        *self.on_add.borrow_mut() = Some(Box::new(f));
    }

    pub fn connect_edit<F: Fn(String) + 'static>(&self, f: F) {
        *self.on_edit.borrow_mut() = Some(Box::new(f));
    }

    pub fn connect_toggle<F: Fn(String, bool) + 'static>(&self, f: F) {
        *self.on_toggle.borrow_mut() = Some(Box::new(f));
    }

    pub fn connect_remove<F: Fn(String) + 'static>(&self, f: F) {
        *self.on_remove.borrow_mut() = Some(Box::new(f));
    }

    pub fn connect_signin<F: Fn(Vec<String>) + 'static>(&self, f: F) {
        *self.on_signin.borrow_mut() = Some(Box::new(f));
    }

    /// Look up a server view by name, if currently loaded.
    pub fn find(&self, name: &str) -> Option<api::McpServerView> {
        self.servers
            .borrow()
            .iter()
            .find(|s| s.name == name)
            .cloned()
    }

    /// Replace the list contents.
    pub fn set_servers(&self, servers: &[api::McpServerView]) {
        while let Some(child) = self.list_box.first_child() {
            self.list_box.remove(&child);
        }
        *self.servers.borrow_mut() = servers.to_vec();

        if servers.is_empty() {
            let row = ListBoxRow::new();
            row.set_selectable(false);
            let placeholder =
                Label::new(Some("No MCP servers configured. Click Add to create one."));
            placeholder.add_css_class("dim-label");
            placeholder.set_margin_start(12);
            placeholder.set_margin_end(12);
            placeholder.set_margin_top(12);
            placeholder.set_margin_bottom(12);
            row.set_child(Some(&placeholder));
            self.list_box.append(&row);
            return;
        }

        for server in servers {
            self.list_box.append(&self.build_row(server));
        }
    }

    fn build_row(&self, server: &api::McpServerView) -> ListBoxRow {
        let row = ListBoxRow::new();
        row.set_selectable(false);

        let hbox = GtkBox::new(Orientation::Horizontal, 12);
        hbox.set_margin_start(10);
        hbox.set_margin_end(10);
        hbox.set_margin_top(8);
        hbox.set_margin_bottom(8);

        // Status dot - an empty label coloured entirely by CSS.
        let (dot_class, status_label) = status_display(&server.status);
        let dot = Label::new(None);
        dot.add_css_class("mcp-dot");
        dot.add_css_class(dot_class);
        dot.set_width_chars(2);
        dot.set_valign(Align::Start);
        dot.set_margin_top(4);
        hbox.append(&dot);

        // Text column: name + chip, status subtitle, and an error detail line.
        let text_col = GtkBox::new(Orientation::Vertical, 2);
        text_col.set_hexpand(true);

        let title_row = GtkBox::new(Orientation::Horizontal, 6);
        // Daemon-provided text: rendered as plain text (never markup).
        let name_label = Label::new(Some(&server.name));
        name_label.set_halign(Align::Start);
        name_label.add_css_class("heading");
        title_row.append(&name_label);
        let chip = Label::new(Some(transport_chip(&server.transport)));
        chip.add_css_class("mcp-chip");
        chip.set_valign(Align::Center);
        title_row.append(&chip);
        text_col.append(&title_row);

        // Subtitle: status label (+ tool count when running) (+ target).
        let mut subtitle = status_label.to_string();
        if server.status == "running" && server.tool_count > 0 {
            let n = server.tool_count;
            subtitle.push_str(&format!(" · {n} tool{}", if n == 1 { "" } else { "s" }));
        }
        if !server.target.is_empty() {
            subtitle.push_str(" · ");
            subtitle.push_str(&server.target);
        }
        let subtitle_label = Label::new(Some(&subtitle));
        subtitle_label.set_halign(Align::Start);
        subtitle_label.set_wrap(true);
        subtitle_label.set_xalign(0.0);
        subtitle_label.add_css_class("dim-label");
        text_col.append(&subtitle_label);

        // Last connect error (only when the daemon reports one).
        if let Some(detail) = server.detail.as_ref().filter(|d| !d.is_empty()) {
            let detail_label = Label::new(Some(detail));
            detail_label.set_halign(Align::Start);
            detail_label.set_wrap(true);
            detail_label.set_xalign(0.0);
            detail_label.add_css_class("mcp-error-label");
            text_col.append(&detail_label);
        }

        hbox.append(&text_col);

        // Sign-in / Configure - only when the daemon offers one (OAuth servers
        // that need authorization). This client runs on the daemon host, so it
        // can drive the configure command (spawns a browser there).
        if let Some(label) = server.configure_label.as_ref().filter(|l| !l.is_empty())
            && !server.configure_command.is_empty()
        {
            let signin_btn = Button::with_label(label);
            signin_btn.add_css_class("suggested-action");
            let argv = server.configure_command.clone();
            signin_btn.connect_clicked(glib::clone!(
                #[strong(rename_to = on_signin)]
                self.on_signin,
                move |_| {
                    if let Some(ref cb) = *on_signin.borrow() {
                        cb(argv.clone());
                    }
                }
            ));
            hbox.append(&signin_btn);
        }

        // Enable/Disable toggle (a button whose label reflects the target
        // action - avoids a Switch's programmatic-set notify loop).
        let toggle_btn = Button::with_label(if server.enabled { "Disable" } else { "Enable" });
        let toggle_name = server.name.clone();
        let toggle_target = !server.enabled;
        toggle_btn.connect_clicked(glib::clone!(
            #[strong(rename_to = on_toggle)]
            self.on_toggle,
            move |_| {
                if let Some(ref cb) = *on_toggle.borrow() {
                    cb(toggle_name.clone(), toggle_target);
                }
            }
        ));
        hbox.append(&toggle_btn);

        // Edit.
        let edit_btn = Button::with_label("Edit");
        let edit_name = server.name.clone();
        edit_btn.connect_clicked(glib::clone!(
            #[strong(rename_to = on_edit)]
            self.on_edit,
            move |_| {
                if let Some(ref cb) = *on_edit.borrow() {
                    cb(edit_name.clone());
                }
            }
        ));
        hbox.append(&edit_btn);

        // Remove.
        let remove_btn = Button::with_label("Remove");
        remove_btn.add_css_class("destructive-action");
        let remove_name = server.name.clone();
        remove_btn.connect_clicked(glib::clone!(
            #[strong(rename_to = on_remove)]
            self.on_remove,
            move |_| {
                if let Some(ref cb) = *on_remove.borrow() {
                    cb(remove_name.clone());
                }
            }
        ));
        hbox.append(&remove_btn);

        row.set_child(Some(&hbox));
        row
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- status_display -------------------------------------------------------

    #[test]
    fn status_display_covers_all_six_states() {
        assert_eq!(status_display("running"), ("mcp-dot-running", "Running"));
        assert_eq!(status_display("stopped"), ("mcp-dot-neutral", "Stopped"));
        assert_eq!(status_display("disabled"), ("mcp-dot-neutral", "Disabled"));
        assert_eq!(
            status_display("needs_auth"),
            ("mcp-dot-warn", "Sign in required")
        );
        assert_eq!(
            status_display("auth_expired"),
            ("mcp-dot-warn", "Sign in expired")
        );
        assert_eq!(status_display("error"), ("mcp-dot-error", "Error"));
    }

    #[test]
    fn status_display_unknown_is_neutral() {
        assert_eq!(
            status_display("teleporting"),
            ("mcp-dot-neutral", "Unknown")
        );
        assert_eq!(status_display(""), ("mcp-dot-neutral", "Unknown"));
    }

    // --- transport_chip -------------------------------------------------------

    #[test]
    fn transport_chip_http_is_remote_else_local() {
        assert_eq!(transport_chip("http"), "remote");
        assert_eq!(transport_chip("stdio"), "local");
        assert_eq!(transport_chip("something-new"), "local");
    }
}
