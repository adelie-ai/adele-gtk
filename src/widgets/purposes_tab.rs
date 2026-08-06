//! Purposes tab of the Settings dialog.
//!
//! Flat list of `(Purpose) (Connection ▾) (Model ▾) (Effort ▾)` for every
//! configured purpose. Interactive must bind to a real connection and
//! model; non-interactive purposes may use the sentinel string `"primary"`
//! to inherit from the interactive purpose.
//!
//! The tab owns the dropdown rows. The parent (Settings dialog) supplies
//! the list of connections, the per-connection models, and is called back
//! on `SetPurpose` writes. Re-hydration after a write is the parent's
//! job — the tab simply re-binds whenever `set_*` is invoked.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use desktop_assistant_api_model as api;
use gtk4::prelude::*;
use gtk4::{Align, Box as GtkBox, DropDown, Label, Orientation, Separator, StringList, glib};

type SetPurposeCb = Box<dyn Fn(api::PurposeKindApi, api::PurposeConfigView)>;
type RequestModelsCb = Box<dyn Fn(String)>;

const PRIMARY_SENTINEL: &str = "primary";

fn purpose_label(p: api::PurposeKindApi) -> &'static str {
    match p {
        api::PurposeKindApi::Interactive => "Interactive",
        api::PurposeKindApi::Dreaming => "Dreaming",
        api::PurposeKindApi::Consolidation => "Consolidation",
        api::PurposeKindApi::Embedding => "Embedding",
        api::PurposeKindApi::Titling => "Titling",
        api::PurposeKindApi::Voice => "Voice",
    }
}

/// What a row's dropdowns currently show, lifted out of GTK so the decision
/// to write is plain data. `None` on a value means the dropdown has nothing
/// real selected — typically because its model list failed to load.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RowSelection {
    connection: Option<String>,
    model: Option<String>,
    effort: Option<api::EffortLevel>,
    max_context_tokens: Option<u64>,
}

/// Decide whether `current` is a write worth sending for `purpose`.
///
/// Why this is a pure function rather than a guard flag: the previous version
/// suppressed writes with a boolean held across `reconcile`, which only covers
/// notifications GTK delivers synchronously. Anything arriving after
/// `reconcile` returned was unguarded, re-emitted a `SetPurpose`, and the
/// resulting refresh reconciled again — a write loop that only ended when the
/// socket dropped.
///
/// Returns `None` — meaning "not a user-intended change, send nothing" — when:
///
/// * either dropdown has no real selection (the model list never loaded, so
///   the UI cannot represent a binding it would be honest to write);
/// * the pair is mixed: `"primary"` means inherit and is only meaningful when
///   *both* connection and model carry it. A real connection with a `"primary"`
///   model is the shape that silently retired a live binding;
/// * `Interactive` claims to inherit — there is no primary above it;
/// * the result equals `last_known`, the state the daemon last reported. This
///   is what makes reconciliation structurally incapable of writing: it sets
///   the widgets to exactly that state, so anything it triggers is a no-op.
fn planned_write(
    purpose: api::PurposeKindApi,
    current: &RowSelection,
    last_known: Option<&api::PurposeConfigView>,
) -> Option<api::PurposeConfigView> {
    let connection = current.connection.as_ref()?;
    let model = current.model.as_ref()?;

    let connection_inherits = connection == PRIMARY_SENTINEL;
    let model_inherits = model == PRIMARY_SENTINEL;
    if connection_inherits != model_inherits {
        return None;
    }
    if connection_inherits && matches!(purpose, api::PurposeKindApi::Interactive) {
        return None;
    }

    let candidate = api::PurposeConfigView {
        connection: connection.clone(),
        model: model.clone(),
        effort: current.effort,
        max_context_tokens: current.max_context_tokens,
    };
    if last_known == Some(&candidate) {
        return None;
    }
    Some(candidate)
}

/// What a row's dropdown should display, lifted out of GTK so the contents
/// are plain data that can be compared before anything is written.
///
/// `values[i]` names what `labels[i]` selects. A label with no value at its
/// index is a **notice** — "Loading models...", "Models unavailable" — which
/// the row reads back as "nothing real selected", so it can never become a
/// binding. That is why `labels` may be longer than `values`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct DropdownOptions {
    labels: Vec<String>,
    values: Vec<String>,
}

/// What is known about one connection's model list.
///
/// Recording the outcome — not only the success — is what stops a failing
/// connection being asked again on every reconcile. Absence of an entry means
/// "never asked", which is the only state that starts a request.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ModelListState {
    /// A request is in flight. A second request would be a duplicate.
    Pending,
    Loaded(Vec<api::ModelListing>),
    /// The request failed. Held so the reconcile loop stops re-asking; the
    /// user retries by reopening Settings, which builds a fresh tab.
    Failed(String),
}

