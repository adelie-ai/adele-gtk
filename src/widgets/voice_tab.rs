//! Voice tab of the Settings dialog.
//!
//! Two controls backed by the standalone voice daemon
//! (`org.desktopAssistant.Voice`, see `crate::voice_client`):
//!
//! - an **Enable "Hey Adele"** toggle → `SetEnabled` / `GetEnabled` (the
//!   always-on wake word). There is no tray in adele-gtk, so this lives here
//!   rather than a tray menu (issue #59).
//! - a **TTS voice** dropdown → `ListVoices` / `SetVoice`.
//!
//! The tab owns the widgets and emits callbacks; the parent (Settings dialog)
//! wires those to the `VoiceController` and re-hydrates the tab from daemon
//! state. When the daemon is absent the parent calls
//! [`VoiceTab::set_unavailable`], which disables the controls and shows a hint
//! rather than presenting dead toggles (graceful degradation).
//!
//! Re-hydration uses a `suppress` flag (mirroring `purposes_tab.rs`) so that
//! programmatically applying daemon state doesn't echo back as a write.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{
    Align, Box as GtkBox, CheckButton, DropDown, Label, Orientation, Separator, StringList, glib,
};

use crate::voice_client::VoiceInfo;

type SetEnabledCb = Box<dyn Fn(bool)>;
type SetVoiceCb = Box<dyn Fn(String)>;

pub struct VoiceTab {
    pub container: GtkBox,
    enable_check: CheckButton,
    voice_dd: DropDown,
    voice_list: StringList,
    /// Voice ids in the same index order as `voice_dd`'s string list, so a
    /// dropdown index maps back to the id to send in `SetVoice`.
    voice_ids: Rc<RefCell<Vec<String>>>,
    /// Shown when the daemon is unreachable; replaces the live hint.
    status: Label,
    on_set_enabled: Rc<RefCell<Option<SetEnabledCb>>>,
    on_set_voice: Rc<RefCell<Option<SetVoiceCb>>>,
    /// While true, we're reconciling the UI to daemon state — suppress the
    /// `toggled` / `selected` write callbacks so re-hydration isn't echoed back.
    suppress: Rc<RefCell<bool>>,
}

impl VoiceTab {
    pub fn new() -> Self {
        let container = GtkBox::new(Orientation::Vertical, 8);
        container.set_margin_start(12);
        container.set_margin_end(12);
        container.set_margin_top(12);
        container.set_margin_bottom(12);

        let header = Label::new(Some("Voice"));
        header.add_css_class("heading");
        header.set_halign(Align::Start);
        container.append(&header);

        let blurb = Label::new(Some(
            "Talk to Adele hands-free. The voice daemon runs on-device; speech \
             never leaves this machine.",
        ));
        blurb.set_wrap(true);
        blurb.set_halign(Align::Start);
        blurb.add_css_class("dim-label");
        container.append(&blurb);

        container.append(&Separator::new(Orientation::Horizontal));

        // --- Wake word toggle ---------------------------------------------
        let enable_check = CheckButton::with_label("Enable \u{201c}Hey Adele\u{201d}");
        enable_check.set_halign(Align::Start);
        enable_check.set_tooltip_text(Some(
            "Listen continuously for the \u{201c}Hey Adele\u{201d} wake word. \
             With this off, use the record button in the chat bar to talk.",
        ));
        enable_check.set_margin_top(6);
        container.append(&enable_check);

        // --- Voice selection ----------------------------------------------
        let voice_row = GtkBox::new(Orientation::Horizontal, 8);
        voice_row.set_margin_top(6);
        let voice_label = Label::new(Some("Spoken voice"));
        voice_label.set_halign(Align::Start);
        voice_label.set_width_chars(14);
        voice_row.append(&voice_label);

        let voice_list = StringList::new(&[]);
        let voice_dd = DropDown::new(Some(voice_list.clone()), gtk4::Expression::NONE);
        voice_dd.set_hexpand(true);
        voice_row.append(&voice_dd);
        container.append(&voice_row);

        // Live hint / unavailable message, sharing the dim style with the blurb.
        let status = Label::new(None);
        status.set_halign(Align::Start);
        status.set_wrap(true);
        status.add_css_class("dim-label");
        status.set_margin_top(6);
        container.append(&status);

        let voice_ids: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let on_set_enabled: Rc<RefCell<Option<SetEnabledCb>>> = Rc::new(RefCell::new(None));
        let on_set_voice: Rc<RefCell<Option<SetVoiceCb>>> = Rc::new(RefCell::new(None));
        let suppress = Rc::new(RefCell::new(false));

        enable_check.connect_toggled(glib::clone!(
            #[strong]
            on_set_enabled,
            #[strong]
            suppress,
            move |btn| {
                if *suppress.borrow() {
                    return;
                }
                if let Some(cb) = on_set_enabled.borrow().as_ref() {
                    cb(btn.is_active());
                }
            }
        ));

        voice_dd.connect_selected_notify(glib::clone!(
            #[strong]
            on_set_voice,
            #[strong]
            voice_ids,
            #[strong]
            suppress,
            move |dd| {
                if *suppress.borrow() {
                    return;
                }
                let idx = dd.selected() as usize;
                if let Some(id) = voice_ids.borrow().get(idx).cloned()
                    && let Some(cb) = on_set_voice.borrow().as_ref()
                {
                    cb(id);
                }
            }
        ));

        Self {
            container,
            enable_check,
            voice_dd,
            voice_list,
            voice_ids,
            status,
            on_set_enabled,
            on_set_voice,
            suppress,
        }
    }

