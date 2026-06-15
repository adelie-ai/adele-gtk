use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{
    Box as GtkBox, Button, DropDown, Image, Label, Orientation, ScrolledWindow, StringList,
    TextView, WrapMode, glib,
};

use crate::async_bridge::AdeleOutput;
use crate::voice_client::VoiceState;

/// Callback fired when the user changes the `You:` (voice input) dropdown (issue
/// #80). The bool is the new per-conversation voice-input state: `true` =
/// Enabled (push-to-talk available), `false` = Disabled (type only).
type VoiceInChangedCb = Box<dyn Fn(bool)>;

/// Callback fired when the user changes the `Adele:` (voice output) dropdown
/// (issue #80). The argument is the new per-conversation output level.
type AdeleOutputChangedCb = Box<dyn Fn(AdeleOutput)>;

/// Dropdown option labels for `You:` (voice input). Index 0 = Disabled, 1 =
/// Enabled — the position is the source of truth the callbacks decode (see
/// [`voice_in_index`] / [`voice_in_from_index`]).
const VOICE_IN_OPTIONS: &[&str] = &["Disabled", "Enabled"];

/// Dropdown option labels for `Adele:` (voice output). Index 0 = Disabled, 1 =
/// On Demand, 2 = Always — matching [`AdeleOutput`]'s ordering.
const ADELE_OUTPUT_OPTIONS: &[&str] = &["Disabled", "On Demand", "Always"];

/// The `You:` dropdown index for a voice-input state. Disabled = 0, Enabled = 1.
fn voice_in_index(enabled: bool) -> u32 {
    if enabled { 1 } else { 0 }
}

/// Decode the `You:` dropdown index back to the voice-input bool. Any
/// unexpected index (shouldn't happen — the StringList is fixed) falls back to
/// Disabled, the safe default (no audio capture offered).
fn voice_in_from_index(index: u32) -> bool {
    index == 1
}

/// The `Adele:` dropdown index for an output level, matching
/// [`ADELE_OUTPUT_OPTIONS`].
fn adele_output_index(level: AdeleOutput) -> u32 {
    match level {
        AdeleOutput::Disabled => 0,
        AdeleOutput::OnDemand => 1,
        AdeleOutput::Always => 2,
    }
}

/// Decode the `Adele:` dropdown index back to an output level. An unexpected
/// index falls back to Disabled (the safe default — never speaks).
fn adele_output_from_index(index: u32) -> AdeleOutput {
    match index {
        1 => AdeleOutput::OnDemand,
        2 => AdeleOutput::Always,
        _ => AdeleOutput::Disabled,
    }
}

