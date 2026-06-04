use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use desktop_assistant_api_model as api;
use desktop_assistant_client_common::{
    AssistantClient, ChatMessage, ConnectionConfig, ConversationDetail, ConversationSummary,
    TransportClient,
};
use gtk4::prelude::*;
use gtk4::{
    Align, Application, ApplicationWindow, Box as GtkBox, Button, CheckButton, Entry, Label,
    MenuButton, Orientation, Paned, Popover, Revealer, RevealerTransitionType, Separator, Stack,
    StackSwitcher, Window, gdk, glib,
};
use tokio::sync::mpsc;

use crate::async_bridge::{AsyncBridge, UiMessage, connection_manager};
use crate::management_client;
use crate::widgets::chat_view::ChatView;
use crate::widgets::conversation_side_pane::{ConversationSidePane, SidePaneAction};
use crate::widgets::input_bar::InputBar;
use crate::widgets::model_picker::ModelPicker;
use crate::widgets::sidebar::Sidebar;
use crate::widgets::tasks_panel::TasksPanel;

/// Shared mutable state for the window.
#[derive(Default)]
struct WindowState {
    conversations: Vec<ConversationSummary>,
    current_conversation_id: Option<String>,
    current_conversation: Option<ConversationDetail>,
    pending_request_id: Option<String>,
    streaming_buffer: String,
    debug_enabled: bool,
}

/// A single observable side-effect produced by [`WindowState::apply`].
///
/// `apply` is a pure decision function: it mutates `WindowState` and returns
/// the list of effects to perform, but performs none of them itself (no GTK,
/// no widget refs, no spawns). The thin executor in [`handle_ui_message`]
/// walks the returned `Vec<Effect>` in order and performs each against the
/// real widgets — mirroring the `TasksModel`/`apply` shape already used by
/// `widgets/tasks_panel.rs`. This keeps the entire state-machine decision
/// logic unit-testable without a live GTK context.
///
/// Effects are emitted in the exact order the legacy `handle_ui_message`
/// performed them, so the observable behavior is identical.
enum Effect {
    /// Stash the freshly connected transport in the window's client cell.
    SetClient(Arc<TransportClient>),
    /// Clear the client cell (on disconnect).
    ClearClient,
    /// Set the bottom status-bar text verbatim.
    SetStatusText(String),
    /// Enable/disable the send button.
    SetSendSensitive(bool),
    /// Repaint the sidebar conversation list.
    SetConversations(Vec<ConversationSummary>),
    /// Run `ensure_active_conversation` (selection sync + auto-load/-create).
    /// Kept as an effect because it needs the live client + ui_tx and spawns
    /// async RPCs; the *decision* to run it lives in `apply`.
    EnsureActiveConversation,
    /// Load an (already debug-filtered) conversation into the chat view.
    LoadConversationIntoChat(ConversationDetail),
    /// Clear the chat view.
    ClearChat,
    /// Set the chat's transient status line.
    SetChatStatus(String),
    /// Clear the chat's transient status line.
    ClearChatStatus,
    /// Append a streaming chunk to the chat view.
    ReceiveChunk(String),
    /// Finalize a streaming response in the chat view.
    CompleteStreaming(String),
    /// Apply (or clear, with `None`) the model-picker selection.
    SetModelSelection(Option<api::ConversationModelSelectionView>),
    /// Replace the model-picker's available models.
    SetModels(Vec<api::ModelListing>),
    /// Set the picker's resolved interactive-purpose default (issue #53). Used
    /// as the fallback selection for conversations with no stored selection so
    /// the button shows a concrete model instead of "(default)".
    SetDefaultModel(Option<crate::selected_models::SelectedModel>),
    /// Show/hide the model picker.
    SetModelPickerVisible(bool),
    /// Reveal a passive toast with the given message.
    ShowToast(String),
    /// Replace the entire background-task list.
    TasksReplaceAll(Vec<api::TaskView>),
    /// A task started.
    TaskStarted(api::TaskView),
    /// A task progress update.
    TaskProgress {
        id: String,
        progress_hint: Option<String>,
    },
    /// A task log line was appended.
    TaskLogAppended {
        id: String,
        entry: api::TaskLogEntry,
    },
    /// A task completed (terminal).
    TaskCompleted { id: String },

    // --- Conversation side pane (issue #60) -------------------------------
    /// Fetch the scratchpad for the given conversation (async RPC + ui_tx),
    /// mirroring `EnsureActiveConversation`. The reply arrives as
    /// `UiMessage::ConversationScratchpadLoaded`.
    FetchScratchpad(String),
    /// Replace the side pane's scratchpad notes (empty clears it).
    SidePaneSetScratchpad(Vec<api::ScratchpadNoteView>),
    /// Recompute the side pane's task list from the authoritative `TasksModel`,
    /// filtered to the active conversation.
    RefreshSidePaneTasks,
}

// Manual `Debug` (can't derive: `TransportClient` is not `Debug`, mirroring
// `UiMessage`). Only `SetClient` needs special handling — it prints a marker
// instead of the opaque transport; every other variant forwards its fields so
// test panic messages stay informative.
impl std::fmt::Debug for Effect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Effect::SetClient(_) => f.debug_tuple("SetClient").field(&"<transport>").finish(),
            Effect::ClearClient => f.write_str("ClearClient"),
            Effect::SetStatusText(t) => f.debug_tuple("SetStatusText").field(t).finish(),
            Effect::SetSendSensitive(b) => f.debug_tuple("SetSendSensitive").field(b).finish(),
            Effect::SetConversations(c) => f.debug_tuple("SetConversations").field(c).finish(),
            Effect::EnsureActiveConversation => f.write_str("EnsureActiveConversation"),
            Effect::LoadConversationIntoChat(d) => {
                f.debug_tuple("LoadConversationIntoChat").field(d).finish()
            }
            Effect::ClearChat => f.write_str("ClearChat"),
            Effect::SetChatStatus(m) => f.debug_tuple("SetChatStatus").field(m).finish(),
            Effect::ClearChatStatus => f.write_str("ClearChatStatus"),
            Effect::ReceiveChunk(c) => f.debug_tuple("ReceiveChunk").field(c).finish(),
            Effect::CompleteStreaming(c) => f.debug_tuple("CompleteStreaming").field(c).finish(),
            Effect::SetModelSelection(s) => f.debug_tuple("SetModelSelection").field(s).finish(),
            Effect::SetModels(m) => f.debug_tuple("SetModels").field(m).finish(),
            Effect::SetDefaultModel(m) => f.debug_tuple("SetDefaultModel").field(m).finish(),
            Effect::SetModelPickerVisible(v) => {
                f.debug_tuple("SetModelPickerVisible").field(v).finish()
            }
            Effect::ShowToast(m) => f.debug_tuple("ShowToast").field(m).finish(),
            Effect::TasksReplaceAll(t) => f.debug_tuple("TasksReplaceAll").field(t).finish(),
            Effect::TaskStarted(t) => f.debug_tuple("TaskStarted").field(t).finish(),
            Effect::TaskProgress { id, progress_hint } => f
                .debug_struct("TaskProgress")
                .field("id", id)
                .field("progress_hint", progress_hint)
                .finish(),
            Effect::TaskLogAppended { id, entry } => f
                .debug_struct("TaskLogAppended")
                .field("id", id)
                .field("entry", entry)
                .finish(),
            Effect::TaskCompleted { id } => {
                f.debug_struct("TaskCompleted").field("id", id).finish()
            }
            Effect::FetchScratchpad(c) => f.debug_tuple("FetchScratchpad").field(c).finish(),
            Effect::SidePaneSetScratchpad(n) => {
                f.debug_tuple("SidePaneSetScratchpad").field(n).finish()
            }
            Effect::RefreshSidePaneTasks => f.write_str("RefreshSidePaneTasks"),
        }
    }
}

