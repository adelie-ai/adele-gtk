//! MCP-servers tab of the Settings dialog (issue #495; merged runner panel #122).
//!
//! Shows one list of the Model Context Protocol servers the user can administer,
//! merging three populations: the **daemon** fleet (`ListMcpServers`), this
//! **client**'s locally-hosted servers (`client-mcp.toml`), and the client's
//! compiled-in **built-in** servers hosted in-process (da#538). Each row carries a
//! runner chip ("daemon"/"client"), a kind/transport chip ("stdio"/"http"/
//! "built-in"), an *honest* per-server status, an enable/disable toggle, add/edit,
//! and remove. A daemon OAuth server that needs sign-in also gets a Configure/
//! Sign-in button; client servers have no daemon-driven OAuth, so they never show
//! one. Built-in rows are informational only: they are hosted inside this client
//! and present in neither config list, so they render read-only (no toggle/edit/
//! remove) and, when an external server of the same name overrides one, disabled
//! with an "overridden by ..." reason. A filter dropdown (All / Daemon / Client)
//! re-projects the same data without a re-fetch; built-ins ride the Client filter.
//!
//! Like [`super::connections_tab`] it is a passive view: it renders the merged
//! rows and asks the parent (the Settings dialog) to perform the actual work via
//! callbacks tagged with the [`Runner`] so the parent can fork to the daemon RPC
//! path or the local `client-mcp.toml`. The parent owns the `Connector`, the
//! async bridge, and the on-disk client config.
//!
//! The merge/sort/filter/label logic is the shared, unit-tested view-model in
//! `client-ui-common` ([`server_rows_with_builtins`], [`filter_rows`],
//! [`runner_label`], [`kind_label`]); the status -> (dot colour, label) mapping
//! and the built-in row's display decision ([`builtin_row_display`]) are the small
//! pure helpers below. The row-building GTK code is the thin shell over them.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use client_ui_common::{
    BuiltinServerDto, ClientServerDto, Runner, RunnerFilter, ServerKind, ServerRow, filter_rows,
    kind_label, runner_label, server_rows_with_builtins, transport_chip,
};
use desktop_assistant_api_model as api;
use gtk4::prelude::*;
use gtk4::{
    Align, Box as GtkBox, Button, DropDown, Label, ListBox, ListBoxRow, Orientation,
    ScrolledWindow, SelectionMode, Separator, StringList, glib,
};

/// Map the coarse status string to a `(dot CSS modifier class, human label)`
/// pair. Covers the daemon's six states plus the two the client surface reports
/// (`enabled`/`disabled`); any unrecognized future state renders as a neutral
/// "Unknown" rather than panicking, so an older client degrades honestly against
/// a newer daemon.
///
/// The class is a `mcp-dot-*` modifier applied alongside the base `mcp-dot`
/// class (see `style.css`): `running`/`enabled` -> green, `needs_auth`/
/// `auth_expired` -> amber, `error` -> red, everything else -> neutral grey.
pub fn status_display(status: &str) -> (&'static str, &'static str) {
    match status {
        "running" => ("mcp-dot-running", "Running"),
        // Client-surface state: the definition is on and gtk hosts it. Shown green
        // to distinguish it from disabled; the row also carries the host's live
        // per-server tool count (adele-gtk#125).
        "enabled" => ("mcp-dot-running", "Enabled"),
        "stopped" => ("mcp-dot-neutral", "Stopped"),
        "disabled" => ("mcp-dot-neutral", "Disabled"),
        "needs_auth" => ("mcp-dot-warn", "Sign in required"),
        "auth_expired" => ("mcp-dot-warn", "Sign in expired"),
        "error" => ("mcp-dot-error", "Error"),
        _ => ("mcp-dot-neutral", "Unknown"),
    }
}

/// Map the filter DropDown index (0 = All, 1 = Daemon, 2 = Client) to a
/// [`RunnerFilter`].
fn filter_from_index(i: u32) -> RunnerFilter {
    match i {
        1 => RunnerFilter::Daemon,
        2 => RunnerFilter::Client,
        _ => RunnerFilter::All,
    }
}

type AddCb = Box<dyn Fn()>;
/// `(name, runner)` - the runner disambiguates a name shared across both sides.
type NameCb = Box<dyn Fn(String, Runner)>;
/// `(name, runner, target_enabled)`.
type ToggleCb = Box<dyn Fn(String, Runner, bool)>;
type SignInCb = Box<dyn Fn(Vec<String>)>;

