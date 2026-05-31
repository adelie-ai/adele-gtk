use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Transport-specific connection settings for a profile.
///
/// Tagged on a `kind` field so the on-disk JSON is self-describing:
/// `{"kind":"local"}` or `{"kind":"websocket","url":...,"subject":...}`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ProtocolConfig {
    /// Local Unix domain socket. A `None` path uses the daemon's default
    /// socket (`$XDG_RUNTIME_DIR/adelie/sock`).
    Local {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<PathBuf>,
    },
    /// WebSocket endpoint (local or remote).
    Websocket {
        url: String,
        #[serde(default = "default_ws_subject")]
        subject: String,
    },
}

impl Default for ProtocolConfig {
    fn default() -> Self {
        // New connections default to the local socket.
        ProtocolConfig::Local { path: None }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConnectionProfile {
    pub id: String,
    pub name: String,
    pub protocol: ProtocolConfig,
}

pub fn default_ws_subject() -> String {
    "desktop-tui".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ProfilesFile {
    profiles: Vec<ConnectionProfile>,
}

fn default_config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("adele-gtk")
}

pub struct ProfileStore {
    path: PathBuf,
}

impl ProfileStore {
    pub fn new() -> Self {
        Self::with_dir(default_config_dir())
    }

    pub fn with_dir(dir: PathBuf) -> Self {
        Self {
            path: dir.join("profiles.json"),
        }
    }

    pub fn load(&self) -> Result<Vec<ConnectionProfile>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let data = std::fs::read_to_string(&self.path)
            .with_context(|| format!("reading {}", self.path.display()))?;
        // No schema migration (#33): a profiles.json written by an older build
        // (the flat `ws_url`/`ws_subject` shape) lacks the `protocol` field and
        // won't parse into the tagged shape. Treat an unparseable file as "no
        // profiles" so the user just re-creates their connections instead of
        // the app erroring on startup. Mirrors `LastConnectionStore::get`'s
        // existing corrupt-file tolerance.
        match serde_json::from_str::<ProfilesFile>(&data) {
            Ok(file) => Ok(file.profiles),
            Err(e) => {
                tracing::warn!(
                    "ignoring unparseable {} ({e}); starting with no profiles",
                    self.path.display()
                );
                Ok(Vec::new())
            }
        }
    }

    pub fn save(&self, profiles: &[ConnectionProfile]) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = ProfilesFile {
            profiles: profiles.to_vec(),
        };
        let data = serde_json::to_string_pretty(&file)?;
        std::fs::write(&self.path, data)?;
        Ok(())
    }

    pub fn add(&self, profile: ConnectionProfile) -> Result<()> {
        let mut profiles = self.load()?;
        profiles.push(profile);
        self.save(&profiles)
    }

