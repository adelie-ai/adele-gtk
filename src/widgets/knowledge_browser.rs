//! Knowledge base browser/editor (#74). Reachable from the hamburger
//! menu — opens as a non-modal top-level window so users can keep
//! chatting while browsing.
//!
//! Async wiring: each call (list, search, save, delete) is spawned on
//! the tokio runtime via `bridge.spawn`; results flow back to the GTK
//! main thread through an internal mpsc channel consumed by
//! `glib::spawn_future_local`. Same shape the main window uses for
//! conversations, scoped to this popup so the global `UiMessage` enum
//! stays focused on chat events.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use desktop_assistant_api_model as api;
use desktop_assistant_client_common::{AssistantClient, Connector, SignalEvent};
use gtk4::prelude::*;
use gtk4::{
    Align, ApplicationWindow, Box as GtkBox, Button, Entry, HeaderBar, Label, ListBox, ListBoxRow,
    MenuButton, Orientation, Paned, Popover, ScrolledWindow, SearchEntry, SelectionMode, TextView,
    Window, WrapMode, glib,
};
use tokio::sync::mpsc;

use crate::async_bridge::AsyncBridge;

const LIST_LIMIT: u32 = 100;
const SEARCH_LIMIT: u32 = 50;
/// Search debounce window. Picked to feel responsive while avoiding a
/// daemon round-trip for every keystroke during fast typing.
const SEARCH_DEBOUNCE_MS: u32 = 250;

/// Internal messages from async work back to the main thread.
enum BrowserMsg {
    EntriesLoaded(Vec<api::KnowledgeEntryView>),
    EntrySaved(api::KnowledgeEntryView),
    EntryDeleted { id: String },
    /// A transient list-status notice (e.g. "Extraction started.") that isn't an
    /// error and doesn't carry entries.
    Notice(String),
    Error(String),
}

/// Editor state — what's currently in the right pane.
#[derive(Default)]
struct EditorState {
    /// `None` = new entry mode, `Some(id)` = editing an existing entry.
    selected_id: Option<String>,
}

struct BrowserState {
    entries: Vec<api::KnowledgeEntryView>,
    editor: EditorState,
    /// Last search query so refresh-after-save uses the same scope.
    last_query: String,
}

pub struct KnowledgeBrowser {
    pub window: Window,
}

