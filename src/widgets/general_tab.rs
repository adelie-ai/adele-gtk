//! General tab of the Settings dialog.
//!
//! Holds client-local preferences (as opposed to the daemon-side settings the
//! other tabs edit over the management RPC). Currently a single toggle: whether
//! to share basic device context with the assistant (desktop-assistant#549).
//!
//! The tab owns the widget and emits a `toggled` callback; the parent (the
//! Settings dialog) loads the initial value from
//! [`crate::preferences::PreferencesStore`] and persists changes. Re-hydration
//! is guarded by a `suppress` flag so applying the persisted state
//! programmatically doesn't echo back as a write - the same pattern as
//! `voice_tab.rs`.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{Align, Box as GtkBox, CheckButton, Label, Orientation, Separator, glib};

type ToggledCb = Box<dyn Fn(bool)>;

/// The Settings dialog's General tab.
pub struct GeneralTab {
    /// Root container appended into the Settings notebook.
    pub container: GtkBox,
    share_check: CheckButton,
    on_toggled: Rc<RefCell<Option<ToggledCb>>>,
    /// While true, applying the persisted state suppresses the `toggled` write
    /// callback so hydration isn't echoed back as a save.
    suppress: Rc<RefCell<bool>>,
}

impl GeneralTab {
    /// Build the tab with the share-device-info toggle checked by default
    /// (sharing is on unless the user opts out, desktop-assistant#549).
    pub fn new() -> Self {
        let container = GtkBox::new(Orientation::Vertical, 8);
        container.set_margin_start(12);
        container.set_margin_end(12);
        container.set_margin_top(12);
        container.set_margin_bottom(12);

        let header = Label::new(Some("General"));
        header.add_css_class("heading");
        header.set_halign(Align::Start);
        container.append(&header);

        container.append(&Separator::new(Orientation::Horizontal));

        let share_check = CheckButton::with_label("Share device info with the assistant");
        share_check.set_halign(Align::Start);
        // Default checked: absent preference means sharing is on (#549).
        share_check.set_active(true);
        share_check.set_margin_top(6);
        share_check.set_tooltip_text(Some(
            "Lets Adele personalize using your name, username, home folder, hostname, \
             timezone, and OS. When off, nothing about your device is sent.",
        ));
        container.append(&share_check);

        let subtext = Label::new(Some(
            "Lets Adele personalize using your name, username, home folder, hostname, \
             timezone, and OS. When off, nothing about your device is sent. The change \
             applies the next time the client connects.",
        ));
        subtext.set_wrap(true);
        subtext.set_halign(Align::Start);
        subtext.add_css_class("dim-label");
        container.append(&subtext);

        let on_toggled: Rc<RefCell<Option<ToggledCb>>> = Rc::new(RefCell::new(None));
        let suppress = Rc::new(RefCell::new(false));

        share_check.connect_toggled(glib::clone!(
            #[strong]
            on_toggled,
            #[strong]
            suppress,
            move |btn| {
                if *suppress.borrow() {
                    return;
                }
                if let Some(cb) = on_toggled.borrow().as_ref() {
                    cb(btn.is_active());
                }
            }
        ));

        Self {
            container,
            share_check,
            on_toggled,
            suppress,
        }
    }

    /// Register the callback fired when the user toggles the share checkbox. The
    /// argument is the new checked state (`true` = share on).
    pub fn connect_toggled<F: Fn(bool) + 'static>(&self, f: F) {
        *self.on_toggled.borrow_mut() = Some(Box::new(f));
    }

    /// Apply the persisted value without echoing a write back. `set_active` fires
    /// `toggled` synchronously, so the `suppress` flag is safely cleared right
    /// after the call returns.
    pub fn set_share_client_context(&self, on: bool) {
        *self.suppress.borrow_mut() = true;
        self.share_check.set_active(on);
        *self.suppress.borrow_mut() = false;
    }
}
