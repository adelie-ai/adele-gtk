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
//! ## Pinned base theme
//!
//! The app's own CSS only styles widgets that carry an app style class; every
//! other GTK-drawn control (list-box surfaces, `MenuButton`s, `CheckButton`s,
//! the task list, the task panel's plain buttons, the sidebar stack-switcher,
//! …) falls back to the ambient GTK theme. To make those controls follow the
//! desktop's light/dark preference, [`install_for_display`] pins the base GTK
//! theme to GTK4's built-in **Adwaita** (`gtk-theme-name = "Adwaita"`). GTK
//! 4.20+ switches Adwaita between its light and dark variants automatically from
//! the `org.freedesktop.appearance color-scheme` portal setting; most
//! third-party system themes do **not**, which is why we pin Adwaita. Accepted
//! trade-off: in dark mode those unstyled GTK controls render as Adwaita-dark
//! rather than the system theme; the app's custom accent palette (below) is
//! unaffected either way.
//!
//! ## Live preference tracking
//!
//! The system color scheme is the single source of truth, read live from the
//! XDG **Settings portal** (`org.freedesktop.portal.Settings`, namespace
//! `org.freedesktop.appearance`, key `color-scheme`: 0 = no preference,
//! 1 = prefer dark, 2 = prefer light). We read it at startup and listen for the
//! `SettingChanged` signal, then drive the light-provider swap directly from
//! each change on the GTK main thread.
//!
//! We deliberately do **not** route this through GTK's
//! `gtk-application-prefer-dark-theme` setting: it was deprecated in GTK 4.20
//! (GTK now follows the portal itself) and only ever reflected the portal value
//! once at startup anyway. Ambient Adwaita controls and the chat WebView both
//! follow the same portal `color-scheme` on their own under GTK 4.20+ / modern
//! WebKitGTK, so all we need to own is the swap between our two custom
//! providers. If the portal is unavailable (no portal, read fails, etc.) we log
//! and leave the dark base in place.
//!
//! The chat WebView renders its own palette via a `prefers-color-scheme` media
//! query (see `markdown::html_template`), which WebKitGTK resolves from the
//! system color scheme.

use std::cell::Cell;
use std::rc::Rc;

use gtk4::{CssProvider, gdk, glib};
use tokio::sync::mpsc;

use crate::async_bridge::spawn_on_runtime;

const STYLE_CSS: &str = include_str!("style.css");
const STYLE_LIGHT_CSS: &str = include_str!("style-light.css");

/// XDG desktop appearance namespace served by `org.freedesktop.portal.Settings`.
const APPEARANCE_NS: &str = "org.freedesktop.appearance";
/// Key within [`APPEARANCE_NS`] carrying the system color-scheme preference.
const COLOR_SCHEME_KEY: &str = "color-scheme";

/// Typed proxy for the XDG Settings portal.
///
/// `ReadOne` is the modern accessor (one un-nested variant); the
/// `SettingChanged` signal fires with the namespace, key and new value
/// whenever a tracked setting changes.
#[zbus::proxy(
    interface = "org.freedesktop.portal.Settings",
    default_service = "org.freedesktop.portal.Desktop",
    default_path = "/org/freedesktop/portal/desktop"
)]
trait Settings {
    /// Read a single setting value (portal interface v2+).
    #[zbus(name = "ReadOne")]
    fn read_one(&self, namespace: &str, key: &str) -> zbus::Result<zbus::zvariant::OwnedValue>;

    /// Emitted when a tracked setting changes.
    #[zbus(signal)]
    fn setting_changed(
        &self,
        namespace: &str,
        key: &str,
        value: zbus::zvariant::Value<'_>,
    ) -> zbus::Result<()>;
}

/// Map the portal `color-scheme` enum to "prefer dark?".
///
/// `1` = prefer dark; everything else (`0` = no preference, `2` = prefer
/// light, or any future value) is treated as not-dark, matching GTK's own
/// interpretation of the setting.
fn color_scheme_prefers_dark(value: u32) -> bool {
    value == 1
}

/// A per-display provider-swap closure: given "prefer dark?", it adds or removes
/// the light-override `CssProvider` for its display. Boxed behind `Rc` so the
/// [`APPLIERS`] registry and the portal listener can share one handle.
type Applier = Rc<dyn Fn(bool)>;

