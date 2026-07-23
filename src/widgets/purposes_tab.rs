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
                move |_| {
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
                }
            ));

            model_dd.connect_selected_notify(glib::clone!(
                #[strong]
                rows,
                #[strong]
                on_set_purpose,
                #[strong]
                suppress,
                move |_| {
                    if *suppress.borrow() {
                        return;
                    }
                    emit_current(purpose, &rows, &on_set_purpose);
                }
            ));

            effort_dd.connect_selected_notify(glib::clone!(
                #[strong]
                rows,
                #[strong]
                on_set_purpose,
                #[strong]
                suppress,
                move |_| {
                    if *suppress.borrow() {
                        return;
                    }
                    emit_current(purpose, &rows, &on_set_purpose);
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
        api::PurposeKindApi::Consolidation => purposes.consolidation.as_ref(),
        api::PurposeKindApi::Embedding => purposes.embedding.as_ref(),
        api::PurposeKindApi::Titling => purposes.titling.as_ref(),
        api::PurposeKindApi::Voice => purposes.voice.as_ref(),
    };
    let Some(cfg) = cfg else {
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
        // Context-window override (#51) isn't editable in this UI, but
        // `SetPurpose` is a full replace — preserve whatever the daemon
        // last reported so we don't clobber an override set elsewhere.
        max_context_tokens: *row.max_context_tokens.borrow(),
    };
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
