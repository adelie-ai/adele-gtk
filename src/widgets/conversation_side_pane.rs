//! Conversation-scoped side pane (issue #60).
//!
//! A collapsible panel, revealed to the right of the chat, bound to the
//! currently-open conversation. It shows two sections:
//!
//! 1. **Tasks** — a filtered view of the global background-task list (only
//!    tasks belonging to this conversation). The pane renders
//!    [`TaskRowViewModel`]s the window projects out of the authoritative
//!    `TasksModel`, so there is no duplicated task-lifecycle state here.
//! 2. **Scratchpad** — the conversation's scratchpad notes, grouped by
//!    `note_type` with `todo`s ordered by `sequence`. Interactive: a todo's
//!    checkbox toggles `done`, a pencil edits the content, a trash deletes it.
//!    Edits/toggles/deletes are surfaced as [`SidePaneAction`]s; the window
//!    translates them into daemon commands and the resulting `ScratchpadChanged`
//!    event refreshes the pane.
//!
//! As elsewhere in this app, the testable logic (note grouping/ordering) is a
//! set of pure functions; the GTK widget is a thin renderer over them.

use std::cell::RefCell;
use std::rc::Rc;

use desktop_assistant_api_model as api;
use gtk4::prelude::*;
use gtk4::{
    Align, Box as GtkBox, Button, CheckButton, Entry, Label, ListBox, ListBoxRow, Orientation,
    Popover, ScrolledWindow, SelectionMode, Separator,
};

use crate::widgets::tasks_panel::TaskRowViewModel;

/// The reserved goal note key — surfaced with a distinct prefix so it reads as
/// the objective rather than a plain note.
const GOAL_KEY: &str = "goal";

/// An interaction the user performed in the pane. The pane is client-agnostic;
/// the window installs a handler (capturing the live client + current
/// conversation) that turns each action into a daemon command.
#[derive(Debug, Clone, PartialEq)]
pub enum SidePaneAction {
    /// Upsert a note. Used both for check-off (`done` flipped) and content
    /// edits; the unchanged fields are echoed back so the daemon upsert keeps
    /// them.
    SetNote {
        key: String,
        content: String,
        note_type: String,
        sequence: Option<i32>,
        done: bool,
    },
    /// Delete a single note by key.
    DeleteNote { key: String },
}

/// A group of scratchpad notes sharing a `note_type`, in display order.
#[derive(Debug, Clone, PartialEq)]
pub struct NoteGroup {
    pub note_type: String,
    pub notes: Vec<api::ScratchpadNoteView>,
}

/// Group notes by `note_type` and order them for display, independently of the
/// order the server returned them in (so the rendering is stable and testable):
///
/// - within a group, by `sequence` ascending with `None` last, then by `key`;
/// - groups ordered `todo` first (the working plan), then the rest
///   alphabetically.
///
/// The reserved `goal` note is pulled out of its group and is **not** returned
/// here — the caller renders it separately above the groups.
pub fn group_notes(notes: &[api::ScratchpadNoteView]) -> Vec<NoteGroup> {
    use std::collections::BTreeMap;

    let mut by_type: BTreeMap<String, Vec<api::ScratchpadNoteView>> = BTreeMap::new();
    for note in notes {
        if note.key == GOAL_KEY {
            continue;
        }
        by_type
            .entry(note.note_type.clone())
            .or_default()
            .push(note.clone());
    }

    let mut groups: Vec<NoteGroup> = by_type
        .into_iter()
        .map(|(note_type, mut notes)| {
            notes.sort_by(|a, b| {
                seq_key(a.sequence)
                    .cmp(&seq_key(b.sequence))
                    .then_with(|| a.key.cmp(&b.key))
            });
            NoteGroup { note_type, notes }
        })
        .collect();

    // `todo` first, then the remaining types alphabetically (BTreeMap already
    // yields them sorted, so a stable sort with a todo-first key suffices).
    groups.sort_by_key(|g| (g.note_type != "todo", g.note_type.clone()));
    groups
}

/// Find the reserved `goal` note, if present.
pub fn goal_note(notes: &[api::ScratchpadNoteView]) -> Option<&api::ScratchpadNoteView> {
    notes.iter().find(|n| n.key == GOAL_KEY)
}

/// Sort key making `None` sequences sort after any concrete value.
fn seq_key(seq: Option<i32>) -> (bool, i32) {
    match seq {
        Some(v) => (false, v),
        None => (true, 0),
    }
}