    pub fn update(&self, profile: &ConnectionProfile) -> Result<()> {
        let mut profiles = self.load()?;
        if let Some(existing) = profiles.iter_mut().find(|p| p.id == profile.id) {
            *existing = profile.clone();
        }
        self.save(&profiles)
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        let mut profiles = self.load()?;
        profiles.retain(|p| p.id != id);
        self.save(&profiles)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct LastConnectionFile {
    profile_id: Option<String>,
}

/// Records the most recently connected profile so the app can silently
/// re-establish that connection on the next launch.
pub struct LastConnectionStore {
    path: PathBuf,
}

impl LastConnectionStore {
    pub fn new() -> Self {
        Self::with_dir(default_config_dir())
    }

    pub fn with_dir(dir: PathBuf) -> Self {
        Self {
            path: dir.join("last_connection.json"),
        }
    }

    pub fn get(&self) -> Option<String> {
        let data = std::fs::read_to_string(&self.path).ok()?;
        let file: LastConnectionFile = serde_json::from_str(&data).ok()?;
        file.profile_id
    }

    pub fn set(&self, profile_id: &str) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = LastConnectionFile {
            profile_id: Some(profile_id.to_string()),
        };
        let data = serde_json::to_string_pretty(&file)?;
        std::fs::write(&self.path, data)?;
        Ok(())
    }

    #[cfg(test)]
    pub fn path(&self) -> &std::path::Path {
        &self.path
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

    fn sample_profile(id: &str) -> ConnectionProfile {
        ConnectionProfile {
            id: id.to_string(),
            name: format!("name-{id}"),
            protocol: ProtocolConfig::Websocket {
                url: format!("ws://example.com/{id}"),
                subject: "desktop-tui".to_string(),
            },
        }
    }

    #[test]
    fn profile_store_round_trip() {
        let dir = TempDir::new("profiles-roundtrip");
        let store = ProfileStore::with_dir(dir.path.clone());

        assert!(store.load().unwrap().is_empty());

        store.add(sample_profile("a")).unwrap();
        store.add(sample_profile("b")).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].id, "a");
        assert_eq!(loaded[1].id, "b");

        let mut updated = sample_profile("a");
        updated.name = "renamed".to_string();
        store.update(&updated).unwrap();
        assert_eq!(store.load().unwrap()[0].name, "renamed");

        store.delete("a").unwrap();
        let remaining = store.load().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, "b");
    }

    #[test]
    fn protocol_config_serializes_both_variants() {
        let local = ProtocolConfig::Local {
            path: Some(PathBuf::from("/run/user/1000/adelie/sock")),
        };
        let local_json = serde_json::to_string(&local).unwrap();
        assert!(local_json.contains("\"kind\":\"local\""));
        assert_eq!(
            serde_json::from_str::<ProtocolConfig>(&local_json).unwrap(),
            local
        );

        // Default Local omits the path entirely.
        let default_local = ProtocolConfig::Local { path: None };
        let json = serde_json::to_string(&default_local).unwrap();
        assert_eq!(json, r#"{"kind":"local"}"#);
        assert_eq!(
            serde_json::from_str::<ProtocolConfig>(&json).unwrap(),
            default_local
        );

        let ws = ProtocolConfig::Websocket {
            url: "wss://host/ws".into(),
            subject: "desktop-tui".into(),
        };
        let ws_json = serde_json::to_string(&ws).unwrap();
        assert!(ws_json.contains("\"kind\":\"websocket\""));
        assert_eq!(
            serde_json::from_str::<ProtocolConfig>(&ws_json).unwrap(),
            ws
        );
    }

    #[test]
    fn new_profile_protocol_defaults_to_local() {
        assert_eq!(
            ProtocolConfig::default(),
            ProtocolConfig::Local { path: None }
        );
    }

    #[test]
    fn load_tolerates_legacy_unparseable_profiles() {
        // The pre-#33 flat shape (ws_url/ws_subject, no `protocol`). With no
        // migration, this must degrade to "no profiles", not an error.
        let dir = TempDir::new("legacy-profiles");
        let store = ProfileStore::with_dir(dir.path.clone());
        std::fs::write(
            dir.path.join("profiles.json"),
            r#"{"profiles":[{"id":"1","name":"old","ws_url":"ws://x","ws_subject":"s"}]}"#,
        )
        .unwrap();
        assert!(store.load().unwrap().is_empty());
    }

    #[test]
    fn last_connection_get_returns_none_when_absent() {
        let dir = TempDir::new("last-absent");
        let store = LastConnectionStore::with_dir(dir.path.clone());
        assert_eq!(store.get(), None);
    }

    #[test]
    fn last_connection_set_then_get() {
        let dir = TempDir::new("last-set-get");
        let store = LastConnectionStore::with_dir(dir.path.clone());
        store.set("profile-xyz").unwrap();
        assert_eq!(store.get().as_deref(), Some("profile-xyz"));
    }

    #[test]
    fn last_connection_set_overwrites() {
        let dir = TempDir::new("last-overwrite");
        let store = LastConnectionStore::with_dir(dir.path.clone());
        store.set("first").unwrap();
        store.set("second").unwrap();
        assert_eq!(store.get().as_deref(), Some("second"));
    }

    #[test]
    fn last_connection_get_handles_corrupt_file() {
        let dir = TempDir::new("last-corrupt");
        let store = LastConnectionStore::with_dir(dir.path.clone());
        std::fs::write(store.path(), "not json").unwrap();
        assert_eq!(store.get(), None);
    }
}
