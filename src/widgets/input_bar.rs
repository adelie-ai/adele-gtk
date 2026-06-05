use std::cell::Cell;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{Box as GtkBox, Button, Image, Orientation, ScrolledWindow, TextView, WrapMode};

use crate::voice_client::VoiceState;

/// Input bar widget with a record/mic button, a text view, and a send button.
pub struct InputBar {
    pub container: GtkBox,
    pub text_view: TextView,
    pub send_button: Button,
    /// Push-to-talk button: starts a dictation turn on the voice daemon,
    /// routed into the active conversation (`PushToTalkInConversation`) or the
    /// daemon's own session (`PushToTalk`) when none is open. Hidden until the
    /// voice service is found on the bus (graceful degradation — see
    /// [`InputBar::set_voice_available`]).
    pub mic_button: Button,
    /// The mic icon, swapped per pipeline state to mirror the plasmoid's glyphs.
    mic_image: Image,
    /// The last pipeline state reflected on the button. The window's click
    /// handler reads it to decide barge-in (click while `Speaking` →
    /// `StopSpeaking`, otherwise start a push-to-talk turn).
    state: Rc<Cell<VoiceState>>,
}

/// Themed-icon fallback chain for the **resting (Idle)** mic glyph. The first
/// entry mirrors the plasmoid (`audio-input-microphone-muted`); the rest are
/// fallbacks so a theme missing the muted glyph still renders a mic.
const MIC_IDLE_ICONS: &[&str] = &[
    "audio-input-microphone-muted-symbolic",
    "audio-input-microphone-muted",
    "microphone-sensitivity-muted-symbolic",
    "audio-input-microphone-symbolic",
];

/// Icon shown while **Listening** — an open mic (plasmoid: `audio-input-microphone`).
const MIC_LISTENING_ICONS: &[&str] = &[
    "audio-input-microphone-symbolic",
    "audio-input-microphone",
    "microphone-sensitivity-high-symbolic",
];

/// Icon shown while **Processing** ("Thinking…") — a refresh/spinner glyph
/// (plasmoid: `view-refresh-symbolic`).
const MIC_PROCESSING_ICONS: &[&str] = &["view-refresh-symbolic", "content-loading-symbolic"];

/// Icon shown while **Speaking** — a speaker (plasmoid: `audio-volume-high`).
const MIC_SPEAKING_ICONS: &[&str] = &["audio-volume-high-symbolic", "audio-volume-high"];

/// The themed-icon fallback chain for a given pipeline state.
fn mic_icons_for(state: VoiceState) -> &'static [&'static str] {
    match state {
        VoiceState::Idle => MIC_IDLE_ICONS,
        VoiceState::Listening => MIC_LISTENING_ICONS,
        VoiceState::Processing => MIC_PROCESSING_ICONS,
        VoiceState::Speaking => MIC_SPEAKING_ICONS,
    }
}

/// The mic button's tooltip for a given state, mirroring the plasmoid: "Stop
/// speaking" while Speaking (the click barges in), otherwise "Push to talk"
/// with the in-progress phase appended.
fn mic_tooltip_for(state: VoiceState) -> String {
    match state {
        VoiceState::Speaking => "Stop speaking".to_string(),
        VoiceState::Idle => "Push to talk".to_string(),
        other => format!("Push to talk — {}", other.label()),
    }
}

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
        mic_button.set_tooltip_text(Some(&mic_tooltip_for(VoiceState::Idle)));
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
            state: Rc::new(Cell::new(VoiceState::Idle)),
        }
    }

    /// Show or hide the mic button based on whether the voice daemon is
    /// reachable. Hidden when absent so the user isn't offered a dead control
    /// (graceful degradation per issue #59).
    pub fn set_voice_available(&self, available: bool) {
        self.mic_button.set_visible(available);
    }

    /// Reflect the voice pipeline state on the mic button: the icon swaps to
    /// the per-state glyph (mirroring the plasmoid), a CSS class drives the
    /// accent while a turn is active, and the tooltip explains the current
    /// phase. The state is also cached for the click handler's barge-in check
    /// (see [`InputBar::current_state`]).
    pub fn reflect_voice_state(&self, state: VoiceState) {
        self.state.set(state);

        // Highlight while a turn is in flight (anything but resting Idle).
        if matches!(state, VoiceState::Idle) {
            self.mic_button.remove_css_class("mic-active");
        } else {
            self.mic_button.add_css_class("mic-active");
        }

        self.mic_image
            .set_from_gicon(&gtk4::gio::ThemedIcon::from_names(mic_icons_for(state)));
        self.mic_button
            .set_tooltip_text(Some(&mic_tooltip_for(state)));
    }

    /// The last pipeline state reflected on the button. The window's click
    /// handler uses this to choose barge-in (`Speaking` → `StopSpeaking`) vs.
    /// starting a push-to-talk turn.
    pub fn current_state(&self) -> VoiceState {
        self.state.get()
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

    /// Replace the input's text. Used by the embedded voice path (issue #65) to
    /// drop a dictated transcript into the box before sending it like a typed
    /// message.
    pub fn set_text(&self, text: &str) {
        self.text_view.buffer().set_text(text);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tooltip_offers_barge_in_only_while_speaking() {
        // Mirrors the plasmoid: clicking while Speaking stops playback, so the
        // tooltip reads "Stop speaking"; every other state invites talking.
        assert_eq!(mic_tooltip_for(VoiceState::Speaking), "Stop speaking");
        assert_eq!(mic_tooltip_for(VoiceState::Idle), "Push to talk");
        assert_eq!(
            mic_tooltip_for(VoiceState::Listening),
            "Push to talk — Listening…"
        );
        assert_eq!(
            mic_tooltip_for(VoiceState::Processing),
            "Push to talk — Processing…"
        );
    }

    #[test]
    fn each_state_maps_to_a_distinct_primary_icon() {
        // The first entry in each chain is the plasmoid glyph; they must differ
        // per state so the button visibly tracks the pipeline.
        assert_eq!(mic_icons_for(VoiceState::Idle)[0], MIC_IDLE_ICONS[0]);
        assert_eq!(
            mic_icons_for(VoiceState::Listening)[0],
            MIC_LISTENING_ICONS[0]
        );
        assert_eq!(
            mic_icons_for(VoiceState::Processing)[0],
            MIC_PROCESSING_ICONS[0]
        );
        assert_eq!(
            mic_icons_for(VoiceState::Speaking)[0],
            MIC_SPEAKING_ICONS[0]
        );
        // No chain is empty (an empty `from_names` would render nothing).
        for state in [
            VoiceState::Idle,
            VoiceState::Listening,
            VoiceState::Processing,
            VoiceState::Speaking,
        ] {
            assert!(!mic_icons_for(state).is_empty());
        }
    }
}
