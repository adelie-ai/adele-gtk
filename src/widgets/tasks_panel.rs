//! Process-manager panel: list of background tasks + per-task log view.
//!
//! Issue #19. The panel listens for `UiMessage::Task*` events delivered by
//! `async_bridge::connection_manager` and drives a `gio::ListStore`-backed
//! `GtkListView` (rows) plus a `GtkTextView` (logs for the selected row).
//!
//! The unit-testable model lives in this file (`TasksModel`, `view_model_for`)
//! so the business outcomes are exercised without spinning up GTK.

use std::cell::RefCell;
use std::rc::Rc;

use desktop_assistant_api_model as api;
use gtk4::prelude::*;
use gtk4::{
    Align, Box as GtkBox, Button, Label, ListBox, ListBoxRow, Orientation, ScrolledWindow,
    SelectionMode, TextBuffer, TextView, WrapMode,
};

// --- Pure-Rust model (no GTK types) ---------------------------------------

/// View-model for a single row in the tasks list. Pure data — formatting
/// decisions live here so the model layer is unit-testable without GTK.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRowViewModel {
    pub id: String,
    pub title: String,
    /// CSS class for the status indicator dot. One of `task-dot-pending`,
    /// `task-dot-running`, `task-dot-completed`, `task-dot-failed`,
    /// `task-dot-cancelled`.
    pub status_class: String,
    /// Human-readable elapsed time, e.g. `"3s"`, `"4m 12s"`, `"1h 2m"`.
    pub age_text: String,
    /// Conversation id the task is associated with, if any. Drives the
    /// `Open Conversation` toolbar button.
    pub conversation_id: Option<String>,
}

/// Convert a `TaskView` into the row's display values given the current
/// wall-clock in epoch ms.
///
/// `now_ms` is injected (rather than read from the clock) so tests stay
/// deterministic.
pub fn view_model_for(task: &api::TaskView, now_ms: i64) -> TaskRowViewModel {
    TaskRowViewModel {
        id: task.id.0.clone(),
        title: task.title.clone(),
        status_class: status_class_for(task.status),
        age_text: format_age(task.started_at, task.ended_at, now_ms),
        conversation_id: conversation_id_for(&task.kind),
    }
}

fn status_class_for(status: api::TaskStatus) -> String {
    match status {
        api::TaskStatus::Pending => "task-dot-pending",
        api::TaskStatus::Running => "task-dot-running",
        api::TaskStatus::Completed => "task-dot-completed",
        api::TaskStatus::Failed => "task-dot-failed",
        api::TaskStatus::Cancelled => "task-dot-cancelled",
    }
    .to_string()
}

fn conversation_id_for(kind: &api::TaskKind) -> Option<String> {
    match kind {
        api::TaskKind::Conversation { conversation_id }
        | api::TaskKind::Subagent {
            conversation_id, ..
        }
        | api::TaskKind::Standalone {
            conversation_id, ..
        } => Some(conversation_id.clone()),
    }
}

/// Format the elapsed time between `started_at` and either `ended_at` or
/// `now_ms`. Negative values are clamped to `0s` defensively so a wonky
/// clock from the daemon doesn't crash the row renderer.
fn format_age(started_at: i64, ended_at: Option<i64>, now_ms: i64) -> String {
    let end = ended_at.unwrap_or(now_ms);
    let mut secs = ((end - started_at) / 1000).max(0);
    if secs < 60 {
        return format!("{secs}s");
    }
    let mut mins = secs / 60;
    secs %= 60;
    if mins < 60 {
        return format!("{mins}m {secs}s");
    }
    let hours = mins / 60;
    mins %= 60;
    format!("{hours}h {mins}m")
}

/// Pure-Rust model layer: list of tasks + per-task log buffers.
///
/// The GTK panel reads from this model and re-renders on change; tests
/// exercise the model directly without GTK.
#[derive(Debug, Default)]
pub struct TasksModel {
    tasks: Vec<api::TaskView>,
    /// Per-task append-only log buffer keyed by task id. Bounded by
    /// `LOG_BUFFER_MAX` to keep memory tame on long-running tasks.
    logs: std::collections::HashMap<String, Vec<api::TaskLogEntry>>,
}

/// Cap on log entries retained per task on the client. Mirrors the
/// daemon-side bounded buffer order of magnitude; the client only needs
/// enough scrollback for the panel.
const LOG_BUFFER_MAX: usize = 500;

