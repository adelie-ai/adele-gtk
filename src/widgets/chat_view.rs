use desktop_assistant_client_common::ConversationDetail;
use gtk4::prelude::*;
use gtk4::{Box as GtkBox, Orientation};

#[cfg(feature = "linux")]
use crate::markdown;

#[cfg(not(feature = "linux"))]
use gtk4::{Label, ScrolledWindow};

/// Chat view widget that displays messages.
///
/// On Linux with the `linux` feature, uses webkit6::WebView for rich HTML rendering.
/// Falls back to a simple Label-based view otherwise.
pub struct ChatView {
    pub container: GtkBox,
    #[cfg(feature = "linux")]
    webview: webkit6::WebView,
    #[cfg(not(feature = "linux"))]
    content_label: Label,
    #[cfg(not(feature = "linux"))]
    scrolled: ScrolledWindow,
    /// Messages stored for re-rendering.
    messages: Vec<(String, String)>,
    streaming_buffer: String,
    #[cfg(feature = "linux")]
    avatars: markdown::AvatarUrls,
}

impl ChatView {
    pub fn new() -> Self {
        let container = GtkBox::new(Orientation::Vertical, 0);
        container.set_hexpand(true);
        container.set_vexpand(true);

        #[cfg(feature = "linux")]
        let avatars = markdown::AvatarUrls {
            adele: crate::avatars::adele_avatar_data_uri(),
            user: crate::avatars::user_avatar_data_uri(),
        };

        #[cfg(feature = "linux")]
        let webview = {
            let wv = crate::webview::create_chat_webview();
            wv.set_hexpand(true);
            wv.set_vexpand(true);
            container.append(&wv);
            wv
        };

        #[cfg(not(feature = "linux"))]
        let (content_label, scrolled) = {
            let scrolled = ScrolledWindow::new();
            scrolled.set_hexpand(true);
            scrolled.set_vexpand(true);

            let label = Label::new(Some("Press '+ New Conversation' to start."));
            label.set_wrap(true);
            label.set_halign(gtk4::Align::Start);
            label.set_valign(gtk4::Align::Start);
            label.set_margin_start(16);
            label.set_margin_end(16);
            label.set_margin_top(16);
            scrolled.set_child(Some(&label));
            container.append(&scrolled);
            (label, scrolled)
        };

        Self {
            container,
            #[cfg(feature = "linux")]
            webview,
            #[cfg(not(feature = "linux"))]
            content_label,
            #[cfg(not(feature = "linux"))]
            scrolled,
            messages: Vec::new(),
            streaming_buffer: String::new(),
            #[cfg(feature = "linux")]
            avatars,
        }
    }

    /// Load a conversation's messages into the view.
    pub fn load_conversation(&mut self, detail: &ConversationDetail) {
        self.messages = detail
            .messages
            .iter()
            .map(|m| (m.role.clone(), m.content.clone()))
            .collect();
        self.streaming_buffer.clear();
        self.render();
    }

    /// Append a streaming chunk.
    pub fn receive_chunk(&mut self, chunk: &str) {
        self.streaming_buffer.push_str(chunk);

        #[cfg(feature = "linux")]
        crate::webview::append_chunk(&self.webview, chunk);

        #[cfg(not(feature = "linux"))]
        self.render();
    }

    /// Finalize streaming: add the full response as an assistant message.
    pub fn complete_streaming(&mut self, full_response: &str) {
        self.messages
            .push(("assistant".to_string(), full_response.to_string()));
        self.streaming_buffer.clear();
        self.render();
    }

    /// Show a transient status message (e.g. "Searching knowledge base...").
    pub fn set_status(&self, message: &str) {
        #[cfg(feature = "linux")]
        crate::webview::set_status(&self.webview, message);

        // Non-linux fallback: no-op (status shown in status bar instead)
    }

    /// Clear the transient status indicator.
    pub fn clear_status(&self) {
        #[cfg(feature = "linux")]
        crate::webview::clear_status(&self.webview);
    }

    /// Add a user message to the display.
    pub fn add_user_message(&mut self, content: &str) {
        self.messages
            .push(("user".to_string(), content.to_string()));
        self.render();
    }

    /// Clear the view.
    pub fn clear(&mut self) {
        self.messages.clear();
        self.streaming_buffer.clear();
        self.render();
    }

    fn render(&self) {
        let streaming = if self.streaming_buffer.is_empty() {
            None
        } else {
            Some(self.streaming_buffer.as_str())
        };

        #[cfg(feature = "linux")]
        {
            let html = markdown::render_messages_html(&self.messages, streaming, &self.avatars);
            crate::webview::update_messages(&self.webview, &html);
        }

        #[cfg(not(feature = "linux"))]
        {
            let mut text = String::new();
            for (role, content) in &self.messages {
                let label = match role.as_str() {
                    "user" => "You",
                    "assistant" => "Adele",
                    _ => "",
                };
                text.push_str(&format!("{label}: {content}\n\n"));
            }
            if let Some(buf) = streaming {
                text.push_str(&format!("Adele: {buf}"));
            }
            self.content_label.set_text(&text);
        }
    }
}