/// Whether `list` must be rewritten to show `desired`.
///
/// This is the whole defence against the rebuild loop. Every write to a
/// `StringList` emits `items-changed`, which makes the bound `DropDown` emit
/// `notify::selected`, which re-enters the handler that rebuilds. Comparing
/// first means an unchanged list is written zero times, so the re-entrant pass
/// emits nothing and the cascade stops. A guard flag cannot do this, because
/// GTK delivers some of those notifications after the guard is cleared.
fn list_needs_sync(current: &[String], desired: &[String]) -> bool {
    current != desired
}

/// The connections a purpose may bind to. Only non-interactive purposes may
/// inherit, because there is no primary above `Interactive`.
fn connection_options(
    purpose: api::PurposeKindApi,
    connections: &[api::ConnectionView],
) -> DropdownOptions {
    let mut opts = DropdownOptions::default();
    if !matches!(purpose, api::PurposeKindApi::Interactive) {
        opts.labels.push("primary (inherit)".to_string());
        opts.values.push(PRIMARY_SENTINEL.to_string());
    }
    for conn in connections {
        opts.labels
            .push(format!("{}  ({})", conn.id, conn.connector_type));
        opts.values.push(conn.id.clone());
    }
    opts
}

/// The models a purpose may bind to, given the connection it currently shows
/// and what is known about that connection's list.
fn model_options(
    purpose: api::PurposeKindApi,
    selected_connection: Option<&str>,
    state: Option<&ModelListState>,
) -> DropdownOptions {
    let mut opts = DropdownOptions::default();
    let Some(connection) = selected_connection else {
        return opts;
    };

    if !matches!(purpose, api::PurposeKindApi::Interactive) {
        opts.labels.push("primary (inherit)".to_string());
        opts.values.push(PRIMARY_SENTINEL.to_string());
    }
    if connection == PRIMARY_SENTINEL {
        return opts;
    }

    match state {
        Some(ModelListState::Loaded(listings)) => {
            for listing in listings {
                opts.labels.push(listing.model.display_name.clone());
                opts.values.push(listing.model.id.clone());
            }
        }
        // Notices carry no value, so selecting one yields no binding.
        Some(ModelListState::Pending) | None => {
            opts.labels.push("Loading models...".to_string());
        }
        Some(ModelListState::Failed(reason)) => {
            opts.labels.push(format!("Models unavailable - {reason}"));
        }
    }
    opts
}

/// Whether to start a model-list request for the connection a row shows.
///
/// True only when nothing is known about the connection yet. `Pending` means a
/// request is already in flight, and `Failed` means the answer is known to be
/// no — re-asking either is what saturated the daemon in #142.
fn should_request_models(
    selected_connection: Option<&str>,
    state: Option<&ModelListState>,
) -> bool {
    match selected_connection {
        None | Some(PRIMARY_SENTINEL) => false,
        Some(_) => state.is_none(),
    }
}

/// The daemon's last reported binding for `purpose`, if it has one.
fn purpose_config(
    purposes: &api::PurposesView,
    purpose: api::PurposeKindApi,
) -> Option<&api::PurposeConfigView> {
    match purpose {
        api::PurposeKindApi::Interactive => purposes.interactive.as_ref(),
        api::PurposeKindApi::Dreaming => purposes.dreaming.as_ref(),
        api::PurposeKindApi::Consolidation => purposes.consolidation.as_ref(),
        api::PurposeKindApi::Embedding => purposes.embedding.as_ref(),
        api::PurposeKindApi::Titling => purposes.titling.as_ref(),
        api::PurposeKindApi::Voice => purposes.voice.as_ref(),
    }
}

/// Ephemeral UI state for each row.
struct Row {
    connection_dd: DropDown,
    connection_list: StringList,
    /// Mirror of the dropdown's string list in the same index order:
    /// either a connection id or the `"primary"` sentinel. Kept separately
    /// so we can map dropdown index → value without re-reading the gtk
    /// model.
    connection_values: Rc<RefCell<Vec<String>>>,
    model_dd: DropDown,
    model_list: StringList,
    model_values: Rc<RefCell<Vec<String>>>,
    effort_dd: DropDown,
    /// Preserved per-purpose context-window override (#51). The UI doesn't
    /// edit this field, but `SetPurpose` is a full replace, so we remember
    /// whatever the daemon reported and send it back unchanged on emit —
    /// otherwise touching a dropdown would silently wipe an override set
    /// elsewhere (TUI/config).
    max_context_tokens: Rc<RefCell<Option<u64>>>,
}