impl TasksModel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of tracked tasks. Used by tests; the GTK panel queries
    /// `is_empty` to toggle the empty-state label.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &api::TaskView> {
        self.tasks.iter()
    }

    pub fn get(&self, idx: usize) -> Option<&api::TaskView> {
        self.tasks.get(idx)
    }

    pub fn position_of(&self, id: &str) -> Option<usize> {
        self.tasks.iter().position(|t| t.id.0 == id)
    }

    /// Replace the entire task list (used on `ListBackgroundTasks` reply).
    /// Resets logs for tasks that are no longer present so memory doesn't
    /// grow without bound across reconnects.
    pub fn replace_all(&mut self, tasks: Vec<api::TaskView>) {
        let live: std::collections::HashSet<String> =
            tasks.iter().map(|t| t.id.0.clone()).collect();
        self.logs.retain(|id, _| live.contains(id));
        self.tasks = tasks;
    }

    /// Insert (or replace) a task. Newest tasks go to the front of the
    /// list so a freshly-started task is immediately visible.
    pub fn upsert(&mut self, task: api::TaskView) {
        let id = task.id.0.clone();
        if let Some(idx) = self.position_of(&id) {
            self.tasks[idx] = task;
        } else {
            self.tasks.insert(0, task);
        }
    }

    /// Apply a progress-hint update to an existing task. No-op if the
    /// task id is unknown (the registry may have garbage-collected it).
    pub fn apply_progress(&mut self, id: &str, progress_hint: Option<String>) {
        if let Some(idx) = self.position_of(id) {
            self.tasks[idx].progress_hint = progress_hint;
        }
    }

    /// Append a log entry to a task's buffer. Drops the oldest entry once
    /// the buffer exceeds `LOG_BUFFER_MAX` so a chatty task can't blow up
    /// client memory.
    pub fn append_log(&mut self, id: &str, entry: api::TaskLogEntry) {
        let buf = self.logs.entry(id.to_string()).or_default();
        // Preserve seq ordering — registry sends in order; defensive insert
        // protects against rapid event bursts arriving slightly out of order.
        let pos = buf.iter().position(|e| e.seq > entry.seq);
        match pos {
            Some(i) if buf[i].seq != entry.seq => buf.insert(i, entry),
            Some(_) => { /* duplicate seq — keep the existing entry */ }
            None => {
                if buf.last().map(|e| e.seq) != Some(entry.seq) {
                    buf.push(entry);
                }
            }
        }
        if buf.len() > LOG_BUFFER_MAX {
            let drop = buf.len() - LOG_BUFFER_MAX;
            buf.drain(0..drop);
        }
    }

    pub fn logs_for(&self, id: &str) -> &[api::TaskLogEntry] {
        self.logs.get(id).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Apply a `TaskCompleted` event to the matching task.
    pub fn apply_completion(
        &mut self,
        id: &str,
        status: api::TaskStatus,
        last_error: Option<String>,
        ended_at: i64,
    ) {
        if let Some(idx) = self.position_of(id) {
            let t = &mut self.tasks[idx];
            t.status = status;
            t.last_error = last_error;
            t.ended_at = Some(ended_at);
        }
    }
}

/// Format a single log entry for the text view. Pure function — kept here
/// so the formatting is unit-testable.
pub fn format_log_line(entry: &api::TaskLogEntry) -> String {
    let level = match entry.level {
        api::LogLevel::Info => "INFO",
        api::LogLevel::Warn => "WARN",
        api::LogLevel::Error => "ERROR",
    };
    let category = match entry.category {
        api::LogCategory::ModelTurn => "model",
        api::LogCategory::ToolCall => "tool",
        api::LogCategory::ToolResult => "result",
        api::LogCategory::Status => "status",
        api::LogCategory::Lifecycle => "lifecycle",
    };
    format!("[{level} {category}] {}", entry.message)
}

// --- GTK widget ----------------------------------------------------------

type StringCallback = Box<dyn Fn(String)>;

