use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use desktop_assistant_api_model as api;
use desktop_assistant_client_common::{AssistantClient, ConnectionConfig, Connector};
use gtk4::prelude::*;
use gtk4::{
    Align, Application, ApplicationWindow, Box as GtkBox, Button, CheckButton, Entry, Label,
    MenuButton, Orientation, Paned, Popover, Revealer, RevealerTransitionType, Separator, Window,
    gdk, glib,
};
use tokio::sync::mpsc;

use crate::async_bridge::{AsyncBridge, UiMessage, connection_manager};
use crate::management_client;
use crate::voice_client::VoiceController;
use crate::widgets::chat_view::ChatView;
use crate::widgets::conversation_side_pane::{ConversationSidePane, SidePaneAction};
use crate::widgets::input_bar::InputBar;
use crate::widgets::model_picker::ModelPicker;
use crate::widgets::sidebar::Sidebar;
use crate::widgets::tasks_panel::TasksPanel;

mod voice;

use client_ui_common::{Effect, WindowState};
use voice::{speak_text, wire_embedded_mic, wire_voice_controls};

/// The window's widget handles, bundled so the bridge's UI-message executor
/// takes one `Rc<WindowWidgets>` instead of 13 separate `Rc`s (and the bridge
/// closure clones one handle instead of a 13-way `clone!` list). Each field is
/// already individually shareable (`Rc`/`Rc<RefCell<…>>`); bundling them keeps
/// the executor and `ensure_active_conversation` signatures small without
/// changing any sharing semantics. Built once in `AdelieWindow::new`.
struct WindowWidgets {
    state: Rc<RefCell<WindowState>>,
    sidebar: Rc<Sidebar>,
    chat_view: Rc<RefCell<ChatView>>,
    status_label: Rc<Label>,
    /// Read-only context-window fill indicator (#341).
    context_label: Rc<Label>,
    client: Rc<RefCell<Option<Arc<Connector>>>>,
    /// Connector handed from the (tokio) connect task to the GTK main thread
    /// (#106). The connect task stores it here just before `UiMessage::Connected`;
    /// the `Connected` handler drains it, registers the voice-mode client tools,
    /// and moves it into `client`. Replaces the removed `UiMessage::ClientReady`
    /// / `Effect::SetClient` round-trip — the core no longer names `Connector`
    /// (wasm) — while keeping the connector's arrival ordered before
    /// `ConversationsLoaded` (the `Connected` trigger rides the same FIFO channel).
    pending_connector: Arc<Mutex<Option<Arc<Connector>>>>,
    input_bar: Rc<InputBar>,
    model_picker: Rc<ModelPicker>,
    tasks_panel: Rc<TasksPanel>,
    side_pane: Rc<ConversationSidePane>,
    toast_revealer: Rc<Revealer>,
    toast_label: Rc<Label>,
    embedded_voice: Rc<Option<crate::voice_embedded::EmbeddedVoice>>,
    voice: Rc<RefCell<Option<VoiceController>>>,
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

        // Layout: resizable paned split between sidebar and chat. The left pane
        // holds the conversation list directly. Background tasks (issue #19)
        // used to be a second `Stack` page here, but they now live in a
        // dedicated popup window (`tasks_window`, built below) so the task list
        // and per-task log have room to show real detail — what each task is
        // doing right now, its MCP/tool calls, etc.
        let paned = Paned::new(Orientation::Horizontal);

        let sidebar = Sidebar::new();
        let tasks_panel = TasksPanel::new();

        let left_box = GtkBox::new(Orientation::Vertical, 0);
        left_box.set_size_request(280, -1);
        sidebar.container.set_vexpand(true);
        left_box.append(&sidebar.container);

        paned.set_start_child(Some(&left_box));

        // Background-tasks popup. Built once and hidden (not destroyed) on close
        // so the long-lived `TasksPanel` — which keeps receiving live `Task*`
        // events whether or not the window is visible — survives reopen with its
        // model and selection intact.
        //
        // Deliberately *not* `transient_for` the main window and *not* added to
        // the `Application`: a transient would stay stacked above the chat and
        // share its taskbar entry, and an app-owned window would keep the
        // process alive after the chat window closed. As an independent toplevel
        // it gets its own taskbar entry and can sit behind the chat; we tie only
        // its teardown to the client below so it can't outlive it.
        let tasks_window = Window::builder()
            .title("Background Tasks")
            .modal(false)
            .default_width(820)
            .default_height(560)
            .build();
        tasks_window.set_child(Some(&tasks_panel.container));
        tasks_window.set_hide_on_close(true);
        let tasks_window = Rc::new(tasks_window);