impl WindowState {
    /// Apply a `UiMessage` to the window state, returning the side-effects to
    /// perform. PURE: mutates `self` and returns effects; performs no GTK
    /// work and holds no widget refs.
    ///
    /// Every `UiMessage` variant is handled here; the executor in
    /// `handle_ui_message` is a mechanical translation of the returned
    /// effects into widget calls.
    fn apply(&mut self, msg: UiMessage) -> Vec<Effect> {
        match msg {
            UiMessage::ClientReady(transport) => {
                // The connection_manager handed us a freshly connected
                // transport. Stash it so the rest of the UI can issue RPCs;
                // this arrives before `ConversationsLoaded`, which relies on
                // the client cell.
                vec![Effect::SetClient(transport)]
            }
            UiMessage::ConversationsLoaded(convs) => {
                self.conversations = convs.clone();
                vec![
                    Effect::SetConversations(convs),
                    Effect::EnsureActiveConversation,
                ]
            }
            UiMessage::ConversationLoaded(detail) => {
                let id = detail.id.clone();
                let filtered = filter_messages(&detail, self.debug_enabled);
                let selection = detail.model_selection.clone();
                self.current_conversation = Some(detail);
                self.current_conversation_id = Some(id.clone());
                vec![
                    Effect::SetModelSelection(selection),
                    Effect::LoadConversationIntoChat(filtered),
                    // Rebind the side pane to the new conversation: clear stale
                    // notes until the fetch returns, refresh the filtered task
                    // list, and fetch this conversation's scratchpad.
                    Effect::SidePaneSetScratchpad(Vec::new()),
                    Effect::RefreshSidePaneTasks,
                    Effect::FetchScratchpad(id),
                ]
            }
            UiMessage::ConversationCreated { id } => {
                self.current_conversation_id = Some(id);
                vec![]
            }
            UiMessage::ConversationDeleted { id } => {
                self.conversations.retain(|c| c.id != id);
                let is_active = self.current_conversation_id.as_deref() == Some(&id);
                if is_active {
                    self.current_conversation_id = None;
                    self.current_conversation = None;
                }
                let convs = self.conversations.clone();
                let mut effects = vec![Effect::SetConversations(convs)];
                if is_active {
                    effects.push(Effect::ClearChat);
                    effects.push(Effect::SidePaneSetScratchpad(Vec::new()));
                    effects.push(Effect::RefreshSidePaneTasks);
                    effects.push(Effect::EnsureActiveConversation);
                }
                effects
            }
            UiMessage::ConversationRenamed { id, title } => {
                for conv in &mut self.conversations {
                    if conv.id == id {
                        conv.title = title.clone();
                    }
                }
                vec![Effect::SetConversations(self.conversations.clone())]
            }
            UiMessage::PromptSent { task_id: _ } => {
                // The wire ack carries either a `task_id` (post-#114
                // `SendMessageAck`) or an empty string (legacy `Ack`). Neither
                // is the chunk-stream `request_id` — that is daemon-generated
                // and arrives inside the first `AssistantDelta`. Use the
                // sentinel until then; `StreamChunk` claims it on first frame.
                // See issue #31.
                self.pending_request_id = Some("__pending__".to_string());
                self.streaming_buffer.clear();
                vec![]
            }
            UiMessage::AssistantStatus {
                request_id,
                message,
            } => {
                if self.pending_request_id.as_deref() == Some(&request_id)
                    || self.pending_request_id.as_deref() == Some("__pending__")
                {
                    vec![Effect::SetChatStatus(message)]
                } else {
                    vec![]
                }
            }
            UiMessage::StreamChunk { request_id, chunk } => {
                // Claim request ID if pending
                if self.pending_request_id.as_deref() == Some("__pending__") {
                    self.pending_request_id = Some(request_id.clone());
                }
                if self.pending_request_id.as_deref() == Some(&request_id) {
                    let first_chunk = self.streaming_buffer.is_empty();
                    self.streaming_buffer.push_str(&chunk);
                    let mut effects = Vec::new();
                    if first_chunk {
                        effects.push(Effect::ClearChatStatus);
                    }
                    effects.push(Effect::ReceiveChunk(chunk));
                    effects
                } else {
                    vec![]
                }
            }
            UiMessage::StreamComplete {
                request_id,
                full_response,
            } => {
                if self.pending_request_id.as_deref() == Some("__pending__") {
                    self.pending_request_id = Some(request_id.clone());
                }
                if self.pending_request_id.as_deref() == Some(&request_id) {
                    self.pending_request_id = None;
                    self.streaming_buffer.clear();
                    if let Some(ref mut conv) = self.current_conversation {
                        conv.messages.push(ChatMessage {
                            role: "assistant".to_string(),
                            content: full_response.clone(),
                        });
                    }
                    let mut effects = vec![
                        Effect::ClearChatStatus,
                        Effect::CompleteStreaming(full_response),
                    ];
                    // The turn may have changed the scratchpad (Adele's todos);
                    // refresh the pane. (The live `ScratchpadChanged` event also
                    // covers this, but a turn-boundary refetch is a cheap
                    // backstop if the event was missed.)
                    if let Some(id) = self.current_conversation_id.clone() {
                        effects.push(Effect::FetchScratchpad(id));
                    }
                    effects
                } else {
                    vec![]
                }
            }
            UiMessage::StreamError { request_id, error } => {
                if self.pending_request_id.as_deref() == Some("__pending__") {
                    self.pending_request_id = Some(request_id.clone());
                }
                if self.pending_request_id.as_deref() == Some(&request_id) {
                    self.pending_request_id = None;
                    self.streaming_buffer.clear();
                    vec![
                        Effect::ClearChatStatus,
                        Effect::SetStatusText(format!("Error: {error}")),
                    ]
                } else {
                    vec![]
                }
            }
            UiMessage::TitleChanged {
                conversation_id,
                title,
            } => {
                for conv in &mut self.conversations {
                    if conv.id == conversation_id {
                        conv.title = title.clone();
                    }
                }
                vec![Effect::SetConversations(self.conversations.clone())]
            }
            UiMessage::ConversationWarning {
                conversation_id,
                warning,
            } => {
                // Single variant today — DanglingModelSelection. The daemon has
                // already cleared its side and fell back; if this is the
                // currently-open conversation, clear the header picker so it
                // doesn't show a stale "stuck" model, then surface a passive
                // toast explaining the fallback.
                match &warning {
                    api::ConversationWarning::DanglingModelSelection {
                        previous_selection,
                        fallback_to,
                    } => {
                        let is_current = self.current_conversation_id.as_deref()
                            == Some(conversation_id.as_str());
                        let mut effects = Vec::new();
                        if is_current {
                            effects.push(Effect::SetModelSelection(None));
                            // Also clear the cached detail's selection so a
                            // later `ModelsLoaded` doesn't re-apply the stale
                            // dangling selection, contradicting this toast.
                            if let Some(ref mut conv) = self.current_conversation {
                                conv.model_selection = None;
                            }
                        }
                        let message = format!(
                            "The model \"{}\" on connection \"{}\" is no longer available — falling back to \"{}\" on \"{}\".",
                            previous_selection.model_id,
                            previous_selection.connection_id,
                            fallback_to.model_id,
                            fallback_to.connection_id,
                        );
                        effects.push(Effect::ShowToast(message));
                        effects
                    }
                }
            }
            UiMessage::StatusUpdate(text) => vec![Effect::SetStatusText(text)],
            UiMessage::Error(text) => vec![Effect::SetStatusText(format!("Error: {text}"))],
            UiMessage::ModelsLoaded(listings) => {
                let visible = !listings.is_empty();
                let mut effects = vec![Effect::SetModels(listings)];
                // Re-apply the active conversation's stored selection (if any)
                // since `set_models` resets the dropdown.
                if let Some(ref detail) = self.current_conversation {
                    effects.push(Effect::SetModelSelection(detail.model_selection.clone()));
                }
                effects.push(Effect::SetModelPickerVisible(visible));
                effects
            }
            UiMessage::DefaultModelLoaded(default) => {
                // The picker uses this as the fallback selection for
                // conversations with no stored selection. Set it independently
                // of `set_selection`; the picker re-resolves
                // stored-or-default on every conversation load, so ordering
                // between the two only requires both to have run.
                vec![Effect::SetDefaultModel(default)]
            }
            UiMessage::Connected { label } => {
                vec![Effect::SetStatusText(label), Effect::SetSendSensitive(true)]
            }
            UiMessage::TasksLoaded(tasks) => {
                vec![Effect::TasksReplaceAll(tasks), Effect::RefreshSidePaneTasks]
            }
            UiMessage::TaskStarted(task) => {
                vec![Effect::TaskStarted(task), Effect::RefreshSidePaneTasks]
            }
            UiMessage::TaskProgress { id, progress_hint } => {
                vec![
                    Effect::TaskProgress { id, progress_hint },
                    Effect::RefreshSidePaneTasks,
                ]
            }
            UiMessage::TaskLogAppended { id, entry } => {
                // Log lines don't change the row set, so the side pane (which
                // shows no logs) needs no refresh here.
                vec![Effect::TaskLogAppended { id, entry }]
            }
            UiMessage::TaskCompleted { id } => {
                vec![Effect::TaskCompleted { id }, Effect::RefreshSidePaneTasks]
            }
            UiMessage::ConversationScratchpadLoaded {
                conversation_id,
                notes,
            } => {
                // Apply only if it's still the active conversation (a fetch may
                // race a conversation switch).
                if self.current_conversation_id.as_deref() == Some(conversation_id.as_str()) {
                    vec![Effect::SidePaneSetScratchpad(notes)]
                } else {
                    vec![]
                }
            }
            UiMessage::ScratchpadChanged { conversation_id } => {
                if self.current_conversation_id.as_deref() == Some(conversation_id.as_str()) {
                    vec![Effect::FetchScratchpad(conversation_id)]
                } else {
                    vec![]
                }
            }
            UiMessage::Disconnected { reason } => {
                let mut effects = vec![
                    Effect::ClearClient,
                    Effect::SetSendSensitive(false),
                    Effect::SetStatusText(format!("Disconnected: {reason}")),
                ];

                // Finalize any in-progress streaming buffer
                if self.pending_request_id.is_some() {
                    self.pending_request_id = None;
                    if !self.streaming_buffer.is_empty() {
                        self.streaming_buffer.push_str("\n\n[Connection lost]");
                        let full = self.streaming_buffer.clone();
                        self.streaming_buffer.clear();
                        if let Some(ref mut conv) = self.current_conversation {
                            conv.messages.push(ChatMessage {
                                role: "assistant".to_string(),
                                content: full.clone(),
                            });
                        }
                        effects.push(Effect::CompleteStreaming(full));
                    }
                }
                effects
            }
        }
    }
}

pub struct AdelieWindow {
    pub window: ApplicationWindow,
}

