use desktop_assistant_client_common::{ConversationDetail, MessageKind};
use gtk4::prelude::*;
use gtk4::{Box as GtkBox, Orientation};

#[cfg(feature = "linux")]
use crate::markdown;

#[cfg(not(feature = "linux"))]
use crate::markdown_text::MarkdownTags;
#[cfg(not(feature = "linux"))]
use gtk4::{ScrolledWindow, TextView};

/// Chat view widget that displays messages.
///
/// On Linux with the `linux` feature, uses webkit6::WebView for rich HTML
/// rendering. Otherwise falls back to a `TextView` whose buffer is rendered
/// from the same markdown via tags (bold/italic/code/headings/lists), so the
/// non-WebView build still shows formatted text rather than a flat string.
pub struct ChatView {
    pub container: GtkBox,
    #[cfg(feature = "linux")]
    webview: webkit6::WebView,
    #[cfg(not(feature = "linux"))]
    text_view: TextView,
    #[cfg(not(feature = "linux"))]
    tags: MarkdownTags,
    /// Messages stored for re-rendering: `(role, content, kind)`. `kind` carries
    /// the explicit presentation metadata (voice#126) so a re-render can badge a
    /// `Spoken` / `SpeechDisabled` `say_this` line without parsing its content.
    messages: Vec<(String, String, MessageKind)>,
    /// Partial streaming reply, used only by the `TextView` fallback's
    /// `render()`. The `linux`/WebView path appends each chunk incrementally
    /// via JS (`webview::append_chunk`) and never re-renders mid-stream, so it
    /// keeps no buffer here — the authoritative streaming buffer lives in
    /// `WindowState`, which re-seeds the WebView on conversation switch-back.
    #[cfg(not(feature = "linux"))]
    streaming_buffer: String,
    #[cfg(feature = "linux")]
    avatars: markdown::AvatarUrls,
}

/// The explicit-metadata marker suffixed onto Adele's role label for a
/// client-local `say_this` line (voice#126): ` · Spoken` for a voiced aside,
/// ` · speech off` for the shown-not-spoken downgrade, empty for an ordinary
/// message. Shared by both render paths (WebView label + TextView header) so
/// they stay identical.
pub(crate) fn kind_marker(kind: MessageKind) -> &'static str {
    match kind {
        MessageKind::Normal => "",
        MessageKind::Spoken => " · Spoken",
        MessageKind::SpeechDisabled => " · speech off",
    }
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
        let (text_view, tags) = {
            let text_view = TextView::new();
            text_view.set_editable(false);
            text_view.set_cursor_visible(false);
            text_view.set_wrap_mode(gtk4::WrapMode::WordChar);
            text_view.set_hexpand(true);
            text_view.set_vexpand(true);
            text_view.set_left_margin(16);
            text_view.set_right_margin(16);
            text_view.set_top_margin(16);

            let buffer = text_view.buffer();
            buffer.set_text("Press '+ New Conversation' to start.");
            let tags = MarkdownTags::install(&buffer);

            let scrolled = ScrolledWindow::new();
            scrolled.set_hexpand(true);
            scrolled.set_vexpand(true);
            scrolled.set_child(Some(&text_view));
            // `scrolled` is parented into `container` (which owns it) and is
            // never touched again; only the view + tags are retained for `render()`.
            container.append(&scrolled);
            (text_view, tags)
        };

        Self {
            container,
            #[cfg(feature = "linux")]
            webview,
            #[cfg(not(feature = "linux"))]
            text_view,
            #[cfg(not(feature = "linux"))]
            tags,
            messages: Vec::new(),
            #[cfg(not(feature = "linux"))]
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
            .map(|m| (m.role.clone(), m.content.clone(), m.kind))
            .collect();
        #[cfg(not(feature = "linux"))]
        self.streaming_buffer.clear();
        self.render();
    }

    /// Append a streaming chunk.
    pub fn receive_chunk(&mut self, chunk: &str) {
        #[cfg(feature = "linux")]
        crate::webview::append_chunk(&self.webview, chunk);

        #[cfg(not(feature = "linux"))]
        {
            self.streaming_buffer.push_str(chunk);
            self.render();
        }
    }

    /// Finalize streaming: add the full response as an assistant message.
    pub fn complete_streaming(&mut self, full_response: &str) {
        self.messages.push((
            "assistant".to_string(),
            full_response.to_string(),
            MessageKind::Normal,
        ));
        #[cfg(not(feature = "linux"))]
        self.streaming_buffer.clear();
        self.render();
    }

    /// Show a transient status message (e.g. "Searching knowledge base...").
    pub fn set_status(&self, message: &str) {
        #[cfg(feature = "linux")]
        crate::webview::set_status(&self.webview, message);

        // Non-linux fallback: no-op (status shown in status bar instead).
        #[cfg(not(feature = "linux"))]
        let _ = message;
    }

    /// Clear the transient status indicator.
    pub fn clear_status(&self) {
        #[cfg(feature = "linux")]
        crate::webview::clear_status(&self.webview);
    }

    /// Add a user message to the display.
    pub fn add_user_message(&mut self, content: &str) {
        self.messages
            .push(("user".to_string(), content.to_string(), MessageKind::Normal));
        self.render();
    }

    /// Append a client-local `say_this` line (issue #76, voice#126). Rendered in
    /// the `assistant` column, badged from `kind` at render time (`Spoken` for a
    /// voiced aside, `SpeechDisabled` for the "shown, not spoken" downgrade) —
    /// the marker is presentation, never baked into `content`.
    pub fn add_local_message(&mut self, content: &str, kind: MessageKind) {
        self.messages
            .push(("assistant".to_string(), content.to_string(), kind));
        self.render();
    }

    /// Clear the view.
    pub fn clear(&mut self) {
        self.messages.clear();
        #[cfg(not(feature = "linux"))]
        self.streaming_buffer.clear();
        self.render();
    }

    fn render(&self) {
        // The WebView path re-renders only complete transcripts (load / clear /
        // complete); the partial reply is appended incrementally via JS, so a
        // full render never carries a mid-stream prefix.
        #[cfg(feature = "linux")]
        {
            let html = markdown::render_messages_html(&self.messages, None, &self.avatars);
            crate::webview::update_messages(&self.webview, &html);
        }

        #[cfg(not(feature = "linux"))]
        {
            let streaming = if self.streaming_buffer.is_empty() {
                None
            } else {
                Some(self.streaming_buffer.as_str())
            };

            let buffer = self.text_view.buffer();
            buffer.set_text("");

            if self.messages.is_empty() && streaming.is_none() {
                buffer.set_text("Press '+ New Conversation' to start.");
                return;
            }

            for (role, content, kind) in &self.messages {
                let label = match role.as_str() {
                    "user" => "You".to_string(),
                    "assistant" => format!("Adele{}", kind_marker(*kind)),
                    _ => String::new(),
                };
                if !label.is_empty() {
                    self.tags.insert_role(&buffer, &label);
                }
                crate::markdown_text::render(&buffer, &self.tags, content);
            }
            if let Some(buf) = streaming {
                self.tags.insert_role(&buffer, "Adele");
                crate::markdown_text::render(&buffer, &self.tags, buf);
            }

            // Keep the newest content in view as the transcript grows / streams.
            let mut end = buffer.end_iter();
            self.text_view
                .scroll_to_iter(&mut end, 0.0, false, 0.0, 0.0);
        }
    }
}