thread_local! {
    /// Displays this process has already installed the theme on. GTK is
    /// single-threaded and effectively single-display, but tracking the actual
    /// display(s) keeps the guard correct if a second one ever appears.
    static THEMED_DISPLAYS: std::cell::RefCell<Vec<gdk::Display>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Install the app's theme handling for `display`.
///
/// Installs the dark base palette unconditionally and the light overrides only
/// while the system `color-scheme` is not dark, then keeps the choice in sync
/// via the portal listener (see [`install_color_scheme_listener`]).
///
/// **Idempotent per display (GTK-6):** the first call for a given display
/// installs the providers and registers its swap closure in [`APPLIERS`]; later
/// calls (e.g. from a second window) return immediately. Without this guard
/// every window stacked a fresh pair of `CssProvider`s and another applier on
/// the shared display.
pub fn install_for_display(display: &gdk::Display) {
    let already = THEMED_DISPLAYS.with(|seen| {
        let mut seen = seen.borrow_mut();
        if seen.iter().any(|d| d == display) {
            true
        } else {
            seen.push(display.clone());
            false
        }
    });
    if already {
        return;
    }

    let base_provider = CssProvider::new();
    base_provider.load_from_string(STYLE_CSS);

    let light_provider = CssProvider::new();
    light_provider.load_from_string(STYLE_LIGHT_CSS);

    // The dark base palette is always present.
    gtk4::style_context_add_provider_for_display(
        display,
        &base_provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    // Pin the base GTK theme to the built-in Adwaita, which GTK 4.20+ switches
    // between its light and dark variants automatically from the portal
    // `color-scheme`. This makes every GTK-drawn control that has no app style
    // class follow the desktop's light/dark preference instead of staying in
    // the system theme's (typically non-switching) palette. Idempotent across
    // windows: setting the property repeatedly to the same value is harmless.
    gtk4::Settings::for_display(display).set_gtk_theme_name(Some("Adwaita"));

    // Own the swap between the two custom providers. `want_dark` is supplied by
    // the portal `color-scheme` (see `install_color_scheme_listener`); while it
    // is dark the light overrides are removed so the original dark base shows
    // through unchanged.
    let light_applied = Cell::new(false);
    let apply: Applier = Rc::new({
        let display = display.clone();
        let light_provider = light_provider.clone();
        move |want_dark: bool| {
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
    });

    // Sync this display to the scheme seen so far (dark until the portal reports
    // otherwise), then register it so every later portal change re-applies here.
    apply(CURRENT_DARK.with(|current| current.get()));
    APPLIERS.with(|appliers| appliers.borrow_mut().push(Rc::clone(&apply)));

    // Start the process-global, idempotent portal listener that feeds every
    // registered applier. A single subscription keeps all displays in sync.
    install_color_scheme_listener();
}

thread_local! {
    /// Whether the portal listener has already been spawned this process.
    static PORTAL_LISTENER_STARTED: Cell<bool> = const { Cell::new(false) };

    /// Latest system preference seen from the portal ("prefer dark?"). Defaults
    /// to dark so a display registered before the portal's first read shows the
    /// unchanged dark base; each newly registered display syncs to this value.
    static CURRENT_DARK: Cell<bool> = const { Cell::new(true) };

    /// One provider-swap closure per themed display. The single portal listener
    /// calls all of them whenever `color-scheme` changes, so every display stays
    /// in sync from one subscription. GTK is effectively single-display, but a
    /// second one is handled correctly if it ever appears.
    static APPLIERS: std::cell::RefCell<Vec<Applier>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Subscribe to the XDG Settings portal and drive the registered per-display
/// provider swaps ([`APPLIERS`]) from `color-scheme` changes on the GTK main
/// thread.
///
/// The zbus connection and signal stream live on the shared Tokio runtime
/// (where zbus has a reactor); only plain `bool` "prefer dark" values cross
/// back to the main thread, where the non-`Send` GTK providers are swapped.
///
/// Idempotent: only the first call per process spawns the listener. If the
/// portal is unavailable or a read fails, it logs and returns, leaving the dark
/// base in place.
fn install_color_scheme_listener() {
    if PORTAL_LISTENER_STARTED.with(|started| started.replace(true)) {
        return;
    }

    // Plain `bool`s ride this channel; the GTK providers never leave the main
    // thread.
    let (tx, mut rx) = mpsc::unbounded_channel::<bool>();

    // Apply updates on the GTK main thread. Skip no-op changes so we don't churn
    // the provider swap when the portal re-announces the current value.
    glib::spawn_future_local(async move {
        while let Some(want_dark) = rx.recv().await {
            if CURRENT_DARK.with(|current| current.replace(want_dark)) != want_dark {
                tracing::debug!(want_dark, "applying live color-scheme change");
                // Clone out the Rc handles so no borrow is held across the calls.
                let appliers = APPLIERS.with(|appliers| appliers.borrow().clone());
                for apply in &appliers {
                    apply(want_dark);
                }
            }
        }
    });

    // Own the portal connection + signal stream on the Tokio runtime.
    spawn_on_runtime(async move {
        if let Err(error) = run_color_scheme_listener(&tx).await {
            tracing::warn!(
                %error,
                "XDG Settings portal unavailable; color scheme will not switch live \
                 (dark base kept)"
            );
        }
    });
}

/// Connect to the Settings portal, push the initial `color-scheme`, then
/// forward every `SettingChanged` for it. Returns when the bus drops or the
/// receiver ([`tx`]) is closed (window gone).
async fn run_color_scheme_listener(tx: &mpsc::UnboundedSender<bool>) -> zbus::Result<()> {
    // `futures_core` is re-exported by zbus, so the signal stream can be driven
    // without taking on a separate `futures-util` dependency for `StreamExt`.
    use std::pin::pin;

    use zbus::export::futures_core::Stream;

    let connection = zbus::Connection::session().await?;
    let proxy = SettingsProxy::new(&connection).await?;

    // Initial value: a successful read seeds the preference; failure here is
    // not fatal — we still attach the signal listener below.
    match proxy.read_one(APPEARANCE_NS, COLOR_SCHEME_KEY).await {
        Ok(value) => {
            if let Some(dark) = color_scheme_value(&value) {
                let _ = tx.send(dark);
            }
        }
        Err(error) => {
            tracing::debug!(%error, "initial color-scheme read failed; relying on signal");
        }
    }

    let changes = proxy.receive_setting_changed().await?;
    let mut changes = pin!(changes);
    while let Some(change) =
        std::future::poll_fn(|cx| Stream::poll_next(changes.as_mut(), cx)).await
    {
        let Ok(args) = change.args() else { continue };
        if args.namespace != APPEARANCE_NS || args.key != COLOR_SCHEME_KEY {
            continue;
        }
        if let Some(dark) = color_scheme_value(&args.value)
            && tx.send(dark).is_err()
        {
            // Receiver gone (window closed) — stop listening.
            return Ok(());
        }
    }

    Ok(())
}

/// Decode a `color-scheme` portal value into "prefer dark?".
///
/// The value is normally a bare `u32`, but some portal versions hand it back
/// still wrapped in a variant; `downcast_ref` transparently unwraps one level
/// of that nesting. Returns `None` (logged) for any other shape rather than
/// guessing.
fn color_scheme_value(value: &zbus::zvariant::Value<'_>) -> Option<bool> {
    match value.downcast_ref::<u32>() {
        Ok(scheme) => Some(color_scheme_prefers_dark(scheme)),
        Err(error) => {
            tracing::debug!(%error, "unexpected color-scheme value type from portal");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use zbus::zvariant::Value;

    use super::*;

    #[test]
    fn only_prefer_dark_enum_value_means_dark() {
        // Portal enum: 0 = no preference, 1 = prefer dark, 2 = prefer light.
        assert!(!color_scheme_prefers_dark(0));
        assert!(color_scheme_prefers_dark(1));
        assert!(!color_scheme_prefers_dark(2));
    }

    #[test]
    fn unknown_future_enum_values_are_not_dark() {
        // Anything we don't recognise stays light, matching GTK's own reading.
        assert!(!color_scheme_prefers_dark(3));
        assert!(!color_scheme_prefers_dark(u32::MAX));
    }

    #[test]
    fn bare_u32_value_decodes() {
        assert_eq!(color_scheme_value(&Value::U32(1)), Some(true));
        assert_eq!(color_scheme_value(&Value::U32(2)), Some(false));
    }

    #[test]
    fn variant_wrapped_value_decodes() {
        // Some portal versions return the value still nested in a variant.
        let nested = Value::Value(Box::new(Value::U32(1)));
        assert_eq!(color_scheme_value(&nested), Some(true));
    }

    #[test]
    fn wrong_type_yields_none() {
        assert_eq!(color_scheme_value(&Value::Str("dark".into())), None);
    }
}