/// Passive MCP-servers list widget. Owns the list surface + the filter and holds
/// a snapshot of the merged rows (plus the raw daemon views for the per-row
/// extras a [`ServerRow`] does not carry); all mutations are delegated to the
/// parent via callbacks.
pub struct McpServersTab {
    pub container: GtkBox,
    /// Rebuilds the visible list from the current data + filter. Shares `Rc`
    /// clones of the state cells below, so mutating them then calling this
    /// re-renders. Used by [`Self::set_data`] and the filter dropdown handler.
    render: Rc<dyn Fn()>,
    /// Daemon views, kept for edit prefill ([`Self::find`]) and the per-row extras
    /// (enabled flag, target, sign-in argv) a [`ServerRow`] omits.
    daemon: Rc<RefCell<Vec<api::McpServerView>>>,
    /// Merged, ordered rows for the current data - re-filtered on filter change
    /// without a re-fetch.
    rows: Rc<RefCell<Vec<ServerRow>>>,
    /// Whether the daemon link is remote, and its host, for the runner chip.
    is_remote: Rc<Cell<bool>>,
    host: Rc<RefCell<Option<String>>>,
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
            "Model Context Protocol servers give Adele extra tools. They run on the daemon (the shared fleet) or on this client (local tools on this machine). Add, edit, enable, or remove them here.",
        ));
        blurb.set_wrap(true);
        blurb.set_halign(Align::Start);
        blurb.add_css_class("dim-label");
        container.append(&blurb);

        // Filter row: "Show:" + a runner filter dropdown.
        let filter_row = GtkBox::new(Orientation::Horizontal, 6);
        let filter_caption = Label::new(Some("Show"));
        filter_caption.add_css_class("dim-label");
        filter_row.append(&filter_caption);
        let filter_list = StringList::new(&["All", "Daemon", "This client"]);
        let filter_dd = DropDown::new(Some(filter_list), gtk4::Expression::NONE);
        filter_dd.set_selected(0);
        filter_row.append(&filter_dd);
        container.append(&filter_row);

        container.append(&Separator::new(Orientation::Horizontal));

        let scrolled = ScrolledWindow::new();
        scrolled.set_vexpand(true);
        let list_box = ListBox::new();
        list_box.set_selection_mode(SelectionMode::None);
        list_box.add_css_class("mcp-servers-list");
        scrolled.set_child(Some(&list_box));
        container.append(&scrolled);

        // Shared state.
        let daemon: Rc<RefCell<Vec<api::McpServerView>>> = Rc::new(RefCell::new(Vec::new()));
        let rows: Rc<RefCell<Vec<ServerRow>>> = Rc::new(RefCell::new(Vec::new()));
        let filter: Rc<Cell<RunnerFilter>> = Rc::new(Cell::new(RunnerFilter::default()));
        let is_remote: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let host: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let on_edit: Rc<RefCell<Option<NameCb>>> = Rc::new(RefCell::new(None));
        let on_toggle: Rc<RefCell<Option<ToggleCb>>> = Rc::new(RefCell::new(None));
        let on_remove: Rc<RefCell<Option<NameCb>>> = Rc::new(RefCell::new(None));
        let on_signin: Rc<RefCell<Option<SignInCb>>> = Rc::new(RefCell::new(None));

        // The render closure: rebuild the visible list from `rows` filtered by
        // `filter`, looking up each daemon row's extras from `daemon`.
        let render: Rc<dyn Fn()> = {
            let list_box = list_box.clone();
            let rows = Rc::clone(&rows);
            let filter = Rc::clone(&filter);
            let daemon = Rc::clone(&daemon);
            let is_remote = Rc::clone(&is_remote);
            let host = Rc::clone(&host);
            let on_edit = Rc::clone(&on_edit);
            let on_toggle = Rc::clone(&on_toggle);
            let on_remove = Rc::clone(&on_remove);
            let on_signin = Rc::clone(&on_signin);
            Rc::new(move || {
                while let Some(child) = list_box.first_child() {
                    list_box.remove(&child);
                }
                let visible = filter_rows(&rows.borrow(), filter.get());
                if visible.is_empty() {
                    let row = ListBoxRow::new();
                    row.set_selectable(false);
                    let placeholder =
                        Label::new(Some("No MCP servers to show. Click Add to create one."));
                    placeholder.add_css_class("dim-label");
                    placeholder.set_margin_start(12);
                    placeholder.set_margin_end(12);
                    placeholder.set_margin_top(12);
                    placeholder.set_margin_bottom(12);
                    row.set_child(Some(&placeholder));
                    list_box.append(&row);
                    return;
                }
                let daemon = daemon.borrow();
                let is_remote = is_remote.get();
                let host = host.borrow();
                for row in &visible {
                    list_box.append(&build_row_widget(
                        row,
                        &daemon,
                        is_remote,
                        host.as_deref(),
                        &on_edit,
                        &on_toggle,
                        &on_remove,
                        &on_signin,
                    ));
                }
            })
        };

        // Re-render on filter change.
        filter_dd.connect_selected_notify(glib::clone!(
            #[strong]
            filter,
            #[strong]
            render,
            move |dd| {
                filter.set(filter_from_index(dd.selected()));
                render();
            }
        ));

        // Initial (empty) render so the placeholder shows before the first fetch.
        render();

        Self {
            container,
            render,
            daemon,
            rows,
            is_remote,
            host,
            on_add,
            on_edit,
            on_toggle,
            on_remove,
            on_signin,
        }
    }

    pub fn connect_add<F: Fn() + 'static>(&self, f: F) {
        *self.on_add.borrow_mut() = Some(Box::new(f));
    }

    pub fn connect_edit<F: Fn(String, Runner) + 'static>(&self, f: F) {
        *self.on_edit.borrow_mut() = Some(Box::new(f));
    }

    pub fn connect_toggle<F: Fn(String, Runner, bool) + 'static>(&self, f: F) {
        *self.on_toggle.borrow_mut() = Some(Box::new(f));
    }

    pub fn connect_remove<F: Fn(String, Runner) + 'static>(&self, f: F) {
        *self.on_remove.borrow_mut() = Some(Box::new(f));
    }

    pub fn connect_signin<F: Fn(Vec<String>) + 'static>(&self, f: F) {
        *self.on_signin.borrow_mut() = Some(Box::new(f));
    }

    /// Look up a *daemon* server view by name, if currently loaded. Used by the
    /// parent to pre-fill the editor for a daemon row (client rows are edited
    /// from the on-disk config the parent caches).
    pub fn find(&self, name: &str) -> Option<api::McpServerView> {
        self.daemon
            .borrow()
            .iter()
            .find(|s| s.name == name)
            .cloned()
    }

    /// The currently-loaded *daemon* server names. The create flow checks a typed
    /// daemon name against these to refuse a silent overwrite (see
    /// [`super::mcp_server_dialog::is_duplicate_new_name`]).
    pub fn daemon_names(&self) -> Vec<String> {
        self.daemon
            .borrow()
            .iter()
            .map(|s| s.name.clone())
            .collect()
    }

    /// Replace the list contents with the merged daemon + client + built-in
    /// populations.
    ///
    /// `daemon` are the daemon fleet's views; `client` the local client-hosted
    /// rows; `builtins` the client's compiled-in in-process servers (da#538);
    /// `is_remote`/`host` describe the client's link to the daemon (for the runner
    /// chip). The rows are merged + sorted by the shared view-model and re-rendered
    /// through the active filter. Built-in rows are tagged [`Runner::Client`] and
    /// [`ServerKind::BuiltIn`], so they ride the Client filter and render read-only.
    pub fn set_data(
        &self,
        daemon: &[api::McpServerView],
        client: &[ClientServerDto],
        builtins: &[BuiltinServerDto],
        is_remote: bool,
        host: Option<&str>,
    ) {
        *self.daemon.borrow_mut() = daemon.to_vec();
        // The render closure owns Rc clones of these same cells; mutate them then
        // render so set_data and the filter handler stay in lockstep.
        *self.rows.borrow_mut() = server_rows_with_builtins(daemon, client, builtins);
        self.is_remote.set(is_remote);
        *self.host.borrow_mut() = host.map(str::to_string);
        (self.render)();
    }
}