type ActionCallback = Box<dyn Fn(SidePaneAction)>;
type StringCallback = Box<dyn Fn(String)>;

/// The conversation side-pane widget.
pub struct ConversationSidePane {
    pub container: GtkBox,
    tasks_list: ListBox,
    tasks_empty: Label,
    goal_label: Label,
    notes_list: ListBox,
    notes_empty: Label,
    /// Cache of the current notes so a per-row toggle/edit can reconstruct the
    /// full `SetNote` (which must echo the unchanged fields).
    notes: Rc<RefCell<Vec<api::ScratchpadNoteView>>>,
    on_action: Rc<RefCell<Option<ActionCallback>>>,
    on_cancel_task: Rc<RefCell<Option<StringCallback>>>,
}

impl ConversationSidePane {
    pub fn new() -> Self {
        let container = GtkBox::new(Orientation::Vertical, 0);
        container.set_size_request(300, -1);
        container.add_css_class("side-pane");

        let heading = Label::new(Some("This conversation"));
        heading.add_css_class("side-pane-heading");
        heading.set_halign(Align::Start);
        heading.set_margin_start(12);
        heading.set_margin_top(8);
        heading.set_margin_bottom(4);
        container.append(&heading);

        // --- Tasks section ------------------------------------------------
        let tasks_header = section_label("Tasks");
        container.append(&tasks_header);

        let tasks_scrolled = ScrolledWindow::new();
        tasks_scrolled.set_vexpand(true);
        tasks_scrolled.set_min_content_height(100);
        let tasks_list = ListBox::new();
        tasks_list.set_selection_mode(SelectionMode::None);
        tasks_list.add_css_class("side-pane-tasks");
        tasks_scrolled.set_child(Some(&tasks_list));
        container.append(&tasks_scrolled);

        let tasks_empty = Label::new(Some("No tasks for this conversation"));
        tasks_empty.add_css_class("dim-label");
        tasks_empty.set_halign(Align::Center);
        tasks_empty.set_margin_top(4);
        tasks_empty.set_margin_bottom(4);
        container.append(&tasks_empty);

        container.append(&Separator::new(Orientation::Horizontal));

        // --- Scratchpad section ------------------------------------------
        let notes_header = section_label("Scratchpad");
        container.append(&notes_header);

        let goal_label = Label::new(None);
        goal_label.add_css_class("side-pane-goal");
        goal_label.set_halign(Align::Start);
        goal_label.set_wrap(true);
        goal_label.set_margin_start(12);
        goal_label.set_margin_end(12);
        goal_label.set_margin_bottom(4);
        goal_label.set_visible(false);
        container.append(&goal_label);

        let notes_scrolled = ScrolledWindow::new();
        notes_scrolled.set_vexpand(true);
        notes_scrolled.set_min_content_height(140);
        let notes_list = ListBox::new();
        notes_list.set_selection_mode(SelectionMode::None);
        notes_list.add_css_class("side-pane-notes");
        notes_scrolled.set_child(Some(&notes_list));
        container.append(&notes_scrolled);

        let notes_empty = Label::new(Some("Scratchpad is empty"));
        notes_empty.add_css_class("dim-label");
        notes_empty.set_halign(Align::Center);
        notes_empty.set_margin_top(4);
        notes_empty.set_margin_bottom(8);
        container.append(&notes_empty);

        Self {
            container,
            tasks_list,
            tasks_empty,
            goal_label,
            notes_list,
            notes_empty,
            notes: Rc::new(RefCell::new(Vec::new())),
            on_action: Rc::new(RefCell::new(None)),
            on_cancel_task: Rc::new(RefCell::new(None)),
        }
    }