impl KnowledgeBrowser {
    /// Build the popup. Caller is responsible for `.present()`-ing it.
    pub fn new(
        parent: &ApplicationWindow,
        transport: Arc<Connector>,
        bridge: Rc<AsyncBridge>,
    ) -> Self {
        let window = Window::builder()
            .title("Knowledge Base")
            .transient_for(parent)
            .modal(false)
            .default_width(900)
            .default_height(560)
            .build();

        let header = HeaderBar::new();
        let title_label = Label::new(Some("Knowledge Base"));
        title_label.add_css_class("title");
        header.set_title_widget(Some(&title_label));
        window.set_titlebar(Some(&header));

        let new_button = Button::with_label("+ New");
        new_button.add_css_class("suggested-action");
        header.pack_start(&new_button);

        let refresh_button = Button::from_icon_name("view-refresh-symbolic");
        refresh_button.set_tooltip_text(Some("Refresh"));
        header.pack_start(&refresh_button);

        // "Maintenance" menu — on-demand "dream cycle" controls. Grouped under a
        // popover so the header stays uncluttered. Extraction/Consolidation run
        // the knowledge passes; "Recalculate Embeddings" force-rebuilds every
        // vector and is gated behind a confirmation (it re-embeds the whole KB).
        let maint_button = MenuButton::new();
        maint_button.set_label("Maintenance");
        maint_button.set_tooltip_text(Some("Run a knowledge-maintenance pass"));
        let maint_popover = Popover::new();
        let maint_box = GtkBox::new(Orientation::Vertical, 4);
        maint_box.set_margin_start(8);
        maint_box.set_margin_end(8);
        maint_box.set_margin_top(8);
        maint_box.set_margin_bottom(8);
        let extraction_button = Button::with_label("Run Extraction");
        let consolidation_button = Button::with_label("Run Consolidation");
        let recalc_button = Button::with_label("Recalculate Embeddings");
        recalc_button.add_css_class("destructive-action");
        maint_box.append(&extraction_button);
        maint_box.append(&consolidation_button);
        maint_box.append(&recalc_button);
        maint_popover.set_child(Some(&maint_box));
        maint_button.set_popover(Some(&maint_popover));
        header.pack_end(&maint_button);

        // Two-pane layout: left = list + search; right = editor.
        let paned = Paned::new(Orientation::Horizontal);
        paned.set_position(360);
        paned.set_resize_start_child(false);
        paned.set_shrink_start_child(false);

        // --- Left pane ---
        let left_box = GtkBox::new(Orientation::Vertical, 0);
        left_box.set_size_request(280, -1);

        let search = SearchEntry::new();
        search.set_placeholder_text(Some("Search entries…"));
        search.set_margin_start(8);
        search.set_margin_end(8);
        search.set_margin_top(8);
        search.set_margin_bottom(4);
        left_box.append(&search);

        let list_scroll = ScrolledWindow::new();
        list_scroll.set_vexpand(true);
        let list_box = ListBox::new();
        list_box.set_selection_mode(SelectionMode::Single);
        list_box.add_css_class("knowledge-list");
        list_scroll.set_child(Some(&list_box));
        left_box.append(&list_scroll);

        let list_status = Label::new(Some("Loading…"));
        list_status.set_halign(Align::Start);
        list_status.set_margin_start(12);
        list_status.set_margin_top(4);
        list_status.set_margin_bottom(8);
        list_status.add_css_class("dim-label");
        left_box.append(&list_status);

        paned.set_start_child(Some(&left_box));

        // --- Right pane (editor) ---
        let right_box = GtkBox::new(Orientation::Vertical, 0);
        right_box.set_hexpand(true);
        right_box.set_margin_start(12);
        right_box.set_margin_end(12);
        right_box.set_margin_top(8);
        right_box.set_margin_bottom(8);

        let id_label = Label::new(Some("New entry"));
        id_label.set_halign(Align::Start);
        id_label.add_css_class("kb-id-label");
        right_box.append(&id_label);

        let updated_label = Label::new(Some(""));
        updated_label.set_halign(Align::Start);
        updated_label.add_css_class("dim-label");
        updated_label.set_margin_bottom(6);
        right_box.append(&updated_label);

        let content_label = Label::new(Some("Content"));
        content_label.set_halign(Align::Start);
        right_box.append(&content_label);

        let content_scroll = ScrolledWindow::new();
        content_scroll.set_vexpand(true);
        content_scroll.set_min_content_height(160);
        let content_view = TextView::new();
        content_view.set_wrap_mode(WrapMode::WordChar);
        content_view.set_top_margin(6);
        content_view.set_bottom_margin(6);
        content_view.set_left_margin(6);
        content_view.set_right_margin(6);
        content_scroll.set_child(Some(&content_view));
        right_box.append(&content_scroll);

        let tags_label = Label::new(Some("Tags (comma-separated)"));
        tags_label.set_halign(Align::Start);
        tags_label.set_margin_top(6);
        right_box.append(&tags_label);

        let tags_entry = Entry::new();
        tags_entry.set_placeholder_text(Some("preference, project:foo, instruction"));
        right_box.append(&tags_entry);

        let metadata_label = Label::new(Some("Metadata (JSON)"));
        metadata_label.set_halign(Align::Start);
        metadata_label.set_margin_top(6);
        right_box.append(&metadata_label);

        let metadata_scroll = ScrolledWindow::new();
        metadata_scroll.set_min_content_height(60);
        let metadata_view = TextView::new();
        metadata_view.set_wrap_mode(WrapMode::WordChar);
        metadata_view.set_top_margin(6);
        metadata_view.set_bottom_margin(6);
        metadata_view.set_left_margin(6);
        metadata_view.set_right_margin(6);
        metadata_view.set_monospace(true);
        // Default empty metadata renders as `{}` so users get a nudge
        // toward the expected shape rather than an empty pane.
        metadata_view.buffer().set_text("{}");
        metadata_scroll.set_child(Some(&metadata_view));
        right_box.append(&metadata_scroll);

        let editor_status = Label::new(Some(""));
        editor_status.set_halign(Align::Start);
        editor_status.add_css_class("dim-label");
        editor_status.set_margin_top(6);
        right_box.append(&editor_status);

        let button_box = GtkBox::new(Orientation::Horizontal, 8);
        button_box.set_halign(Align::End);
        button_box.set_margin_top(6);

        let delete_button = Button::with_label("Delete");
        delete_button.add_css_class("destructive-action");
        delete_button.set_sensitive(false);
        button_box.append(&delete_button);

        let save_button = Button::with_label("Save");
        save_button.add_css_class("suggested-action");
        button_box.append(&save_button);

        right_box.append(&button_box);

        paned.set_end_child(Some(&right_box));
        paned.set_resize_end_child(true);

        window.set_child(Some(&paned));

        // Shared state.
        let state = Rc::new(RefCell::new(BrowserState {
            entries: Vec::new(),
            editor: EditorState::default(),
            last_query: String::new(),
        }));

        // Internal mpsc channel for async results back to the main thread.
        let (msg_tx, mut msg_rx) = mpsc::unbounded_channel::<BrowserMsg>();

        // Refresh closure — runs on the GTK main thread, dispatches a
        // list/search call to the daemon, and routes the result back
        // through `msg_tx`. Wrapped in `Rc<dyn Fn()>` so every signal
        // handler that needs to refresh (initial load, refresh button,
        // search debouncer, post-save/delete) holds a clone of the same
        // closure rather than each maintaining their own copy of the
        // captured `transport` / `bridge` / `msg_tx`.
        let refresh: Rc<dyn Fn()> = Rc::new(glib::clone!(
            #[strong]
            transport,
            #[strong]
            bridge,
            #[strong]
            msg_tx,
            #[strong]
            state,
            move || {
                let query = state.borrow().last_query.clone();
                let transport = Arc::clone(&transport);
                let msg_tx = msg_tx.clone();
                bridge.spawn(async move {
                    let client = transport.client();
                    let result = if query.trim().is_empty() {
                        client.list_knowledge_entries(LIST_LIMIT, 0, None).await
                    } else {
                        client
                            .search_knowledge_entries(&query, None, SEARCH_LIMIT)
                            .await
                    };
                    let _ = match result {
                        Ok(entries) => msg_tx.send(BrowserMsg::EntriesLoaded(entries)),
                        Err(e) => msg_tx.send(BrowserMsg::Error(e.to_string())),
                    };
                })
            }
        ));

        // GTK-side message pump. Dropped automatically when the window
        // closes (because the cloned widget refs are dropped, breaking
        // the channel).
        glib::spawn_future_local(glib::clone!(
            #[strong]
            list_box,
            #[strong]
            list_status,
            #[strong]
            editor_status,
            #[strong]
            id_label,
            #[strong]
            updated_label,
            #[strong]
            content_view,
            #[strong]
            tags_entry,
            #[strong]
            metadata_view,
            #[strong]
            delete_button,
            #[strong]
            state,
            #[strong]
            refresh,
            async move {
                while let Some(msg) = msg_rx.recv().await {
                    match msg {
                        BrowserMsg::EntriesLoaded(entries) => {
                            populate_list(&list_box, &entries);
                            let count = entries.len();
                            list_status.set_text(&format_list_status(count));
                            state.borrow_mut().entries = entries;
                        }
                        BrowserMsg::EntrySaved(entry) => {
                            editor_status.set_text("Saved.");
                            // Reflect the saved id + timestamps back into
                            // the editor so subsequent saves update in place.
                            apply_entry_to_editor(
                                &id_label,
                                &updated_label,
                                &content_view,
                                &tags_entry,
                                &metadata_view,
                                &delete_button,
                                &entry,
                            );
                            state.borrow_mut().editor.selected_id = Some(entry.id.clone());
                            refresh();
                        }
                        BrowserMsg::EntryDeleted { id } => {
                            // If the deleted entry is the one being
                            // edited, drop the editor back to "new entry"
                            // mode so the user isn't editing a ghost.
                            let mut s = state.borrow_mut();
                            if s.editor.selected_id.as_deref() == Some(id.as_str()) {
                                s.editor.selected_id = None;
                                drop(s);
                                clear_editor(
                                    &id_label,
                                    &updated_label,
                                    &content_view,
                                    &tags_entry,
                                    &metadata_view,
                                    &delete_button,
                                );
                            }
                            editor_status.set_text("Deleted.");
                            refresh();
                        }
                        BrowserMsg::Notice(text) => {
                            list_status.set_text(&text);
                        }
                        BrowserMsg::Error(e) => {
                            editor_status.set_text(&format!("Error: {e}"));
                            list_status.set_text(&format!("Error: {e}"));
                        }
                    }
                }
            }
        ));

        // Initial load.
        refresh();

        // Live refresh: reload whenever the daemon broadcasts a knowledge change
        // — a maintenance pass writing entries as it runs, or an edit from another
        // client. The popup owns its own signal subscription (scoped here, like
        // the rest of its wiring) so no global `UiMessage` plumbing is needed; the
        // receiver — and thus the loop — drops when the window closes.
        let mut knowledge_signal_rx = transport.subscribe();
        glib::spawn_future_local(glib::clone!(
            #[strong]
            refresh,
            async move {
                while let Some(event) = knowledge_signal_rx.recv().await {
                    if matches!(event, SignalEvent::KnowledgeChanged) {
                        refresh();
                    }
                }
            }
        ));

        // Refresh button.
        refresh_button.connect_clicked(glib::clone!(
            #[strong]
            refresh,
            move |_| refresh()
        ));

        // Maintenance menu: each action spawns the pass and confirms via the
        // list status; the live-refresh subscription above repaints the list as
        // entries land. "Recalculate Embeddings" confirms first (full rebuild).
        extraction_button.connect_clicked(glib::clone!(
            #[strong]
            transport,
            #[strong]
            bridge,
            #[strong]
            msg_tx,
            #[weak]
            maint_popover,
            #[weak]
            list_status,
            move |_| {
                maint_popover.popdown();
                list_status.set_text("Extraction started…");
                spawn_maintenance(
                    &transport,
                    &bridge,
                    &msg_tx,
                    api::MaintenanceOp::Extraction,
                    "Extraction",
                );
            }
        ));
        consolidation_button.connect_clicked(glib::clone!(
            #[strong]
            transport,
            #[strong]
            bridge,
            #[strong]
            msg_tx,
            #[weak]
            maint_popover,
            #[weak]
            list_status,
            move |_| {
                maint_popover.popdown();
                list_status.set_text("Consolidation started…");
                spawn_maintenance(
                    &transport,
                    &bridge,
                    &msg_tx,
                    api::MaintenanceOp::Consolidation,
                    "Consolidation",
                );
            }
        ));
        recalc_button.connect_clicked(glib::clone!(
            #[strong]
            transport,
            #[strong]
            bridge,
            #[strong]
            msg_tx,
            #[weak]
            maint_popover,
            #[weak]
            list_status,
            #[weak(rename_to = parent_window)]
            window,
            move |_| {
                maint_popover.popdown();
                // Re-embedding the whole KB is expensive — confirm first, reusing
                // the same tiny-modal pattern as the delete confirmation.
                let confirm = Window::builder()
                    .title("Recalculate embeddings?")
                    .transient_for(&parent_window)
                    .modal(true)
                    .resizable(false)
                    .default_width(380)
                    .build();
                let layout = GtkBox::new(Orientation::Vertical, 12);
                layout.set_margin_start(16);
                layout.set_margin_end(16);
                layout.set_margin_top(16);
                layout.set_margin_bottom(16);
                let prompt = Label::new(Some(
                    "Re-embed every knowledge entry from scratch? This can take a \
                     while on a large knowledge base.",
                ));
                prompt.set_wrap(true);
                prompt.set_xalign(0.0);
                layout.append(&prompt);
                let buttons = GtkBox::new(Orientation::Horizontal, 8);
                buttons.set_halign(Align::End);
                let cancel = Button::with_label("Cancel");
                let confirm_btn = Button::with_label("Recalculate");
                confirm_btn.add_css_class("destructive-action");
                buttons.append(&cancel);
                buttons.append(&confirm_btn);
                layout.append(&buttons);
                confirm.set_child(Some(&layout));

                cancel.connect_clicked(glib::clone!(
                    #[weak]
                    confirm,
                    move |_| confirm.close()
                ));
                confirm_btn.connect_clicked(glib::clone!(
                    #[strong]
                    transport,
                    #[strong]
                    bridge,
                    #[strong]
                    msg_tx,
                    #[weak]
                    list_status,
                    #[weak(rename_to = confirm_window)]
                    confirm,
                    move |_| {
                        confirm_window.close();
                        list_status.set_text("Recalculating embeddings…");
                        spawn_maintenance(
                            &transport,
                            &bridge,
                            &msg_tx,
                            api::MaintenanceOp::RecalculateEmbeddings,
                            "Embedding recompute",
                        );
                    }
                ));

                confirm.present();
            }
        ));

        // Search entry: debounce keystrokes, then re-run the query.
        {
            let search_handle: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
            search.connect_search_changed(glib::clone!(
                #[strong]
                state,
                #[strong]
                list_status,
                #[strong]
                refresh,
                move |entry| {
                    let q = entry.text().to_string();
                    state.borrow_mut().last_query = q;
                    list_status.set_text("Searching…");
                    // Cancel any pending debounce.
                    if let Some(prev) = search_handle.borrow_mut().take() {
                        prev.remove();
                    }
                    let handle_slot = Rc::clone(&search_handle);
                    let refresh_for_timeout = Rc::clone(&refresh);
                    let timeout = glib::timeout_add_local_once(
                        std::time::Duration::from_millis(SEARCH_DEBOUNCE_MS as u64),
                        move || {
                            // Drop our slot so we don't try to remove an
                            // already-fired source on the next keystroke.
                            let _ = handle_slot.borrow_mut().take();
                            refresh_for_timeout();
                        },
                    );
                    *search_handle.borrow_mut() = Some(timeout);
                }
            ));
        }

        // List selection: load the selected entry into the editor.
        list_box.connect_row_selected(glib::clone!(
            #[strong]
            state,
            #[strong]
            id_label,
            #[strong]
            updated_label,
            #[strong]
            content_view,
            #[strong]
            tags_entry,
            #[strong]
            metadata_view,
            #[strong]
            delete_button,
            #[strong]
            editor_status,
            move |_, row| {
                let Some(row) = row else {
                    return;
                };
                let idx = row.index();
                if idx < 0 {
                    return;
                }
                let entries = state.borrow().entries.clone();
                let Some(entry) = entries.get(idx as usize) else {
                    return;
                };
                state.borrow_mut().editor.selected_id = Some(entry.id.clone());
                editor_status.set_text("");
                apply_entry_to_editor(
                    &id_label,
                    &updated_label,
                    &content_view,
                    &tags_entry,
                    &metadata_view,
                    &delete_button,
                    entry,
                );
            }
        ));

        // "+ New" button: clear the editor.
        new_button.connect_clicked(glib::clone!(
            #[strong]
            state,
            #[strong]
            id_label,
            #[strong]
            updated_label,
            #[strong]
            content_view,
            #[strong]
            tags_entry,
            #[strong]
            metadata_view,
            #[strong]
            delete_button,
            #[strong]
            editor_status,
            #[strong]
            list_box,
            move |_| {
                state.borrow_mut().editor.selected_id = None;
                list_box.unselect_all();
                clear_editor(
                    &id_label,
                    &updated_label,
                    &content_view,
                    &tags_entry,
                    &metadata_view,
                    &delete_button,
                );
                editor_status.set_text("New entry — fill in content and Save.");
                content_view.grab_focus();
            }
        ));

        // Save button: create or update.
        save_button.connect_clicked(glib::clone!(
            #[strong]
            transport,
            #[strong]
            bridge,
            #[strong]
            msg_tx,
            #[strong]
            state,
            #[strong]
            content_view,
            #[strong]
            tags_entry,
            #[strong]
            metadata_view,
            #[strong]
            editor_status,
            move |_| {
                let buffer = content_view.buffer();
                let content = buffer
                    .text(&buffer.start_iter(), &buffer.end_iter(), false)
                    .to_string();
                let trimmed = content.trim();
                if trimmed.is_empty() {
                    editor_status.set_text("Content is empty — nothing to save.");
                    return;
                }
                let tags = parse_tags(&tags_entry.text());

                let metadata_buf = metadata_view.buffer();
                let metadata_raw = metadata_buf
                    .text(&metadata_buf.start_iter(), &metadata_buf.end_iter(), false)
                    .to_string();
                let metadata = match parse_metadata(&metadata_raw) {
                    Ok(v) => v,
                    Err(e) => {
                        editor_status.set_text(&format!("Invalid metadata JSON: {e}"));
                        return;
                    }
                };

                let selected_id = state.borrow().editor.selected_id.clone();
                editor_status.set_text("Saving…");

                let transport = Arc::clone(&transport);
                let msg_tx = msg_tx.clone();
                let content_owned = content.clone();
                bridge.spawn(async move {
                    let client = transport.client();
                    let result = match selected_id {
                        Some(id) => {
                            client
                                .update_knowledge_entry(&id, &content_owned, tags, metadata)
                                .await
                        }
                        None => {
                            client
                                .create_knowledge_entry(&content_owned, tags, metadata)
                                .await
                        }
                    };
                    let _ = match result {
                        Ok(entry) => msg_tx.send(BrowserMsg::EntrySaved(entry)),
                        Err(e) => msg_tx.send(BrowserMsg::Error(e.to_string())),
                    };
                });
            }
        ));

        // Delete button: confirm via a tiny modal Window, then delete.
        // (`AlertDialog` is gated behind gtk4-rs `v4_10` which the
        // crate doesn't currently enable; rolling our own keeps the
        // dependency surface unchanged.)
        delete_button.connect_clicked(glib::clone!(
            #[strong]
            transport,
            #[strong]
            bridge,
            #[strong]
            msg_tx,
            #[strong]
            state,
            #[strong]
            editor_status,
            #[weak(rename_to = parent_window)]
            window,
            move |_| {
                let Some(id) = state.borrow().editor.selected_id.clone() else {
                    return;
                };
                let confirm = Window::builder()
                    .title("Delete entry?")
                    .transient_for(&parent_window)
                    .modal(true)
                    .resizable(false)
                    .default_width(360)
                    .build();
                let layout = GtkBox::new(Orientation::Vertical, 12);
                layout.set_margin_start(16);
                layout.set_margin_end(16);
                layout.set_margin_top(16);
                layout.set_margin_bottom(16);
                let prompt = Label::new(Some("Delete this entry? This cannot be undone."));
                prompt.set_wrap(true);
                prompt.set_xalign(0.0);
                layout.append(&prompt);
                let buttons = GtkBox::new(Orientation::Horizontal, 8);
                buttons.set_halign(Align::End);
                let cancel = Button::with_label("Cancel");
                let confirm_btn = Button::with_label("Delete");
                confirm_btn.add_css_class("destructive-action");
                buttons.append(&cancel);
                buttons.append(&confirm_btn);
                layout.append(&buttons);
                confirm.set_child(Some(&layout));

                cancel.connect_clicked(glib::clone!(
                    #[weak]
                    confirm,
                    move |_| confirm.close()
                ));
                confirm_btn.connect_clicked(glib::clone!(
                    #[strong]
                    transport,
                    #[strong]
                    bridge,
                    #[strong]
                    msg_tx,
                    #[strong]
                    editor_status,
                    #[weak(rename_to = confirm_window)]
                    confirm,
                    move |_| {
                        confirm_window.close();
                        editor_status.set_text("Deleting…");
                        let id_for_async = id.clone();
                        let transport = Arc::clone(&transport);
                        let msg_tx = msg_tx.clone();
                        bridge.spawn(async move {
                            let result = transport
                                .client()
                                .delete_knowledge_entry(&id_for_async)
                                .await;
                            let _ = match result {
                                Ok(()) => {
                                    msg_tx.send(BrowserMsg::EntryDeleted { id: id_for_async })
                                }
                                Err(e) => msg_tx.send(BrowserMsg::Error(e.to_string())),
                            };
                        });
                    }
                ));

                confirm.present();
            }
        ));

        Self { window }
    }