/// Process-manager panel widget.
pub struct TasksPanel {
    pub container: GtkBox,
    pub list_box: ListBox,
    /// Held to keep the GTK widget alive even though the panel writes
    /// through `log_buffer`; dropping the view would unparent it.
    #[allow(dead_code)]
    pub log_view: TextView,
    pub log_buffer: TextBuffer,
    pub cancel_button: Button,
    pub open_conversation_button: Button,
    pub empty_label: Label,
    model: Rc<RefCell<TasksModel>>,
    on_cancel: Rc<RefCell<Option<StringCallback>>>,
    on_open_conversation: Rc<RefCell<Option<StringCallback>>>,
}

impl TasksPanel {
    pub fn new() -> Self {
        let container = GtkBox::new(Orientation::Vertical, 0);
        container.set_hexpand(true);
        container.set_vexpand(true);

        // Toolbar: Cancel | Open Conversation
        let toolbar = GtkBox::new(Orientation::Horizontal, 8);
        toolbar.set_margin_start(8);
        toolbar.set_margin_end(8);
        toolbar.set_margin_top(8);
        toolbar.set_margin_bottom(4);

        let cancel_button = Button::with_label("Cancel");
        cancel_button.add_css_class("destructive-action");
        cancel_button.set_sensitive(false);
        toolbar.append(&cancel_button);

        let open_conversation_button = Button::with_label("Open Conversation");
        open_conversation_button.set_sensitive(false);
        toolbar.append(&open_conversation_button);

        container.append(&toolbar);

        // Task list
        let list_scrolled = ScrolledWindow::new();
        list_scrolled.set_vexpand(true);
        list_scrolled.set_min_content_height(120);

        let list_box = ListBox::new();
        list_box.set_selection_mode(SelectionMode::Single);
        list_box.add_css_class("task-list");
        list_scrolled.set_child(Some(&list_box));
        container.append(&list_scrolled);

        // Empty-state label hidden until the list is empty.
        let empty_label = Label::new(Some("No background tasks"));
        empty_label.add_css_class("dim-label");
        empty_label.set_halign(Align::Center);
        empty_label.set_margin_top(8);
        empty_label.set_margin_bottom(8);
        empty_label.set_visible(true);
        container.append(&empty_label);

        // Log view (selected task)
        let log_scrolled = ScrolledWindow::new();
        log_scrolled.set_vexpand(true);
        log_scrolled.set_min_content_height(120);

        let log_buffer = TextBuffer::new(None);
        let log_view = TextView::with_buffer(&log_buffer);
        log_view.set_editable(false);
        log_view.set_monospace(true);
        log_view.set_wrap_mode(WrapMode::WordChar);
        log_view.set_top_margin(8);
        log_view.set_bottom_margin(8);
        log_view.set_left_margin(12);
        log_view.set_right_margin(12);
        log_view.add_css_class("task-log-view");
        log_scrolled.set_child(Some(&log_view));
        container.append(&log_scrolled);

        let panel = Self {
            container,
            list_box,
            log_view,
            log_buffer,
            cancel_button,
            open_conversation_button,
            empty_label,
            model: Rc::new(RefCell::new(TasksModel::new())),
            on_cancel: Rc::new(RefCell::new(None)),
            on_open_conversation: Rc::new(RefCell::new(None)),
        };

        panel.wire_selection();
        panel.wire_toolbar();
        panel
    }

    fn wire_selection(&self) {
        let model = Rc::clone(&self.model);
        let log_buffer = self.log_buffer.clone();
        let cancel_button = self.cancel_button.clone();
        let open_btn = self.open_conversation_button.clone();
        self.list_box.connect_row_selected(move |_, row| {
            let Some(row) = row else {
                log_buffer.set_text("");
                cancel_button.set_sensitive(false);
                open_btn.set_sensitive(false);
                return;
            };
            let idx = row.index() as usize;
            let m = model.borrow();
            let Some(task) = m.get(idx) else {
                cancel_button.set_sensitive(false);
                open_btn.set_sensitive(false);
                return;
            };
            // Cancel only applies to non-terminal states.
            let cancellable = matches!(
                task.status,
                api::TaskStatus::Pending | api::TaskStatus::Running
            );
            cancel_button.set_sensitive(cancellable);
            open_btn.set_sensitive(conversation_id_for(&task.kind).is_some());

            let text = m
                .logs_for(&task.id.0)
                .iter()
                .map(format_log_line)
                .collect::<Vec<_>>()
                .join("\n");
            log_buffer.set_text(&text);
        });
    }