/// Build one server row widget from a [`ServerRow`], looking up daemon-only
/// extras (enabled flag, target, sign-in argv) from `daemon` when the row is
/// daemon-run. Client rows never show a sign-in button.
#[allow(clippy::too_many_arguments)]
fn build_row_widget(
    row: &ServerRow,
    daemon: &[api::McpServerView],
    is_remote: bool,
    host: Option<&str>,
    on_edit: &Rc<RefCell<Option<NameCb>>>,
    on_toggle: &Rc<RefCell<Option<ToggleCb>>>,
    on_remove: &Rc<RefCell<Option<NameCb>>>,
    on_signin: &Rc<RefCell<Option<SignInCb>>>,
) -> ListBoxRow {
    // Built-in (in-process) rows are informational: hosted inside this client and
    // present in neither the daemon fleet nor client-mcp.toml, so they render
    // read-only (no toggle/edit/remove, which index into those config lists) with
    // a "built-in" kind chip, and disabled when overridden (da#538 Phase D).
    if row.kind == ServerKind::BuiltIn {
        return build_builtin_row_widget(row);
    }

    // Daemon rows carry extra fields on their view; client rows don't.
    let view = if row.runner == Runner::Daemon {
        daemon.iter().find(|s| s.name == row.name)
    } else {
        None
    };

    let list_row = ListBoxRow::new();
    list_row.set_selectable(false);

    let hbox = GtkBox::new(Orientation::Horizontal, 12);
    hbox.set_margin_start(10);
    hbox.set_margin_end(10);
    hbox.set_margin_top(8);
    hbox.set_margin_bottom(8);

    // Status dot - an empty label coloured entirely by CSS.
    let (dot_class, status_label) = status_display(&row.status);
    let dot = Label::new(None);
    dot.add_css_class("mcp-dot");
    dot.add_css_class(dot_class);
    dot.set_width_chars(2);
    dot.set_valign(Align::Start);
    dot.set_margin_top(4);
    hbox.append(&dot);

    // Text column: name + runner/transport chips, status subtitle, error detail.
    let text_col = GtkBox::new(Orientation::Vertical, 2);
    text_col.set_hexpand(true);

    let title_row = GtkBox::new(Orientation::Horizontal, 6);
    // Server-provided text: rendered as plain text (never markup).
    let name_label = Label::new(Some(&row.name));
    name_label.set_halign(Align::Start);
    name_label.add_css_class("heading");
    title_row.append(&name_label);

    // Runner chip: "daemon"/"daemon · host"/"client".
    let runner_chip = Label::new(Some(&runner_label(row.runner, is_remote, host)));
    runner_chip.add_css_class("mcp-runner-chip");
    runner_chip.set_valign(Align::Center);
    title_row.append(&runner_chip);

    // Transport chip: honest "stdio"/"http".
    let chip = Label::new(Some(transport_chip(&row.transport)));
    chip.add_css_class("mcp-chip");
    chip.set_valign(Align::Center);
    title_row.append(&chip);
    text_col.append(&title_row);

    // Subtitle: status label (+ tool count when the server is up) (+ daemon
    // target). A daemon row reports a count while "running"; a client row while
    // "enabled" (gtk hosts it), from the live host snapshot (adele-gtk#125).
    let mut subtitle = status_label.to_string();
    if (row.status == "running" || row.status == "enabled") && row.tool_count > 0 {
        let n = row.tool_count;
        subtitle.push_str(&format!(" · {n} tool{}", if n == 1 { "" } else { "s" }));
    }
    if let Some(view) = view
        && !view.target.is_empty()
    {
        subtitle.push_str(" · ");
        subtitle.push_str(&view.target);
    }
    let subtitle_label = Label::new(Some(&subtitle));
    subtitle_label.set_halign(Align::Start);
    subtitle_label.set_wrap(true);
    subtitle_label.set_xalign(0.0);
    subtitle_label.add_css_class("dim-label");
    text_col.append(&subtitle_label);

    // Last connect error (only when a source reports one).
    if let Some(detail) = row.detail.as_ref().filter(|d| !d.is_empty()) {
        let detail_label = Label::new(Some(detail));
        detail_label.set_halign(Align::Start);
        detail_label.set_wrap(true);
        detail_label.set_xalign(0.0);
        detail_label.add_css_class("mcp-error-label");
        text_col.append(&detail_label);
    }

    hbox.append(&text_col);

    // Sign-in / Configure - daemon OAuth servers only. This client runs on the
    // daemon host, so it can drive the configure command (spawns a browser
    // there). Client-run servers have no daemon-driven OAuth and never show one.
    if let Some(view) = view
        && let Some(label) = view.configure_label.as_ref().filter(|l| !l.is_empty())
        && !view.configure_command.is_empty()
    {
        let signin_btn = Button::with_label(label);
        signin_btn.add_css_class("suggested-action");
        let argv = view.configure_command.clone();
        signin_btn.connect_clicked(glib::clone!(
            #[strong]
            on_signin,
            move |_| {
                if let Some(ref cb) = *on_signin.borrow() {
                    cb(argv.clone());
                }
            }
        ));
        hbox.append(&signin_btn);
    }

    // The "currently enabled" state drives the toggle label + target. For a
    // daemon row it is the view's config flag; for a client row it is the coarse
    // status the surface reports.
    let enabled = match view {
        Some(view) => view.enabled,
        None => row.status == "enabled",
    };

    // Enable/Disable toggle (a button whose label reflects the target action -
    // avoids a Switch's programmatic-set notify loop).
    let toggle_btn = Button::with_label(if enabled { "Disable" } else { "Enable" });
    let toggle_name = row.name.clone();
    let toggle_runner = row.runner;
    let toggle_target = !enabled;
    toggle_btn.connect_clicked(glib::clone!(
        #[strong]
        on_toggle,
        move |_| {
            if let Some(ref cb) = *on_toggle.borrow() {
                cb(toggle_name.clone(), toggle_runner, toggle_target);
            }
        }
    ));
    hbox.append(&toggle_btn);

    // Edit.
    let edit_btn = Button::with_label("Edit");
    let edit_name = row.name.clone();
    let edit_runner = row.runner;
    edit_btn.connect_clicked(glib::clone!(
        #[strong]
        on_edit,
        move |_| {
            if let Some(ref cb) = *on_edit.borrow() {
                cb(edit_name.clone(), edit_runner);
            }
        }
    ));
    hbox.append(&edit_btn);

    // Remove.
    let remove_btn = Button::with_label("Remove");
    remove_btn.add_css_class("destructive-action");
    let remove_name = row.name.clone();
    let remove_runner = row.runner;
    remove_btn.connect_clicked(glib::clone!(
        #[strong]
        on_remove,
        move |_| {
            if let Some(ref cb) = *on_remove.borrow() {
                cb(remove_name.clone(), remove_runner);
            }
        }
    ));
    hbox.append(&remove_btn);

    list_row.set_child(Some(&hbox));
    list_row
}