/// Input bar widget: a text view + Send button on the top row, and a labeled
/// voice-controls row underneath (issue #80) with the `You:` (input) and
/// `Adele:` (output) dropdowns plus a push-to-talk control shown only while
/// `You == Enabled`.
pub struct InputBar {
    pub container: GtkBox,
    pub text_view: TextView,
    pub send_button: Button,
    /// Push-to-talk button (issue #80). Lives in the voice-controls row and is
    /// shown only when `You == Enabled` AND voice capture is available on this
    /// machine (the daemon owns its bus name, or the embedded engine is
    /// active). Clicking it starts a dictation turn routed into the active
    /// conversation — the same mechanic the removed left mic used. Hidden by
    /// default (You defaults to Disabled). The window wires its `connect_clicked`
    /// exactly as it did the old mic button.
    pub mic_button: Button,
    /// The push-to-talk icon, swapped per pipeline state to mirror the plasmoid.
    mic_image: Image,
    /// Whether voice capture is available on this machine (the daemon is present
    /// or the embedded engine is active). Combined with the `You` selection to
    /// decide whether the push-to-talk button is actually shown — an Enabled
    /// `You` with no capture backend must not offer a dead control.
    voice_available: Rc<Cell<bool>>,
    /// The `You:` (voice input) dropdown. Disabled (type only) / Enabled
    /// (push-to-talk available). The window owns the authoritative
    /// per-conversation state and drives `set_voice_in_active` on conversation
    /// switch; this dropdown is the affordance + write source.
    pub voice_in_dropdown: DropDown,
    /// User callback for `You:` dropdown changes. Guarded by `suppress` so a
    /// programmatic `set_voice_in_active` (on conversation switch) does not echo
    /// a write back.
    on_voice_in_changed: Rc<RefCell<Option<VoiceInChangedCb>>>,
    /// The `Adele:` (voice output) dropdown. Disabled / On Demand / Always. The
    /// window owns the authoritative per-conversation state and drives
    /// `set_adele_output_active` on conversation switch and on a model
    /// `request_voice` / `stop_voice`; this dropdown is the affordance + write
    /// source.
    pub adele_output_dropdown: DropDown,
    /// User callback for `Adele:` dropdown changes. Guarded by `suppress` so a
    /// programmatic `set_adele_output_active` does not echo a write back.
    on_adele_output_changed: Rc<RefCell<Option<AdeleOutputChangedCb>>>,
    /// Re-entrancy guard shared by `set_voice_in_active` and
    /// `set_adele_output_active`: a programmatic set must not echo a write back
    /// through its callback.
    suppress: Rc<Cell<bool>>,
    /// The last pipeline state reflected on the push-to-talk button. The
    /// window's click handler reads it to decide barge-in (click while
    /// `Speaking` → stop, otherwise start a push-to-talk turn).
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

/// The push-to-talk button's tooltip for a given state, mirroring the plasmoid:
/// "Stop speaking" while Speaking (the click barges in), otherwise "Push to
/// talk" with the in-progress phase appended.
fn mic_tooltip_for(state: VoiceState) -> String {
    match state {
        VoiceState::Speaking => "Stop speaking".to_string(),
        VoiceState::Idle => "Push to talk".to_string(),
        other => format!("Push to talk — {}", other.label()),
    }
}

impl Default for InputBar {
    fn default() -> Self {
        Self::new()
    }
}

impl InputBar {
    pub fn new() -> Self {
        // The bar is now vertical: an entry+Send row on top, a labeled
        // voice-controls row underneath (issue #80).
        let container = GtkBox::new(Orientation::Vertical, 4);
        container.set_margin_start(8);
        container.set_margin_end(8);
        container.set_margin_top(4);
        container.set_margin_bottom(8);

        // --- Top row: text entry + Send ------------------------------------
        let entry_row = GtkBox::new(Orientation::Horizontal, 8);

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
        entry_row.append(&scrolled);

        let send_button = Button::with_label("Send");
        send_button.add_css_class("send-button");
        send_button.set_valign(gtk4::Align::End);
        entry_row.append(&send_button);
        container.append(&entry_row);

        // --- Voice-controls row (issue #80) --------------------------------
        // A labeled row directly under the input: `You:` (input) and `Adele:`
        // (output) dropdowns, visually matched, plus a push-to-talk button
        // shown only while `You == Enabled`.
        let voice_row = GtkBox::new(Orientation::Horizontal, 8);
        voice_row.add_css_class("voice-controls-row");

        // Lead-in label so the `You:` / `Adele:` dropdowns read unambiguously as
        // voice controls rather than free-floating options (issue #80).
        let voice_leadin = Label::new(Some("Voice:"));
        voice_leadin.add_css_class("voice-controls-leadin");
        voice_row.append(&voice_leadin);

        let you_label = Label::new(Some("You:"));
        you_label.add_css_class("voice-control-label");
        voice_row.append(&you_label);

        let voice_in_dropdown = DropDown::new(
            Some(StringList::new(VOICE_IN_OPTIONS)),
            gtk4::Expression::NONE,
        );
        voice_in_dropdown.add_css_class("voice-in-dropdown");
        voice_in_dropdown.set_tooltip_text(Some(
            "Voice input — Enabled offers a push-to-talk button to speak your turns.",
        ));
        voice_row.append(&voice_in_dropdown);

        // Push-to-talk, right after the `You:` dropdown so it reads as part of
        // that control. Hidden until `You == Enabled` AND capture is available.
        let mic_image = Image::from_gicon(&gtk4::gio::ThemedIcon::from_names(MIC_IDLE_ICONS));
        let mic_button = Button::new();
        mic_button.set_child(Some(&mic_image));
        mic_button.add_css_class("mic-button");
        mic_button.set_tooltip_text(Some(&mic_tooltip_for(VoiceState::Idle)));
        mic_button.set_visible(false);
        voice_row.append(&mic_button);

        let adele_label = Label::new(Some("Adele:"));
        adele_label.add_css_class("voice-control-label");
        adele_label.set_margin_start(12);
        voice_row.append(&adele_label);

        let adele_output_dropdown = DropDown::new(
            Some(StringList::new(ADELE_OUTPUT_OPTIONS)),
            gtk4::Expression::NONE,
        );
        adele_output_dropdown.add_css_class("adele-output-dropdown");
        adele_output_dropdown.set_tooltip_text(Some(
            "Voice output — On Demand speaks while you're talking by voice; Always reads every \
             reply aloud.",
        ));
        voice_row.append(&adele_output_dropdown);

        container.append(&voice_row);

        let on_voice_in_changed: Rc<RefCell<Option<VoiceInChangedCb>>> =
            Rc::new(RefCell::new(None));
        let on_adele_output_changed: Rc<RefCell<Option<AdeleOutputChangedCb>>> =
            Rc::new(RefCell::new(None));
        // A single re-entrancy guard for both dropdowns: a programmatic
        // `set_*_active` must not echo a write back through its callback.
        let suppress = Rc::new(Cell::new(false));
        let voice_available = Rc::new(Cell::new(false));

        // `You:` dropdown changes: decode the new selection, toggle the
        // push-to-talk button's visibility, and (unless suppressed) fire the
        // user callback.
        voice_in_dropdown.connect_selected_notify(glib::clone!(
            #[strong]
            on_voice_in_changed,
            #[strong]
            suppress,
            #[strong]
            voice_available,
            #[weak]
            mic_button,
            move |dd| {
                let enabled = voice_in_from_index(dd.selected());
                // Always reflect the push-to-talk visibility, even on a
                // suppressed (programmatic) change, so the control never lies.
                mic_button.set_visible(enabled && voice_available.get());
                if suppress.get() {
                    return;
                }
                if let Some(cb) = on_voice_in_changed.borrow().as_ref() {
                    cb(enabled);
                }
            }
        ));

        // `Adele:` dropdown changes: decode the level and (unless suppressed)
        // fire the user callback.
        adele_output_dropdown.connect_selected_notify(glib::clone!(
            #[strong]
            on_adele_output_changed,
            #[strong]
            suppress,
            move |dd| {
                let level = adele_output_from_index(dd.selected());
                if suppress.get() {
                    return;
                }
                if let Some(cb) = on_adele_output_changed.borrow().as_ref() {
                    cb(level);
                }
            }
        ));

        Self {
            container,
            text_view,
            send_button,
            mic_button,
            mic_image,
            voice_available,
            voice_in_dropdown,
            on_voice_in_changed,
            adele_output_dropdown,
            on_adele_output_changed,
            suppress,
            state: Rc::new(Cell::new(VoiceState::Idle)),
        }
    }