pub struct PurposesTab {
    pub container: GtkBox,
    rows: Rc<RefCell<BTreeMap<String, Row>>>,
    connections: Rc<RefCell<Vec<api::ConnectionView>>>,
    purposes: Rc<RefCell<api::PurposesView>>,
    /// What is known about each connection's model list, keyed by connection
    /// id. Holds failures as well as successes, so a connection that cannot
    /// list its models is asked once rather than on every reconcile.
    models_by_connection: Rc<RefCell<BTreeMap<String, ModelListState>>>,
    on_set_purpose: Rc<RefCell<Option<SetPurposeCb>>>,
    on_request_models: Rc<RefCell<Option<RequestModelsCb>>>,
    /// When true, we're reconciling the UI to state. This skips the work of
    /// re-deriving a write from notifications GTK delivers synchronously — an
    /// optimization, *not* the correctness guarantee. Guarding on this flag
    /// alone is what allowed both loops this tab has had: a notification
    /// arriving after `reconcile` returned found it cleared, and re-entered.
    ///
    /// Two pure rules carry the actual guarantees, one per loop:
    ///
    /// * `planned_write` drops any write matching last-known server state, so
    ///   reconciliation cannot produce a `SetPurpose` (#142);
    /// * `list_needs_sync` drops any dropdown write that would not change the
    ///   contents, so a rebuild cannot produce another rebuild (#158).
    suppress: Rc<RefCell<bool>>,
}

impl PurposesTab {
    pub fn new() -> Self {
        let container = GtkBox::new(Orientation::Vertical, 8);
        container.set_margin_start(12);
        container.set_margin_end(12);
        container.set_margin_top(12);
        container.set_margin_bottom(12);

        let header = Label::new(Some("Purposes"));
        header.add_css_class("heading");
        header.set_halign(Align::Start);
        container.append(&header);

        let blurb = Label::new(Some(
            "Each purpose maps to a connection and model. Non-interactive purposes may inherit from Interactive by choosing \"primary\".",
        ));
        blurb.set_wrap(true);
        blurb.set_halign(Align::Start);
        blurb.add_css_class("dim-label");
        container.append(&blurb);

        container.append(&Separator::new(Orientation::Horizontal));

        let rows: Rc<RefCell<BTreeMap<String, Row>>> = Rc::new(RefCell::new(BTreeMap::new()));
        let connections: Rc<RefCell<Vec<api::ConnectionView>>> = Rc::new(RefCell::new(Vec::new()));
        let purposes: Rc<RefCell<api::PurposesView>> =
            Rc::new(RefCell::new(api::PurposesView::default()));
        let models_by_connection: Rc<RefCell<BTreeMap<String, ModelListState>>> =
            Rc::new(RefCell::new(BTreeMap::new()));
        let on_set_purpose: Rc<RefCell<Option<SetPurposeCb>>> = Rc::new(RefCell::new(None));
        let on_request_models: Rc<RefCell<Option<RequestModelsCb>>> = Rc::new(RefCell::new(None));
        let suppress = Rc::new(RefCell::new(false));

        for purpose in api::PurposeKindApi::all() {
            let row_widget = GtkBox::new(Orientation::Horizontal, 8);
            row_widget.set_margin_top(6);
            row_widget.set_margin_bottom(6);

            let label = Label::new(Some(purpose_label(purpose)));
            label.set_width_chars(12);
            label.set_halign(Align::Start);
            row_widget.append(&label);

            let connection_list = StringList::new(&[]);
            let connection_dd =
                DropDown::new(Some(connection_list.clone()), gtk4::Expression::NONE);
            connection_dd.set_hexpand(true);
            row_widget.append(&connection_dd);

            let model_list = StringList::new(&[]);
            let model_dd = DropDown::new(Some(model_list.clone()), gtk4::Expression::NONE);
            model_dd.set_hexpand(true);
            row_widget.append(&model_dd);

            let effort_list = StringList::new(&["None", "Low", "Medium", "High"]);
            let effort_dd = DropDown::new(Some(effort_list.clone()), gtk4::Expression::NONE);
            row_widget.append(&effort_dd);

            container.append(&row_widget);

            let row = Row {
                connection_dd: connection_dd.clone(),
                connection_list,
                connection_values: Rc::new(RefCell::new(Vec::new())),
                model_dd: model_dd.clone(),
                model_list,
                model_values: Rc::new(RefCell::new(Vec::new())),
                effort_dd: effort_dd.clone(),
                max_context_tokens: Rc::new(RefCell::new(None)),
            };

            // When connection changes: rebuild models dropdown and emit a
            // write if we're not currently reconciling.
            connection_dd.connect_selected_notify(glib::clone!(
                #[strong]
                rows,
                #[strong]
                connections,
                #[strong]
                models_by_connection,
                #[strong]
                on_set_purpose,
                #[strong]
                on_request_models,
                #[strong]
                suppress,
                #[strong]
                purposes,
                move |_| {
                    if *suppress.borrow() {
                        return;
                    }
                    // Rebuild model dropdown to reflect the new connection.
                    // Safe to re-enter: a rebuild that reaches the same answer
                    // writes nothing, so it raises no further notifications.
                    repopulate_models_for_purpose(
                        purpose,
                        &rows,
                        &connections,
                        &models_by_connection,
                        &on_request_models,
                        &suppress,
                    );
                    emit_current(purpose, &rows, &purposes, &on_set_purpose);
                }
            ));

            model_dd.connect_selected_notify(glib::clone!(
                #[strong]
                rows,
                #[strong]
                on_set_purpose,
                #[strong]
                suppress,
                #[strong]
                purposes,
                move |_| {
                    if *suppress.borrow() {
                        return;
                    }
                    emit_current(purpose, &rows, &purposes, &on_set_purpose);
                }
            ));

            effort_dd.connect_selected_notify(glib::clone!(
                #[strong]
                rows,
                #[strong]
                on_set_purpose,
                #[strong]
                suppress,
                #[strong]
                purposes,
                move |_| {
                    if *suppress.borrow() {
                        return;
                    }
                    emit_current(purpose, &rows, &purposes, &on_set_purpose);
                }
            ));

            rows.borrow_mut().insert(purpose.as_key().to_string(), row);
        }

        Self {
            container,
            rows,
            connections,
            purposes,
            models_by_connection,
            on_set_purpose,
            on_request_models,
            suppress,
        }
    }