    pub fn present(&self) {
        self.window.present();
    }
}

// --- Local helpers ---------------------------------------------------------

/// Spawn an on-demand knowledge-maintenance pass (the "dream cycle" controls)
/// and report the trigger outcome back through the browser's message pump. The
/// pass runs as a tracked background task on the daemon; the list repaints live
/// via the `KnowledgeChanged` subscription as entries land, so we only surface a
/// "started" notice (or the error) here.
fn spawn_maintenance(
    transport: &Arc<Connector>,
    bridge: &AsyncBridge,
    msg_tx: &mpsc::UnboundedSender<BrowserMsg>,
    op: api::MaintenanceOp,
    label: &'static str,
) {
    let transport = Arc::clone(transport);
    let msg_tx = msg_tx.clone();
    bridge.spawn(async move {
        // `start_knowledge_maintenance` lives on the transport-agnostic
        // `AssistantCommands` channel (not the high-level `AssistantClient`
        // facade), reached via `as_commands()` — `Some` for every transport.
        let client = transport.client();
        let msg = match client.as_commands() {
            Some(cmds) => match cmds.start_knowledge_maintenance(op).await {
                Ok(_task_id) => BrowserMsg::Notice(format!("{label} started.")),
                Err(e) => BrowserMsg::Error(e.to_string()),
            },
            None => BrowserMsg::Error("transport has no command channel".to_string()),
        };
        let _ = msg_tx.send(msg);
    });
}