/// Pure display decision for a built-in [`ServerRow`]: the kind chip text
/// ([`kind_label`]), the optional override reason, and whether the row renders
/// disabled/dimmed. Kept GTK-free so the overridden/disabled decision is
/// unit-testable without a display.
struct BuiltinRowDisplay {
    /// The kind chip text — `"built-in"` for a built-in row.
    chip: &'static str,
    /// `Some(reason)` when an external server of the same name overrides this
    /// built-in (so it renders disabled); `None` when it is active.
    reason: Option<String>,
    /// Whether the row renders disabled/dimmed (true iff overridden).
    disabled: bool,
}

/// Build the display decision for a built-in row: its kind chip, and — when an
/// external server of the same name shadows it — the override reason plus the
/// disabled flag that dims the whole row.
fn builtin_row_display(row: &ServerRow) -> BuiltinRowDisplay {
    BuiltinRowDisplay {
        chip: kind_label(row.kind),
        reason: row.disabled_reason.clone(),
        disabled: row.disabled_reason.is_some(),
    }
}

/// The on/off presentation of a built-in row's enable/disable switch, decided
/// purely from the [`BuiltinServerDto`] so it is unit-testable without a display
/// (da#538 slice 4).
// Spec commit: the widget consumer lands with the real body in the next commit;
// this narrow allow is removed there.
#[allow(dead_code)]
struct BuiltinToggleState {
    /// The switch reads ON when the built-in is enabled in this client's config
    /// (i.e. NOT `disabled_by_config`) and OFF when it has been turned off.
    active: bool,
    /// The switch is interactive unless the built-in is *only* shadowed by a
    /// same-name external server: that override wins regardless of the toggle, so
    /// the switch is greyed rather than implying it can countermand the override. A
    /// config-disabled built-in stays interactive so the user can turn it back on
    /// (even if it is also overridden).
    sensitive: bool,
}

