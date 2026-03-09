use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionProfile {
    pub id: String,
    pub name: String,
    pub ws_url: String,
    #[serde(default = "default_ws_subject")]
    pub ws_subject: String,
}

fn default_ws_subject() -> String {
    "desktop-tui".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ProfilesFile {
    profiles: Vec<ConnectionProfile>,
}

pub struct ProfileStore {
    path: PathBuf,
}

impl ProfileStore {
    pub fn new() -> Self {
        let config_dir = dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("adelie-gtk");
        Self {
            path: config_dir.join("profiles.json"),
        }
    }

    pub fn load(&self) -> Result<Vec<ConnectionProfile>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let data = std::fs::read_to_string(&self.path)
            .with_context(|| format!("reading {}", self.path.display()))?;
        let file: ProfilesFile =
            serde_json::from_str(&data).with_context(|| "parsing profiles.json")?;
        Ok(file.profiles)
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