    pub fn connect_set_purpose<F>(&self, f: F)
    where
        F: Fn(api::PurposeKindApi, api::PurposeConfigView) + 'static,
    {
        *self.on_set_purpose.borrow_mut() = Some(Box::new(f));
    }

    pub fn connect_request_models<F>(&self, f: F)
    where
        F: Fn(String) + 'static,
    {
        *self.on_request_models.borrow_mut() = Some(Box::new(f));
    }

    /// Replace the connection list. Resets dropdowns.
    pub fn set_connections(&self, connections: &[api::ConnectionView]) {
        *self.connections.borrow_mut() = connections.to_vec();
        self.reconcile();
    }

    pub fn set_purposes(&self, purposes: api::PurposesView) {
        *self.purposes.borrow_mut() = purposes;
        self.reconcile();
    }

    pub fn set_models(&self, connection_id: &str, listings: Vec<api::ModelListing>) {
        self.models_by_connection
            .borrow_mut()
            .insert(connection_id.to_string(), ModelListState::Loaded(listings));
        self.reconcile();
    }

    /// Record that a connection could not list its models.
    ///
    /// Without this the failure is invisible to the tab, so the connection
    /// looks un-asked and every reconcile asks again — the amplifier that made
    /// #142 saturate the daemon. The row shows the reason, and the user retries
    /// by reopening Settings.
    pub fn set_models_failed(&self, connection_id: &str, reason: &str) {
        self.models_by_connection.borrow_mut().insert(
            connection_id.to_string(),
            ModelListState::Failed(reason.to_string()),
        );
        self.reconcile();
    }

    fn reconcile(&self) {
        *self.suppress.borrow_mut() = true;
        for purpose in api::PurposeKindApi::all() {
            repopulate_models_for_purpose(
                purpose,
                &self.rows,
                &self.connections,
                &self.models_by_connection,
                &self.on_request_models,
                &self.suppress,
            );
            apply_purpose_config(purpose, &self.rows, &self.connections, &self.purposes);
        }
        *self.suppress.borrow_mut() = false;
    }
}

/// Write `desired` into `list`, but only if it differs from what is there.
///
/// The comparison is the point. See [`list_needs_sync`].
fn sync_string_list(list: &StringList, desired: &[String]) -> bool {
    let current: Vec<String> = (0..list.n_items())
        .map(|i| list.string(i).map(|s| s.to_string()).unwrap_or_default())
        .collect();
    if !list_needs_sync(&current, desired) {
        return false;
    }
    while list.n_items() > 0 {
        list.remove(0);
    }
    for label in desired {
        list.append(label);
    }
    true
}