        // Window-lifetime shutdown signal (GTK-1). The `connection_manager`
        // task (spawned below) subscribes to this; `close-request` flips it so
        // the manager stops reconnecting, drops its `Arc<Connector>` clone
        // (ending the daemon session), and drops its `ui_tx` clone — the first
        // domino in letting the bridge channel close and the widget tree free.
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        // The popup isn't owned by the `Application`, so closing the main window
        // won't reap it on its own. Destroy it when the chat window closes so it
        // never lingers past client exit (`hide_on_close` is bypassed by an
        // explicit `destroy`). The same close also signals connection shutdown
        // (GTK-1) — both for the titlebar close button and the Disconnect menu
        // item (which routes through `window.close()`).
        window.connect_close_request(glib::clone!(
            #[strong]
            tasks_window,
            move |_| {
                let _ = shutdown_tx.send(true);
                tasks_window.destroy();
                glib::Propagation::Proceed
            }
        ));
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
        //
        // Themed-icon fallback chain so the toggle renders across icon themes:
        // Breeze (KDE) lacks the GNOME `sidebar-show-*-symbolic` names (it shows
        // a broken glyph), so prefer Breeze's `tools-symbolic` (a wrench), then
        // fall back to `applications-utilities-symbolic` /
        // `applications-engineering-symbolic`, which exist in both Breeze and
        // Adwaita.
        let toggle_icon = gtk4::gio::ThemedIcon::from_names(&[
            "tools-symbolic",
            "applications-utilities-symbolic",
            "applications-engineering-symbolic",
        ]);
        let side_pane_toggle = Button::new();
        side_pane_toggle.set_child(Some(&gtk4::Image::from_gicon(&toggle_icon)));
        side_pane_toggle.add_css_class("flat");
        side_pane_toggle.set_tooltip_text(Some("Tasks & scratchpad for this conversation"));
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

        let tasks_btn = Button::with_label("Background Tasks");
        tasks_btn.add_css_class("context-button");
        tasks_btn.set_halign(Align::Fill);
        menu_box.append(&tasks_btn);

        // Per-conversation personality override (#70). Opens a modal picker
        // pre-filled from the active conversation's stored override.
        let personality_btn = Button::with_label("Personality…");
        personality_btn.add_css_class("context-button");
        personality_btn.set_halign(Align::Fill);
        menu_box.append(&personality_btn);

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

        // Read-only context-window fill indicator (#341). Right-aligned,
        // before the Debug check. Hidden until the first reading arrives.
        let context_label = Label::new(None);
        context_label.set_halign(gtk4::Align::End);
        context_label.set_margin_end(8);
        context_label.add_css_class("context-fill");
        context_label.set_visible(false);
        context_label.set_tooltip_text(Some(
            "Context window used / budget. Amber near the compaction line, red at overflow.",
        ));
        status_bar.append(&context_label);

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
        let state = Rc::new(RefCell::new(WindowState::default()));

        // Wrap widgets in Rc for closures
        let sidebar = Rc::new(sidebar);
        let chat_view = Rc::new(RefCell::new(chat_view));
        let input_bar = Rc::new(input_bar);
        let status_label = Rc::new(status_label);
        let context_label = Rc::new(context_label);
        let model_picker = Rc::new(model_picker);
        let tasks_panel = Rc::new(tasks_panel);
        let side_pane = Rc::new(side_pane);
        let toast_revealer = Rc::new(toast_revealer);
        let toast_label = Rc::new(toast_label);

        // Voice config (issue #65): pick between the standalone voice daemon
        // (default; `org.desktopAssistant.Voice` over D-Bus) and an in-process
        // embedded engine. Loaded once from `~/.config/adele-gtk/voice.toml`; an
        // absent/partial file resolves to the daemon mode, so existing users see
        // no change. When embedded, `embedded_voice` is the in-process engine
        // (lazily initialized on first mic use — no models load, no mic opens,
        // at startup); `None` on the daemon path.
        let voice_config = crate::voice_config::VoiceConfig::load();
        let embedded_voice: Rc<Option<crate::voice_embedded::EmbeddedVoice>> =
            Rc::new(if voice_config.is_embedded() {
                Some(crate::voice_embedded::EmbeddedVoice::new(
                    crate::voice_embedded::EmbeddedConfig {
                        audio: voice_config.audio.clone(),
                        vad: voice_config.vad.clone(),
                        stt: voice_config.stt.clone(),
                        tts: voice_config.tts.clone(),
                    },
                ))
            } else {
                None
            });
        // Connector wrapped in Arc for async tasks, Rc<RefCell<>> for GTK
        // thread. The `Connector` owns the transport; call `.client()` for the
        // `&TransportClient` surface (`as_commands()`, `AssistantClient` RPCs).
        let client: Rc<RefCell<Option<Arc<Connector>>>> = Rc::new(RefCell::new(None));

        // Cross-thread connector hand-off (#106): the tokio connect task stores
        // the freshly connected `Arc<Connector>` here just before it sends
        // `UiMessage::Connected`; the `Connected` handler (main thread) drains it.
        // `Arc<Connector>` is `Send`, so this `Arc<Mutex<…>>` is the seam that
        // carries the handle across the thread boundary now that the core can't.
        let pending_connector: Arc<Mutex<Option<Arc<Connector>>>> = Arc::new(Mutex::new(None));

        // Handle to the standalone voice daemon (`org.desktopAssistant.Voice`),
        // declared here — *before* the bridge — so the `handle_ui_message`
        // executor can also reach it (issue #80): narration prefers the daemon's
        // warm Speaker over the slow embedded engine. Populated later by
        // `wire_voice_controls` (daemon path) once the async connect lands;
        // until then it's `None` (and `Effect::Speak` falls back to embedded).
        let voice: Rc<RefCell<Option<VoiceController>>> = Rc::new(RefCell::new(None));