fn populate_list(list_box: &ListBox, entries: &[api::KnowledgeEntryView]) {
    while let Some(child) = list_box.first_child() {
        list_box.remove(&child);
    }
    for entry in entries {
        let row = ListBoxRow::new();
        let row_box = GtkBox::new(Orientation::Vertical, 2);
        row_box.set_margin_start(8);
        row_box.set_margin_end(8);
        row_box.set_margin_top(6);
        row_box.set_margin_bottom(6);

        let snippet = first_line_snippet(&entry.content, 80);
        let snippet_label = Label::new(Some(&snippet));
        snippet_label.set_halign(Align::Start);
        snippet_label.set_xalign(0.0);
        snippet_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        snippet_label.set_max_width_chars(40);
        row_box.append(&snippet_label);

        if !entry.tags.is_empty() {
            let tags_text = entry.tags.join(", ");
            let tags_label = Label::new(Some(&tags_text));
            tags_label.set_halign(Align::Start);
            tags_label.set_xalign(0.0);
            tags_label.add_css_class("dim-label");
            tags_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            tags_label.set_max_width_chars(40);
            row_box.append(&tags_label);
        }

        row.set_child(Some(&row_box));
        list_box.append(&row);
    }
}

fn apply_entry_to_editor(
    id_label: &Label,
    updated_label: &Label,
    content_view: &TextView,
    tags_entry: &Entry,
    metadata_view: &TextView,
    delete_button: &Button,
    entry: &api::KnowledgeEntryView,
) {
    id_label.set_text(&entry.id);
    updated_label.set_text(&format!(
        "Updated {} · created {}",
        entry.updated_at, entry.created_at
    ));
    content_view.buffer().set_text(&entry.content);
    tags_entry.set_text(&entry.tags.join(", "));
    let metadata_pretty = serde_json::to_string_pretty(&entry.metadata)
        .unwrap_or_else(|_| entry.metadata.to_string());
    metadata_view.buffer().set_text(&metadata_pretty);
    delete_button.set_sensitive(true);
}

