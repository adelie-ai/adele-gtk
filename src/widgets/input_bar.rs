use gtk4::prelude::*;
use gtk4::{Box as GtkBox, Button, Image, Orientation, ScrolledWindow, TextView, WrapMode};

use crate::voice_client::VoiceState;

/// Input bar widget with a record/mic button, a text view, and a send button.
pub struct InputBar {
    pub container: GtkBox,
    pub text_view: TextView,
    pub send_button: Button,
    /// Push-to-talk button: starts a dictation turn on the voice daemon
    /// (`PushToTalk`). Hidden until the voice service is found on the bus
    /// (graceful degradation — see [`InputBar::set_voice_available`]).
    pub mic_button: Button,
    /// The mic icon, swapped between resting and active glyphs as the pipeline
    /// state changes.
    mic_image: Image,
}

/// Themed-icon fallback chain for the resting (idle) mic glyph. Breeze (KDE),
/// Adwaita and most icon themes ship at least one of these; listing several
/// avoids a broken glyph on a theme that lacks the first.
const MIC_IDLE_ICONS: &[&str] = &[
    "audio-input-microphone-symbolic",
    "microphone-sensitivity-high-symbolic",
    "audio-input-microphone",
];

/// Icon shown while the pipeline is actively listening/processing/speaking —
/// a "recording" dot so the active turn reads at a glance.
const MIC_ACTIVE_ICONS: &[&str] = &[
    "media-record-symbolic",
    "audio-input-microphone-high-symbolic",
    "media-record",
];

impl InputBar {
    pub fn new() -> Self {
        let container = GtkBox::new(Orientation::Horizontal, 8);
        container.set_margin_start(8);
        container.set_margin_end(8);
        container.set_margin_top(4);
        container.set_margin_bottom(8);

        // Push-to-talk button, left of the text entry. Starts hidden; the
        // window reveals it once the voice daemon is found on the bus.
        let mic_image = Image::from_gicon(&gtk4::gio::ThemedIcon::from_names(MIC_IDLE_ICONS));
        let mic_button = Button::new();
        mic_button.set_child(Some(&mic_image));
        mic_button.add_css_class("mic-button");
        mic_button.set_valign(gtk4::Align::End);
        mic_button.set_margin_bottom(4);
        mic_button.set_tooltip_text(Some("Hold a conversation — click to start talking"));
        mic_button.set_visible(false);
        container.append(&mic_button);

        let scrolled = ScrolledWindow::new();
        scrolled.set_hexpand(true);
        scrolled.set_max_content_height(100); // ~4 lines
        scrolled.set_propagate_natural_height(true);

        let text_view = TextView::new();
        text_view.set_wrap_mode(WrapMode::WordChar);
        text_view.set_top_margin(8);
        text_view.set_bottom_margin(8);
        text_view.set_left_margin(12);
        text_view.set_right_margin(12);
        text_view.add_css_class("input-textview");
        scrolled.set_child(Some(&text_view));
        container.append(&scrolled);

        let send_button = Button::with_label("Send");
        send_button.add_css_class("send-button");
        send_button.set_valign(gtk4::Align::End);
        send_button.set_margin_bottom(4);
        container.append(&send_button);

        Self {
            container,
            text_view,
            send_button,
            mic_button,
            mic_image,
        }
    }

    /// Show or hide the mic button based on whether the voice daemon is
    /// reachable. Hidden when absent so the user isn't offered a dead control
    /// (graceful degradation per issue #59).
    pub fn set_voice_available(&self, available: bool) {
        self.mic_button.set_visible(available);
    }

    /// Reflect the voice pipeline state on the mic button: a CSS class drives
    /// the accent while active, the icon swaps to a record glyph, and the
    /// tooltip explains the current phase.
    pub fn reflect_voice_state(&self, state: VoiceState) {
        let active = !matches!(state, VoiceState::Idle);
        if active {
            self.mic_button.add_css_class("mic-active");
            self.mic_image
                .set_from_gicon(&gtk4::gio::ThemedIcon::from_names(MIC_ACTIVE_ICONS));
        } else {
            self.mic_button.remove_css_class("mic-active");
            self.mic_image
                .set_from_gicon(&gtk4::gio::ThemedIcon::from_names(MIC_IDLE_ICONS));
        }
        let tooltip = match state {
            VoiceState::Idle => "Hold a conversation — click to start talking".to_string(),
            other => format!("Voice: {}", other.label()),
        };
        self.mic_button.set_tooltip_text(Some(&tooltip));
    }

    /// Get the current text content and clear the input.
    pub fn take_text(&self) -> String {
        let buffer = self.text_view.buffer();
        let text = buffer
            .text(&buffer.start_iter(), &buffer.end_iter(), false)
            .to_string();
        buffer.set_text("");
        text
    }

    /// Get the current text content without clearing.
    // Read-only counterpart to `take_text` (which is used). Part of the public
    // InputBar API; not yet called from `window.rs` but kept for the input-bar
    // consumers added by the connections/model-selector work (#1).
    #[allow(dead_code)]
    pub fn text(&self) -> String {
        let buffer = self.text_view.buffer();
        buffer
            .text(&buffer.start_iter(), &buffer.end_iter(), false)
            .to_string()
    }
}