impl AdelieWindow {
    pub fn new(app: &Application, config: ConnectionConfig) -> Self {
        let window = ApplicationWindow::builder()
            .application(app)
            .title("Adelie Desktop Assistant")
            .default_width(1100)
            .default_height(700)
            .build();

        // Set application icon for taskbar
        install_app_icon();

        // Apply theme-aware CSS: the dark palette is always installed; the
        // light overrides (see `crate::theme`) are layered on only when the
        // system/GTK preference is light, and re-applied on preference change.
        crate::theme::install_for_display(&gdk::Display::default().expect("display"));

        // Layout: resizable paned split between sidebar and chat. The left
        // pane is a `Stack` that swaps between the conversation list and the
        // process-manager view (issue #19) — minimal disruption to the
        // existing sidebar widget, the Tasks page just becomes a sibling.
        let paned = Paned::new(Orientation::Horizontal);

        let sidebar = Sidebar::new();
        let tasks_panel = TasksPanel::new();

        let left_box = GtkBox::new(Orientation::Vertical, 0);
        left_box.set_size_request(280, -1);

        let stack = Stack::new();
        stack.set_vexpand(true);
        stack.add_titled(&sidebar.container, Some("conversations"), "Conversations");
        stack.add_titled(&tasks_panel.container, Some("tasks"), "Tasks");

        let stack_switcher = StackSwitcher::new();
        stack_switcher.set_stack(Some(&stack));
        stack_switcher.set_halign(Align::Center);
        stack_switcher.set_margin_top(8);
        stack_switcher.set_margin_bottom(4);
        stack_switcher.add_css_class("sidebar-stack-switcher");
        left_box.append(&stack_switcher);
        left_box.append(&stack);

        paned.set_start_child(Some(&left_box));
        paned.set_resize_start_child(false);
        paned.set_shrink_start_child(false);
        paned.set_position(280);

        let right_box = GtkBox::new(Orientation::Vertical, 0);
        right_box.set_hexpand(true);
        right_box.set_vexpand(true);

        // Header bar with hamburger menu
        let header_bar = GtkBox::new(Orientation::Horizontal, 8);
        header_bar.set_margin_start(8);
        header_bar.set_margin_end(8);
        header_bar.set_margin_top(4);
        header_bar.set_margin_bottom(4);

        // Per-conversation model picker — populated on connect, selection
        // tracks `ConversationView.model_selection` after each load.
        let model_picker = ModelPicker::new();
        header_bar.append(&model_picker.container);

        // Spacer to push menu button to the right
        let spacer = GtkBox::new(Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        header_bar.append(&spacer);

        // Toggle for the conversation side pane (issue #60). Wired to the
        // revealer once it's constructed below.
        let side_pane_toggle = Button::from_icon_name("sidebar-show-right-symbolic");
        side_pane_toggle.add_css_class("flat");
        side_pane_toggle.set_tooltip_text(Some("Toggle conversation panel"));
        header_bar.append(&side_pane_toggle);

        // Hamburger menu button
        let menu_button = MenuButton::new();
        menu_button.set_icon_name("open-menu-symbolic");
        menu_button.add_css_class("flat");

        let menu_popover = Popover::new();
        menu_popover.add_css_class("context-popover");
        let menu_box = GtkBox::new(Orientation::Vertical, 0);

        let settings_btn = Button::with_label("Settings");
        settings_btn.add_css_class("context-button");
        settings_btn.set_halign(Align::Fill);
        menu_box.append(&settings_btn);

        let switch_conn_btn = Button::with_label("Switch Connection…");
        switch_conn_btn.add_css_class("context-button");
        switch_conn_btn.set_halign(Align::Fill);
        menu_box.append(&switch_conn_btn);

        let knowledge_btn = Button::with_label("Knowledge Base");
        knowledge_btn.add_css_class("context-button");
        knowledge_btn.set_halign(Align::Fill);
        menu_box.append(&knowledge_btn);

        let disconnect_btn = Button::with_label("Disconnect");
        disconnect_btn.add_css_class("context-button");
        disconnect_btn.set_halign(Align::Fill);
        menu_box.append(&disconnect_btn);

        menu_popover.set_child(Some(&menu_box));
        menu_button.set_popover(Some(&menu_popover));
        header_bar.append(&menu_button);

        right_box.append(&header_bar);

        let header_sep = Separator::new(Orientation::Horizontal);
        right_box.append(&header_sep);

        // The chat column and the conversation side pane sit side by side below
        // the (full-width) header, so the pane stays visible while chatting.
        let body = GtkBox::new(Orientation::Horizontal, 0);
        body.set_hexpand(true);
        body.set_vexpand(true);

        let chat_column = GtkBox::new(Orientation::Vertical, 0);
        chat_column.set_hexpand(true);
        chat_column.set_vexpand(true);

        let chat_view = ChatView::new();
        chat_column.append(&chat_view.container);

        // Passive toast for advisory warnings (e.g. a dangling model
        // selection cleared by the daemon). The revealer is always in the
        // layout; we reveal it with a message when something needs
        // attention, and the user can dismiss it.
        let toast_revealer = Revealer::new();
        toast_revealer.set_transition_type(RevealerTransitionType::SlideUp);
        toast_revealer.set_reveal_child(false);
        let toast_row = GtkBox::new(Orientation::Horizontal, 8);
        toast_row.add_css_class("toast-row");
        toast_row.set_margin_start(12);
        toast_row.set_margin_end(12);
        toast_row.set_margin_top(6);
        toast_row.set_margin_bottom(6);
        let toast_label = Label::new(None);
        toast_label.set_halign(Align::Start);
        toast_label.set_hexpand(true);
        toast_label.set_wrap(true);
        toast_row.append(&toast_label);
        let toast_dismiss = Button::from_icon_name("window-close-symbolic");
        toast_dismiss.add_css_class("flat");
        toast_dismiss.connect_clicked(glib::clone!(
            #[weak]
            toast_revealer,
            move |_| toast_revealer.set_reveal_child(false)
        ));
        toast_row.append(&toast_dismiss);
        toast_revealer.set_child(Some(&toast_row));
        chat_column.append(&toast_revealer);

        let input_sep = Separator::new(Orientation::Horizontal);
        chat_column.append(&input_sep);

        let input_bar = InputBar::new();
        input_bar.send_button.set_sensitive(false); // disabled until connected
        chat_column.append(&input_bar.container);

        let status_bar = GtkBox::new(Orientation::Horizontal, 0);
        status_bar.set_margin_top(4);
        status_bar.set_margin_bottom(4);

        let status_label = Label::new(Some("Connecting..."));
        status_label.set_halign(gtk4::Align::Start);
        status_label.set_hexpand(true);
        status_label.set_margin_start(12);
        status_label.add_css_class("status-bar");
        status_bar.append(&status_label);

        let debug_check = CheckButton::with_label("Debug");
        debug_check.set_halign(gtk4::Align::End);
        debug_check.set_margin_end(12);
        debug_check.add_css_class("debug-check");
        status_bar.append(&debug_check);

        chat_column.append(&status_bar);

        // Conversation side pane (issue #60): tasks + scratchpad for the active
        // conversation, revealed from the right via the header toggle. The
        // divider lives inside the revealer so it only shows when revealed.
        let side_pane = ConversationSidePane::new();
        let side_revealer = Revealer::new();
        side_revealer.set_transition_type(RevealerTransitionType::SlideLeft);
        side_revealer.set_reveal_child(false);
        let side_box = GtkBox::new(Orientation::Horizontal, 0);
        side_box.append(&Separator::new(Orientation::Vertical));
        side_box.append(&side_pane.container);
        side_revealer.set_child(Some(&side_box));

        body.append(&chat_column);
        body.append(&side_revealer);
        right_box.append(&body);

        side_pane_toggle.connect_clicked(glib::clone!(
            #[weak]
            side_revealer,
            move |_| side_revealer.set_reveal_child(!side_revealer.reveals_child())
        ));

        paned.set_end_child(Some(&right_box));
        paned.set_resize_end_child(true);
        paned.set_shrink_end_child(false);
        window.set_child(Some(&paned));

        // Shared state
        let state = Rc::new(RefCell::new(WindowState {
            conversations: Vec::new(),
            current_conversation_id: None,
            current_conversation: None,
            pending_request_id: None,
            streaming_buffer: String::new(),
            debug_enabled: false,
        }));

        // Wrap widgets in Rc for closures
        let sidebar = Rc::new(sidebar);
        let chat_view = Rc::new(RefCell::new(chat_view));
        let input_bar = Rc::new(input_bar);
        let status_label = Rc::new(status_label);
        let model_picker = Rc::new(model_picker);
        let tasks_panel = Rc::new(tasks_panel);
        let side_pane = Rc::new(side_pane);
        let stack = Rc::new(stack);
        let toast_revealer = Rc::new(toast_revealer);
        let toast_label = Rc::new(toast_label);

        // Client wrapped in Arc for async tasks, Rc<RefCell<>> for GTK thread
        let client: Rc<RefCell<Option<Arc<TransportClient>>>> = Rc::new(RefCell::new(None));

        // Set up async bridge with UI message handler
        let bridge = AsyncBridge::new(glib::clone!(
            #[strong]
            state,
            #[strong]
            sidebar,
            #[strong]
            chat_view,
            #[strong]
            status_label,
            #[strong]
            client,
            #[strong]
            input_bar,
            #[strong]
            model_picker,
            #[strong]
            tasks_panel,
            #[strong]
            side_pane,
            #[strong]
            toast_revealer,
            #[strong]
            toast_label,
            move |msg, ui_tx| {
                handle_ui_message(
                    msg,
                    &state,
                    &sidebar,
                    &chat_view,
                    &status_label,
                    &client,
                    &input_bar,
                    &model_picker,
                    &tasks_panel,
                    &side_pane,
                    &toast_revealer,
                    &toast_label,
                    ui_tx,
                );
            }
        ));
        let bridge = Rc::new(bridge);

        // Wire the side pane's interactions to daemon commands. Edits/toggles/
        // deletes issue scratchpad commands; the daemon's `ScratchpadChanged`
        // event then refreshes the pane (issue #60).
        side_pane.set_on_action(glib::clone!(
            #[strong]
            state,
            #[strong]
            client,
            move |action: SidePaneAction| {
                let Some(conv) = state.borrow().current_conversation_id.clone() else {
                    return;
                };
                let Some(transport) = client.borrow().clone() else {
                    return;
                };
                crate::async_bridge::spawn_on_runtime(async move {
                    let Some(cmds) = transport.as_commands() else {
                        return;
                    };
                    let result = match action {
                        SidePaneAction::SetNote {
                            key,
                            content,
                            note_type,
                            sequence,
                            done,
                        } => cmds
                            .set_scratchpad_note(&conv, &key, &content, &note_type, sequence, done)
                            .await
                            .map(|_| ()),
                        SidePaneAction::DeleteNote { key } => {
                            cmds.delete_scratchpad_notes(&conv, vec![key], false).await
                        }
                    };
                    if let Err(e) = result {
                        tracing::warn!("scratchpad action failed: {e}");
                    }
                });
            }
        ));
        side_pane.set_on_cancel_task(glib::clone!(
            #[strong]
            client,
            move |id: String| {
                let Some(transport) = client.borrow().clone() else {
                    return;
                };
                crate::async_bridge::spawn_on_runtime(async move {
                    if let Some(cmds) = transport.as_commands() {
                        let _ = cmds
                            .send_command(api::Command::CancelBackgroundTask { id })
                            .await;
                    }
                });
            }
        ));

        // Spawn persistent connection manager (connect → forward → reconnect).
        // It now delivers the freshly connected transport to the main thread
        // via `UiMessage::ClientReady` on the same channel as every other UI
        // message (handled in `handle_ui_message`).
        {
            let ui_tx = bridge.ui_sender();
            bridge.spawn(connection_manager(config.clone(), ui_tx));
        }

        // Sidebar row activation → load conversation
        sidebar.list_box.connect_row_activated(glib::clone!(
            #[strong]
            client,
            #[strong]
            state,
            #[strong]
            bridge,
            move |_, row| {
                let idx = row.index() as usize;
                let state_borrow = state.borrow();
                if let Some(conv) = state_borrow.conversations.get(idx) {
                    let conv_id = conv.id.clone();
                    drop(state_borrow);

                    if let Some(client) = client.borrow().clone() {
                        let tx = bridge.ui_sender();
                        bridge.spawn(async move {
                            match client.get_conversation(&conv_id).await {
                                Ok(detail) => {
                                    let _ = tx.send(UiMessage::ConversationLoaded(detail));
                                }
                                Err(e) => {
                                    let _ = tx
                                        .send(UiMessage::Error(format!("Load conversation: {e}")));
                                }
                            }
                        });
                    }
                }
            }
        ));

        // New conversation button
        sidebar.new_button.connect_clicked(glib::clone!(
            #[strong]
            client,
            #[strong]
            bridge,
            move |_| {
                if let Some(client) = client.borrow().clone() {
                    let tx = bridge.ui_sender();
                    bridge.spawn(async move {
                        match client.create_conversation("New Conversation").await {
                            Ok(id) => {
                                let _ = tx.send(UiMessage::ConversationCreated { id: id.clone() });
                                // Refresh conversation list
                                if let Ok(convs) = client.list_conversations().await {
                                    let _ = tx.send(UiMessage::ConversationsLoaded(convs));
                                }
                                // Load the new conversation
                                if let Ok(detail) = client.get_conversation(&id).await {
                                    let _ = tx.send(UiMessage::ConversationLoaded(detail));
                                }
                            }
                            Err(e) => {
                                let _ =
                                    tx.send(UiMessage::Error(format!("Create conversation: {e}")));
                            }
                        }
                    });
                }
            }
        ));

        // Context menu: Delete conversation
        sidebar.connect_delete(glib::clone!(
            #[strong]
            client,
            #[strong]
            bridge,
            #[strong]
            state,
            move |idx| {
                let id = {
                    let s = state.borrow();
                    match s.conversations.get(idx) {
                        Some(conv) => conv.id.clone(),
                        None => return,
                    }
                };
                if let Some(client) = client.borrow().clone() {
                    let tx = bridge.ui_sender();
                    let id = id.clone();
                    bridge.spawn(async move {
                        match client.delete_conversation(&id).await {
                            Ok(()) => {
                                let _ = tx.send(UiMessage::ConversationDeleted { id });
                            }
                            Err(e) => {
                                let _ =
                                    tx.send(UiMessage::Error(format!("Delete conversation: {e}")));
                            }
                        }
                    });
                }
            }
        ));

        // Context menu: Rename conversation
        sidebar.connect_rename(glib::clone!(
            #[strong]
            client,
            #[strong]
            bridge,
            #[strong]
            state,
            #[weak]
            window,
            move |idx| {
                let (id, current_title) = {
                    let s = state.borrow();
                    match s.conversations.get(idx) {
                        Some(conv) => (conv.id.clone(), conv.title.clone()),
                        None => return,
                    }
                };

                let dialog = Window::builder()
                    .title("Rename Conversation")
                    .transient_for(&window)
                    .modal(true)
                    .default_width(360)
                    .default_height(10)
                    .resizable(false)
                    .build();

                let vbox = GtkBox::new(Orientation::Vertical, 8);
                vbox.set_margin_start(16);
                vbox.set_margin_end(16);
                vbox.set_margin_top(16);
                vbox.set_margin_bottom(16);

                let entry = Entry::new();
                entry.set_text(&current_title);
                entry.set_activates_default(true);
                vbox.append(&entry);

                let btn_box = GtkBox::new(Orientation::Horizontal, 8);
                btn_box.set_halign(gtk4::Align::End);

                let cancel_btn = Button::with_label("Cancel");
                cancel_btn.connect_clicked(glib::clone!(
                    #[weak]
                    dialog,
                    move |_| {
                        dialog.close();
                    }
                ));
                btn_box.append(&cancel_btn);

                let confirm_btn = Button::with_label("Rename");
                confirm_btn.add_css_class("suggested-action");
                confirm_btn.connect_clicked(glib::clone!(
                    #[strong]
                    client,
                    #[strong]
                    bridge,
                    #[weak]
                    dialog,
                    #[weak]
                    entry,
                    move |_| {
                        let new_title = entry.text().trim().to_string();
                        if new_title.is_empty() {
                            return;
                        }
                        dialog.close();
                        if let Some(client) = client.borrow().clone() {
                            let tx = bridge.ui_sender();
                            let id = id.clone();
                            let title = new_title.clone();
                            bridge.spawn(async move {
                                match client.rename_conversation(&id, &title).await {
                                    Ok(()) => {
                                        let _ =
                                            tx.send(UiMessage::ConversationRenamed { id, title });
                                    }
                                    Err(e) => {
                                        let _ = tx.send(UiMessage::Error(format!(
                                            "Rename conversation: {e}"
                                        )));
                                    }
                                }
                            });
                        }
                    }
                ));
                btn_box.append(&confirm_btn);

                // Enter key in entry confirms
                entry.connect_activate(glib::clone!(
                    #[weak]
                    confirm_btn,
                    move |_| {
                        confirm_btn.emit_clicked();
                    }
                ));

                vbox.append(&btn_box);
                dialog.set_child(Some(&vbox));
                dialog.present();
            }
        ));

        // Context menu: Archive/unarchive conversation
        sidebar.connect_archive(glib::clone!(
            #[strong]
            client,
            #[strong]
            bridge,
            #[strong]
            state,
            move |idx| {
                let (id, archived) = {
                    let s = state.borrow();
                    match s.conversations.get(idx) {
                        Some(conv) => (conv.id.clone(), conv.archived),
                        None => return,
                    }
                };
                if let Some(client) = client.borrow().clone() {
                    let tx = bridge.ui_sender();
                    let id = id.clone();
                    bridge.spawn(async move {
                        let result = if archived {
                            client.unarchive_conversation(&id).await
                        } else {
                            client.archive_conversation(&id).await
                        };
                        match result {
                            Ok(()) => {
                                // Refresh conversation list
                                if let Ok(convs) = client.list_conversations().await {
                                    let _ = tx.send(UiMessage::ConversationsLoaded(convs));
                                }
                            }
                            Err(e) => {
                                let _ =
                                    tx.send(UiMessage::Error(format!("Archive conversation: {e}")));
                            }
                        }
                    });
                }
            }
        ));

        // Show archived checkbox toggle
        sidebar.connect_show_archived_toggled(glib::clone!(
            #[strong]
            client,
            #[strong]
            bridge,
            move |include_archived| {
                if let Some(client) = client.borrow().clone() {
                    let tx = bridge.ui_sender();
                    bridge.spawn(async move {
                        let result = if include_archived {
                            client.list_conversations_with_archived().await
                        } else {
                            client.list_conversations().await
                        };
                        match result {
                            Ok(convs) => {
                                let _ = tx.send(UiMessage::ConversationsLoaded(convs));
                            }
                            Err(e) => {
                                let _ =
                                    tx.send(UiMessage::Error(format!("Load conversations: {e}")));
                            }
                        }
                    });
                }
            }
        ));

        // Send button / Enter key → send prompt
        {
            let send_action = Rc::new(glib::clone!(
                #[strong]
                client,
                #[strong(rename_to = bridge_ref)]
                bridge,
                #[strong]
                state,
                #[strong]
                input_bar,
                #[strong]
                chat_view,
                #[strong]
                model_picker,
                move || {
                    let text = input_bar.take_text();
                    let text = text.trim().to_string();
                    if text.is_empty() {
                        return;
                    }
                    let state_borrow = state.borrow();
                    let conv_id = match &state_borrow.current_conversation_id {
                        Some(id) => id.clone(),
                        None => return,
                    };
                    drop(state_borrow);

                    // Show user message immediately
                    chat_view.borrow_mut().add_user_message(&text);

                    // Track in local conversation copy
                    {
                        let mut s = state.borrow_mut();
                        if let Some(ref mut conv) = s.current_conversation {
                            conv.messages.push(ChatMessage {
                                role: "user".to_string(),
                                content: text.clone(),
                            });
                        }
                    }

                    let override_selection = model_picker.current_override();

                    if let Some(client) = client.borrow().clone() {
                        let tx = bridge_ref.ui_sender();
                        let text = text.clone();
                        bridge_ref.spawn(async move {
                            // Use the command-channel override path when
                            // available so the picker's selection is honoured.
                            // The shared AssistantClient trait can't carry the
                            // override because the D-Bus surface doesn't expose
                            // it; on D-Bus we fall through to the plain
                            // send_prompt.
                            let result = match (client.as_commands(), override_selection) {
                                (Some(cmds), Some(over)) => {
                                    cmds.send_prompt_with_override(&conv_id, &text, Some(over))
                                        .await
                                }
                                _ => client.send_prompt(&conv_id, &text).await,
                            };
                            match result {
                                Ok(task_id) => {
                                    let _ = tx.send(UiMessage::PromptSent { task_id });
                                }
                                Err(e) => {
                                    let _ = tx.send(UiMessage::Error(format!("Send error: {e}")));
                                }
                            }
                        });
                    }
                }
            ));

            // Send button click
            input_bar.send_button.connect_clicked(glib::clone!(
                #[strong]
                send_action,
                move |_| {
                    send_action();
                }
            ));

            // Enter key in text view (Shift+Enter for newline)
            let key_controller = gtk4::EventControllerKey::new();
            key_controller.connect_key_pressed(glib::clone!(
                #[strong]
                send_action,
                move |_, key, _, modifiers| {
                    if key == gdk::Key::Return
                        && !modifiers.contains(gdk::ModifierType::SHIFT_MASK)
                        && !modifiers.contains(gdk::ModifierType::CONTROL_MASK)
                    {
                        send_action();
                        glib::Propagation::Stop
                    } else {
                        glib::Propagation::Proceed
                    }
                }
            ));
            input_bar.text_view.add_controller(key_controller);
        }

        // Hamburger menu: Switch Connection → open the picker in a new
        // window. The current connection's window is intentionally left open;
        // selecting a profile spawns a fresh AdelieWindow alongside it.
        switch_conn_btn.connect_clicked(glib::clone!(
            #[weak]
            app,
            #[weak]
            menu_popover,
            move |_| {
                menu_popover.popdown();
                let login = crate::widgets::login_screen::LoginScreen::new(&app);
                login.present();
            }
        ));

        // Hamburger menu: Settings → Connections / Purposes tabs (#1). The
        // dialog is WS-only (named-connection management isn't exposed over
        // D-Bus); on a D-Bus transport we status-message and no-op — the
        // header model picker is already hidden there.
        settings_btn.connect_clicked(glib::clone!(
            #[weak]
            menu_popover,
            #[weak]
            window,
            #[strong]
            client,
            #[strong]
            bridge,
            #[strong]
            status_label,
            move |_| {
                menu_popover.popdown();
                let Some(transport) = client.borrow().clone() else {
                    status_label.set_text("Not connected — settings unavailable");
                    return;
                };
                if transport.as_commands().is_none() {
                    status_label.set_text(
                        "Settings require a local-socket or WebSocket connection (not available over D-Bus)",
                    );
                    return;
                }
                crate::widgets::settings_dialog::show_settings_dialog(
                    &window,
                    Arc::clone(&transport),
                    Rc::clone(&bridge),
                );
                // The user may have added/removed connections; re-query the
                // aggregated model list so the header picker reflects the new
                // set. Fire-and-forget — errors are non-fatal. Runs once when
                // Settings is opened (so it picks up the previous session's
                // changes); the dialog itself keeps its own tabs in sync.
                let tx = bridge.ui_sender();
                bridge.spawn(async move {
                    match management_client::list_available_models(&transport, None, false).await {
                        Ok(listings) => {
                            let _ = tx.send(UiMessage::ModelsLoaded(listings));
                        }
                        Err(e) => {
                            tracing::warn!("Failed to refresh models after settings: {e}");
                        }
                    }
                    // The user may have changed the interactive purpose; re-fetch
                    // purposes so the picker's default updates for conversations
                    // still on the default (issue #53). Graceful: on failure we
                    // emit `None` (picker degrades to "Model").
                    let default_model =
                        match management_client::get_purposes(&transport).await {
                            Ok(purposes) => {
                                crate::async_bridge::interactive_default_from_purposes(&purposes)
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "Failed to refresh purposes after settings: {e}"
                                );
                                None
                            }
                        };
                    let _ = tx.send(UiMessage::DefaultModelLoaded(default_model));
                });
            }
        ));

        // Hamburger menu: Knowledge Base → open the KB browser/editor (#74)
        knowledge_btn.connect_clicked(glib::clone!(
            #[weak]
            menu_popover,
            #[weak]
            window,
            #[strong]
            client,
            #[strong]
            bridge,
            #[strong]
            status_label,
            move |_| {
                menu_popover.popdown();
                let Some(transport) = client.borrow().clone() else {
                    status_label.set_text("Not connected — knowledge base unavailable");
                    return;
                };
                let browser = crate::widgets::knowledge_browser::KnowledgeBrowser::new(
                    &window,
                    transport,
                    Rc::clone(&bridge),
                );
                browser.present();
            }
        ));

        // Hamburger menu: Disconnect → close this window, show login screen
        disconnect_btn.connect_clicked(glib::clone!(
            #[weak]
            app,
            #[weak]
            window,
            #[weak]
            menu_popover,
            move |_| {
                menu_popover.popdown();
                let login = crate::widgets::login_screen::LoginScreen::new(&app);
                login.present();
                window.close();
            }
        ));

        // Debug checkbox toggle → re-fetch conversation with filtering
        debug_check.connect_toggled(glib::clone!(
            #[strong]
            client,
            #[strong]
            bridge,
            #[strong]
            state,
            move |btn| {
                state.borrow_mut().debug_enabled = btn.is_active();
                let conv_id = state.borrow().current_conversation_id.clone();
                if let Some(conv_id) = conv_id
                    && let Some(client) = client.borrow().clone()
                {
                    let tx = bridge.ui_sender();
                    bridge.spawn(async move {
                        match client.get_conversation(&conv_id).await {
                            Ok(detail) => {
                                let _ = tx.send(UiMessage::ConversationLoaded(detail));
                            }
                            Err(e) => {
                                let _ =
                                    tx.send(UiMessage::Error(format!("Reload conversation: {e}")));
                            }
                        }
                    });
                }
            }
        ));

        // Tasks panel: toolbar wiring (#19).
        //
        // `Cancel` sends `CancelBackgroundTask` over the command channel;
        // `Open Conversation`
        // routes the user back to the Conversations stack page and loads the
        // task's conversation so the streaming output keeps flowing into the
        // chat view.
        tasks_panel.connect_cancel(glib::clone!(
            #[strong]
            client,
            #[strong]
            bridge,
            move |task_id| {
                let Some(transport) = client.borrow().clone() else {
                    return;
                };
                let tx = bridge.ui_sender();
                bridge.spawn(async move {
                    let Some(cmds) = transport.as_commands() else {
                        let _ = tx.send(UiMessage::Error(
                            "Background tasks require a local-socket or WebSocket connection \
                             (not available over D-Bus)"
                                .to_string(),
                        ));
                        return;
                    };
                    if let Err(e) = cmds
                        .send_command(api::Command::CancelBackgroundTask { id: task_id })
                        .await
                    {
                        let _ = tx.send(UiMessage::Error(format!("Cancel task: {e}")));
                    }
                });
            }
        ));

        tasks_panel.connect_open_conversation(glib::clone!(
            #[strong]
            client,
            #[strong]
            bridge,
            #[strong]
            stack,
            move |conv_id| {
                stack.set_visible_child_name("conversations");
                let Some(transport) = client.borrow().clone() else {
                    return;
                };
                let tx = bridge.ui_sender();
                bridge.spawn(async move {
                    match transport.get_conversation(&conv_id).await {
                        Ok(detail) => {
                            let _ = tx.send(UiMessage::ConversationLoaded(detail));
                        }
                        Err(e) => {
                            let _ = tx.send(UiMessage::Error(format!("Load conversation: {e}")));
                        }
                    }
                });
            }
        ));

        Self { window }
    }