/// Rebuild the connection/model dropdowns and request models for the
/// currently-selected connection if nothing is known about it yet.
///
/// Writes to the dropdowns only where the contents actually change, so a
/// rebuild that reaches the same answer is silent. Without that, each rebuild
/// emits `notify::selected`, a notification delivered after this returns
/// re-enters the connection handler, and the two rebuild each other until the
/// main loop is starved (#158).
fn repopulate_models_for_purpose(
    purpose: api::PurposeKindApi,
    rows: &Rc<RefCell<BTreeMap<String, Row>>>,
    connections: &Rc<RefCell<Vec<api::ConnectionView>>>,
    models_by_connection: &Rc<RefCell<BTreeMap<String, ModelListState>>>,
    on_request_models: &Rc<RefCell<Option<RequestModelsCb>>>,
    suppress: &Rc<RefCell<bool>>,
) {
    let rows_borrow = rows.borrow();
    // Taken before `suppress` is raised, so an early return cannot leave the
    // flag stuck true and the tab inert.
    let Some(row) = rows_borrow.get(purpose.as_key()) else {
        return;
    };

    let was_suppressed = *suppress.borrow();
    *suppress.borrow_mut() = true;

    let conn_opts = connection_options(purpose, &connections.borrow());
    let prev_conn = row
        .connection_values
        .borrow()
        .get(row.connection_dd.selected() as usize)
        .cloned();

    sync_string_list(&row.connection_list, &conn_opts.labels);
    *row.connection_values.borrow_mut() = conn_opts.values.clone();

    // Selection is preserved by value, not index: the list may have grown or
    // shrunk underneath it.
    if let Some(prev) = prev_conn.as_ref()
        && let Some(idx) = conn_opts.values.iter().position(|v| v == prev)
    {
        row.connection_dd.set_selected(idx as u32);
    }

    let selected_conn = conn_opts
        .values
        .get(row.connection_dd.selected() as usize)
        .cloned();

    let known = models_by_connection.borrow();
    let state = selected_conn.as_deref().and_then(|id| known.get(id));
    let need_request = should_request_models(selected_conn.as_deref(), state);
    // A request is about to start, so show the row as loading rather than
    // waiting a whole reconcile to say so.
    let display_state = if need_request {
        Some(ModelListState::Pending)
    } else {
        state.cloned()
    };
    drop(known);

    let model_opts = model_options(purpose, selected_conn.as_deref(), display_state.as_ref());
    let prev_model = row
        .model_values
        .borrow()
        .get(row.model_dd.selected() as usize)
        .cloned();

    sync_string_list(&row.model_list, &model_opts.labels);
    *row.model_values.borrow_mut() = model_opts.values.clone();

    if let Some(prev) = prev_model.as_ref()
        && let Some(idx) = model_opts.values.iter().position(|v| v == prev)
    {
        row.model_dd.set_selected(idx as u32);
    }

    *suppress.borrow_mut() = was_suppressed;

    // Record the request before making it, so a re-entrant rebuild sees
    // `Pending` and does not ask a second time.
    if need_request && let Some(id) = selected_conn {
        models_by_connection
            .borrow_mut()
            .insert(id.clone(), ModelListState::Pending);
        if let Some(ref cb) = *on_request_models.borrow() {
            cb(id);
        }
    }
}

/// Apply the server-side `PurposesView` to the dropdowns. Non-existent
/// purpose entries leave the dropdowns on their defaults.
fn apply_purpose_config(
    purpose: api::PurposeKindApi,
    rows: &Rc<RefCell<BTreeMap<String, Row>>>,
    _connections: &Rc<RefCell<Vec<api::ConnectionView>>>,
    purposes: &Rc<RefCell<api::PurposesView>>,
) {
    let rows_borrow = rows.borrow();
    let Some(row) = rows_borrow.get(purpose.as_key()) else {
        return;
    };
    let purposes = purposes.borrow();
    let Some(cfg) = purpose_config(&purposes, purpose) else {
        return;
    };

    // Remember the daemon's context-window override so a later emit can send
    // it back unchanged (the UI doesn't edit this field).
    *row.max_context_tokens.borrow_mut() = cfg.max_context_tokens;

    if let Some(idx) = row
        .connection_values
        .borrow()
        .iter()
        .position(|v| v == &cfg.connection)
    {
        row.connection_dd.set_selected(idx as u32);
    }
    if let Some(idx) = row
        .model_values
        .borrow()
        .iter()
        .position(|v| v == &cfg.model)
    {
        row.model_dd.set_selected(idx as u32);
    }
    let effort_idx = match cfg.effort {
        None => 0,
        Some(api::EffortLevel::Low) => 1,
        Some(api::EffortLevel::Medium) => 2,
        Some(api::EffortLevel::High) => 3,
    };
    row.effort_dd.set_selected(effort_idx as u32);
}

