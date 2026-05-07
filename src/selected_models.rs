use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Identifies a single (connection, model) pair the user has chosen to keep
/// in the model picker dropdown. The set is filtered client-side so the
/// dropdown stays manageable even when a connector exposes hundreds of
/// models (e.g. Bedrock).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectedModel {
    pub connection_id: String,
    pub model_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SelectedModelsFile {
    #[serde(default)]
    models: Vec<SelectedModel>,
}

fn default_config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("adele-gtk")
}

/// Persists the user's curated subset of available models. Stored as JSON
/// next to the existing profile/last-connection files.
pub struct SelectedModelsStore {
    path: PathBuf,
}

impl SelectedModelsStore {
    pub fn new() -> Self {
        Self::with_dir(default_config_dir())
    }

    pub fn with_dir(dir: PathBuf) -> Self {
        Self {
            path: dir.join("selected_models.json"),
        }
    }

    /// True when the user has never persisted a selection. Callers use this
    /// to decide whether to seed the list with a sensible default (typically
    /// the first non-embedding model from `list_available_models`).
    pub fn is_initialized(&self) -> bool {
        self.path.exists()
    }

    pub fn load(&self) -> Result<Vec<SelectedModel>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let data = std::fs::read_to_string(&self.path)
            .with_context(|| format!("reading {}", self.path.display()))?;
        let file: SelectedModelsFile =
            serde_json::from_str(&data).with_context(|| "parsing selected_models.json")?;
        Ok(file.models)
    }

    pub fn save(&self, models: &[SelectedModel]) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = SelectedModelsFile {
            models: models.to_vec(),
        };
        let data = serde_json::to_string_pretty(&file)?;
        std::fs::write(&self.path, data)?;
        Ok(())
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
    fn fresh_store_is_uninitialized_and_loads_empty() {
        let dir = TempDir::new("selected-fresh");
        let store = SelectedModelsStore::with_dir(dir.path.clone());
        assert!(!store.is_initialized());
        assert!(store.load().unwrap().is_empty());
    }

    #[test]
    fn save_then_load_round_trip() {
        let dir = TempDir::new("selected-roundtrip");
        let store = SelectedModelsStore::with_dir(dir.path.clone());
        let entries = vec![
            SelectedModel {
                connection_id: "openai".to_string(),
                model_id: "gpt-4o".to_string(),
            },
            SelectedModel {
                connection_id: "anthropic".to_string(),
                model_id: "claude-opus-4-7".to_string(),
            },
        ];
        store.save(&entries).unwrap();
        assert!(store.is_initialized());
        assert_eq!(store.load().unwrap(), entries);
    }

    #[test]
    fn save_empty_list_marks_initialized() {
        let dir = TempDir::new("selected-empty");
        let store = SelectedModelsStore::with_dir(dir.path.clone());
        store.save(&[]).unwrap();
        assert!(store.is_initialized());
        assert!(store.load().unwrap().is_empty());
    }
}
