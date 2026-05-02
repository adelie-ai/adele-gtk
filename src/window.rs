use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use desktop_assistant_client_common::{
    AssistantClient, ChatMessage, ConnectionConfig, ConversationDetail, ConversationSummary,
    TransportClient,
};
use gtk4::prelude::*;
use gtk4::{
    Align, Application, ApplicationWindow, Box as GtkBox, Button, CheckButton, Entry, Label,
    MenuButton, Orientation, Paned, Popover, Separator, Window, gdk, glib,
};
use tokio::sync::mpsc;

use crate::async_bridge::{AsyncBridge, InternalMsg, UiMessage, connection_manager};
use crate::widgets::chat_view::ChatView;
use crate::widgets::input_bar::InputBar;
use crate::widgets::model_picker::ModelPicker;
use crate::widgets::sidebar::Sidebar;

/// Shared mutable state for the window.
struct WindowState {
    conversations: Vec<ConversationSummary>,
    current_conversation_id: Option<String>,
    current_conversation: Option<ConversationDetail>,
    pending_request_id: Option<String>,
    streaming_buffer: String,
    debug_enabled: bool,
}

// InternalMsg is now defined in async_bridge and re-imported.

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

        // Apply CSS
        let provider = gtk4::CssProvider::new();
        provider.load_from_data(include_str!("style.css"));
        gtk4::style_context_add_provider_for_display(
            &gdk::Display::default().expect("display"),
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );

        // Layout: resizable paned split between sidebar and chat
        let paned = Paned::new(Orientation::Horizontal);

        let sidebar = Sidebar::new();
        sidebar.container.set_size_request(280, -1); // minimum width
        paned.set_start_child(Some(&sidebar.container));
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

        // Hamburger menu button
        let menu_button = MenuButton::new();
        menu_button.set_icon_name("open-menu-symbolic");
        menu_button.add_css_class("flat");

        let menu_popover = Popover::new();
        menu_popover.add_css_class("context-popover");
        let menu_box = GtkBox::new(Orientation::Vertical, 0);

        let new_conn_btn = Button::with_label("New Connection");
        new_conn_btn.add_css_class("context-button");
        new_conn_btn.set_halign(Align::Fill);
        menu_box.append(&new_conn_btn);

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

        let chat_view = ChatView::new();
        right_box.append(&chat_view.container);

        let input_sep = Separator::new(Orientation::Horizontal);
        right_box.append(&input_sep);

        let input_bar = InputBar::new();
        input_bar.send_button.set_sensitive(false); // disabled until connected
        right_box.append(&input_bar.container);

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

        right_box.append(&status_bar);

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

        // Client wrapped in Arc for async tasks, Rc<RefCell<>> for GTK thread
        let client: Rc<RefCell<Option<Arc<TransportClient>>>> = Rc::new(RefCell::new(None));

        // Internal channel for transport bootstrap (sends non-Send types via main thread)
        let (internal_tx, mut internal_rx) = mpsc::unbounded_channel::<InternalMsg>();

        // Set up async bridge with UI message handler
        let bridge = {
            let state = Rc::clone(&state);
            let sidebar = Rc::clone(&sidebar);
            let chat_view = Rc::clone(&chat_view);
            let status_label = Rc::clone(&status_label);
            let client = Rc::clone(&client);
            let input_bar = Rc::clone(&input_bar);
            let model_picker = Rc::clone(&model_picker);

            AsyncBridge::new(move |msg| {
                handle_ui_message(
                    msg,
                    &state,
                    &sidebar,
                    &chat_view,
                    &status_label,
                    &client,
                    &input_bar,
                    &model_picker,
                );
            })
        };
        let bridge = Rc::new(bridge);

        // Spawn a local future to receive the transport client on the main thread
        {
            let client_ref = Rc::clone(&client);
            glib::spawn_future_local(async move {
                while let Some(msg) = internal_rx.recv().await {
                    match msg {
                        InternalMsg::ClientReady(transport) => {
                            *client_ref.borrow_mut() = Some(transport);
                        }
                    }
                }
            });
        }

        // Spawn persistent connection manager (connect → forward → reconnect)
        {
            let ui_tx = bridge.ui_sender();
            bridge.spawn(connection_manager(config.clone(), ui_tx, internal_tx));
        }

        // Sidebar row activation → load conversation
        {
            let client_ref = Rc::clone(&client);
            let state = Rc::clone(&state);
            let bridge = Rc::clone(&bridge);
            sidebar.list_box.connect_row_activated(move |_, row| {
                let idx = row.index() as usize;
                let state_borrow = state.borrow();
                if let Some(conv) = state_borrow.conversations.get(idx) {
                    let conv_id = conv.id.clone();
                    drop(state_borrow);

                    if let Some(client) = client_ref.borrow().clone() {
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
            });
        }

        // New conversation button
        {
            let client_ref = Rc::clone(&client);
            let bridge = Rc::clone(&bridge);
            sidebar.new_button.connect_clicked(move |_| {
                if let Some(client) = client_ref.borrow().clone() {
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
            });
        }

        // Context menu: Delete conversation
        {
            let client_ref = Rc::clone(&client);
            let bridge = Rc::clone(&bridge);
            let state = Rc::clone(&state);
            sidebar.connect_delete(move |idx| {
                let id = {
                    let s = state.borrow();
                    match s.conversations.get(idx) {
                        Some(conv) => conv.id.clone(),
                        None => return,
                    }
                };
                if let Some(client) = client_ref.borrow().clone() {
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
            });
        }

        // Context menu: Rename conversation
        {
            let client_ref = Rc::clone(&client);
            let bridge = Rc::clone(&bridge);
            let state = Rc::clone(&state);
            let window_ref = window.clone();
            sidebar.connect_rename(move |idx| {
                let (id, current_title) = {
                    let s = state.borrow();
                    match s.conversations.get(idx) {
                        Some(conv) => (conv.id.clone(), conv.title.clone()),
                        None => return,
                    }
                };

                let dialog = Window::builder()
                    .title("Rename Conversation")
                    .transient_for(&window_ref)
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
                let dialog_ref = dialog.clone();
                cancel_btn.connect_clicked(move |_| {
                    dialog_ref.close();
                });
                btn_box.append(&cancel_btn);

                let confirm_btn = Button::with_label("Rename");
                confirm_btn.add_css_class("suggested-action");
                let client_ref_inner = Rc::clone(&client_ref);
                let bridge_inner = Rc::clone(&bridge);
                let dialog_ref = dialog.clone();
                let entry_ref = entry.clone();
                confirm_btn.connect_clicked(move |_| {
                    let new_title = entry_ref.text().trim().to_string();
                    if new_title.is_empty() {
                        return;
                    }
                    dialog_ref.close();
                    if let Some(client) = client_ref_inner.borrow().clone() {
                        let tx = bridge_inner.ui_sender();
                        let id = id.clone();
                        let title = new_title.clone();
                        bridge_inner.spawn(async move {
                            match client.rename_conversation(&id, &title).await {
                                Ok(()) => {
                                    let _ = tx.send(UiMessage::ConversationRenamed { id, title });
                                }
                                Err(e) => {
                                    let _ = tx.send(UiMessage::Error(format!(
                                        "Rename conversation: {e}"
                                    )));
                                }
                            }
                        });
                    }
                });
                btn_box.append(&confirm_btn);

                // Enter key in entry confirms
                let confirm_ref = confirm_btn.clone();
                entry.connect_activate(move |_| {
                    confirm_ref.emit_clicked();
                });

                vbox.append(&btn_box);
                dialog.set_child(Some(&vbox));
                dialog.present();
            });
        }

        // Context menu: Archive/unarchive conversation
        {
            let client_ref = Rc::clone(&client);
            let bridge = Rc::clone(&bridge);
            let state = Rc::clone(&state);
            sidebar.connect_archive(move |idx| {
                let (id, archived) = {
                    let s = state.borrow();
                    match s.conversations.get(idx) {
                        Some(conv) => (conv.id.clone(), conv.archived),
                        None => return,
                    }
                };
                if let Some(client) = client_ref.borrow().clone() {
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
            });
        }

        // Show archived checkbox toggle
        {
            let client_ref = Rc::clone(&client);
            let bridge = Rc::clone(&bridge);
            sidebar.connect_show_archived_toggled(move |include_archived| {
                if let Some(client) = client_ref.borrow().clone() {
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
            });
        }

        // Send button / Enter key → send prompt
        {
            let client_ref = Rc::clone(&client);
            let bridge_ref = Rc::clone(&bridge);
            let state = Rc::clone(&state);
            let input_bar_ref = Rc::clone(&input_bar);
            let chat_view_ref = Rc::clone(&chat_view);
            let model_picker_ref = Rc::clone(&model_picker);

            let send_action = Rc::new(move || {
                let text = input_bar_ref.take_text();
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
                chat_view_ref.borrow_mut().add_user_message(&text);

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

                let override_selection = model_picker_ref.current_override();

                if let Some(client) = client_ref.borrow().clone() {
                    let tx = bridge_ref.ui_sender();
                    let text = text.clone();
                    bridge_ref.spawn(async move {
                        // Use the WS-specific override path when available so
                        // the picker's selection is honoured. The shared
                        // AssistantClient trait can't carry the override
                        // because the D-Bus surface doesn't expose it; on
                        // D-Bus we fall through to the plain send_prompt.
                        let result = match (client.as_ws(), override_selection) {
                            (Some(ws), Some(over)) => {
                                ws.send_prompt_with_override(&conv_id, &text, Some(over)).await
                            }
                            _ => client.send_prompt(&conv_id, &text).await,
                        };
                        match result {
                            Ok(request_id) => {
                                let _ = tx.send(UiMessage::PromptSent { request_id });
                            }
                            Err(e) => {
                                let _ = tx.send(UiMessage::Error(format!("Send error: {e}")));
                            }
                        }
                    });
                }
            });

            // Send button click
            let send_action_click = Rc::clone(&send_action);
            input_bar.send_button.connect_clicked(move |_| {
                send_action_click();
            });

            // Enter key in text view (Shift+Enter for newline)
            let send_action_key = Rc::clone(&send_action);
            let key_controller = gtk4::EventControllerKey::new();
            key_controller.connect_key_pressed(move |_, key, _, modifiers| {
                if key == gdk::Key::Return
                    && !modifiers.contains(gdk::ModifierType::SHIFT_MASK)
                    && !modifiers.contains(gdk::ModifierType::CONTROL_MASK)
                {
                    send_action_key();
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            });
            input_bar.text_view.add_controller(key_controller);
        }

        // Hamburger menu: New Connection → open login screen in a new window
        {
            let app_ref = app.clone();
            let popover_ref = menu_popover.clone();
            new_conn_btn.connect_clicked(move |_| {
                popover_ref.popdown();
                let login = crate::widgets::login_screen::LoginScreen::new(&app_ref);
                login.present();
            });
        }

        // Hamburger menu: Knowledge Base → open the KB browser/editor (#74)
        {
            let popover_ref = menu_popover.clone();
            let window_ref = window.clone();
            let client_ref = Rc::clone(&client);
            let bridge_ref = Rc::clone(&bridge);
            let status_label_ref = Rc::clone(&status_label);
            knowledge_btn.connect_clicked(move |_| {
                popover_ref.popdown();
                let Some(transport) = client_ref.borrow().clone() else {
                    status_label_ref
                        .set_text("Not connected — knowledge base unavailable");
                    return;
                };
                let browser = crate::widgets::knowledge_browser::KnowledgeBrowser::new(
                    &window_ref,
                    transport,
                    Rc::clone(&bridge_ref),
                );
                browser.present();
            });
        }

        // Hamburger menu: Disconnect → close this window, show login screen
        {
            let app_ref = app.clone();
            let window_ref = window.clone();
            let popover_ref = menu_popover.clone();
            disconnect_btn.connect_clicked(move |_| {
                popover_ref.popdown();
                let login = crate::widgets::login_screen::LoginScreen::new(&app_ref);
                login.present();
                window_ref.close();
            });
        }

        // Debug checkbox toggle → re-fetch conversation with filtering
        {
            let client_ref = Rc::clone(&client);
            let bridge_ref = Rc::clone(&bridge);
            let state = Rc::clone(&state);
            debug_check.connect_toggled(move |btn| {
                state.borrow_mut().debug_enabled = btn.is_active();
                let conv_id = state.borrow().current_conversation_id.clone();
                if let Some(conv_id) = conv_id {
                    if let Some(client) = client_ref.borrow().clone() {
                        let tx = bridge_ref.ui_sender();
                        bridge_ref.spawn(async move {
                            match client.get_conversation(&conv_id).await {
                                Ok(detail) => {
                                    let _ = tx.send(UiMessage::ConversationLoaded(detail));
                                }
                                Err(e) => {
                                    let _ = tx.send(UiMessage::Error(format!(
                                        "Reload conversation: {e}"
                                    )));
                                }
                            }
                        });
                    }
                }
            });
        }

        Self { window }
    }

    pub fn present(&self) {
        self.window.present();
    }
}

fn handle_ui_message(
    msg: UiMessage,
    state: &Rc<RefCell<WindowState>>,
    sidebar: &Rc<Sidebar>,
    chat_view: &Rc<RefCell<ChatView>>,
    status_label: &Rc<Label>,
    client: &Rc<RefCell<Option<Arc<TransportClient>>>>,
    input_bar: &Rc<InputBar>,
    model_picker: &Rc<ModelPicker>,
) {
    match msg {
        UiMessage::ConversationsLoaded(convs) => {
            sidebar.set_conversations(&convs);
            state.borrow_mut().conversations = convs;
        }
        UiMessage::ConversationLoaded(detail) => {
            let id = detail.id.clone();
            let debug = state.borrow().debug_enabled;
            let filtered = filter_messages(&detail, debug);
            model_picker.set_selection(detail.model_selection.as_ref());
            let mut s = state.borrow_mut();
            s.current_conversation = Some(detail);
            s.current_conversation_id = Some(id);
            drop(s);
            chat_view.borrow_mut().load_conversation(&filtered);
        }
        UiMessage::ConversationCreated { id } => {
            state.borrow_mut().current_conversation_id = Some(id);
        }
        UiMessage::ConversationDeleted { id } => {
            let mut s = state.borrow_mut();
            s.conversations.retain(|c| c.id != id);
            let is_active = s.current_conversation_id.as_deref() == Some(&id);
            if is_active {
                s.current_conversation_id = None;
                s.current_conversation = None;
            }
            let convs = s.conversations.clone();
            drop(s);
            sidebar.set_conversations(&convs);
            if is_active {
                chat_view.borrow_mut().clear();
            }
        }
        UiMessage::ConversationRenamed { id, title } => {
            let mut s = state.borrow_mut();
            for conv in &mut s.conversations {
                if conv.id == id {
                    conv.title = title.clone();
                }
            }
            let convs = s.conversations.clone();
            drop(s);
            sidebar.set_conversations(&convs);
        }
        UiMessage::PromptSent { request_id } => {
            let mut s = state.borrow_mut();
            if request_id.is_empty() {
                // WS doesn't return request ID upfront; use a sentinel.
                s.pending_request_id = Some("__pending__".to_string());
            } else {
                s.pending_request_id = Some(request_id);
            }
            s.streaming_buffer.clear();
        }
        UiMessage::AssistantStatus {
            request_id,
            message,
        } => {
            let s = state.borrow();
            if s.pending_request_id.as_deref() == Some(&request_id)
                || s.pending_request_id.as_deref() == Some("__pending__")
            {
                drop(s);
                chat_view.borrow().set_status(&message);
            }
        }
        UiMessage::StreamChunk { request_id, chunk } => {
            let mut s = state.borrow_mut();
            // Claim request ID if pending
            if s.pending_request_id.as_deref() == Some("__pending__") {
                s.pending_request_id = Some(request_id.clone());
            }
            if s.pending_request_id.as_deref() == Some(&request_id) {
                let first_chunk = s.streaming_buffer.is_empty();
                s.streaming_buffer.push_str(&chunk);
                drop(s);
                if first_chunk {
                    chat_view.borrow().clear_status();
                }
                chat_view.borrow_mut().receive_chunk(&chunk);
            }
        }
        UiMessage::StreamComplete {
            request_id,
            full_response,
        } => {
            let mut s = state.borrow_mut();
            if s.pending_request_id.as_deref() == Some("__pending__") {
                s.pending_request_id = Some(request_id.clone());
            }
            if s.pending_request_id.as_deref() == Some(&request_id) {
                s.pending_request_id = None;
                s.streaming_buffer.clear();
                if let Some(ref mut conv) = s.current_conversation {
                    conv.messages.push(ChatMessage {
                        role: "assistant".to_string(),
                        content: full_response.clone(),
                    });
                }
                drop(s);
                let cv = chat_view.borrow();
                cv.clear_status();
                drop(cv);
                chat_view.borrow_mut().complete_streaming(&full_response);
            }
        }
        UiMessage::StreamError { request_id, error } => {
            let mut s = state.borrow_mut();
            if s.pending_request_id.as_deref() == Some("__pending__") {
                s.pending_request_id = Some(request_id.clone());
            }
            if s.pending_request_id.as_deref() == Some(&request_id) {
                s.pending_request_id = None;
                s.streaming_buffer.clear();
                drop(s);
                chat_view.borrow().clear_status();
                status_label.set_text(&format!("Error: {error}"));
            }
        }
        UiMessage::TitleChanged {
            conversation_id,
            title,
        } => {
            let mut s = state.borrow_mut();
            for conv in &mut s.conversations {
                if conv.id == conversation_id {
                    conv.title = title.clone();
                }
            }
            let convs = s.conversations.clone();
            drop(s);
            sidebar.set_conversations(&convs);
        }
        UiMessage::StatusUpdate(text) => {
            status_label.set_text(&text);
        }
        UiMessage::Error(text) => {
            status_label.set_text(&format!("Error: {text}"));
        }
        UiMessage::ModelsLoaded(listings) => {
            let visible = !listings.is_empty();
            model_picker.set_models(&listings);
            // Re-apply the active conversation's stored selection (if any)
            // since `set_models` resets the dropdown.
            if let Some(ref detail) = state.borrow().current_conversation {
                model_picker.set_selection(detail.model_selection.as_ref());
            }
            model_picker.set_visible(visible);
        }
        UiMessage::Connected { label } => {
            status_label.set_text(&label);
            input_bar.send_button.set_sensitive(true);
        }
        UiMessage::Disconnected { reason } => {
            *client.borrow_mut() = None;
            input_bar.send_button.set_sensitive(false);
            status_label.set_text(&format!("Disconnected: {reason}"));

            // Finalize any in-progress streaming buffer
            let mut s = state.borrow_mut();
            if s.pending_request_id.is_some() {
                s.pending_request_id = None;
                if !s.streaming_buffer.is_empty() {
                    s.streaming_buffer.push_str("\n\n[Connection lost]");
                    let full = s.streaming_buffer.clone();
                    s.streaming_buffer.clear();
                    if let Some(ref mut conv) = s.current_conversation {
                        conv.messages.push(ChatMessage {
                            role: "assistant".to_string(),
                            content: full.clone(),
                        });
                    }
                    drop(s);
                    chat_view.borrow_mut().complete_streaming(&full);
                }
            }
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
    let icon_dir = cache_root
        .join("hicolor")
        .join("512x512")
        .join("apps");
    let icon_path = icon_dir.join(format!("{ICON_NAME}.png"));

    if let Err(e) = std::fs::create_dir_all(&icon_dir) {
        tracing::warn!("Failed to create icon dir: {e}");
        return;
    }
    // Use create_new to avoid TOCTOU: the write either atomically creates the
    // file or harmlessly fails because it already exists.
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&icon_path)
    {
        Ok(mut file) => {
            if let Err(e) = std::io::Write::write_all(&mut file, ICON_BYTES) {
                tracing::warn!("Failed to write icon: {e}");
                return;
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => {
            tracing::warn!("Failed to create icon file: {e}");
            return;
        }
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