    fn wire_toolbar(&self) {
        let model = Rc::clone(&self.model);
        let list_box = self.list_box.clone();
        let on_cancel = Rc::clone(&self.on_cancel);
        self.cancel_button.connect_clicked(move |_| {
            let Some(row) = list_box.selected_row() else {
                return;
            };
            let idx = row.index() as usize;
            let id = match model.borrow().get(idx) {
                Some(t) => t.id.0.clone(),
                None => return,
            };
            if let Some(ref cb) = *on_cancel.borrow() {
                cb(id);
            }
        });

        let model = Rc::clone(&self.model);
        let list_box = self.list_box.clone();
        let on_open = Rc::clone(&self.on_open_conversation);
        self.open_conversation_button.connect_clicked(move |_| {
            let Some(row) = list_box.selected_row() else {
                return;
            };
            let idx = row.index() as usize;
            let conv_id = match model.borrow().get(idx) {
                Some(t) => conversation_id_for(&t.kind),
                None => None,
            };
            let Some(conv_id) = conv_id else { return };
            if let Some(ref cb) = *on_open.borrow() {
                cb(conv_id);
            }
        });
    }

    pub fn connect_cancel<F: Fn(String) + 'static>(&self, f: F) {
        *self.on_cancel.borrow_mut() = Some(Box::new(f));
    }

    pub fn connect_open_conversation<F: Fn(String) + 'static>(&self, f: F) {
        *self.on_open_conversation.borrow_mut() = Some(Box::new(f));
    }

    /// Apply a `UiMessage::TaskStarted` event to the panel.
    pub fn handle_task_started(&self, task: api::TaskView, now_ms: i64) {
        self.model.borrow_mut().upsert(task);
        self.refresh_rows(now_ms);
    }

    /// Apply a `UiMessage::TaskProgress` event.
    pub fn handle_task_progress(&self, id: String, progress_hint: Option<String>, now_ms: i64) {
        self.model.borrow_mut().apply_progress(&id, progress_hint);
        self.refresh_rows(now_ms);
    }

    /// Apply a `UiMessage::TaskLogAppended` event.
    pub fn handle_task_log_appended(&self, id: String, entry: api::TaskLogEntry) {
        self.model.borrow_mut().append_log(&id, entry);
        // If the appended entry belongs to the currently-selected task, repaint
        // the log buffer so the user sees new lines without re-clicking.
        if let Some(row) = self.list_box.selected_row() {
            let idx = row.index() as usize;
            let m = self.model.borrow();
            if let Some(t) = m.get(idx)
                && t.id.0 == id
            {
                let text = m
                    .logs_for(&id)
                    .iter()
                    .map(format_log_line)
                    .collect::<Vec<_>>()
                    .join("\n");
                self.log_buffer.set_text(&text);
            }
        }
    }

    /// Apply a `UiMessage::TaskCompleted` event.
    pub fn handle_task_completed(
        &self,
        id: String,
        status: api::TaskStatus,
        last_error: Option<String>,
        now_ms: i64,
    ) {
        self.model
            .borrow_mut()
            .apply_completion(&id, status, last_error, now_ms);
        self.refresh_rows(now_ms);
    }

    /// Replace the entire task list (used on initial `ListBackgroundTasks`
    /// reply and on reconnect).
    pub fn replace_all(&self, tasks: Vec<api::TaskView>, now_ms: i64) {
        self.model.borrow_mut().replace_all(tasks);
        self.refresh_rows(now_ms);
    }

    fn refresh_rows(&self, now_ms: i64) {
        // Preserve selection by id.
        let selected_id = self.list_box.selected_row().and_then(|row| {
            let idx = row.index() as usize;
            self.model.borrow().get(idx).map(|t| t.id.0.clone())
        });

        while let Some(child) = self.list_box.first_child() {
            self.list_box.remove(&child);
        }

        let m = self.model.borrow();
        for task in m.iter() {
            let vm = view_model_for(task, now_ms);
            let row = build_row(&vm, task.progress_hint.as_deref());
            self.list_box.append(&row);
        }
        drop(m);

        self.empty_label.set_visible(self.model.borrow().is_empty());

        if let Some(id) = selected_id
            && let Some(idx) = self.model.borrow().position_of(&id)
            && let Some(row) = self.list_box.row_at_index(idx as i32)
        {
            self.list_box.select_row(Some(&row));
        }
    }

    /// Test/integration accessor for the underlying model. Public so smoke
    /// tests can inspect state without poking the widget tree.
    #[allow(dead_code)]
    pub fn model(&self) -> std::cell::Ref<'_, TasksModel> {
        self.model.borrow()
    }
}