    /// Install the handler that turns a [`SidePaneAction`] into a daemon
    /// command. Called once by the window.
    pub fn set_on_action<F: Fn(SidePaneAction) + 'static>(&self, f: F) {
        *self.on_action.borrow_mut() = Some(Box::new(f));
    }

    /// Install the handler for the per-task Cancel button.
    pub fn set_on_cancel_task<F: Fn(String) + 'static>(&self, f: F) {
        *self.on_cancel_task.borrow_mut() = Some(Box::new(f));
    }

    /// Replace the tasks section with the given (already conversation-filtered)
    /// rows.
    pub fn set_tasks(&self, rows: &[TaskRowViewModel]) {
        clear_list(&self.tasks_list);
        self.tasks_empty.set_visible(rows.is_empty());
        for vm in rows {
            self.tasks_list.append(&self.build_task_row(vm));
        }
    }

    /// Replace the scratchpad section with the given notes.
    pub fn set_scratchpad(&self, notes: Vec<api::ScratchpadNoteView>) {
        // Goal note surfaced separately above the groups.
        match goal_note(&notes) {
            Some(goal) => {
                self.goal_label.set_text(&format!("Goal: {}", goal.content));
                self.goal_label.set_visible(true);
            }
            None => {
                self.goal_label.set_text("");
                self.goal_label.set_visible(false);
            }
        }

        clear_list(&self.notes_list);
        let groups = group_notes(&notes);
        let non_goal = notes.iter().filter(|n| n.key != GOAL_KEY).count();
        self.notes_empty
            .set_visible(non_goal == 0 && goal_note(&notes).is_none());

        for group in &groups {
            self.notes_list.append(&group_header_row(&group.note_type));
            for note in &group.notes {
                self.notes_list.append(&self.build_note_row(note));
            }
        }

        *self.notes.borrow_mut() = notes;
    }

    fn build_task_row(&self, vm: &TaskRowViewModel) -> ListBoxRow {
        let row = ListBoxRow::new();
        row.set_selectable(false);
        let hbox = GtkBox::new(Orientation::Horizontal, 8);
        hbox.set_margin_start(12);
        hbox.set_margin_end(8);
        hbox.set_margin_top(4);
        hbox.set_margin_bottom(4);

        // Status dot — empty label coloured entirely by CSS (mirrors the
        // tasks panel's dot).
        let dot = Label::new(None);
        dot.add_css_class("task-dot");
        dot.add_css_class(&vm.status_class);
        dot.set_width_chars(2);
        hbox.append(&dot);

        let text = Label::new(Some(&format!(
            "{}  ·  {} · {}",
            vm.title, vm.status_text, vm.age_text
        )));
        text.set_halign(Align::Start);
        text.set_hexpand(true);
        text.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        hbox.append(&text);

        // Cancel is only meaningful for in-flight tasks.
        if vm.status_text == "Running" || vm.status_text == "Pending" {
            let cancel = Button::from_icon_name("process-stop-symbolic");
            cancel.add_css_class("flat");
            cancel.set_tooltip_text(Some("Cancel task"));
            let id = vm.id.clone();
            let on_cancel = Rc::clone(&self.on_cancel_task);
            cancel.connect_clicked(move |_| {
                if let Some(cb) = on_cancel.borrow().as_ref() {
                    cb(id.clone());
                }
            });
            hbox.append(&cancel);
        }

        row.set_child(Some(&hbox));
        row
    }

    fn build_note_row(&self, note: &api::ScratchpadNoteView) -> ListBoxRow {
        let row = ListBoxRow::new();
        row.set_selectable(false);
        let hbox = GtkBox::new(Orientation::Horizontal, 6);
        hbox.set_margin_start(12);
        hbox.set_margin_end(8);
        hbox.set_margin_top(2);
        hbox.set_margin_bottom(2);

        // A todo gets a check-off box that toggles `done`.
        if note.note_type == "todo" {
            let check = CheckButton::new();
            check.set_active(note.done);
            let n = note.clone();
            let on_action = Rc::clone(&self.on_action);
            check.connect_toggled(move |c| {
                if let Some(cb) = on_action.borrow().as_ref() {
                    cb(SidePaneAction::SetNote {
                        key: n.key.clone(),
                        content: n.content.clone(),
                        note_type: n.note_type.clone(),
                        sequence: n.sequence,
                        done: c.is_active(),
                    });
                }
            });
            hbox.append(&check);
        }

        let key_label = Label::new(Some(&note.key));
        key_label.add_css_class("side-pane-note-key");
        key_label.set_halign(Align::Start);
        hbox.append(&key_label);

        let content = Label::new(Some(&note.content));
        content.set_halign(Align::Start);
        content.set_hexpand(true);
        content.set_wrap(true);
        content.set_xalign(0.0);
        if note.done {
            content.add_css_class("side-pane-note-done");
        }
        hbox.append(&content);

        // Edit (pencil) — opens a small popover entry, commits on activate.
        let edit = Button::from_icon_name("document-edit-symbolic");
        edit.add_css_class("flat");
        edit.set_tooltip_text(Some("Edit note"));
        self.wire_edit(&edit, note);
        hbox.append(&edit);

        // Delete (trash).
        let delete = Button::from_icon_name("user-trash-symbolic");
        delete.add_css_class("flat");
        delete.set_tooltip_text(Some("Delete note"));
        let key = note.key.clone();
        let on_action = Rc::clone(&self.on_action);
        delete.connect_clicked(move |_| {
            if let Some(cb) = on_action.borrow().as_ref() {
                cb(SidePaneAction::DeleteNote { key: key.clone() });
            }
        });
        hbox.append(&delete);

        row.set_child(Some(&hbox));
        row
    }

    /// Wire the pencil button to a popover containing a prefilled entry; on
    /// activate it emits a `SetNote` echoing the note's other fields with the
    /// edited content.
    fn wire_edit(&self, edit: &Button, note: &api::ScratchpadNoteView) {
        let popover = Popover::new();
        popover.set_parent(edit);
        let pbox = GtkBox::new(Orientation::Horizontal, 6);
        pbox.set_margin_start(6);
        pbox.set_margin_end(6);
        pbox.set_margin_top(6);
        pbox.set_margin_bottom(6);
        let entry = Entry::new();
        entry.set_text(&note.content);
        entry.set_width_chars(28);
        pbox.append(&entry);
        popover.set_child(Some(&pbox));

        let n = note.clone();
        let on_action = Rc::clone(&self.on_action);
        let pop_for_commit = popover.clone();
        entry.connect_activate(move |e| {
            if let Some(cb) = on_action.borrow().as_ref() {
                cb(SidePaneAction::SetNote {
                    key: n.key.clone(),
                    content: e.text().to_string(),
                    note_type: n.note_type.clone(),
                    sequence: n.sequence,
                    done: n.done,
                });
            }
            pop_for_commit.popdown();
        });

        edit.connect_clicked(move |_| {
            entry.grab_focus();
            popover.popup();
        });
    }
}

