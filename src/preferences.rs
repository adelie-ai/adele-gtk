//! Client-local user preferences for adele-gtk.
//!
//! A tiny JSON document stored next to the profile / selected-model files in the
//! app's config dir (`~/.config/adele-gtk/preferences.json`). It holds the
//! handful of client-side toggles that are this client's own to remember - as
//! opposed to daemon-side settings (connections, purposes, MCP servers), which
//! live on the daemon and are edited over the management RPC.
//!
//! The store is deliberately separate from [`crate::profile`]: those files key
//! off connection identity, whereas these preferences are global to this client
//! install and outlive any single connection.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// The default for [`Preferences::share_client_context`].
///
/// Sharing is **on** by default (desktop-assistant#549): an absent field, an
/// absent file, or an unreadable one all resolve to `true`, matching
/// [`ConnectionConfig`](desktop_assistant_client_common::ConnectionConfig)'s own
/// default so the two never disagree on the resolved value.
fn default_share_client_context() -> bool {
    true
}

/// Client-local, install-global preferences persisted as JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preferences {
    /// Share basic device context (real name, username, home dir, hostname,
    /// timezone, OS) with the assistant so it can personalize
    /// (desktop-assistant#549). Default **on**; when off the client attaches no
    /// device context to the connect handshake. A missing key deserializes to on
    /// for backward compatibility with pre-#549 files.
    #[serde(default = "default_share_client_context")]
    pub share_client_context: bool,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            share_client_context: default_share_client_context(),
        }
    }
}

/// The app's config directory (`~/.config/adele-gtk`), or the current directory
/// if the platform config dir cannot be resolved. Mirrors [`crate::profile`].
fn default_config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("adele-gtk")
}

/// Reads and writes [`Preferences`] as `preferences.json` in the app config dir.
pub struct PreferencesStore {
    path: PathBuf,
}

impl PreferencesStore {
    /// Store backed by the default config location (`~/.config/adele-gtk`).
    pub fn new() -> Self {
        Self::with_dir(default_config_dir())
    }

    /// Store backed by an explicit directory (used by tests).
    pub fn with_dir(dir: PathBuf) -> Self {
        Self {
            path: dir.join("preferences.json"),
        }
    }

    /// Load the preferences, defaulting on any absence or corruption.
    ///
    /// A missing file, an unreadable one, or an unparseable one all yield
    /// [`Preferences::default`] (sharing on) rather than an error, so a bad file
    /// can never wedge the connect path - it just falls back to the defaults and
    /// logs. Mirrors [`crate::profile::LastConnectionStore`]'s corrupt-file
    /// tolerance.
    pub fn load(&self) -> Preferences {
        let Ok(data) = std::fs::read_to_string(&self.path) else {
            return Preferences::default();
        };
        match serde_json::from_str::<Preferences>(&data) {
            Ok(prefs) => prefs,
            Err(e) => {
                tracing::warn!(
                    "ignoring unparseable {} ({e}); using default preferences",
                    self.path.display()
                );
                Preferences::default()
            }
        }
    }

    /// Persist `prefs`, creating the config dir if needed.
    pub fn save(&self, prefs: &Preferences) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_string_pretty(prefs).context("serializing preferences")?;
        std::fs::write(&self.path, data)
            .with_context(|| format!("writing {}", self.path.display()))?;
        Ok(())
    }

    /// Persist just the `share_client_context` toggle, preserving any other
    /// stored preferences (loads the full document first, so unrelated fields
    /// survive the write).
    pub fn set_share_client_context(&self, on: bool) -> Result<()> {
        let mut prefs = self.load();
        prefs.share_client_context = on;
        self.save(&prefs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "adele-gtk-test-{}-{}-{}",
                name,
                std::process::id(),
                uuid::Uuid::new_v4(),
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn share_client_context_defaults_on() {
        assert!(Preferences::default().share_client_context);
    }

    #[test]
    fn absent_file_loads_default_on() {
        let dir = TempDir::new("prefs-absent");
        let store = PreferencesStore::with_dir(dir.path.clone());
        assert!(store.load().share_client_context);
    }

    #[test]
    fn missing_key_deserializes_on() {
        // A document written before the field existed (here, an empty object)
        // must load with sharing on, not off.
        let dir = TempDir::new("prefs-missing-key");
        let store = PreferencesStore::with_dir(dir.path.clone());
        std::fs::write(dir.path.join("preferences.json"), "{}").unwrap();
        assert!(store.load().share_client_context);
    }

    #[test]
    fn save_then_load_round_trips_off() {
        let dir = TempDir::new("prefs-roundtrip");
        let store = PreferencesStore::with_dir(dir.path.clone());
        store
            .save(&Preferences {
                share_client_context: false,
            })
            .unwrap();
        assert!(!store.load().share_client_context);
    }

    #[test]
    fn set_share_client_context_persists_both_values() {
        let dir = TempDir::new("prefs-set");
        let store = PreferencesStore::with_dir(dir.path.clone());
        store.set_share_client_context(false).unwrap();
        assert!(!store.load().share_client_context);
        store.set_share_client_context(true).unwrap();
        assert!(store.load().share_client_context);
    }

    #[test]
    fn corrupt_file_loads_default_on() {
        let dir = TempDir::new("prefs-corrupt");
        let store = PreferencesStore::with_dir(dir.path.clone());
        std::fs::write(dir.path.join("preferences.json"), "not json").unwrap();
        assert!(store.load().share_client_context);
    }
}