fn clear_editor(
    id_label: &Label,
    updated_label: &Label,
    content_view: &TextView,
    tags_entry: &Entry,
    metadata_view: &TextView,
    delete_button: &Button,
) {
    id_label.set_text("New entry");
    updated_label.set_text("");
    content_view.buffer().set_text("");
    tags_entry.set_text("");
    metadata_view.buffer().set_text("{}");
    delete_button.set_sensitive(false);
}

fn parse_tags(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn parse_metadata(raw: &str) -> Result<serde_json::Value, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(serde_json::Value::Object(serde_json::Map::new()));
    }
    serde_json::from_str(trimmed).map_err(|e| e.to_string())
}

fn first_line_snippet(content: &str, max_chars: usize) -> String {
    let first = content.lines().next().unwrap_or("").trim();
    if first.is_empty() {
        return "(empty)".into();
    }
    if first.chars().count() <= max_chars {
        return first.to_string();
    }
    let truncated: String = first.chars().take(max_chars).collect();
    format!("{truncated}…")
}

fn format_list_status(count: usize) -> String {
    match count {
        0 => "No entries match.".into(),
        1 => "1 entry".into(),
        n => format!("{n} entries"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tags_splits_and_trims() {
        assert_eq!(parse_tags(""), Vec::<String>::new());
        assert_eq!(parse_tags(" foo ,bar,, baz "), vec!["foo", "bar", "baz"]);
    }

    #[test]
    fn parse_metadata_handles_empty_and_object() {
        let empty = parse_metadata("").unwrap();
        assert_eq!(empty, serde_json::json!({}));
        let parsed = parse_metadata("{\"k\":1}").unwrap();
        assert_eq!(parsed, serde_json::json!({"k": 1}));
    }

    #[test]
    fn parse_metadata_rejects_invalid_json() {
        assert!(parse_metadata("{not json").is_err());
    }

    #[test]
    fn first_line_snippet_truncates_with_ellipsis() {
        assert_eq!(first_line_snippet("hello\nworld", 80), "hello");
        let long: String = std::iter::repeat_n('x', 90).collect();
        let snip = first_line_snippet(&long, 80);
        assert!(snip.ends_with('…'));
        assert!(snip.chars().count() <= 81);
    }

    #[test]
    fn first_line_snippet_handles_empty() {
        assert_eq!(first_line_snippet("", 80), "(empty)");
        assert_eq!(first_line_snippet("   \n", 80), "(empty)");
    }

    #[test]
    fn list_status_formats() {
        assert_eq!(format_list_status(0), "No entries match.");
        assert_eq!(format_list_status(1), "1 entry");
        assert_eq!(format_list_status(7), "7 entries");
    }
}
