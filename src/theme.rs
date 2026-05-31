//! Theme handling.
//!
//! The app ships a custom accent palette split across two files:
//!
//! - `style.css` — the historical **dark** palette, applied unchanged.
//! - `style-light.css` — **light**-mode overrides that re-assert the
//!   colour-bearing properties with a light palette.
//!
//! Two `CssProvider`s carry them:
//!
//! - A *base* provider loaded from `style.css` is always installed, so dark
//!   mode is byte-for-byte what it was before this module existed.
//! - A *light* provider loaded from `style-light.css` is installed at a higher
//!   priority only while the system/GTK preference is *not* dark. Its overrides
//!   then win over the dark base; in dark mode it is removed entirely, leaving
//!   the original dark appearance untouched.
//!
//! GTK4 populates `gtk-application-prefer-dark-theme` from the
//! `org.freedesktop.appearance color-scheme` portal setting, so we treat that
//! property as the source of truth and re-apply on change — toggling the
//! desktop color scheme flips the app live without a restart.
//!
//! The chat WebView renders its own palette via a `prefers-color-scheme` media
//! query (see `markdown::html_template`), which WebKitGTK drives from the same
//! GTK dark preference.

use std::cell::Cell;
use std::rc::Rc;

use gtk4::{CssProvider, gdk, glib};

const STYLE_CSS: &str = include_str!("style.css");
const STYLE_LIGHT_CSS: &str = include_str!("style-light.css");

/// Install the app's theme handling for `display`.
///
/// Installs the dark base palette unconditionally and the light overrides only
/// while the GTK light preference is active, then keeps the choice in sync
/// with `gtk-application-prefer-dark-theme`.
///
/// Idempotent across windows: each call installs its own providers, but
/// `style_context_add_provider_for_display` is a set keyed on the provider, so
/// re-adding from a second window is harmless.
pub fn install_for_display(display: &gdk::Display) {
    let base_provider = CssProvider::new();
    base_provider.load_from_data(STYLE_CSS);

    let light_provider = CssProvider::new();
    light_provider.load_from_data(STYLE_LIGHT_CSS);

    // The dark base palette is always present.
    gtk4::style_context_add_provider_for_display(
        display,
        &base_provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    let settings = gtk4::Settings::for_display(display);
    let light_applied = Rc::new(Cell::new(false));

    let apply = {
        let display = display.clone();
        let light_provider = light_provider.clone();
        let settings = settings.clone();
        let light_applied = Rc::clone(&light_applied);
        move || {
            let want_dark = settings.is_gtk_application_prefer_dark_theme();
            if !want_dark && !light_applied.get() {
                gtk4::style_context_add_provider_for_display(
                    &display,
                    &light_provider,
                    // Above the base provider so the light overrides win.
                    gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
                );
                light_applied.set(true);
            } else if want_dark && light_applied.get() {
                gtk4::style_context_remove_provider_for_display(&display, &light_provider);
                light_applied.set(false);
            }
        }
    };

    apply();

    settings.connect_gtk_application_prefer_dark_theme_notify(glib::clone!(
        #[strong]
        apply,
        move |_| apply()
    ));
}
