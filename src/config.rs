use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::folder::{clean_path, display_path};

pub const ORIGINAL_CLAUDE_PROFILE: &str = "__original_claude__";

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fable_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opus_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sonnet_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub haiku_model: Option<String>,
}

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq, Eq)]
pub struct Config {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_profile: Option<String>,
    #[serde(default)]
    pub last_selected_profile: Option<String>,
    #[serde(default)]
    pub profile_history: Vec<String>,
    #[serde(default)]
    pub recent_folders: Vec<PathBuf>,
    #[serde(default)]
    pub last_selected_folder: Option<PathBuf>,
    #[serde(default)]
    pub auto_mode: bool,
    #[serde(default)]
    pub profiles: HashMap<String, Profile>,
}

pub fn config_path() -> PathBuf {
    dirs::home_dir().expect("no home dir").join(".ccat.json")
}

impl Config {
    pub fn load_from_path(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file at {:?}", path))?;
        if content.trim().is_empty() {
            return Ok(Self::default());
        }
        let mut config: Self = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse JSON config at {:?}", path))?;

        // Clean any existing UNC prefixes from loaded config
        config.recent_folders = config
            .recent_folders
            .into_iter()
            .map(|f| PathBuf::from(display_path(&f)))
            .collect();
        if let Some(f) = config.last_selected_folder {
            config.last_selected_folder = Some(PathBuf::from(display_path(&f)));
        }

        Ok(config)
    }

    pub fn save_to_path(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self)
            .context("Failed to serialize config to JSON")?;
        fs::write(path, json)
            .with_context(|| format!("Failed to write config file to {:?}", path))?;
        Ok(())
    }

    pub fn load() -> Result<Self> {
        Self::load_from_path(&config_path())
    }

    pub fn save(&self) -> Result<()> {
        Self::save_to_path(self, &config_path())
    }

    pub fn insert_profile(&mut self, name: String, profile: Profile) {
        self.profiles.insert(name, profile);
    }

    pub fn delete_profile(&mut self, name: &str) -> bool {
        let removed = self.profiles.remove(name).is_some();
        if removed {
            if let Some(default_name) = &self.default_profile {
                if default_name == name {
                    self.default_profile = None;
                }
            }
            if let Some(last_name) = &self.last_selected_profile {
                if last_name == name {
                    self.last_selected_profile = None;
                }
            }
            self.profile_history.retain(|n| n != name);
        }
        removed
    }

    pub fn set_default(&mut self, name: Option<String>) {
        self.default_profile = name;
    }

    /// Record a profile being selected/used, putting it at the front of history
    pub fn record_profile_used(&mut self, profile_name: &str) {
        self.last_selected_profile = Some(profile_name.to_string());
        self.profile_history.retain(|n| n != profile_name);
        self.profile_history.insert(0, profile_name.to_string());
    }

    /// Record a folder being selected/used, putting it at the front of recent folders
    pub fn record_folder_used(&mut self, folder: PathBuf) {
        let cleaned = PathBuf::from(display_path(&clean_path(&folder)));
        self.last_selected_folder = Some(cleaned.clone());
        self.recent_folders.retain(|f| f != &cleaned);
        self.recent_folders.insert(0, cleaned);
        if self.recent_folders.len() > 50 {
            self.recent_folders.truncate(50);
        }
    }

    /// Get sorted profile names based on recent usage history
    pub fn sorted_profiles_by_history(&self) -> Vec<String> {
        let mut result = Vec::new();
        let mut seen = std::collections::HashSet::new();

        // 1. First add from history
        for name in &self.profile_history {
            if self.profiles.contains_key(name) && seen.insert(name.clone()) {
                result.push(name.clone());
            }
        }

        // 2. Add remaining profiles alphabetically
        let mut remaining: Vec<String> = self
            .profiles
            .keys()
            .filter(|k| !seen.contains(*k))
            .cloned()
            .collect();
        remaining.sort();

        for name in remaining {
            result.push(name);
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_empty_config_defaults() {
        let config = Config::default();
        assert!(config.default_profile.is_none());
        assert!(config.profiles.is_empty());
        assert!(config.recent_folders.is_empty());
        assert!(!config.auto_mode);
    }

    #[test]
    fn test_save_and_load_config_with_history() {
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path();

        let mut config = Config::default();
        config.auto_mode = true;
        config.insert_profile("staging".to_string(), Profile::default());
        config.insert_profile("dev".to_string(), Profile::default());
        config.record_profile_used("dev");
        config.record_folder_used(PathBuf::from("."));

        config.save_to_path(path).unwrap();

        let loaded = Config::load_from_path(path).unwrap();
        assert!(loaded.auto_mode);
        assert_eq!(loaded.last_selected_profile, Some("dev".to_string()));
        assert_eq!(loaded.profile_history.first(), Some(&"dev".to_string()));
        assert_eq!(loaded.sorted_profiles_by_history(), vec!["dev", "staging"]);
        assert_eq!(loaded.recent_folders.len(), 1);
    }

    #[test]
    fn test_delete_profile_cleans_history() {
        let mut config = Config::default();
        config.insert_profile("dev".to_string(), Profile::default());
        config.record_profile_used("dev");

        assert!(config.delete_profile("dev"));
        assert_eq!(config.last_selected_profile, None);
        assert!(config.profile_history.is_empty());
    }
}