    /// Register the callback fired when the user changes the `You:` dropdown
    /// (issue #80). The argument is the new per-conversation voice-input state.
    pub fn connect_voice_in_changed<F: Fn(bool) + 'static>(&self, f: F) {
        *self.on_voice_in_changed.borrow_mut() = Some(Box::new(f));
    }

    /// Reflect the active conversation's voice-input state on the `You:`
    /// dropdown WITHOUT echoing a write back (issue #80). Called by the window
    /// on conversation switch so the dropdown always shows the conversation it
    /// belongs to — per-conversation state, no cross-conversation bleed. Also
    /// re-evaluates the push-to-talk button's visibility.
    pub fn set_voice_in_active(&self, enabled: bool) {
        self.suppress.set(true);
        self.voice_in_dropdown.set_selected(voice_in_index(enabled));
        // `connect_selected_notify` only fires on a *change*; ensure the
        // push-to-talk visibility is correct even when the value didn't change.
        self.mic_button
            .set_visible(enabled && self.voice_available.get());
        self.suppress.set(false);
    }

    /// Register the callback fired when the user changes the `Adele:` dropdown
    /// (issue #80). The argument is the new per-conversation output level.
    pub fn connect_adele_output_changed<F: Fn(AdeleOutput) + 'static>(&self, f: F) {
        *self.on_adele_output_changed.borrow_mut() = Some(Box::new(f));
    }

    /// Reflect the active conversation's output level on the `Adele:` dropdown
    /// WITHOUT echoing a write back (issue #80). Called by the window on
    /// conversation switch and when the model drives the level via
    /// `request_voice` / `stop_voice`, so the dropdown always tracks the
    /// conversation's state — no cross-conversation bleed.
    pub fn set_adele_output_active(&self, level: AdeleOutput) {
        self.suppress.set(true);
        self.adele_output_dropdown
            .set_selected(adele_output_index(level));
        self.suppress.set(false);
    }

    /// Record whether voice capture is available on this machine and refresh the
    /// push-to-talk button's visibility accordingly (issue #80, graceful
    /// degradation per #59). When capture is unavailable the push-to-talk button
    /// stays hidden even if `You == Enabled`, so the user is never offered a dead
    /// control. The `Adele` output dropdown is independent and unaffected.
    pub fn set_voice_available(&self, available: bool) {
        self.voice_available.set(available);
        // Show push-to-talk only when capture is available AND You == Enabled.
        let you_enabled = voice_in_from_index(self.voice_in_dropdown.selected());
        self.mic_button.set_visible(available && you_enabled);
    }

    /// Reflect the voice pipeline state on the push-to-talk button: the icon
    /// swaps to the per-state glyph (mirroring the plasmoid), a CSS class drives
    /// the accent while a turn is active, and the tooltip explains the current
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

    /// The last pipeline state reflected on the push-to-talk button. The
    /// window's click handler uses this to choose barge-in (`Speaking` → stop)
    /// vs. starting a push-to-talk turn.
    pub fn current_state(&self) -> VoiceState {
        self.state.get()
    }

    /// Get the current text content *without* clearing it — a read-only snapshot
    /// of the composer (issue #2). Used to save the outgoing conversation's
    /// unsent draft on a switch, and to read the prompt on send (the live editor
    /// is cleared separately, only once the core accepts the send).
    pub fn peek_text(&self) -> String {
        let buffer = self.text_view.buffer();
        buffer
            .text(&buffer.start_iter(), &buffer.end_iter(), false)
            .to_string()
    }

    /// Replace the input's text. Used by the embedded voice path (issue #65) to
    /// drop a dictated transcript into the box before sending it like a typed
    /// message.
    pub fn set_text(&self, text: &str) {
        self.text_view.buffer().set_text(text);
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

    /// Issue #80: the `You:` dropdown index round-trips the voice-input bool —
    /// index 0 == Disabled, index 1 == Enabled — and an out-of-range index
    /// decodes to the safe Disabled default (no capture offered).
    #[test]
    fn voice_in_index_round_trips_and_defaults_disabled() {
        assert_eq!(voice_in_index(false), 0);
        assert_eq!(voice_in_index(true), 1);
        assert!(!voice_in_from_index(0));
        assert!(voice_in_from_index(1));
        // Out-of-range → Disabled (the safe default).
        assert!(!voice_in_from_index(99));
        // The labels exist and match the indices.
        assert_eq!(VOICE_IN_OPTIONS[0], "Disabled");
        assert_eq!(VOICE_IN_OPTIONS[1], "Enabled");
    }

    /// Issue #80: the `Adele:` dropdown index round-trips every output level,
    /// matching the label order, and an out-of-range index decodes to the safe
    /// Disabled default (never speaks).
    #[test]
    fn adele_output_index_round_trips_and_defaults_disabled() {
        for level in [
            AdeleOutput::Disabled,
            AdeleOutput::OnDemand,
            AdeleOutput::Always,
        ] {
            assert_eq!(adele_output_from_index(adele_output_index(level)), level);
        }
        assert_eq!(adele_output_index(AdeleOutput::Disabled), 0);
        assert_eq!(adele_output_index(AdeleOutput::OnDemand), 1);
        assert_eq!(adele_output_index(AdeleOutput::Always), 2);
        // Out-of-range → Disabled (the safe default).
        assert_eq!(adele_output_from_index(99), AdeleOutput::Disabled);
        // The labels exist and match the indices.
        assert_eq!(ADELE_OUTPUT_OPTIONS[0], "Disabled");
        assert_eq!(ADELE_OUTPUT_OPTIONS[1], "On Demand");
        assert_eq!(ADELE_OUTPUT_OPTIONS[2], "Always");
    }
}
