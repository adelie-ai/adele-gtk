use std::cell::RefCell;
use std::rc::Rc;

use desktop_assistant_client_common::{ConversationDetail, MessageKind};
use gtk4::prelude::*;
use gtk4::{Box as GtkBox, GestureClick, Orientation, Widget, glib};

use crate::transcript::{TranscriptEntry, TurnIdAction, turn_id_action_for_generation};
use crate::widgets::context_menu;

#[cfg(feature = "linux")]
use crate::markdown;
#[cfg(feature = "linux")]
use webkit6::prelude::WebViewExt;

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
    /// Messages stored for re-rendering. Each entry carries the presentation
    /// metadata a re-render needs (voice#126) and the turn identity the
    /// right-click menu copies (gtk#169), so neither has to be recovered by
    /// parsing the content back out.
    messages: Vec<TranscriptEntry>,
    /// Bumped every time the entries are replaced wholesale, so a click whose
    /// resolution is still in flight can tell that the positions it counted
    /// into are gone. Appending a message does not bump it: an append leaves
    /// every existing position where it was.
    generation: u64,
    /// Where each entry begins in the `TextView` fallback's buffer, in
    /// ascending order. The fallback has no DOM to hit-test, so a right-click
    /// is resolved against these marks instead.
    #[cfg(not(feature = "linux"))]
    entry_starts: Vec<i32>,
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
            generation: 0,
            #[cfg(not(feature = "linux"))]
            entry_starts: Vec::new(),
            #[cfg(not(feature = "linux"))]
            streaming_buffer: String::new(),
            #[cfg(feature = "linux")]
            avatars,
        }
    }

    /// Load a conversation's messages into the view.
    pub fn load_conversation(&mut self, detail: &ConversationDetail) {
        self.generation = self.generation.wrapping_add(1);
        self.messages = detail
            .messages
            .iter()
            .map(|m| {
                TranscriptEntry::new(
                    m.role.clone(),
                    m.content.clone(),
                    m.kind,
                    // The persisted turn identity (#570 Phase 1b). `None` on
                    // an assistant row, on a row written before the daemon
                    // stored keys, and on every row over the legacy D-Bus
                    // conversation API, which returns (role, content) only.
                    m.idempotency_key.clone(),
                )
            })
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
        self.messages.push(TranscriptEntry::new(
            "assistant",
            full_response,
            MessageKind::Normal,
            None,
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
    ///
    /// `turn_id` is the idempotency key the send carries (#570), which the
    /// daemon persists as this turn's identity. `None` for a message this
    /// client did not send - one echoed live from a sibling client arrives
    /// without the key that started it, and only a reload recovers it.
    pub fn add_user_message(&mut self, content: &str, turn_id: Option<String>) {
        self.messages.push(TranscriptEntry::new(
            "user",
            content,
            MessageKind::Normal,
            turn_id,
        ));
        self.render();
    }

    /// Which transcript the entries currently belong to. Read when a click is
    /// made, and handed back with the resolved index so a reload that lands in
    /// between is caught.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The turn-id action for the entry at `index`, as resolved against
    /// generation `clicked`.
    pub fn turn_id_action_at(&self, clicked: u64, index: usize) -> TurnIdAction {
        turn_id_action_for_generation(&self.messages, self.generation, clicked, index)
    }

    /// Append a client-local `say_this` line (issue #76, voice#126). Rendered in
    /// the `assistant` column, badged from `kind` at render time (`Spoken` for a
    /// voiced aside, `SpeechDisabled` for the "shown, not spoken" downgrade) —
    /// the marker is presentation, never baked into `content`.
    pub fn add_local_message(&mut self, content: &str, kind: MessageKind) {
        self.messages
            .push(TranscriptEntry::new("assistant", content, kind, None));
        self.render();
    }

    /// Clear the view.
    pub fn clear(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.messages.clear();
        #[cfg(not(feature = "linux"))]
        self.entry_starts.clear();
        #[cfg(not(feature = "linux"))]
        self.streaming_buffer.clear();
        self.render();
    }

    fn render(&mut self) {
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
                self.entry_starts.clear();
                return;
            }

            let mut entry_starts = Vec::with_capacity(self.messages.len());
            for entry in &self.messages {
                entry_starts.push(buffer.end_iter().offset());
                let label = match entry.role.as_str() {
                    "user" => "You".to_string(),
                    "assistant" => format!("Adele{}", kind_marker(entry.kind)),
                    _ => String::new(),
                };
                if !label.is_empty() {
                    self.tags.insert_role(&buffer, &label);
                }
                crate::markdown_text::render(&buffer, &self.tags, &entry.content);
            }
            self.entry_starts = entry_starts;
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

/// Wire the transcript's right-click menu (gtk#169 item 1).
///
/// Takes the shared handle rather than `&self` because the menu is built from
/// the transcript as it stands when the click lands, not as it stood when the
/// view was created.
pub fn install_turn_context_menu(chat: &Rc<RefCell<ChatView>>) {
    #[cfg(feature = "linux")]
    {
        let webview = chat.borrow().webview.clone();
        // Take the transcript's secondary CLICK over from WebKit. Its own menu
        // offers page actions - reload, navigation, view source - that mean
        // nothing in a transcript, and it cannot carry a turn entry, because
        // which turn the pointer is over is only answerable by the web
        // process, and the answer arrives after the menu would have to be
        // built. `Copy` is the one entry worth keeping, so the menu below
        // offers it whenever there is a selection to copy.
        //
        // A keyboard request (Shift+F10, the Menu key) is left alone. It
        // reaches no pointer gesture and carries no position to hit-test, so
        // suppressing it too would leave the keyboard with no menu at all
        // rather than with a lesser one. Anything this cannot positively
        // identify as a keyboard request is suppressed, so an unrecognised
        // trigger shows one menu rather than two.
        webview.connect_context_menu(|_webview, menu, _hit| {
            let from_keyboard = menu
                .event()
                .is_some_and(|event| matches!(event.event_type(), gtk4::gdk::EventType::KeyPress));
            !from_keyboard
        });

        let gesture = GestureClick::new();
        gesture.set_button(3);
        // Capture, so the press is seen before the WebView acts on it.
        gesture.set_propagation_phase(gtk4::PropagationPhase::Capture);
        // Weak on both sides. The controller lives on the WebView, and the
        // WebView is owned by the `ChatView` this closure reads, so a strong
        // capture of either would close a cycle that outlives the window and
        // keep a closed window's transcript alive.
        gesture.connect_pressed(glib::clone!(
            #[weak]
            chat,
            #[weak]
            webview,
            move |gesture, _n_press, x, y| {
                gesture.set_state(gtk4::EventSequenceState::Claimed);
                let Some(widget) = gesture.widget() else {
                    return;
                };
                // Read now, checked when the answer comes back: the pointer is
                // over THIS transcript, and the round trip below is long enough
                // for a reload to replace it.
                let clicked = chat.borrow().generation();
                crate::webview::query_transcript_click(&webview, x, y, move |click| {
                    let action = click
                        .entry_index
                        .map(|index| chat.borrow().turn_id_action_at(clicked, index))
                        .unwrap_or(TurnIdAction::Unavailable);
                    show_turn_menu(&widget, x, y, action, &click.selection);
                });
            }
        ));
        webview.add_controller(gesture);
    }

    #[cfg(not(feature = "linux"))]
    {
        let text_view = chat.borrow().text_view.clone();
        let gesture = GestureClick::new();
        gesture.set_button(3);
        gesture.set_propagation_phase(gtk4::PropagationPhase::Capture);
        // Weak for the same reason as the WebView path above: the controller
        // sits on a widget the `ChatView` owns.
        gesture.connect_pressed(glib::clone!(
            #[weak]
            chat,
            #[weak]
            text_view,
            move |gesture, _n_press, x, y| {
                gesture.set_state(gtk4::EventSequenceState::Claimed);
                let Some(widget) = gesture.widget() else {
                    return;
                };
                let (buffer_x, buffer_y) = text_view.window_to_buffer_coords(
                    gtk4::TextWindowType::Widget,
                    x as i32,
                    y as i32,
                );
                let chat_ref = chat.borrow();
                let action = text_view
                    .iter_at_location(buffer_x, buffer_y)
                    .and_then(|iter| {
                        crate::transcript::entry_index_at_offset(
                            &chat_ref.entry_starts,
                            iter.offset(),
                        )
                    })
                    .map(|index| chat_ref.turn_id_action_at(chat_ref.generation(), index))
                    .unwrap_or(TurnIdAction::Unavailable);
                drop(chat_ref);
                let selection = text_view
                    .buffer()
                    .selection_bounds()
                    .map(|(start, end)| start.text(&end).to_string())
                    .unwrap_or_default();
                show_turn_menu(&widget, x, y, action, &selection);
            }
        ));
        text_view.add_controller(gesture);
    }
}

/// Pop the transcript's right-click menu at `(x, y)`.
///
/// `Copy turn id` is always shown, and is unavailable when the turn under the
/// pointer has no id - which is an ordinary state, not a fault (see
/// [`crate::transcript`]). Shown-and-unavailable rather than absent so a
/// person can see the action exists and is not offered here, instead of
/// wondering whether they missed the target.
fn show_turn_menu(widget: &Widget, x: f64, y: f64, action: TurnIdAction, selection: &str) {
    // The WebView path answers the hit test after a round trip to the web
    // process, and the window can close inside it. Parenting a popover to a
    // widget that is no longer in a window is a GTK error, so drop the menu
    // for a click whose target has gone.
    if widget.root().is_none() {
        return;
    }

    let mut items = Vec::new();

    if !selection.is_empty() {
        let text = selection.to_string();
        let target = widget.clone();
        items.push(context_menu::MenuItem::new("Copy", move || {
            target.clipboard().set_text(&text);
        }));
    }

    match action {
        TurnIdAction::Copy(turn_id) => {
            let target = widget.clone();
            items.push(context_menu::MenuItem::new("Copy turn id", move || {
                target.clipboard().set_text(&turn_id);
            }));
        }
        TurnIdAction::Unavailable => {
            items.push(context_menu::MenuItem::unavailable("Copy turn id"));
        }
    }

    context_menu::show(widget, x, y, items);
}