fn build_row(vm: &TaskRowViewModel, progress_hint: Option<&str>) -> ListBoxRow {
    let row = ListBoxRow::new();
    let hbox = GtkBox::new(Orientation::Horizontal, 8);
    hbox.set_margin_start(12);
    hbox.set_margin_end(12);
    hbox.set_margin_top(6);
    hbox.set_margin_bottom(6);

    // Status dot — empty label whose colour is driven entirely by CSS.
    let dot = Label::new(None);
    dot.add_css_class("task-dot");
    dot.add_css_class(&vm.status_class);
    dot.set_width_chars(2);
    hbox.append(&dot);

    let vbox = GtkBox::new(Orientation::Vertical, 2);
    vbox.set_hexpand(true);

    let title_label = Label::new(Some(&vm.title));
    title_label.set_halign(Align::Start);
    title_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    vbox.append(&title_label);

    if let Some(hint) = progress_hint {
        let hint_label = Label::new(Some(hint));
        hint_label.add_css_class("dim-label");
        hint_label.set_halign(Align::Start);
        hint_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        vbox.append(&hint_label);
    }
    hbox.append(&vbox);

    let age_label = Label::new(Some(&vm.age_text));
    age_label.add_css_class("dim-label");
    hbox.append(&age_label);

    row.set_child(Some(&hbox));
    row
}