impl Default for ConversationSidePane {
    fn default() -> Self {
        Self::new()
    }
}

fn section_label(text: &str) -> Label {
    let label = Label::new(Some(text));
    label.add_css_class("side-pane-section");
    label.set_halign(Align::Start);
    label.set_margin_start(12);
    label.set_margin_top(6);
    label.set_margin_bottom(2);
    label
}

fn group_header_row(note_type: &str) -> ListBoxRow {
    let row = ListBoxRow::new();
    row.set_selectable(false);
    row.set_activatable(false);
    let label = Label::new(Some(note_type));
    label.add_css_class("side-pane-group");
    label.set_halign(Align::Start);
    label.set_margin_start(8);
    label.set_margin_top(4);
    row.set_child(Some(&label));
    row
}

fn clear_list(list: &ListBox) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(
        key: &str,
        content: &str,
        note_type: &str,
        seq: Option<i32>,
        done: bool,
    ) -> api::ScratchpadNoteView {
        api::ScratchpadNoteView {
            id: format!("id-{key}"),
            key: key.to_string(),
            content: content.to_string(),
            note_type: note_type.to_string(),
            sequence: seq,
            done,
            updated_at: "t".to_string(),
        }
    }

    #[test]
    fn group_notes_orders_todos_by_sequence_then_groups_todo_first() {
        let notes = vec![
            note("b", "second", "todo", Some(2), false),
            note("misc", "a plain note", "note", None, false),
            note("a", "first", "todo", Some(1), false),
            note("c", "third", "todo", None, false), // no sequence -> last
        ];
        let groups = group_notes(&notes);
        assert_eq!(groups.len(), 2);
        // todo group comes first.
        assert_eq!(groups[0].note_type, "todo");
        let keys: Vec<&str> = groups[0].notes.iter().map(|n| n.key.as_str()).collect();
        assert_eq!(keys, vec!["a", "b", "c"], "seq asc, nulls last");
        assert_eq!(groups[1].note_type, "note");
    }

    #[test]
    fn goal_note_is_extracted_and_excluded_from_groups() {
        let notes = vec![
            note("goal", "ship the pane", "note", None, false),
            note("n1", "a note", "note", None, false),
        ];
        assert_eq!(goal_note(&notes).unwrap().content, "ship the pane");
        let groups = group_notes(&notes);
        // The goal note must not appear in any group.
        assert!(
            groups
                .iter()
                .all(|g| g.notes.iter().all(|n| n.key != "goal")),
            "goal is rendered separately, not in a group"
        );
    }

    #[test]
    fn group_notes_empty_is_empty() {
        assert!(group_notes(&[]).is_empty());
    }
}