    pub fn present(&self) {
        self.window.present();
    }
}

/// Current wall-clock time in epoch milliseconds. Centralized so the
/// task-panel callers all use the same units as `TaskView.started_at`.
fn now_epoch_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[allow(clippy::too_many_arguments)]
fn handle_ui_message(
    msg: UiMessage,
    state: &Rc<RefCell<WindowState>>,
    sidebar: &Rc<Sidebar>,
    chat_view: &Rc<RefCell<ChatView>>,
    status_label: &Rc<Label>,
    client: &Rc<RefCell<Option<Arc<TransportClient>>>>,
    input_bar: &Rc<InputBar>,
    model_picker: &Rc<ModelPicker>,
    tasks_panel: &Rc<TasksPanel>,
    side_pane: &Rc<ConversationSidePane>,
    toast_revealer: &Rc<Revealer>,
    toast_label: &Rc<Label>,
    ui_tx: &mpsc::UnboundedSender<UiMessage>,
) {
    // Pure decision: mutate state + compute the effects to perform.
    let effects = state.borrow_mut().apply(msg);

    // Thin executor: perform each effect against the real widgets, in order.
    for effect in effects {
        match effect {
            Effect::SetClient(transport) => {
                *client.borrow_mut() = Some(transport);
            }
            Effect::ClearClient => {
                *client.borrow_mut() = None;
            }
            Effect::SetStatusText(text) => {
                status_label.set_text(&text);
            }
            Effect::SetSendSensitive(sensitive) => {
                input_bar.send_button.set_sensitive(sensitive);
            }
            Effect::SetConversations(convs) => {
                sidebar.set_conversations(&convs);
            }
            Effect::EnsureActiveConversation => {
                ensure_active_conversation(state, sidebar, client, ui_tx);
            }
            Effect::LoadConversationIntoChat(filtered) => {
                chat_view.borrow_mut().load_conversation(&filtered);
            }
            Effect::ClearChat => {
                chat_view.borrow_mut().clear();
            }
            Effect::SetChatStatus(message) => {
                chat_view.borrow().set_status(&message);
            }
            Effect::ClearChatStatus => {
                chat_view.borrow().clear_status();
            }
            Effect::ReceiveChunk(chunk) => {
                chat_view.borrow_mut().receive_chunk(&chunk);
            }
            Effect::CompleteStreaming(full) => {
                chat_view.borrow_mut().complete_streaming(&full);
            }
            Effect::SetModelSelection(selection) => {
                model_picker.set_selection(selection.as_ref());
            }
            Effect::SetModels(listings) => {
                model_picker.set_models(&listings);
            }
            Effect::SetDefaultModel(default) => {
                model_picker.set_default_model(default);
            }
            Effect::SetModelPickerVisible(visible) => {
                model_picker.set_visible(visible);
            }
            Effect::ShowToast(message) => {
                toast_label.set_text(&message);
                toast_revealer.set_reveal_child(true);
            }
            Effect::TasksReplaceAll(tasks) => {
                tasks_panel.replace_all(tasks, now_epoch_ms());
            }
            Effect::TaskStarted(task) => {
                tasks_panel.handle_task_started(task, now_epoch_ms());
            }
            Effect::TaskProgress { id, progress_hint } => {
                tasks_panel.handle_task_progress(id, progress_hint, now_epoch_ms());
            }
            Effect::TaskLogAppended { id, entry } => {
                tasks_panel.handle_task_log_appended(id, entry);
            }
            Effect::TaskCompleted { id } => {
                tasks_panel.handle_task_completed(id, now_epoch_ms());
            }
            Effect::FetchScratchpad(conversation_id) => {
                if let Some(transport) = client.borrow().clone() {
                    let tx = ui_tx.clone();
                    crate::async_bridge::spawn_on_runtime(async move {
                        let Some(cmds) = transport.as_commands() else {
                            return;
                        };
                        match cmds
                            .get_conversation_scratchpad(&conversation_id, None)
                            .await
                        {
                            Ok(notes) => {
                                let _ = tx.send(UiMessage::ConversationScratchpadLoaded {
                                    conversation_id,
                                    notes,
                                });
                            }
                            Err(e) => {
                                tracing::warn!("get_conversation_scratchpad failed: {e}");
                            }
                        }
                    });
                }
            }
            Effect::SidePaneSetScratchpad(notes) => {
                side_pane.set_scratchpad(notes);
            }
            Effect::RefreshSidePaneTasks => {
                let conv = state.borrow().current_conversation_id.clone();
                let rows = tasks_panel.task_view_models_for(conv.as_deref(), now_epoch_ms());
                side_pane.set_tasks(&rows);
            }
        }
    }
}