/// Assemble a `PurposeConfigView` from the current dropdown state and
/// emit a write callback.
fn emit_current(
    purpose: api::PurposeKindApi,
    rows: &Rc<RefCell<BTreeMap<String, Row>>>,
    purposes: &Rc<RefCell<api::PurposesView>>,
    on_set_purpose: &Rc<RefCell<Option<SetPurposeCb>>>,
) {
    let rows_borrow = rows.borrow();
    let Some(row) = rows_borrow.get(purpose.as_key()) else {
        return;
    };

    // Read the dropdowns into plain data. A selection that does not index into
    // the row's value mirror means the list is empty or out of sync, which is
    // "nothing real is selected" rather than a value worth writing.
    let conn_idx = row.connection_dd.selected() as usize;
    let model_idx = row.model_dd.selected() as usize;
    let current = RowSelection {
        connection: row.connection_values.borrow().get(conn_idx).cloned(),
        model: row.model_values.borrow().get(model_idx).cloned(),
        effort: match row.effort_dd.selected() {
            1 => Some(api::EffortLevel::Low),
            2 => Some(api::EffortLevel::Medium),
            3 => Some(api::EffortLevel::High),
            _ => None,
        },
        // Context-window override (#51) isn't editable in this UI, but
        // `SetPurpose` is a full replace — preserve whatever the daemon
        // last reported so we don't clobber an override set elsewhere.
        max_context_tokens: *row.max_context_tokens.borrow(),
    };

    let purposes_borrow = purposes.borrow();
    let last_known = purpose_config(&purposes_borrow, purpose);
    let Some(config) = planned_write(purpose, &current, last_known) else {
        return;
    };
    drop(purposes_borrow);

    if let Some(ref cb) = *on_set_purpose.borrow() {
        cb(purpose, config);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(connection: Option<&str>, model: Option<&str>) -> RowSelection {
        RowSelection {
            connection: connection.map(str::to_string),
            model: model.map(str::to_string),
            effort: None,
            max_context_tokens: None,
        }
    }

    fn cfg(connection: &str, model: &str) -> api::PurposeConfigView {
        api::PurposeConfigView {
            connection: connection.into(),
            model: model.into(),
            effort: None,
            max_context_tokens: None,
        }
    }

    fn conn(id: &str, connector_type: &str) -> api::ConnectionView {
        api::ConnectionView {
            id: id.into(),
            connector_type: connector_type.into(),
            display_label: format!("{id} ({connector_type})"),
            availability: api::ConnectionAvailability::Ok,
            has_credentials: true,
            config: None,
        }
    }

    fn listing(connection_id: &str, model_id: &str, display_name: &str) -> api::ModelListing {
        api::ModelListing {
            connection_id: connection_id.into(),
            connection_label: connection_id.into(),
            model: api::ModelInfoView {
                id: model_id.into(),
                display_name: display_name.into(),
                context_limit: None,
                capabilities: api::ModelCapabilitiesView::default(),
            },
            notices: Vec::new(),
        }
    }

    fn loaded(connection_id: &str, models: &[(&str, &str)]) -> ModelListState {
        ModelListState::Loaded(
            models
                .iter()
                .map(|(id, name)| listing(connection_id, id, name))
                .collect(),
        )
    }

    // -- The hang: a rebuild must not be able to cause another rebuild. ------

    /// A rebuild writes to the row's `StringList`s, and every write emits
    /// `items-changed`, which makes the bound `DropDown` emit
    /// `notify::selected`. A notification GTK delivers after the rebuild
    /// returns re-enters the connection handler and rebuilds again. The cycle
    /// terminates only if a rebuild that changes nothing writes nothing, so
    /// the second pass emits no signal.
    #[test]
    fn a_rebuild_cannot_trigger_another_rebuild() {
        let connections = vec![conn("bedrock", "bedrock"), conn("ollama", "ollama")];
        let state = loaded("bedrock", &[("zai.glm-5", "GLM 5")]);

        for purpose in api::PurposeKindApi::all() {
            let conn_opts = connection_options(purpose, &connections);
            assert!(
                !list_needs_sync(&conn_opts.labels, &conn_opts.labels),
                "{purpose:?}: rebuilding the connection list with its own contents must write nothing"
            );

            for candidate in [
                Some(&state),
                Some(&ModelListState::Pending),
                Some(&ModelListState::Failed("connection is not live".into())),
                None,
            ] {
                let model_opts = model_options(purpose, Some("bedrock"), candidate);
                assert!(
                    !list_needs_sync(&model_opts.labels, &model_opts.labels),
                    "{purpose:?}/{candidate:?}: rebuilding the model list with its own contents must write nothing"
                );
            }
        }
    }

    /// Switching the provider changes the model list exactly once: the first
    /// rebuild writes, and recomputing for the same connection does not.
    #[test]
    fn changing_the_connection_rebuilds_the_model_list_once() {
        let purpose = api::PurposeKindApi::Embedding;
        let bedrock = loaded("bedrock", &[("zai.glm-5", "GLM 5")]);
        let ollama = loaded("ollama", &[("nomic-embed-text", "Nomic Embed")]);

        let before = model_options(purpose, Some("bedrock"), Some(&bedrock));
        let after = model_options(purpose, Some("ollama"), Some(&ollama));
        assert!(
            list_needs_sync(&before.labels, &after.labels),
            "switching bedrock -> ollama must change the model list"
        );

        let again = model_options(purpose, Some("ollama"), Some(&ollama));
        assert!(
            !list_needs_sync(&after.labels, &again.labels),
            "recomputing for the same connection must not change the model list"
        );
    }

    #[test]
    fn an_unchanged_connection_list_is_not_rewritten() {
        let connections = vec![conn("bedrock", "bedrock")];
        let first = connection_options(api::PurposeKindApi::Voice, &connections);
        let second = connection_options(api::PurposeKindApi::Voice, &connections);
        assert!(!list_needs_sync(&first.labels, &second.labels));
    }

    #[test]
    fn a_genuinely_different_list_is_rewritten() {
        assert!(list_needs_sync(
            &["a".to_string()],
            &["a".to_string(), "b".to_string()]
        ));
        assert!(list_needs_sync(&["a".to_string()], &["b".to_string()]));
        assert!(!list_needs_sync(&["a".to_string()], &["a".to_string()]));
    }

    // -- The model-list request must not repeat. ----------------------------

    /// #142's amplifier: a failed list was never cached, so every reconcile
    /// re-fired the same doomed request. One request per connection, whatever
    /// the outcome.
    #[test]
    fn failed_model_list_is_requested_once_per_connection() {
        assert!(
            should_request_models(Some("bedrock"), None),
            "a connection with no recorded state must be requested once"
        );
        assert!(
            !should_request_models(Some("bedrock"), Some(&ModelListState::Pending)),
            "a request already in flight must not be repeated"
        );
        assert!(
            !should_request_models(
                Some("bedrock"),
                Some(&ModelListState::Failed("not live".into()))
            ),
            "a failed list must not be re-requested on every reconcile"
        );
        assert!(
            !should_request_models(
                Some("bedrock"),
                Some(&loaded("bedrock", &[("zai.glm-5", "GLM 5")]))
            ),
            "a loaded list must not be re-requested"
        );
    }

    #[test]
    fn the_inherit_sentinel_never_requests_a_model_list() {
        assert!(!should_request_models(Some(PRIMARY_SENTINEL), None));
        assert!(!should_request_models(None, None));
    }

    // -- A failed list must be visible, and must not be writable. -----------

    #[test]
    fn model_list_failure_is_surfaced_on_the_row() {
        let opts = model_options(
            api::PurposeKindApi::Embedding,
            Some("bedrock"),
            Some(&ModelListState::Failed("connection is not live".into())),
        );
        assert!(
            opts.labels.iter().any(|l| l.contains("unavailable")),
            "the row must say the list failed, not show an empty dropdown: {:?}",
            opts.labels
        );
    }

    #[test]
    fn a_pending_model_list_is_surfaced_on_the_row() {
        let opts = model_options(
            api::PurposeKindApi::Embedding,
            Some("bedrock"),
            Some(&ModelListState::Pending),
        );
        assert!(
            opts.labels.iter().any(|l| l.contains("Loading")),
            "an in-flight list must show as loading: {:?}",
            opts.labels
        );
    }

    /// A label with no matching value cannot be turned into a binding: the
    /// row reads it back as "nothing real selected", which `planned_write`
    /// already refuses.
    #[test]
    fn purpose_row_with_unavailable_models_is_not_writable() {
        let opts = model_options(
            api::PurposeKindApi::Embedding,
            Some("bedrock"),
            Some(&ModelListState::Failed("not live".into())),
        );
        let notice_idx = opts
            .labels
            .iter()
            .position(|l| l.contains("unavailable"))
            .expect("the failure notice must be present");
        let selected = opts.values.get(notice_idx).cloned();
        assert_eq!(
            selected, None,
            "selecting the failure notice must not yield a model value"
        );
        assert_eq!(
            planned_write(
                api::PurposeKindApi::Embedding,
                &sel(Some("bedrock"), selected.as_deref()),
                None
            ),
            None,
            "a row whose model list failed must not be writable"
        );
    }

    // -- Option contents. ---------------------------------------------------

    #[test]
    fn only_non_interactive_purposes_offer_the_inherit_sentinel() {
        let connections = vec![conn("bedrock", "bedrock")];

        let interactive = connection_options(api::PurposeKindApi::Interactive, &connections);
        assert!(
            !interactive.values.iter().any(|v| v == PRIMARY_SENTINEL),
            "Interactive has no primary above it, so it must not offer inherit"
        );

        let embedding = connection_options(api::PurposeKindApi::Embedding, &connections);
        assert_eq!(
            embedding.values.first().map(String::as_str),
            Some(PRIMARY_SENTINEL)
        );
    }

    #[test]
    fn every_option_label_past_the_values_is_a_notice() {
        // The mirror contract: `values[i]` names what `labels[i]` selects.
        // Extra trailing labels are notices, which is what makes them
        // unselectable as a binding.
        let connections = vec![conn("bedrock", "bedrock")];
        let opts = connection_options(api::PurposeKindApi::Embedding, &connections);
        assert_eq!(opts.labels.len(), opts.values.len());

        let loaded_opts = model_options(
            api::PurposeKindApi::Embedding,
            Some("bedrock"),
            Some(&loaded("bedrock", &[("zai.glm-5", "GLM 5")])),
        );
        assert_eq!(loaded_opts.labels.len(), loaded_opts.values.len());
    }

    #[test]
    fn reconciling_to_server_state_is_not_a_write() {
        // The loop: refresh -> reconcile -> stray notify -> emit -> refresh.
        // Reconcile sets the widgets to exactly `last_known`, so the write it
        // would produce is a no-op and must be dropped.
        let server = cfg("bedrock", "zai.glm-5");
        assert_eq!(
            planned_write(
                api::PurposeKindApi::Embedding,
                &sel(Some("bedrock"), Some("zai.glm-5")),
                Some(&server),
            ),
            None
        );
    }

    #[test]
    fn unavailable_model_list_is_not_writable() {
        // Bedrock's model list failed, so the dropdown holds nothing real.
        // The row must not be writable at all.
        assert_eq!(
            planned_write(
                api::PurposeKindApi::Embedding,
                &sel(Some("bedrock"), None),
                None
            ),
            None
        );
    }

    #[test]
    fn unavailable_connection_list_is_not_writable() {
        assert_eq!(
            planned_write(
                api::PurposeKindApi::Embedding,
                &sel(None, Some("nomic-embed-text")),
                None
            ),
            None
        );
    }

    #[test]
    fn mixed_primary_pair_is_never_emitted() {
        // The exact shape that retired a live binding in production:
        // a real connection with the inherit sentinel as the model.
        assert_eq!(
            planned_write(
                api::PurposeKindApi::Embedding,
                &sel(Some("bedrock"), Some(PRIMARY_SENTINEL)),
                Some(&cfg("default", "nomic-embed-text")),
            ),
            None
        );
    }

    #[test]
    fn mixed_primary_pair_is_never_emitted_in_either_order() {
        assert_eq!(
            planned_write(
                api::PurposeKindApi::Embedding,
                &sel(Some(PRIMARY_SENTINEL), Some("zai.glm-5")),
                None
            ),
            None
        );
    }

    #[test]
    fn interactive_cannot_inherit() {
        // There is no primary above interactive to inherit from.
        assert_eq!(
            planned_write(
                api::PurposeKindApi::Interactive,
                &sel(Some(PRIMARY_SENTINEL), Some(PRIMARY_SENTINEL)),
                None
            ),
            None
        );
    }

    #[test]
    fn a_genuine_change_is_a_write() {
        assert_eq!(
            planned_write(
                api::PurposeKindApi::Embedding,
                &sel(Some("default"), Some("nomic-embed-text")),
                Some(&cfg("bedrock", "zai.glm-5")),
            ),
            Some(cfg("default", "nomic-embed-text"))
        );
    }

    #[test]
    fn a_deliberate_inherit_pair_is_a_write() {
        assert_eq!(
            planned_write(
                api::PurposeKindApi::Dreaming,
                &sel(Some(PRIMARY_SENTINEL), Some(PRIMARY_SENTINEL)),
                Some(&cfg("bedrock", "zai.glm-5")),
            ),
            Some(cfg(PRIMARY_SENTINEL, PRIMARY_SENTINEL))
        );
    }

    #[test]
    fn an_effort_only_change_is_a_write() {
        let current = RowSelection {
            connection: Some("bedrock".into()),
            model: Some("zai.glm-5".into()),
            effort: Some(api::EffortLevel::High),
            max_context_tokens: None,
        };
        let written = planned_write(
            api::PurposeKindApi::Titling,
            &current,
            Some(&cfg("bedrock", "zai.glm-5")),
        )
        .expect("changing only the effort is still a real change");
        assert_eq!(written.effort, Some(api::EffortLevel::High));
    }

    #[test]
    fn a_context_window_override_is_preserved() {
        // SetPurpose is a full replace and the UI does not edit this field,
        // so an override set elsewhere must survive a dropdown edit.
        let current = RowSelection {
            connection: Some("default".into()),
            model: Some("nomic-embed-text".into()),
            effort: None,
            max_context_tokens: Some(8192),
        };
        let written = planned_write(
            api::PurposeKindApi::Embedding,
            &current,
            Some(&cfg("bedrock", "zai.glm-5")),
        )
        .expect("a real change");
        assert_eq!(written.max_context_tokens, Some(8192));
    }

    #[test]
    fn first_write_with_no_known_server_state_is_allowed() {
        assert_eq!(
            planned_write(
                api::PurposeKindApi::Voice,
                &sel(Some("bedrock"), Some("zai.glm-5")),
                None
            ),
            Some(cfg("bedrock", "zai.glm-5"))
        );
    }
}