// --- Tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use desktop_assistant_api_model as api;

    fn running_task(id: &str, conv: &str) -> api::TaskView {
        api::TaskView {
            id: api::TaskId(id.into()),
            kind: api::TaskKind::Standalone {
                name: "agent".into(),
                conversation_id: conv.into(),
            },
            status: api::TaskStatus::Running,
            started_at: 1_700_000_000_000,
            ended_at: None,
            last_error: None,
            parent: None,
            children: vec![],
            title: format!("Task {id}"),
            progress_hint: None,
        }
    }

    fn log_entry(seq: u64, message: &str) -> api::TaskLogEntry {
        api::TaskLogEntry {
            seq,
            timestamp: 1_700_000_000_000,
            level: api::LogLevel::Info,
            category: api::LogCategory::Status,
            message: message.into(),
            data: None,
        }
    }

    #[test]
    fn task_row_view_model_from_task_view() {
        let t = running_task("t-1", "conv-9");
        let now = t.started_at + 12_345; // ~12 seconds later
        let vm = view_model_for(&t, now);

        assert_eq!(vm.id, "t-1");
        assert_eq!(vm.title, "Task t-1");
        assert_eq!(vm.status_class, "task-dot-running");
        assert_eq!(vm.age_text, "12s");
        assert_eq!(vm.conversation_id.as_deref(), Some("conv-9"));
    }

    #[test]
    fn task_row_view_model_completed_uses_ended_at_not_now() {
        let mut t = running_task("t-end", "c");
        t.status = api::TaskStatus::Completed;
        t.ended_at = Some(t.started_at + 5_000);
        let now = t.started_at + 9_999_000; // far in the future
        let vm = view_model_for(&t, now);
        assert_eq!(vm.status_class, "task-dot-completed");
        assert_eq!(vm.age_text, "5s");
    }

    #[test]
    fn task_row_view_model_clamps_negative_age() {
        // Defensive: a daemon with a clock skew shouldn't crash the panel.
        let mut t = running_task("t-clock", "c");
        t.started_at = 1_000_000;
        let now = 999_000; // earlier than started_at
        let vm = view_model_for(&t, now);
        assert_eq!(vm.age_text, "0s");
    }

    #[test]
    fn task_row_view_model_formats_minutes_and_hours() {
        let mut t = running_task("t-mins", "c");
        t.started_at = 0;
        assert_eq!(view_model_for(&t, 90_000).age_text, "1m 30s");
        assert_eq!(view_model_for(&t, 3_725_000).age_text, "1h 2m");
    }

    #[test]
    fn internal_msg_task_started_inserts_row_in_store() {
        // Business outcome: a TaskStarted event lands in the model and the
        // row count goes from 0 to 1 without any GTK widget involvement.
        let mut model = TasksModel::new();
        assert_eq!(model.len(), 0);
        model.upsert(running_task("t-1", "c"));
        assert_eq!(model.len(), 1);
        assert_eq!(model.get(0).unwrap().id.0, "t-1");
    }

    #[test]
    fn business_outcome_user_can_see_their_running_standalone_agent_appear_in_real_time() {
        // Acceptance criterion (issue #19): launching a standalone agent
        // surfaces a new row in the panel without manual refresh.
        let mut model = TasksModel::new();
        model.upsert(running_task("agent-1", "conv-1"));
        let view = model.iter().collect::<Vec<_>>();
        assert_eq!(view.len(), 1);
        assert_eq!(view[0].title, "Task agent-1");
        assert_eq!(view[0].status, api::TaskStatus::Running);
    }

    #[test]
    fn internal_msg_task_log_appended_appends_to_buffer() {
        let mut model = TasksModel::new();
        model.upsert(running_task("t-log", "c"));
        model.append_log("t-log", log_entry(1, "step 1"));
        model.append_log("t-log", log_entry(2, "step 2"));
        let logs = model.logs_for("t-log");
        assert_eq!(logs.len(), 2);
        assert_eq!(logs[0].message, "step 1");
        assert_eq!(logs[1].message, "step 2");
    }

    #[test]
    fn rapid_event_burst_preserves_seq_ordering() {
        // Unhappy path: events arrive out of order during a burst — the
        // model must store them sorted by seq so the log view doesn't
        // show stale rewinds.
        let mut model = TasksModel::new();
        model.append_log("t", log_entry(3, "third"));
        model.append_log("t", log_entry(1, "first"));
        model.append_log("t", log_entry(2, "second"));
        let logs = model.logs_for("t");
        assert_eq!(
            logs.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn duplicate_log_seq_does_not_duplicate_entries() {
        // Unhappy path: a transient that resends the same seq must not
        // double up the buffer.
        let mut model = TasksModel::new();
        model.append_log("t", log_entry(1, "first"));
        model.append_log("t", log_entry(1, "first"));
        assert_eq!(model.logs_for("t").len(), 1);
    }

    #[test]
    fn log_buffer_is_bounded() {
        let mut model = TasksModel::new();
        for i in 0..(LOG_BUFFER_MAX as u64 + 50) {
            model.append_log("t", log_entry(i, "x"));
        }
        assert_eq!(model.logs_for("t").len(), LOG_BUFFER_MAX);
        // Oldest entries dropped — the buffer keeps the highest-seq tail.
        let logs = model.logs_for("t");
        assert!(logs.first().unwrap().seq >= 50);
    }

    #[test]
    fn apply_progress_updates_existing_task() {
        let mut model = TasksModel::new();
        model.upsert(running_task("t-1", "c"));
        model.apply_progress("t-1", Some("step 2/4".into()));
        assert_eq!(
            model.get(0).unwrap().progress_hint.as_deref(),
            Some("step 2/4")
        );
    }

    #[test]
    fn apply_progress_for_unknown_task_is_noop() {
        // Unhappy path: a stray TaskProgress for an id we never saw must
        // not crash and must not introduce a phantom row.
        let mut model = TasksModel::new();
        model.apply_progress("ghost", Some("hint".into()));
        assert_eq!(model.len(), 0);
    }

    #[test]
    fn apply_completion_transitions_status() {
        let mut model = TasksModel::new();
        model.upsert(running_task("t-end", "c"));
        model.apply_completion(
            "t-end",
            api::TaskStatus::Failed,
            Some("boom".into()),
            1_700_000_999_000,
        );
        let t = model.get(0).unwrap();
        assert_eq!(t.status, api::TaskStatus::Failed);
        assert_eq!(t.last_error.as_deref(), Some("boom"));
        assert_eq!(t.ended_at, Some(1_700_000_999_000));
    }

    #[test]
    fn replace_all_clears_orphan_log_buffers() {
        // Unhappy path: after reconnect, ListBackgroundTasks returns a new
        // snapshot. Per-task log buffers for tasks no longer in the list
        // should be reclaimed so memory doesn't grow without bound.
        let mut model = TasksModel::new();
        model.upsert(running_task("old", "c"));
        model.append_log("old", log_entry(1, "x"));
        assert_eq!(model.logs_for("old").len(), 1);

        model.replace_all(vec![running_task("new", "c")]);
        assert_eq!(model.len(), 1);
        assert_eq!(model.logs_for("old").len(), 0);
    }

    #[test]
    fn upsert_replaces_existing_task_in_place() {
        let mut model = TasksModel::new();
        model.upsert(running_task("t-1", "c"));
        let mut updated = running_task("t-1", "c");
        updated.title = "renamed".into();
        model.upsert(updated);
        assert_eq!(model.len(), 1);
        assert_eq!(model.get(0).unwrap().title, "renamed");
    }

    #[test]
    fn newest_task_appears_at_top_of_list() {
        let mut model = TasksModel::new();
        model.upsert(running_task("older", "c"));
        model.upsert(running_task("newer", "c"));
        assert_eq!(model.get(0).unwrap().id.0, "newer");
        assert_eq!(model.get(1).unwrap().id.0, "older");
    }

    #[test]
    fn malformed_task_kind_does_not_crash_view_model() {
        // Unhappy path: a Conversation-kind task with empty conv id is
        // still surfaced as a row — the model should not panic on edge data.
        let t = api::TaskView {
            id: api::TaskId("t".into()),
            kind: api::TaskKind::Conversation {
                conversation_id: String::new(),
            },
            status: api::TaskStatus::Running,
            started_at: 0,
            ended_at: None,
            last_error: None,
            parent: None,
            children: vec![],
            title: "t".into(),
            progress_hint: None,
        };
        let vm = view_model_for(&t, 1_000);
        assert_eq!(vm.conversation_id.as_deref(), Some(""));
    }

    #[test]
    fn format_log_line_includes_level_and_category() {
        let mut e = log_entry(1, "calling search");
        e.level = api::LogLevel::Warn;
        e.category = api::LogCategory::ToolCall;
        let line = format_log_line(&e);
        assert!(line.contains("WARN"));
        assert!(line.contains("tool"));
        assert!(line.contains("calling search"));
    }

    // --- Controller tests ------------------------------------------------
    //
    // The GTK widget itself can't easily be instantiated under `cargo test`
    // (no display, no GTK init). These tests target the small controller
    // helpers the widget delegates to so the business logic — "Cancel
    // emits the right id" / "Open Conversation routes to the right id" —
    // is verifiable without a display.

    /// Mirror of the controller logic in `TasksPanel::wire_toolbar` for the
    /// cancel button. Pulled into a free function so it can be unit-tested
    /// without instantiating a GTK widget.
    fn cancel_id_for_selection(model: &TasksModel, selected: Option<usize>) -> Option<String> {
        selected.and_then(|i| model.get(i)).map(|t| t.id.0.clone())
    }

    /// Mirror of the open-conversation controller logic.
    fn open_conversation_id_for_selection(
        model: &TasksModel,
        selected: Option<usize>,
    ) -> Option<String> {
        selected
            .and_then(|i| model.get(i))
            .and_then(|t| conversation_id_for(&t.kind))
    }

    #[test]
    fn cancel_button_emits_cancel_command_for_selection() {
        let mut model = TasksModel::new();
        model.upsert(running_task("first", "c1"));
        model.upsert(running_task("second", "c2"));
        // newest is at index 0
        assert_eq!(
            cancel_id_for_selection(&model, Some(0)).as_deref(),
            Some("second")
        );
        assert_eq!(
            cancel_id_for_selection(&model, Some(1)).as_deref(),
            Some("first")
        );
    }

    #[test]
    fn cancel_with_no_selection_is_noop() {
        let model = TasksModel::new();
        assert_eq!(cancel_id_for_selection(&model, None), None);
    }

    #[test]
    fn cancel_with_stale_index_is_noop() {
        // Unhappy path: the selected row index could be beyond the current
        // model (e.g. a TaskCompleted just shrank the list).
        let model = TasksModel::new();
        assert_eq!(cancel_id_for_selection(&model, Some(42)), None);
    }

    #[test]
    fn open_conversation_routes_to_correct_id() {
        let mut model = TasksModel::new();
        model.upsert(running_task("agent", "the-conversation"));
        assert_eq!(
            open_conversation_id_for_selection(&model, Some(0)).as_deref(),
            Some("the-conversation")
        );
    }

    #[test]
    fn open_conversation_with_no_selection_is_noop() {
        let model = TasksModel::new();
        assert_eq!(open_conversation_id_for_selection(&model, None), None);
    }
}