    /// Register the callback fired when the user toggles the wake word.
    pub fn connect_set_enabled<F: Fn(bool) + 'static>(&self, f: F) {
        *self.on_set_enabled.borrow_mut() = Some(Box::new(f));
    }

    /// Register the callback fired when the user picks a different voice. The
    /// argument is the chosen voice id.
    pub fn connect_set_voice<F: Fn(String) + 'static>(&self, f: F) {
        *self.on_set_voice.borrow_mut() = Some(Box::new(f));
    }

    /// Apply the daemon's current wake-word enabled flag without echoing a
    /// write back.
    pub fn set_enabled_state(&self, enabled: bool) {
        let _guard = SuppressGuard::new(&self.suppress);
        self.enable_check.set_active(enabled);
    }

    /// Populate the voice dropdown from `ListVoices` and select `current` (the
    /// active voice id from `GetVoice`, if any). Setting the model + selection
    /// is suppressed so it doesn't fire a spurious `SetVoice`.
    ///
    /// `SetVoice` is always issued with the default speaker (`-1`); a
    /// per-speaker sub-picker is out of scope for the single voice dropdown the
    /// issue calls for, and `-1` selects each model's default speaker.
    pub fn set_voices(&self, voices: &[VoiceInfo], current: Option<&str>) {
        let _guard = SuppressGuard::new(&self.suppress);

        // Reset the GTK string list in place.
        while self.voice_list.n_items() > 0 {
            self.voice_list.remove(0);
        }
        let mut ids = Vec::with_capacity(voices.len());
        for v in voices {
            self.voice_list.append(&display_label(v));
            ids.push(v.id.clone());
        }

        let selected = current
            .and_then(|cur| ids.iter().position(|id| id == cur))
            .unwrap_or(0);
        *self.voice_ids.borrow_mut() = ids;

        let has_voices = self.voice_list.n_items() > 0;
        self.voice_dd.set_sensitive(has_voices);
        if has_voices {
            self.voice_dd.set_selected(selected as u32);
        }

        self.status.set_text(if has_voices {
            ""
        } else {
            "No voices installed. Run the voice daemon's setup to provision a voice model."
        });
    }

    /// Disable the controls and explain why — used when the voice daemon has
    /// no owner on the bus (not running / models unprovisioned).
    pub fn set_unavailable(&self) {
        let _guard = SuppressGuard::new(&self.suppress);
        self.enable_check.set_active(false);
        self.enable_check.set_sensitive(false);
        self.voice_dd.set_sensitive(false);
        self.status.set_text(
            "Voice service not available. Start the voice daemon \
             (org.desktopAssistant.Voice) to enable these controls.",
        );
    }

    /// Re-enable the controls after a transition back to available (e.g. the
    /// dialog is re-presented while the daemon is now up). Selection state is
    /// applied separately via [`VoiceTab::set_enabled_state`] /
    /// [`VoiceTab::set_voices`].
    pub fn set_available(&self) {
        self.enable_check.set_sensitive(true);
        self.voice_dd.set_sensitive(true);
        self.status.set_text("");
    }
}

/// RAII guard that sets the `suppress` flag for the duration of a
/// state-reconciliation, so programmatic widget updates don't fire the user
/// `toggled` / `selected` callbacks. Mirrors the suppression pattern in
/// `purposes_tab.rs` but bundled into a guard so early returns can't leak it.
struct SuppressGuard<'a> {
    flag: &'a Rc<RefCell<bool>>,
}

impl<'a> SuppressGuard<'a> {
    fn new(flag: &'a Rc<RefCell<bool>>) -> Self {
        *flag.borrow_mut() = true;
        Self { flag }
    }
}

impl Drop for SuppressGuard<'_> {
    fn drop(&mut self) {
        *self.flag.borrow_mut() = false;
    }
}

/// Render a voice's dropdown label. Prefers the display name, appends the
/// language when present, and falls back to the raw id if the name is empty.
fn display_label(v: &VoiceInfo) -> String {
    let name = if v.display_name.trim().is_empty() {
        v.id.as_str()
    } else {
        v.display_name.as_str()
    };
    if v.language.trim().is_empty() {
        name.to_string()
    } else {
        format!("{name} ({})", v.language)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn voice(id: &str, name: &str, lang: &str, speakers: u32) -> VoiceInfo {
        VoiceInfo {
            id: id.to_string(),
            display_name: name.to_string(),
            language: lang.to_string(),
            num_speakers: speakers,
        }
    }

    #[test]
    fn label_combines_name_and_language() {
        assert_eq!(
            display_label(&voice("en_US-amy", "Amy", "en_US", 1)),
            "Amy (en_US)"
        );
    }

    #[test]
    fn label_falls_back_to_id_when_name_empty() {
        assert_eq!(
            display_label(&voice("en_US-amy", "", "en_US", 1)),
            "en_US-amy (en_US)"
        );
    }

    #[test]
    fn label_omits_empty_language() {
        assert_eq!(display_label(&voice("custom", "Custom", "", 2)), "Custom");
    }
}