/// Make sure the window has an active conversation. The daemon returns the
/// conversation list sorted by `updated_at` desc, so picking index 0 yields
/// the most-recently-used conversation. When the list is empty we ask the
/// daemon to create a new one and load it.
///
/// No-op when an active conversation is already set and still present in the
/// list — this lets the function be called freely from `ConversationsLoaded`
/// (which fires on every reconnect) without disturbing in-progress work.
fn ensure_active_conversation(
    state: &Rc<RefCell<WindowState>>,
    sidebar: &Rc<Sidebar>,
    client: &Rc<RefCell<Option<Arc<TransportClient>>>>,
    ui_tx: &mpsc::UnboundedSender<UiMessage>,
) {
    let (target_id, target_index) = {
        let s = state.borrow();

        // Already-active and still present → just sync the sidebar selection.
        if let Some(active_id) = s.current_conversation_id.as_deref()
            && let Some(idx) = s.conversations.iter().position(|c| c.id == active_id)
        {
            drop(s);
            sidebar.select_index(idx);
            return;
        }

        match s.conversations.first() {
            Some(conv) => (Some(conv.id.clone()), Some(0usize)),
            None => (None, None),
        }
    };

    let Some(transport) = client.borrow().clone() else {
        // Not connected yet — connection_manager will resend
        // ConversationsLoaded once the transport is up, and we'll re-run.
        return;
    };

    let tx = ui_tx.clone();
    match (target_id, target_index) {
        (Some(id), Some(idx)) => {
            sidebar.select_index(idx);
            crate::async_bridge::spawn_on_runtime(async move {
                match transport.get_conversation(&id).await {
                    Ok(detail) => {
                        let _ = tx.send(UiMessage::ConversationLoaded(detail));
                    }
                    Err(e) => {
                        let _ = tx.send(UiMessage::Error(format!("Load conversation: {e}")));
                    }
                }
            });
        }
        _ => {
            crate::async_bridge::spawn_on_runtime(async move {
                match transport.create_conversation("New Conversation").await {
                    Ok(id) => {
                        let _ = tx.send(UiMessage::ConversationCreated { id: id.clone() });
                        if let Ok(convs) = transport.list_conversations().await {
                            let _ = tx.send(UiMessage::ConversationsLoaded(convs));
                        }
                        if let Ok(detail) = transport.get_conversation(&id).await {
                            let _ = tx.send(UiMessage::ConversationLoaded(detail));
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(UiMessage::Error(format!("Auto-create conversation: {e}")));
                    }
                }
            });
        }
    }
}

/// Install the Adele icon into the GTK icon theme so it appears in the taskbar.
///
/// Writes the embedded PNG to a temporary hicolor icon theme directory and adds
/// it to the display's icon search path. Uses the app ID as the icon name so
/// the desktop environment can match it to the window.
pub fn install_app_icon() {
    const ICON_BYTES: &[u8] = include_bytes!("../assets/adele.png");
    const ICON_NAME: &str = "org.adelie.DesktopAssistant";

    let cache_root = dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("adele-gtk-icons");

    // Resolves to <cache_root>/hicolor/512x512/apps/<ICON_NAME>.png; the
    // helper creates the parent dirs and writes idempotently.
    let icon_rel = format!("adele-gtk-icons/hicolor/512x512/apps/{ICON_NAME}.png");
    if let Err(e) = crate::assets::extract_to_cache(ICON_BYTES, &icon_rel) {
        tracing::warn!("Failed to install icon: {e}");
        return;
    }

    let display = gdk::Display::default().expect("display");
    let icon_theme = gtk4::IconTheme::for_display(&display);
    icon_theme.add_search_path(cache_root.to_str().unwrap_or_default());

    gtk4::Window::set_default_icon_name(ICON_NAME);
}

/// Filter a conversation's messages based on debug mode.
///
/// When debug is off, only user and assistant messages are shown.
/// When debug is on, tool messages are included as well.
fn filter_messages(detail: &ConversationDetail, debug: bool) -> ConversationDetail {
    ConversationDetail {
        id: detail.id.clone(),
        title: detail.title.clone(),
        messages: detail
            .messages
            .iter()
            .filter(|m| {
                if debug {
                    return true;
                }
                match m.role.as_str() {
                    "user" => true,
                    // Hide empty assistant messages (tool_calls-only)
                    "assistant" => !m.content.trim().is_empty(),
                    _ => false,
                }
            })
            .cloned()
            .collect(),
        model_selection: detail.model_selection.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Fixtures --------------------------------------------------------

    fn summary(id: &str, title: &str, archived: bool) -> ConversationSummary {
        ConversationSummary {
            id: id.to_string(),
            title: title.to_string(),
            message_count: 0,
            archived,
        }
    }

    fn msg(role: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: role.to_string(),
            content: content.to_string(),
        }
    }

    fn detail(id: &str, messages: Vec<ChatMessage>) -> ConversationDetail {
        ConversationDetail {
            id: id.to_string(),
            title: format!("conv {id}"),
            messages,
            model_selection: None,
        }
    }

    fn selection(connection_id: &str, model_id: &str) -> api::ConversationModelSelectionView {
        api::ConversationModelSelectionView {
            connection_id: connection_id.to_string(),
            model_id: model_id.to_string(),
            effort: None,
        }
    }

    fn listing(connection_id: &str, model_id: &str) -> api::ModelListing {
        api::ModelListing {
            connection_id: connection_id.to_string(),
            connection_label: connection_id.to_string(),
            model: api::ModelInfoView {
                id: model_id.to_string(),
                display_name: model_id.to_string(),
                context_limit: None,
                capabilities: api::ModelCapabilitiesView::default(),
            },
        }
    }

    // --- __pending__ sentinel handoff (#31) ------------------------------

    #[test]
    fn prompt_sent_sets_pending_sentinel_and_clears_buffer() {
        let mut state = WindowState {
            streaming_buffer: "leftover".to_string(),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::PromptSent {
            task_id: "ack-1".to_string(),
        });
        assert!(effects.is_empty(), "PromptSent performs no widget effects");
        assert_eq!(state.pending_request_id.as_deref(), Some("__pending__"));
        assert!(state.streaming_buffer.is_empty());
    }

    #[test]
    fn first_stream_chunk_claims_real_request_id_from_pending_sentinel() {
        let mut state = WindowState {
            pending_request_id: Some("__pending__".to_string()),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::StreamChunk {
            request_id: "req-real".to_string(),
            chunk: "hello".to_string(),
        });
        // Sentinel is replaced by the daemon's real request id...
        assert_eq!(state.pending_request_id.as_deref(), Some("req-real"));
        assert_eq!(state.streaming_buffer, "hello");
        // ...and because this is the first chunk, the chat status is cleared
        // before the chunk is rendered.
        assert!(
            matches!(effects.as_slice(), [Effect::ClearChatStatus, Effect::ReceiveChunk(c)] if c == "hello"),
            "unexpected effects: {effects:?}"
        );
    }

    #[test]
    fn subsequent_stream_chunk_appends_without_clearing_status() {
        let mut state = WindowState {
            pending_request_id: Some("req-real".to_string()),
            streaming_buffer: "hello".to_string(),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::StreamChunk {
            request_id: "req-real".to_string(),
            chunk: " world".to_string(),
        });
        assert_eq!(state.streaming_buffer, "hello world");
        // Non-first chunk: only the chunk is rendered, no status clear.
        assert!(
            matches!(effects.as_slice(), [Effect::ReceiveChunk(c)] if c == " world"),
            "unexpected effects: {effects:?}"
        );
    }

    #[test]
    fn stream_chunk_for_unrelated_request_id_is_ignored() {
        let mut state = WindowState {
            pending_request_id: Some("req-real".to_string()),
            streaming_buffer: "hello".to_string(),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::StreamChunk {
            request_id: "some-other-req".to_string(),
            chunk: "noise".to_string(),
        });
        assert!(effects.is_empty(), "stray chunk must not render");
        assert_eq!(state.streaming_buffer, "hello", "buffer must be untouched");
    }

    #[test]
    fn assistant_status_matches_pending_sentinel_before_request_id_known() {
        let mut state = WindowState {
            pending_request_id: Some("__pending__".to_string()),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::AssistantStatus {
            request_id: "req-not-yet-claimed".to_string(),
            message: "Searching...".to_string(),
        });
        assert!(
            matches!(effects.as_slice(), [Effect::SetChatStatus(m)] if m == "Searching..."),
            "status during the __pending__ window must reach the chat: {effects:?}"
        );
    }

    #[test]
    fn stream_complete_claims_sentinel_appends_message_and_clears_pending() {
        let mut state = WindowState {
            pending_request_id: Some("__pending__".to_string()),
            streaming_buffer: "partial".to_string(),
            current_conversation: Some(detail("c1", vec![msg("user", "hi")])),
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::StreamComplete {
            request_id: "req-real".to_string(),
            full_response: "the answer".to_string(),
        });
        assert!(state.pending_request_id.is_none());
        assert!(state.streaming_buffer.is_empty());
        let conv = state.current_conversation.as_ref().unwrap();
        assert_eq!(conv.messages.last().unwrap().role, "assistant");
        assert_eq!(conv.messages.last().unwrap().content, "the answer");
        assert!(
            matches!(
                effects.as_slice(),
                [
                    Effect::ClearChatStatus,
                    Effect::CompleteStreaming(c),
                    Effect::FetchScratchpad(conv),
                ] if c == "the answer" && conv == "c1"
            ),
            "unexpected effects: {effects:?}"
        );
    }

    #[test]
    fn stream_error_clears_pending_and_sets_error_status() {
        let mut state = WindowState {
            pending_request_id: Some("req-real".to_string()),
            streaming_buffer: "partial".to_string(),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::StreamError {
            request_id: "req-real".to_string(),
            error: "boom".to_string(),
        });
        assert!(state.pending_request_id.is_none());
        assert!(state.streaming_buffer.is_empty());
        assert!(
            matches!(effects.as_slice(), [Effect::ClearChatStatus, Effect::SetStatusText(t)] if t == "Error: boom"),
            "unexpected effects: {effects:?}"
        );
    }

    #[test]
    fn disconnect_finalizes_in_progress_stream_with_connection_lost_marker() {
        let mut state = WindowState {
            pending_request_id: Some("req-real".to_string()),
            streaming_buffer: "half a thought".to_string(),
            current_conversation: Some(detail("c1", vec![])),
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::Disconnected {
            reason: "socket closed".to_string(),
        });
        assert!(state.pending_request_id.is_none());
        assert!(state.streaming_buffer.is_empty());
        // The partial response is committed to the conversation with the marker.
        let last = state
            .current_conversation
            .as_ref()
            .unwrap()
            .messages
            .last()
            .unwrap();
        assert_eq!(last.content, "half a thought\n\n[Connection lost]");
        // Effects: clear client, desensitize send, status text, then finalize.
        assert!(
            matches!(
                effects.as_slice(),
                [
                    Effect::ClearClient,
                    Effect::SetSendSensitive(false),
                    Effect::SetStatusText(t),
                    Effect::CompleteStreaming(c),
                ] if t == "Disconnected: socket closed" && c == "half a thought\n\n[Connection lost]"
            ),
            "unexpected effects: {effects:?}"
        );
    }

    #[test]
    fn disconnect_without_active_stream_does_not_emit_complete_streaming() {
        let mut state = WindowState::default();
        let effects = state.apply(UiMessage::Disconnected {
            reason: "bye".to_string(),
        });
        assert!(
            matches!(
                effects.as_slice(),
                [
                    Effect::ClearClient,
                    Effect::SetSendSensitive(false),
                    Effect::SetStatusText(_)
                ]
            ),
            "no streaming buffer => no CompleteStreaming: {effects:?}"
        );
    }

    // --- Archived-list refresh -------------------------------------------

    #[test]
    fn conversations_loaded_stores_list_and_refreshes_sidebar_then_ensures_active() {
        // The "show archived" toggle re-fetches and re-delivers the list via
        // ConversationsLoaded; apply must repaint the sidebar with the new
        // (possibly archived-including) set and re-run ensure-active.
        let mut state = WindowState::default();
        let convs = vec![
            summary("c1", "Active one", false),
            summary("c2", "Archived one", true),
        ];
        let effects = state.apply(UiMessage::ConversationsLoaded(convs.clone()));
        assert_eq!(state.conversations.len(), 2);
        assert_eq!(state.conversations[1].id, "c2");
        assert!(state.conversations[1].archived);
        match effects.as_slice() {
            [
                Effect::SetConversations(got),
                Effect::EnsureActiveConversation,
            ] => {
                assert_eq!(got.len(), 2);
                assert_eq!(got[1].id, "c2");
            }
            other => panic!("unexpected effects: {other:?}"),
        }
    }

    #[test]
    fn deleting_active_conversation_clears_chat_and_re_ensures_active() {
        let mut state = WindowState {
            conversations: vec![summary("c1", "one", false), summary("c2", "two", false)],
            current_conversation_id: Some("c1".to_string()),
            current_conversation: Some(detail("c1", vec![])),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::ConversationDeleted {
            id: "c1".to_string(),
        });
        assert_eq!(state.conversations.len(), 1);
        assert_eq!(state.conversations[0].id, "c2");
        assert!(state.current_conversation_id.is_none());
        assert!(state.current_conversation.is_none());
        assert!(
            matches!(
                effects.as_slice(),
                [
                    Effect::SetConversations(_),
                    Effect::ClearChat,
                    Effect::SidePaneSetScratchpad(_),
                    Effect::RefreshSidePaneTasks,
                    Effect::EnsureActiveConversation
                ]
            ),
            "deleting the active conversation must clear chat + side pane + re-ensure: {effects:?}"
        );
    }

    fn note_view(key: &str) -> api::ScratchpadNoteView {
        api::ScratchpadNoteView {
            id: format!("id-{key}"),
            key: key.to_string(),
            content: "x".to_string(),
            note_type: "note".to_string(),
            sequence: None,
            done: false,
            updated_at: "t".to_string(),
        }
    }

    #[test]
    fn scratchpad_loaded_applies_only_for_active_conversation() {
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        // Matching conversation → set the pane.
        let effects = state.apply(UiMessage::ConversationScratchpadLoaded {
            conversation_id: "c1".to_string(),
            notes: vec![note_view("goal")],
        });
        assert!(
            matches!(effects.as_slice(), [Effect::SidePaneSetScratchpad(n)] if n.len() == 1),
            "unexpected: {effects:?}"
        );
        // A fetch that resolves after a conversation switch is ignored.
        let effects = state.apply(UiMessage::ConversationScratchpadLoaded {
            conversation_id: "stale".to_string(),
            notes: vec![note_view("goal")],
        });
        assert!(effects.is_empty(), "stale scratchpad must be dropped");
    }

    #[test]
    fn scratchpad_changed_refetches_only_for_active_conversation() {
        let mut state = WindowState {
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::ScratchpadChanged {
            conversation_id: "c1".to_string(),
        });
        assert!(matches!(effects.as_slice(), [Effect::FetchScratchpad(c)] if c == "c1"));
        let effects = state.apply(UiMessage::ScratchpadChanged {
            conversation_id: "other".to_string(),
        });
        assert!(
            effects.is_empty(),
            "a change to another conversation is ignored"
        );
    }

    #[test]
    fn tasks_loaded_also_refreshes_the_side_pane() {
        let mut state = WindowState::default();
        let effects = state.apply(UiMessage::TasksLoaded(vec![]));
        assert!(matches!(
            effects.as_slice(),
            [Effect::TasksReplaceAll(_), Effect::RefreshSidePaneTasks]
        ));
    }

    #[test]
    fn deleting_inactive_conversation_only_refreshes_sidebar() {
        let mut state = WindowState {
            conversations: vec![summary("c1", "one", false), summary("c2", "two", false)],
            current_conversation_id: Some("c1".to_string()),
            current_conversation: Some(detail("c1", vec![])),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::ConversationDeleted {
            id: "c2".to_string(),
        });
        assert!(state.current_conversation_id.as_deref() == Some("c1"));
        assert!(
            matches!(effects.as_slice(), [Effect::SetConversations(got)] if got.len() == 1),
            "deleting an inactive conversation must not touch the chat: {effects:?}"
        );
    }

    #[test]
    fn rename_updates_matching_conversation_title_and_refreshes_sidebar() {
        let mut state = WindowState {
            conversations: vec![summary("c1", "old", false), summary("c2", "keep", false)],
            ..Default::default()
        };
        let effects = state.apply(UiMessage::ConversationRenamed {
            id: "c1".to_string(),
            title: "new title".to_string(),
        });
        assert_eq!(state.conversations[0].title, "new title");
        assert_eq!(state.conversations[1].title, "keep");
        match effects.as_slice() {
            [Effect::SetConversations(got)] => assert_eq!(got[0].title, "new title"),
            other => panic!("unexpected effects: {other:?}"),
        }
    }

    #[test]
    fn title_changed_signal_updates_matching_conversation_and_refreshes_sidebar() {
        let mut state = WindowState {
            conversations: vec![summary("c1", "untitled", false)],
            ..Default::default()
        };
        let effects = state.apply(UiMessage::TitleChanged {
            conversation_id: "c1".to_string(),
            title: "Auto Title".to_string(),
        });
        assert_eq!(state.conversations[0].title, "Auto Title");
        assert!(matches!(effects.as_slice(), [Effect::SetConversations(_)]));
    }

    // --- Debug filter ----------------------------------------------------

    #[test]
    fn conversation_loaded_hides_tool_messages_when_debug_off() {
        let mut state = WindowState {
            debug_enabled: false,
            ..Default::default()
        };
        let d = detail(
            "c1",
            vec![
                msg("user", "hi"),
                msg("tool", "tool noise"),
                msg("assistant", "answer"),
                msg("assistant", "   "), // empty (tool-calls only) assistant
            ],
        );
        let effects = state.apply(UiMessage::ConversationLoaded(d));
        // The cached (unfiltered) conversation keeps all 4 messages...
        assert_eq!(
            state.current_conversation.as_ref().unwrap().messages.len(),
            4
        );
        // ...but the chat view receives only user + non-empty assistant.
        match effects.as_slice() {
            [
                Effect::SetModelSelection(_),
                Effect::LoadConversationIntoChat(filtered),
                Effect::SidePaneSetScratchpad(_),
                Effect::RefreshSidePaneTasks,
                Effect::FetchScratchpad(_),
            ] => {
                let roles: Vec<&str> = filtered.messages.iter().map(|m| m.role.as_str()).collect();
                assert_eq!(roles, vec!["user", "assistant"]);
                assert_eq!(filtered.messages[1].content, "answer");
            }
            other => panic!("unexpected effects: {other:?}"),
        }
    }

    #[test]
    fn conversation_loaded_shows_tool_messages_when_debug_on() {
        let mut state = WindowState {
            debug_enabled: true,
            ..Default::default()
        };
        let d = detail(
            "c1",
            vec![
                msg("user", "hi"),
                msg("tool", "tool noise"),
                msg("assistant", "   "),
            ],
        );
        let effects = state.apply(UiMessage::ConversationLoaded(d));
        match effects.as_slice() {
            [
                Effect::SetModelSelection(_),
                Effect::LoadConversationIntoChat(filtered),
                Effect::SidePaneSetScratchpad(_),
                Effect::RefreshSidePaneTasks,
                Effect::FetchScratchpad(_),
            ] => {
                // Debug on: nothing is filtered out.
                assert_eq!(filtered.messages.len(), 3);
            }
            other => panic!("unexpected effects: {other:?}"),
        }
    }

    #[test]
    fn conversation_loaded_sets_active_id_and_applies_stored_model_selection() {
        let mut state = WindowState::default();
        let mut d = detail("c9", vec![msg("user", "hi")]);
        d.model_selection = Some(selection("work", "claude"));
        let effects = state.apply(UiMessage::ConversationLoaded(d));
        assert_eq!(state.current_conversation_id.as_deref(), Some("c9"));
        match effects.as_slice() {
            [
                Effect::SetModelSelection(Some(sel)),
                Effect::LoadConversationIntoChat(_),
                Effect::SidePaneSetScratchpad(_),
                Effect::RefreshSidePaneTasks,
                Effect::FetchScratchpad(conv),
            ] => {
                assert_eq!(sel.connection_id, "work");
                assert_eq!(sel.model_id, "claude");
                assert_eq!(conv, "c9");
            }
            other => panic!("unexpected effects: {other:?}"),
        }
    }

    // --- Model-picker re-application -------------------------------------

    #[test]
    fn models_loaded_reapplies_active_conversation_selection_and_shows_picker() {
        let mut conv = detail("c1", vec![]);
        conv.model_selection = Some(selection("work", "claude"));
        let mut state = WindowState {
            current_conversation: Some(conv),
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        let effects = state.apply(UiMessage::ModelsLoaded(vec![listing("work", "claude")]));
        // set_models resets the dropdown, so apply must re-emit the stored
        // selection, then make the picker visible (non-empty list).
        match effects.as_slice() {
            [
                Effect::SetModels(models),
                Effect::SetModelSelection(Some(sel)),
                Effect::SetModelPickerVisible(true),
            ] => {
                assert_eq!(models.len(), 1);
                assert_eq!(sel.model_id, "claude");
            }
            other => panic!("unexpected effects: {other:?}"),
        }
    }

    #[test]
    fn models_loaded_empty_list_hides_picker_and_skips_reapply_when_no_conversation() {
        let mut state = WindowState::default();
        let effects = state.apply(UiMessage::ModelsLoaded(Vec::new()));
        match effects.as_slice() {
            [
                Effect::SetModels(models),
                Effect::SetModelPickerVisible(false),
            ] => {
                assert!(models.is_empty());
            }
            other => panic!("unexpected effects (no conversation => no reapply): {other:?}"),
        }
    }

    #[test]
    fn default_model_loaded_emits_set_default_model_effect() {
        let mut state = WindowState::default();
        let default = crate::selected_models::SelectedModel {
            connection_id: "work".to_string(),
            model_id: "claude".to_string(),
        };
        let effects = state.apply(UiMessage::DefaultModelLoaded(Some(default.clone())));
        match effects.as_slice() {
            [Effect::SetDefaultModel(Some(got))] => {
                assert_eq!(got.connection_id, "work");
                assert_eq!(got.model_id, "claude");
            }
            other => panic!("unexpected effects: {other:?}"),
        }
    }

    #[test]
    fn default_model_loaded_none_emits_set_default_model_none() {
        let mut state = WindowState::default();
        let effects = state.apply(UiMessage::DefaultModelLoaded(None));
        assert!(
            matches!(effects.as_slice(), [Effect::SetDefaultModel(None)]),
            "unresolved default must still emit a (None) effect: {effects:?}"
        );
    }

    #[test]
    fn dangling_model_warning_for_current_conversation_clears_picker_and_cached_selection() {
        let mut conv = detail("c1", vec![]);
        conv.model_selection = Some(selection("old", "gone"));
        let mut state = WindowState {
            current_conversation: Some(conv),
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        let warning = api::ConversationWarning::DanglingModelSelection {
            previous_selection: selection("old", "gone"),
            fallback_to: selection("work", "claude"),
        };
        let effects = state.apply(UiMessage::ConversationWarning {
            conversation_id: "c1".to_string(),
            warning,
        });
        // Cached selection must be cleared so a later ModelsLoaded doesn't
        // re-apply the stale dangling selection, contradicting the toast.
        assert!(
            state
                .current_conversation
                .as_ref()
                .unwrap()
                .model_selection
                .is_none()
        );
        match effects.as_slice() {
            [Effect::SetModelSelection(None), Effect::ShowToast(message)] => {
                assert!(message.contains("gone"));
                assert!(message.contains("claude"));
            }
            other => panic!("unexpected effects: {other:?}"),
        }
    }

    #[test]
    fn dangling_model_warning_for_other_conversation_only_toasts() {
        let mut conv = detail("c1", vec![]);
        conv.model_selection = Some(selection("old", "gone"));
        let mut state = WindowState {
            current_conversation: Some(conv),
            current_conversation_id: Some("c1".to_string()),
            ..Default::default()
        };
        let warning = api::ConversationWarning::DanglingModelSelection {
            previous_selection: selection("old", "gone"),
            fallback_to: selection("work", "claude"),
        };
        let effects = state.apply(UiMessage::ConversationWarning {
            conversation_id: "c2-not-current".to_string(),
            warning,
        });
        // Not the current conversation: don't touch the picker or cached
        // selection — only surface the advisory toast.
        assert!(
            state
                .current_conversation
                .as_ref()
                .unwrap()
                .model_selection
                .is_some()
        );
        assert!(
            matches!(effects.as_slice(), [Effect::ShowToast(_)]),
            "unexpected effects: {effects:?}"
        );
    }

    // --- Simple passthrough variants -------------------------------------

    #[test]
    fn status_update_sets_status_text_verbatim() {
        let mut state = WindowState::default();
        let effects = state.apply(UiMessage::StatusUpdate("Connecting".to_string()));
        assert!(matches!(effects.as_slice(), [Effect::SetStatusText(t)] if t == "Connecting"));
    }

    #[test]
    fn error_message_is_prefixed_in_status_bar() {
        let mut state = WindowState::default();
        let effects = state.apply(UiMessage::Error("nope".to_string()));
        assert!(matches!(effects.as_slice(), [Effect::SetStatusText(t)] if t == "Error: nope"));
    }

    #[test]
    fn connected_sets_label_and_enables_send() {
        let mut state = WindowState::default();
        let effects = state.apply(UiMessage::Connected {
            label: "Local daemon".to_string(),
        });
        assert!(
            matches!(
                effects.as_slice(),
                [Effect::SetStatusText(t), Effect::SetSendSensitive(true)] if t == "Local daemon"
            ),
            "unexpected effects: {effects:?}"
        );
    }

    #[test]
    fn conversation_created_sets_active_id_without_effects() {
        let mut state = WindowState::default();
        let effects = state.apply(UiMessage::ConversationCreated {
            id: "new-c".to_string(),
        });
        assert_eq!(state.current_conversation_id.as_deref(), Some("new-c"));
        assert!(effects.is_empty());
    }
}