        // Bundle every widget handle the UI-message executor needs into one
        // shareable struct, so the bridge closure captures a single
        // `Rc<WindowWidgets>` instead of a 13-way `clone!` list and
        // `handle_ui_message` takes one argument instead of 13. The individual
        // `let` bindings above stay live — other closures (send action, sidebar
        // signals, side-pane callbacks) still clone them directly.
        let widgets = Rc::new(WindowWidgets {
            state: state.clone(),
            sidebar: sidebar.clone(),
            chat_view: chat_view.clone(),
            status_label: status_label.clone(),
            context_label: context_label.clone(),
            client: client.clone(),
            pending_connector: pending_connector.clone(),
            input_bar: input_bar.clone(),
            model_picker: model_picker.clone(),
            tasks_panel: tasks_panel.clone(),
            side_pane: side_pane.clone(),
            toast_revealer: toast_revealer.clone(),
            toast_label: toast_label.clone(),
            embedded_voice: embedded_voice.clone(),
            voice: voice.clone(),
        });

        // Set up async bridge with UI message handler
        let bridge = AsyncBridge::new(glib::clone!(
            #[strong]
            widgets,
            move |msg, ui_tx| {
                handle_ui_message(msg, &widgets, ui_tx);
            }
        ));
        let bridge = Rc::new(bridge);

        // Voice controls (issue #59). The voice daemon is a *separate* D-Bus
        // service (`org.desktopAssistant.Voice`); it has no relationship to the
        // orchestrator transport above. We connect once, share the handle so
        // both the mic button and the Settings → Voice tab can drive it, and
        // gate the UI on the daemon actually owning its bus name (graceful
        // degradation when it isn't running / models aren't provisioned). The
        // `voice` handle itself was declared above (before the bridge) so the
        // `Effect::Speak` executor can prefer the daemon for narration (#80).
        // The daemon controls are wired only on the daemon path. In embedded
        // mode the mic button is driven in-process (wired in the send block
        // below, where it can reuse the send action) and shown immediately,
        // since there is no daemon to probe for.
        if embedded_voice.is_some() {
            input_bar.set_voice_available(true);
        } else {
            wire_voice_controls(&voice, &input_bar, &bridge, &state);
        }

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
                let Some(connector) = client.borrow().clone() else {
                    return;
                };
                crate::async_bridge::spawn_on_runtime(async move {
                    let Some(cmds) = connector.client().as_commands() else {
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
                let Some(connector) = client.borrow().clone() else {
                    return;
                };
                crate::async_bridge::spawn_on_runtime(async move {
                    if let Some(cmds) = connector.client().as_commands() {
                        let _ = cmds
                            .send_command(api::Command::CancelBackgroundTask { id })
                            .await;
                    }
                });
            }
        ));

