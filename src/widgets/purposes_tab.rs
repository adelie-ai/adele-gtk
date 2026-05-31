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
use gtk4::{Align, Box as GtkBox, DropDown, Label, Orientation, Separator, StringList};

type SetPurposeCb = Box<dyn Fn(api::PurposeKindApi, api::PurposeConfigView)>;
type RequestModelsCb = Box<dyn Fn(String)>;

const PRIMARY_SENTINEL: &str = "primary";

fn purpose_label(p: api::PurposeKindApi) -> &'static str {
    match p {
        api::PurposeKindApi::Interactive => "Interactive",
        api::PurposeKindApi::Dreaming => "Dreaming",
        api::PurposeKindApi::Embedding => "Embedding",
        api::PurposeKindApi::Titling => "Titling",
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
}

pub struct PurposesTab {
    pub container: GtkBox,
    rows: Rc<RefCell<BTreeMap<String, Row>>>,
    connections: Rc<RefCell<Vec<api::ConnectionView>>>,
    purposes: Rc<RefCell<api::PurposesView>>,
    /// Model lists keyed by connection id.
    models_by_connection: Rc<RefCell<BTreeMap<String, Vec<api::ModelListing>>>>,
    on_set_purpose: Rc<RefCell<Option<SetPurposeCb>>>,
    on_request_models: Rc<RefCell<Option<RequestModelsCb>>>,
    /// When true, we're reconciling the UI to state — suppress
    /// `set_purpose` callbacks on change notifications.
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
        let models_by_connection: Rc<RefCell<BTreeMap<String, Vec<api::ModelListing>>>> =
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
            let connection_dd = DropDown::new(Some(connection_list.clone()), gtk4::Expression::NONE);
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
            };

            // When connection changes: rebuild models dropdown and emit a
            // write if we're not currently reconciling.
            {
                let rows = Rc::clone(&rows);
                let connections = Rc::clone(&connections);
                let models_by_connection = Rc::clone(&models_by_connection);
                let on_set_purpose = Rc::clone(&on_set_purpose);
                let on_request_models = Rc::clone(&on_request_models);
                let suppress = Rc::clone(&suppress);
                connection_dd.connect_selected_notify(move |_| {
                    if *suppress.borrow() {
                        return;
                    }
                    // Rebuild model dropdown to reflect the new connection.
                    let _ = repopulate_models_for_purpose(
                        purpose,
                        &rows,
                        &connections,
                        &models_by_connection,
                        &on_request_models,
                        &suppress,
                    );
                    emit_current(purpose, &rows, &on_set_purpose);
                });
            }

            {
                let rows = Rc::clone(&rows);
                let on_set_purpose = Rc::clone(&on_set_purpose);
                let suppress = Rc::clone(&suppress);
                model_dd.connect_selected_notify(move |_| {
                    if *suppress.borrow() {
                        return;
                    }
                    emit_current(purpose, &rows, &on_set_purpose);
                });
            }

            {
                let rows = Rc::clone(&rows);
                let on_set_purpose = Rc::clone(&on_set_purpose);
                let suppress = Rc::clone(&suppress);
                effort_dd.connect_selected_notify(move |_| {
                    if *suppress.borrow() {
                        return;
                    }
                    emit_current(purpose, &rows, &on_set_purpose);
                });
            }

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
            .insert(connection_id.to_string(), listings);
        self.reconcile();
    }

    fn reconcile(&self) {
        *self.suppress.borrow_mut() = true;
        for purpose in api::PurposeKindApi::all() {
            let _ = repopulate_models_for_purpose(
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

/// Rebuild the connection/model dropdowns and request models for the
/// currently-selected connection if not already cached.
fn repopulate_models_for_purpose(
    purpose: api::PurposeKindApi,
    rows: &Rc<RefCell<BTreeMap<String, Row>>>,
    connections: &Rc<RefCell<Vec<api::ConnectionView>>>,
    models_by_connection: &Rc<RefCell<BTreeMap<String, Vec<api::ModelListing>>>>,
    on_request_models: &Rc<RefCell<Option<RequestModelsCb>>>,
    suppress: &Rc<RefCell<bool>>,
) -> Option<()> {
    let was_suppressed = *suppress.borrow();
    *suppress.borrow_mut() = true;

    let rows_borrow = rows.borrow();
    let row = rows_borrow.get(purpose.as_key())?;

    // Rebuild connection list. Interactive may not inherit from itself,
    // so only non-interactive purposes see the "primary" sentinel.
    let prev_conn_idx = row.connection_dd.selected() as usize;
    let prev_conn_value = row.connection_values.borrow().get(prev_conn_idx).cloned();

    while row.connection_list.n_items() > 0 {
        row.connection_list.remove(0);
    }
    let mut conn_values: Vec<String> = Vec::new();
    if !matches!(purpose, api::PurposeKindApi::Interactive) {
        row.connection_list.append("primary (inherit)");
        conn_values.push(PRIMARY_SENTINEL.to_string());
    }
    for conn in connections.borrow().iter() {
        row.connection_list
            .append(&format!("{}  ({})", conn.id, conn.connector_type));
        conn_values.push(conn.id.clone());
    }
    *row.connection_values.borrow_mut() = conn_values.clone();

    // Restore previous selection if still present.
    if let Some(prev) = prev_conn_value
        && let Some(idx) = conn_values.iter().position(|v| v == &prev)
    {
        row.connection_dd.set_selected(idx as u32);
    }

    // Which connection's models should we display in the model dropdown?
    let selected_idx = row.connection_dd.selected() as usize;
    let selected_conn = conn_values.get(selected_idx).cloned();
    let cache = models_by_connection.borrow();
    let (models, need_request): (Vec<api::ModelListing>, Option<String>) =
        match selected_conn.as_deref() {
            Some(PRIMARY_SENTINEL) | None => (Vec::new(), None),
            Some(id) => match cache.get(id) {
                Some(list) => (list.clone(), None),
                None => (Vec::new(), Some(id.to_string())),
            },
        };
    drop(cache);

    // Rebuild model dropdown.
    let prev_model_idx = row.model_dd.selected() as usize;
    let prev_model_value = row.model_values.borrow().get(prev_model_idx).cloned();

    while row.model_list.n_items() > 0 {
        row.model_list.remove(0);
    }
    let mut model_values: Vec<String> = Vec::new();
    if !matches!(purpose, api::PurposeKindApi::Interactive) {
        row.model_list.append("primary (inherit)");
        model_values.push(PRIMARY_SENTINEL.to_string());
    }
    for m in &models {
        row.model_list.append(&m.model.display_name);
        model_values.push(m.model.id.clone());
    }
    *row.model_values.borrow_mut() = model_values.clone();

    if let Some(prev) = prev_model_value
        && let Some(idx) = model_values.iter().position(|v| v == &prev)
    {
        row.model_dd.set_selected(idx as u32);
    }

    *suppress.borrow_mut() = was_suppressed;

    // Kick off a model fetch for the newly-selected connection if we don't
    // have it yet.
    if let Some(id) = need_request
        && let Some(ref cb) = *on_request_models.borrow()
    {
        cb(id);
    }
    Some(())
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
    let cfg = match purpose {
        api::PurposeKindApi::Interactive => purposes.interactive.as_ref(),
        api::PurposeKindApi::Dreaming => purposes.dreaming.as_ref(),
        api::PurposeKindApi::Embedding => purposes.embedding.as_ref(),
        api::PurposeKindApi::Titling => purposes.titling.as_ref(),
    };
    let Some(cfg) = cfg else {
        return;
    };

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
    on_set_purpose: &Rc<RefCell<Option<SetPurposeCb>>>,
) {
    let rows_borrow = rows.borrow();
    let Some(row) = rows_borrow.get(purpose.as_key()) else {
        return;
    };
    let conn_idx = row.connection_dd.selected() as usize;
    let Some(connection) = row.connection_values.borrow().get(conn_idx).cloned() else {
        return;
    };
    let model_idx = row.model_dd.selected() as usize;
    let Some(model) = row.model_values.borrow().get(model_idx).cloned() else {
        return;
    };
    let effort = match row.effort_dd.selected() {
        0 => None,
        1 => Some(api::EffortLevel::Low),
        2 => Some(api::EffortLevel::Medium),
        3 => Some(api::EffortLevel::High),
        _ => None,
    };
    let config = api::PurposeConfigView {
        connection,
        model,
        effort,
        // Context-window override (#51) isn't exposed in this UI yet; leave
        // it unset so the daemon uses its curated table / universal
        // fallback.
        max_context_tokens: None,
    };
    if let Some(ref cb) = *on_set_purpose.borrow() {
        cb(purpose, config);
    }
}