/// Decide the enable/disable switch's `active`/`sensitive` from the built-in's
/// DTO: on iff enabled in config; interactive unless the row is disabled *only*
/// because an external server of the same name overrides it.
#[allow(dead_code)]
fn builtin_toggle_state(dto: &BuiltinServerDto) -> BuiltinToggleState {
    // STUB (spec commit): deliberately wrong so the tests below fail red.
    let _ = dto;
    BuiltinToggleState {
        active: false,
        sensitive: false,
    }
}

/// Build one read-only widget for a built-in (in-process) [`ServerRow`].
///
/// Built-ins are hosted inside this client and present in neither config list, so
/// the row is informational: a status dot, the name, a runner chip ("client"), a
/// "built-in" kind chip, and a status subtitle with the tool count. When an
/// external server of the same name overrides it, the whole row renders dimmed
/// (`mcp-row-disabled`) and a `dim-label` line (also a tooltip) surfaces the
/// "overridden by ..." reason. It is deliberately never wired to the toggle/edit/
/// remove actions, which index into the daemon/client config lists.
fn build_builtin_row_widget(row: &ServerRow) -> ListBoxRow {
    let display = builtin_row_display(row);

    let list_row = ListBoxRow::new();
    list_row.set_selectable(false);
    if display.disabled {
        list_row.add_css_class("mcp-row-disabled");
    }

    let hbox = GtkBox::new(Orientation::Horizontal, 12);
    hbox.set_margin_start(10);
    hbox.set_margin_end(10);
    hbox.set_margin_top(8);
    hbox.set_margin_bottom(8);

    // Status dot: an active built-in reads green ("running"); an overridden one
    // reads neutral ("disabled"), from the same status map the other rows use.
    let (dot_class, status_label) = status_display(&row.status);
    let dot = Label::new(None);
    dot.add_css_class("mcp-dot");
    dot.add_css_class(dot_class);
    dot.set_width_chars(2);
    dot.set_valign(Align::Start);
    dot.set_margin_top(4);
    hbox.append(&dot);

    let text_col = GtkBox::new(Orientation::Vertical, 2);
    text_col.set_hexpand(true);

    let title_row = GtkBox::new(Orientation::Horizontal, 6);
    // Server-provided text: rendered as plain text (never markup).
    let name_label = Label::new(Some(&row.name));
    name_label.set_halign(Align::Start);
    name_label.add_css_class("heading");
    title_row.append(&name_label);

    // Runner chip: built-ins always run in the client, so is_remote/host are
    // irrelevant (runner_label ignores them for a client row).
    let runner_chip = Label::new(Some(&runner_label(row.runner, false, None)));
    runner_chip.add_css_class("mcp-runner-chip");
    runner_chip.set_valign(Align::Center);
    title_row.append(&runner_chip);

    // Kind chip: names the in-process "built-in" kind (kind_label(row.kind)).
    let kind_chip = Label::new(Some(display.chip));
    kind_chip.add_css_class("mcp-kind-chip");
    kind_chip.set_valign(Align::Center);
    title_row.append(&kind_chip);
    text_col.append(&title_row);

    // Subtitle: status label (+ tool count when the built-in advertises tools).
    let mut subtitle = status_label.to_string();
    if row.tool_count > 0 {
        let n = row.tool_count;
        subtitle.push_str(&format!(" · {n} tool{}", if n == 1 { "" } else { "s" }));
    }
    let subtitle_label = Label::new(Some(&subtitle));
    subtitle_label.set_halign(Align::Start);
    subtitle_label.set_wrap(true);
    subtitle_label.set_xalign(0.0);
    subtitle_label.add_css_class("dim-label");
    text_col.append(&subtitle_label);

    // Override reason (only when a same-named external server shadows this
    // built-in): a dim line — and tooltip — surfacing *why* it is disabled.
    if let Some(reason) = display.reason.as_ref() {
        let reason_label = Label::new(Some(reason));
        reason_label.set_halign(Align::Start);
        reason_label.set_wrap(true);
        reason_label.set_xalign(0.0);
        reason_label.add_css_class("dim-label");
        reason_label.set_tooltip_text(Some(reason));
        text_col.append(&reason_label);
    }

    hbox.append(&text_col);
    list_row.set_child(Some(&hbox));
    list_row
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- status_display -------------------------------------------------------

    #[test]
    fn status_display_covers_daemon_states() {
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
    fn status_display_covers_client_enabled() {
        // The client surface reports enabled/disabled; enabled reads green.
        assert_eq!(status_display("enabled"), ("mcp-dot-running", "Enabled"));
    }

    #[test]
    fn status_display_unknown_is_neutral() {
        assert_eq!(
            status_display("teleporting"),
            ("mcp-dot-neutral", "Unknown")
        );
        assert_eq!(status_display(""), ("mcp-dot-neutral", "Unknown"));
    }

    // --- filter_from_index ----------------------------------------------------

    #[test]
    fn filter_from_index_maps_dropdown() {
        assert_eq!(filter_from_index(0), RunnerFilter::All);
        assert_eq!(filter_from_index(1), RunnerFilter::Daemon);
        assert_eq!(filter_from_index(2), RunnerFilter::Client);
        // Out-of-range degrades to All rather than panicking.
        assert_eq!(filter_from_index(99), RunnerFilter::All);
    }

    // --- builtin_row_display (da#538 Phase D, slice 3) ------------------------

    /// A built-in [`ServerRow`] as produced by `server_rows_with_builtins`: an
    /// in-process, client-run server whose `disabled_reason` is `Some` iff an
    /// external server of the same name overrides it.
    fn builtin_row(name: &str, tool_count: u32, reason: Option<&str>) -> ServerRow {
        ServerRow {
            name: name.into(),
            runner: Runner::Client,
            transport: "builtin".into(),
            status: if reason.is_some() {
                "disabled"
            } else {
                "running"
            }
            .into(),
            tool_count,
            detail: None,
            kind: client_ui_common::ServerKind::BuiltIn,
            disabled_reason: reason.map(Into::into),
        }
    }

    #[test]
    fn builtin_row_display_active_has_no_reason() {
        let d = builtin_row_display(&builtin_row("fileio", 7, None));
        assert!(!d.disabled, "an active built-in must not render disabled");
        assert!(
            d.reason.is_none(),
            "an active built-in has no override reason"
        );
        assert_eq!(d.chip, "built-in", "the kind chip names the built-in kind");
    }

    #[test]
    fn builtin_row_display_overridden_dims_and_shows_reason() {
        let d = builtin_row_display(&builtin_row(
            "web",
            3,
            Some("overridden by the external \"web\""),
        ));
        assert!(
            d.disabled,
            "an overridden built-in must render disabled/dimmed"
        );
        let reason = d
            .reason
            .as_deref()
            .expect("an overridden built-in must surface a reason");
        assert!(
            reason.contains("overridden"),
            "reason explains the override: {reason}"
        );
        assert!(
            reason.contains("web"),
            "reason names the overriding server: {reason}"
        );
        assert_eq!(d.chip, "built-in");
    }

    // --- builtin_toggle_state (da#538 slice 4) --------------------------------

    /// A [`BuiltinServerDto`] with the flags the toggle-state decision reads.
    fn dto(disabled_by_config: bool, overridden_by: Option<&str>) -> BuiltinServerDto {
        BuiltinServerDto {
            name: "web".into(),
            namespace: "web".into(),
            tool_count: 3,
            overridden_by: overridden_by.map(Into::into),
            disabled_by_config,
        }
    }

    #[test]
    fn toggle_state_enabled_builtin_is_on_and_interactive() {
        let s = builtin_toggle_state(&dto(false, None));
        assert!(s.active, "an enabled built-in shows the switch on");
        assert!(s.sensitive, "an enabled built-in's switch is interactive");
    }

    #[test]
    fn toggle_state_config_disabled_builtin_is_off_and_interactive() {
        let s = builtin_toggle_state(&dto(true, None));
        assert!(!s.active, "a config-disabled built-in shows the switch off");
        assert!(
            s.sensitive,
            "a config-disabled built-in stays interactive so it can be re-enabled"
        );
    }

    #[test]
    fn toggle_state_overridden_only_builtin_is_on_but_inert() {
        // Enabled in config but shadowed by a same-name external server: the
        // switch reads on (it *is* enabled in config) yet is greyed, since the
        // override wins regardless of the toggle.
        let s = builtin_toggle_state(&dto(false, Some("web")));
        assert!(s.active, "an overridden-but-enabled built-in reads on");
        assert!(
            !s.sensitive,
            "a purely-overridden built-in's switch is inert (the override wins)"
        );
    }

    #[test]
    fn toggle_state_config_disabled_and_overridden_stays_interactive() {
        // Both reasons apply: the config-disable is the user's explicit choice, so
        // the switch stays interactive (off) so they can turn it back on.
        let s = builtin_toggle_state(&dto(true, Some("web")));
        assert!(!s.active, "config-disable shows the switch off");
        assert!(
            s.sensitive,
            "a config-disabled built-in is interactive even when also overridden"
        );
    }
}