        // Spawn persistent connection manager (connect → forward → reconnect).
        // It delivers the freshly connected `Connector` to the main thread by
        // stashing it in `pending_connector` and sending `UiMessage::Connected`;
        // the `Connected` handler drains it, registers voice-mode tools, and moves
        // it into `client` (#106 — the core no longer carries the handle, for
        // wasm). It exits when the window's `close-request` signals `shutdown_rx`
        // (GTK-1), dropping its connector and sender clones.
        {
            let ui_tx = bridge.ui_sender();
            bridge.spawn(connection_manager(
                config.clone(),
                ui_tx,
                pending_connector.clone(),
                shutdown_rx,
            ));
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

                    if let Some(connector) = client.borrow().clone() {
                        let tx = bridge.ui_sender();
                        bridge.spawn(async move {
                            match connector.client().get_conversation(&conv_id).await {
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
                if let Some(connector) = client.borrow().clone() {
                    let tx = bridge.ui_sender();
                    bridge.spawn(async move {
                        let client = connector.client();
                        match client.create_conversation("New Conversation").await {
                            Ok(id) => {
                                // Set the new conversation active, then refresh
                                // the list. The reducer's `ConversationsLoaded`
                                // handler then issues a SINGLE picker-setting
                                // `LoadConversation` for it (GTK-10) — no
                                // separate explicit fetch here.
                                let _ = tx.send(UiMessage::ConversationCreated { id });
                                if let Ok(convs) = client.list_conversations().await {
                                    let _ = tx.send(UiMessage::ConversationsLoaded(convs));
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
            move |id: &str| {
                if let Some(connector) = client.borrow().clone() {
                    let tx = bridge.ui_sender();
                    let id = id.to_string();
                    bridge.spawn(async move {
                        match connector.client().delete_conversation(&id).await {
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
            move |id: &str| {
                let (id, current_title) = {
                    let s = state.borrow();
                    match s.conversations.iter().find(|c| c.id == id) {
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
                        if let Some(connector) = client.borrow().clone() {
                            let tx = bridge.ui_sender();
                            let id = id.clone();
                            let title = new_title.clone();
                            bridge.spawn(async move {
                                match connector.client().rename_conversation(&id, &title).await {
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
            move |id: &str| {
                let (id, archived) = {
                    let s = state.borrow();
                    match s.conversations.iter().find(|c| c.id == id) {
                        Some(conv) => (conv.id.clone(), conv.archived),
                        None => return,
                    }
                };
                if let Some(connector) = client.borrow().clone() {
                    let tx = bridge.ui_sender();
                    let id = id.clone();
                    bridge.spawn(async move {
                        let client = connector.client();
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
                if let Some(connector) = client.borrow().clone() {
                    let tx = bridge.ui_sender();
                    bridge.spawn(async move {
                        let client = connector.client();
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
                input_bar,
                move || {
                    // Peek (not take): if the core rejects the send — a reply is
                    // still streaming (TUI-7) — or the connection gate below
                    // blocks it, the text must stay in the live composer. The
                    // send *decision* lives in the shared core now: emit
                    // `UiMessage::SubmitPrompt` and let `apply` run the streaming
                    // gate, draw the optimistic bubble, drop the saved draft, and
                    // pick the per-turn voice refinement. The editor is cleared
                    // only once the core accepts, in the `Effect::SendPrompt` arm
                    // below. Trim + skip-empty stays client-side (no whitespace).
                    let text = input_bar.peek_text();
                    let text = text.trim().to_string();
                    if text.is_empty() {
                        return;
                    }
                    // Connection gate: transport state the core doesn't own (the
                    // TUI gates the same way before `SubmitPrompt`). Keep the text
                    // and surface why, not a bubble that can't be sent.
                    if client.borrow().is_none() {
                        let _ = bridge_ref.ui_sender().send(UiMessage::Error(
                            "Not connected — message not sent (your text is preserved)".to_string(),
                        ));
                        return;
                    }
                    let _ = bridge_ref
                        .ui_sender()
                        .send(UiMessage::SubmitPrompt { prompt: text });
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

            // Embedded voice (issue #65): when the in-process engine is active,
            // the mic button dictates locally and sends via the same
            // `send_action` as a typed message. Wired here (inside the send
            // block) so it can reuse `send_action`; the daemon path wires the
            // mic separately in `wire_voice_controls`.
            if let Some(engine) = (*embedded_voice).clone() {
                wire_embedded_mic(engine, &input_bar, &send_action, &state, &bridge);
            }
        }

        // Per-conversation `You:` (voice input) dropdown (issue #80). A user
        // change routes a `SetVoiceIn` for the *current* conversation through
        // the same handler as everything else, so the pure `apply` records it.
        // The `set_voice_in_active` reflection on conversation switch (below, in
        // the `LoadConversationIntoChat` effect) is suppressed, so it never
        // echoes back here. With no active conversation the change is dropped
        // (the dropdown is only meaningful against a conversation).
        input_bar.connect_voice_in_changed(glib::clone!(
            #[strong]
            state,
            #[strong]
            bridge,
            move |enabled| {
                let Some(conv_id) = state.borrow().current_conversation_id.clone() else {
                    return;
                };
                let _ = bridge.ui_sender().send(UiMessage::SetVoiceIn {
                    conversation_id: conv_id,
                    enabled,
                });
            }
        ));

        // Per-conversation `Adele:` (voice output) dropdown (issue #80). A user
        // change routes a `SetAdeleOutput` for the *current* conversation through
        // the same handler so the pure `apply` records it. The
        // `set_adele_output_active` reflection on conversation switch / model
        // drive is suppressed, so it never echoes back here. With no active
        // conversation the change is dropped.
        input_bar.connect_adele_output_changed(glib::clone!(
            #[strong]
            state,
            #[strong]
            bridge,
            move |level| {
                let Some(conv_id) = state.borrow().current_conversation_id.clone() else {
                    return;
                };
                let _ = bridge.ui_sender().send(UiMessage::SetAdeleOutput {
                    conversation_id: conv_id,
                    level,
                });
            }
        ));

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
            #[strong]
            voice,
            move |_| {
                menu_popover.popdown();
                let Some(connector) = client.borrow().clone() else {
                    status_label.set_text("Not connected — settings unavailable");
                    return;
                };
                if connector.client().as_commands().is_none() {
                    status_label.set_text(
                        "Settings require a local-socket or WebSocket connection (not available over D-Bus)",
                    );
                    return;
                }
                // The Voice tab talks to its own daemon, so hand the dialog a
                // controller regardless of which orchestrator transport we're
                // on. When voice hasn't connected yet (or has no session bus),
                // fall back to an inert controller — the tab then shows its
                // "unavailable" state.
                let voice_handle = match voice.borrow().clone() {
                    Some(v) => v,
                    None => VoiceController::unavailable(),
                };
                crate::widgets::settings_dialog::show_settings_dialog(
                    &window,
                    Arc::clone(&connector),
                    Rc::clone(&bridge),
                    voice_handle,
                );
                // The user may have added/removed connections; re-query the
                // aggregated model list so the header picker reflects the new
                // set. Fire-and-forget — errors are non-fatal. Runs once when
                // Settings is opened (so it picks up the previous session's
                // changes); the dialog itself keeps its own tabs in sync.
                let tx = bridge.ui_sender();
                bridge.spawn(async move {
                    let transport = connector.client();
                    match management_client::list_available_models(transport, None, false).await {
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
                        match management_client::get_purposes(transport).await {
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
                let Some(connector) = client.borrow().clone() else {
                    status_label.set_text("Not connected — knowledge base unavailable");
                    return;
                };
                let browser = crate::widgets::knowledge_browser::KnowledgeBrowser::new(
                    &window,
                    connector,
                    Rc::clone(&bridge),
                );
                browser.present();
            }
        ));

        // Hamburger menu: Background Tasks → present the tasks popup. The window
        // and its `TasksPanel` already exist and stay current via the live
        // `Task*` event stream, so opening is just a `present()`.
        tasks_btn.connect_clicked(glib::clone!(
            #[weak]
            menu_popover,
            #[strong]
            tasks_window,
            move |_| {
                menu_popover.popdown();
                tasks_window.present();
            }
        ));

        // Hamburger menu: Personality… → open the per-conversation personality
        // picker (#70). Mirrors the model picker's gating: the override travels
        // on the command channel (UDS/WS), which D-Bus doesn't expose. Pre-fills
        // from the active conversation's stored override and, on Save, dispatches
        // `set_conversation_personality` through the async bridge, then reloads
        // the conversation so `current_conversation.conversation_personality`
        // (the next pre-fill source) stays in sync with the daemon.
        personality_btn.connect_clicked(glib::clone!(
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
            #[strong]
            state,
            move |_| {
                menu_popover.popdown();
                let Some(connector) = client.borrow().clone() else {
                    status_label.set_text("Not connected — personality unavailable");
                    return;
                };
                if connector.client().as_commands().is_none() {
                    status_label.set_text(
                        "Personality settings require a local-socket or WebSocket connection (not available over D-Bus)",
                    );
                    return;
                }
                let (conv_id, prefill) = {
                    let s = state.borrow();
                    let id = s.current_conversation_id.clone();
                    let prefill = s
                        .current_conversation()
                        .and_then(|c| c.conversation_personality);
                    (id, prefill)
                };
                let Some(conv_id) = conv_id else {
                    status_label.set_text("Open a conversation first to set its personality");
                    return;
                };

                let bridge_for_save = Rc::clone(&bridge);
                let connector_for_save = Arc::clone(&connector);
                let status_for_save = status_label.clone();
                crate::widgets::personality_dialog::show_personality_dialog(
                    &window,
                    prefill.as_ref(),
                    move |personality| {
                        let tx = bridge_for_save.ui_sender();
                        let connector = Arc::clone(&connector_for_save);
                        let conv_id = conv_id.clone();
                        let status = status_for_save.clone();
                        status.set_text("Saving personality…");
                        bridge_for_save.spawn(async move {
                            let transport = connector.client();
                            match management_client::set_conversation_personality(
                                transport,
                                &conv_id,
                                personality,
                            )
                            .await
                            {
                                Ok(_stored) => {
                                    // Reload so the next pre-fill reflects the
                                    // stored personality override (and any
                                    // all-None clear). Use `ConversationReloaded`
                                    // so refreshing the personality cache doesn't
                                    // reset the model picker (issue #72).
                                    match transport.get_conversation(&conv_id).await {
                                        Ok(detail) => {
                                            let _ =
                                                tx.send(UiMessage::ConversationReloaded(detail));
                                        }
                                        Err(e) => {
                                            let _ = tx.send(UiMessage::Error(format!(
                                                "Reload after personality save: {e}"
                                            )));
                                        }
                                    }
                                }
                                Err(e) => {
                                    let _ = tx.send(UiMessage::Error(format!(
                                        "Save personality: {e}"
                                    )));
                                }
                            }
                        });
                    },
                );
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
                    && let Some(connector) = client.borrow().clone()
                {
                    let tx = bridge.ui_sender();
                    bridge.spawn(async move {
                        match connector.client().get_conversation(&conv_id).await {
                            Ok(detail) => {
                                // Reload (re-filter) the same conversation — keep
                                // the model picker (issue #72).
                                let _ = tx.send(UiMessage::ConversationReloaded(detail));
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
        // `Open Conversation` hides the tasks popup and loads the task's
        // conversation into the main window so the streaming output keeps
        // flowing into the chat view.
        tasks_panel.connect_cancel(glib::clone!(
            #[strong]
            client,
            #[strong]
            bridge,
            move |task_id| {
                let Some(connector) = client.borrow().clone() else {
                    return;
                };
                let tx = bridge.ui_sender();
                bridge.spawn(async move {
                    let Some(cmds) = connector.client().as_commands() else {
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
            tasks_window,
            move |conv_id| {
                tasks_window.set_visible(false);
                let Some(connector) = client.borrow().clone() else {
                    return;
                };
                let tx = bridge.ui_sender();
                bridge.spawn(async move {
                    match connector.client().get_conversation(&conv_id).await {
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

fn handle_ui_message(
    msg: UiMessage,
    widgets: &WindowWidgets,
    ui_tx: &mpsc::UnboundedSender<UiMessage>,
) {
    let WindowWidgets {
        state,
        sidebar,
        chat_view,
        status_label,
        context_label,
        client,
        pending_connector,
        input_bar,
        model_picker,
        tasks_panel,
        side_pane,
        toast_revealer,
        toast_label,
        embedded_voice,
        voice,
    } = widgets;

    // Connector hand-off (#106): the connect task stashes the `Arc<Connector>`
    // in `pending_connector` just before sending `Connected`. Note it before
    // `apply` consumes `msg`; the actual adopt happens after the effects below,
    // matching the old ordering (the connector was stored *after* `Connected`'s
    // own effects ran, since `ClientReady` followed `Connected` on the channel).
    let connector_arrived = matches!(&msg, UiMessage::Connected { .. });

    // Composer draft (#2): a switch to a *different* conversation must save the
    // outgoing conversation's unsent text and restore the incoming one. Detect
    // it here — before `apply` repoints `current_conversation_id` — and snapshot
    // the outgoing draft into the model now (the text view stays the live editor;
    // the model owns the saved draft). A same-conversation reload
    // (`ConversationReloaded`, or re-selecting the open conversation) is NOT a
    // switch, so it leaves the in-progress draft untouched. The matching restore
    // runs after the effects below, once the new transcript is loaded.
    let switch_target = if let UiMessage::ConversationLoaded(detail) = &msg {
        let mut s = state.borrow_mut();
        match s.current_conversation_id.clone() {
            // Leaving another conversation: save its draft, then restore ours.
            Some(outgoing) if outgoing != detail.id => {
                s.set_composer_draft(&outgoing, input_bar.peek_text());
                Some(detail.id.clone())
            }
            // First load (nothing open yet): nothing to save, still restore ours.
            None => Some(detail.id.clone()),
            // Re-selecting the already-open conversation: not a switch.
            Some(_) => None,
        }
    } else {
        None
    };

    // Failed send (#12): capture the un-sent prompt before `apply` consumes
    // `msg`, so the post-effect step can refill the live composer with it. The
    // core's `SendFailed` rolls the optimistic bubble out of the model but,
    // being view-agnostic, can't put the editor text back — so the client does,
    // mirroring the TUI's composer refill on a failed send.
    let failed_prompt = if let UiMessage::SendFailed { prompt, .. } = &msg {
        Some(prompt.clone())
    } else {
        None
    };

    // Pure decision: mutate state + compute the effects to perform.
    let effects = state.borrow_mut().apply(msg);

    // Thin executor: perform each effect against the real widgets, in order.
    for effect in effects {
        match effect {
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
                // Reflect the newly-active conversation's `You:` + `Adele:`
                // dropdowns on the input bar (issue #80). `apply` has already
                // pointed `current_conversation_id` at this conversation, so
                // these read the right per-conversation state. Suppressed inside
                // each setter, so they don't echo a write back.
                let (voice_in, adele_output) = {
                    let s = state.borrow();
                    (s.voice_in_for_current(), s.adele_output_for_current())
                };
                input_bar.set_voice_in_active(voice_in);
                input_bar.set_adele_output_active(adele_output);
            }
            Effect::ReloadConversation(id) => {
                // Re-fetch an already-open conversation (reconnect / debug /
                // personality refresh) and refresh the cached detail + chat via
                // `ConversationReloaded`, which deliberately leaves the model
                // picker untouched (issue #72).
                if let Some(connector) = client.borrow().clone() {
                    let tx = ui_tx.clone();
                    crate::async_bridge::spawn_on_runtime(async move {
                        match connector.client().get_conversation(&id).await {
                            Ok(detail) => {
                                let _ = tx.send(UiMessage::ConversationReloaded(detail));
                            }
                            Err(e) => {
                                tracing::warn!("reload conversation failed: {e}");
                            }
                        }
                    });
                }
            }
            Effect::LoadConversation(id) => {
                // Fresh switch-style load: the reply is `ConversationLoaded`,
                // which applies the model picker selection (GTK-10).
                if let Some(connector) = client.borrow().clone() {
                    let tx = ui_tx.clone();
                    crate::async_bridge::spawn_on_runtime(async move {
                        match connector.client().get_conversation(&id).await {
                            Ok(detail) => {
                                let _ = tx.send(UiMessage::ConversationLoaded(detail));
                            }
                            Err(e) => {
                                let _ =
                                    tx.send(UiMessage::Error(format!("Load conversation: {e}")));
                            }
                        }
                    });
                }
            }
            Effect::RefetchConversationList => {
                // A sibling client or the voice daemon changed the user's list
                // (#1). Re-fetch it over the live transport and deliver it as
                // `ConversationListRefetched` — a sidebar-only repaint that
                // leaves the open conversation + model picker untouched (the
                // reducer deliberately does not reload the chat here). Same RPC
                // as the connect-time refresh; only the reply variant differs.
                if let Some(connector) = client.borrow().clone() {
                    let tx = ui_tx.clone();
                    crate::async_bridge::spawn_on_runtime(async move {
                        match connector.client().list_conversations().await {
                            Ok(convs) => {
                                let _ = tx.send(UiMessage::ConversationListRefetched(convs));
                            }
                            Err(e) => {
                                tracing::warn!("refetch conversation list failed: {e}");
                            }
                        }
                    });
                }
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
            Effect::SetContextUsage(usage) => {
                // Read-only fill indicator (#341): set text + colour class, or
                // hide when there is no reading for the open conversation.
                for class in crate::context_usage::ContextFillLevel::all_classes() {
                    context_label.remove_css_class(class);
                }
                match usage {
                    Some(u) => {
                        context_label.set_text(&u.readout());
                        context_label.add_css_class(u.level().css_class());
                        context_label.set_visible(true);
                    }
                    None => {
                        context_label.set_text("");
                        context_label.set_visible(false);
                    }
                }
            }
            Effect::AddUserMessage(content) => {
                chat_view.borrow_mut().add_user_message(&content);
            }
            Effect::ReceiveChunk(chunk) => {
                chat_view.borrow_mut().receive_chunk(&chunk);
            }
            Effect::CompleteStreaming(full) => {
                chat_view.borrow_mut().complete_streaming(&full);
                // Reply narration is owned entirely by the `AdeleOutput` gate
                // (#80): `apply()` emits `Effect::Speak` iff the reply's
                // conversation narrates, so there is nothing to do here. The
                // legacy #65 `voice_reply_pending` hook that spoke every
                // dictated reply regardless of the gate (and double-spoke
                // alongside `Effect::Speak`) was deleted — see GTK-3.
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
            Effect::SubscribeConversations(conversation_ids) => {
                // Tell the daemon which conversations we're viewing so it fans
                // their turn events to us — including turns started by another
                // client or the voice daemon (#1). Set-replace, fire-and-forget
                // (the daemon Acks). Only the command channel (Uds/Ws) carries
                // this; over D-Bus there's nothing to subscribe to, so skip.
                if let Some(connector) = client.borrow().clone() {
                    crate::async_bridge::spawn_on_runtime(async move {
                        let Some(cmds) = connector.client().as_commands() else {
                            return;
                        };
                        if let Err(e) = cmds
                            .send_command(api::Command::SubscribeConversations { conversation_ids })
                            .await
                        {
                            tracing::warn!("SubscribeConversations failed: {e}");
                        }
                    });
                }
            }
            Effect::FetchScratchpad(conversation_id) => {
                if let Some(connector) = client.borrow().clone() {
                    let tx = ui_tx.clone();
                    crate::async_bridge::spawn_on_runtime(async move {
                        let Some(cmds) = connector.client().as_commands() else {
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
            Effect::Speak(text) => {
                // Spoken output, daemon-first + chunked (#80). `apply` only emits
                // this when the active conversation has speech ON, so this is the
                // spoken path of both reply narration and a `say_this` aside. The
                // shared `speak_text` helper prefers a connected voice daemon's
                // warm `SayText` and otherwise falls back to the embedded engine,
                // chunking the text into one short sentence per synth call so a
                // long reply can't trip the per-synth ~20s timeout. Audio is
                // still produced by exactly one engine (daemon OR embedded).
                let voice = voice.borrow().clone();
                let embedded = (**embedded_voice).clone();
                let ui_tx = ui_tx.clone();
                crate::async_bridge::spawn_on_runtime(async move {
                    speak_text(voice, embedded, ui_tx, text).await;
                });
            }
            Effect::AddInlineNote(note) => {
                chat_view.borrow_mut().add_inline_note(&note);
            }
            Effect::SetAdeleOutputDropdown(level) => {
                // The model drove the output level (request_voice → OnDemand /
                // stop_voice → Disabled); reflect it on the dropdown. Suppressed
                // inside, so it doesn't echo a `SetAdeleOutput` write back
                // through the user callback.
                input_bar.set_adele_output_active(level);
            }
            Effect::SubmitClientToolResult {
                task_id,
                tool_call_id,
                result,
            } => {
                // Post the outcome back to the daemon so the suspended turn
                // resumes (issue #76). Spawned off the GTK thread; failures to
                // deliver are logged (the daemon's suspension timeout, #262, is
                // the backstop if delivery never lands).
                if let Some(connector) = client.borrow().clone() {
                    crate::async_bridge::spawn_on_runtime(async move {
                        if let Err(e) = connector
                            .submit_client_tool_result(&task_id, &tool_call_id, result)
                            .await
                        {
                            tracing::warn!("submit_client_tool_result failed: {e}");
                        }
                    });
                } else {
                    tracing::warn!("no connector to submit client-tool result for task {task_id}");
                }
            }
            Effect::SendPrompt {
                conversation_id,
                prompt,
                system_refinement,
            } => {
                // The core accepted the send (`UiMessage::SubmitPrompt`): it ran
                // the streaming gate (TUI-7), pushed the optimistic user message
                // into the model, and dropped this conversation's saved draft.
                // Mirror adele-tui's `send_prompt_from_input`: draw the bubble in
                // the chat widget (the core is view-agnostic so it can't), clear
                // the live composer, and run the actual RPC off the GTK loop.
                if let Some(connector) = client.borrow().clone() {
                    chat_view.borrow_mut().add_user_message(&prompt);
                    input_bar.set_text("");
                    let override_selection = model_picker.current_override();
                    let refinement = system_refinement.unwrap_or_default();
                    let ui_tx = ui_tx.clone();
                    crate::async_bridge::spawn_on_runtime(async move {
                        let client = connector.client();
                        // Socket transports (UDS/WS) carry the model override AND
                        // the refinement together via `send_prompt_full`; the
                        // shared `AssistantClient` exposes neither, so over D-Bus
                        // we fall back to `send_prompt_with_system_refinement`
                        // (the override is unavailable there regardless).
                        let result = match client.as_commands() {
                            Some(cmds) => {
                                cmds.send_prompt_full(
                                    &conversation_id,
                                    &prompt,
                                    override_selection,
                                    refinement,
                                )
                                .await
                            }
                            None => {
                                connector
                                    .send_prompt_with_system_refinement(
                                        &conversation_id,
                                        &prompt,
                                        &refinement,
                                    )
                                    .await
                            }
                        };
                        match result {
                            Ok(task_id) => {
                                // `conversation_id` was captured at send time
                                // (GTK-2): the stream stays tied to the
                                // conversation it was sent into even if the user
                                // switches away before the ack lands.
                                let _ = ui_tx.send(UiMessage::PromptSent {
                                    task_id,
                                    conversation_id,
                                });
                            }
                            Err(e) => {
                                // Roll the optimistic bubble back out of the model
                                // and refill the composer (the `SendFailed`
                                // post-hook above), then surface the error.
                                let _ = ui_tx.send(UiMessage::SendFailed {
                                    conversation_id,
                                    prompt,
                                });
                                let _ = ui_tx.send(UiMessage::Error(format!("Send error: {e}")));
                            }
                        }
                    });
                } else {
                    // The connection dropped between the send gate (in the send
                    // closure) and here — a queued `Disconnected` was applied
                    // first. The core already echoed the message into the model,
                    // but leave the live composer intact (don't clear, don't draw)
                    // so the text isn't lost, and surface why.
                    tracing::warn!("Effect::SendPrompt with no connector — message not sent");
                    let _ = ui_tx.send(UiMessage::Error(
                        "Not connected — message not sent (your text is preserved)".to_string(),
                    ));
                }
            }
        }
    }

    // Composer draft (#2): now that the switched-to conversation's transcript is
    // loaded, restore its saved draft into the live editor (empty if none). Done
    // here — not in the `LoadConversationIntoChat` effect — because that effect
    // also fires on a same-conversation reload (reconnect / debug refresh), which
    // must not overwrite the user's in-progress text. `switch_target` is `Some`
    // only on a real switch.
    if let Some(id) = switch_target {
        let draft = state.borrow().composer_draft(&id).to_string();
        input_bar.set_text(&draft);
    }

    // Composer refill on a failed send (#12; see `failed_prompt` above): put the
    // un-sent text back into the live editor so a transport error doesn't lose
    // it. The chat widget still shows the now-model-less optimistic bubble until
    // the next reload — a pre-existing GTK widget/model reconcile gap (ChatView
    // has no remove-last), not introduced here.
    if let Some(prompt) = failed_prompt {
        input_bar.set_text(&prompt);
    }

    // Adopt the connector the connect task handed off (#106), now that any
    // `Connected` effects (status, send-enable) have been applied. Lifted from
    // the old `Effect::SetClient` arm; the only change is the source — the
    // `pending_connector` cell instead of a reducer effect.
    //
    // Client-tool registration (gtk's built-in voice-mode tools, issue #78,
    // merged with any client-hosted MCP host tools, desktop-assistant#464) is
    // done once per (re)connect on the async side — in `connection_manager`'s
    // `drive_connection`, where the MCP host lives — so the merge happens in one
    // place. The Connector still replays the registered set after an internal
    // auto-reconnect (#246), so a daemon restart won't drop it.
    if connector_arrived && let Some(connector) = pending_connector.lock().unwrap().take() {
        *client.borrow_mut() = Some(connector);
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
    client: &Rc<RefCell<Option<Arc<Connector>>>>,
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

    let Some(connector) = client.borrow().clone() else {
        // Not connected yet — connection_manager will resend
        // ConversationsLoaded once the transport is up, and we'll re-run.
        return;
    };

    let tx = ui_tx.clone();
    match (target_id, target_index) {
        (Some(id), Some(idx)) => {
            sidebar.select_index(idx);
            crate::async_bridge::spawn_on_runtime(async move {
                match connector.client().get_conversation(&id).await {
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
                let client = connector.client();
                match client.create_conversation("New Conversation").await {
                    Ok(id) => {
                        // Same single-load flow as the New button (GTK-10): set
                        // it active + refresh the list; the reducer's
                        // `ConversationsLoaded` issues one picker-setting
                        // `LoadConversation`.
                        let _ = tx.send(UiMessage::ConversationCreated { id });
                        if let Ok(convs) = client.list_conversations().await {
                            let _ = tx.send(UiMessage::ConversationsLoaded(convs));
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
    const ICON_BYTES: &[u8] = include_bytes!("../../assets/adele.png");
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
