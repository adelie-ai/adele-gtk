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
//! We treat `gtk-application-prefer-dark-theme` as the source of truth and
//! re-apply the provider choice whenever it changes. GTK4 reads that property
//! from the `org.freedesktop.appearance color-scheme` portal setting only
//! **once at startup**, though — it does not follow later changes — so on its
//! own the app would only pick up a new desktop color scheme after a restart.
//!
//! To switch live, we subscribe to the XDG **Settings portal**
//! (`org.freedesktop.portal.Settings`, namespace `org.freedesktop.appearance`,
//! key `color-scheme`: 0 = no preference, 1 = prefer dark, 2 = prefer light),
//! read it at startup and listen for the `SettingChanged` signal, then push
//! each change into `gtk-application-prefer-dark-theme` on the GTK main thread.
//! Writing that property drives the existing provider swap **and** WebKit's
//! `prefers-color-scheme`. If the portal is unavailable (no portal, read
//! fails, etc.) we log and fall back to the startup-only behaviour above.
//!
//! The chat WebView renders its own palette via a `prefers-color-scheme` media
//! query (see `markdown::html_template`), which WebKitGTK drives from the same
//! GTK dark preference.

use std::cell::Cell;
use std::rc::Rc;

use gtk4::{CssProvider, Settings, gdk, glib};
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

    // Track the system color scheme live. `install_for_display` runs once per
    // window, but the GTK preference is process-global and the notify above
    // re-applies for every display, so a single portal listener is enough.
    install_color_scheme_listener(settings);
}

thread_local! {
    /// Whether the portal listener has already been spawned this process.
    static PORTAL_LISTENER_STARTED: Cell<bool> = const { Cell::new(false) };
}

/// Subscribe to the XDG Settings portal and mirror `color-scheme` changes into
/// `gtk-application-prefer-dark-theme` on the GTK main thread.
///
/// The zbus connection and signal stream live on the shared Tokio runtime
/// (where zbus has a reactor); only plain `bool` "prefer dark" values cross
/// back to the main thread, where the non-`Send` [`Settings`] is mutated.
///
/// Idempotent: only the first call per process spawns the listener. If the
/// portal is unavailable or a read fails, it logs and returns, leaving the
/// startup-derived GTK preference in place.
fn install_color_scheme_listener(settings: Settings) {
    if PORTAL_LISTENER_STARTED.with(|started| started.replace(true)) {
        return;
    }

    // Plain `bool`s ride this channel; `Settings` never leaves the main thread.
    let (tx, mut rx) = mpsc::unbounded_channel::<bool>();

    // Apply updates on the GTK main thread. Skip no-op writes so we don't
    // churn the provider swap when the portal re-announces the current value.
    glib::spawn_future_local(async move {
        while let Some(want_dark) = rx.recv().await {
            if settings.is_gtk_application_prefer_dark_theme() != want_dark {
                tracing::debug!(want_dark, "applying live color-scheme change");
                settings.set_gtk_application_prefer_dark_theme(want_dark);
            }
        }
    });

    // Own the portal connection + signal stream on the Tokio runtime.
    spawn_on_runtime(async move {
        if let Err(error) = run_color_scheme_listener(&tx).await {
            tracing::warn!(
                %error,
                "XDG Settings portal unavailable; color scheme will not switch live \
                 (startup preference kept)"
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
