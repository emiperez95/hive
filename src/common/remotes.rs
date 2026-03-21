//! Remote machine registry — TOML-based configuration for SSH remotes.
//!
//! Follows the same pattern as ProjectRegistry: load/save from ~/.hive/remotes.toml.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Configuration for a single remote machine
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteConfig {
    /// SSH host name (from ~/.ssh/config)
    pub ssh_host: String,
    /// Display label in the TUI
    pub label: String,
    /// Emoji badge for remote sessions
    pub emoji: String,
}

/// Root configuration containing all remotes
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RemoteRegistry {
    #[serde(default)]
    pub remotes: HashMap<String, RemoteConfig>,
}

/// Get the path to remotes.toml
pub fn get_remotes_file_path() -> Option<PathBuf> {
    crate::common::persistence::hive_home().map(|p| p.join("remotes.toml"))
}

impl RemoteRegistry {
    /// Load the remote registry from disk. Returns empty registry on any error.
    pub fn load() -> Self {
        let Some(path) = get_remotes_file_path() else {
            return Self::default();
        };
        let Ok(content) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        toml::from_str(&content).unwrap_or_else(|e| {
            eprintln!("Warning: failed to parse {}: {}", path.display(), e);
            Self::default()
        })
    }

    /// Save the registry to disk atomically (write .tmp, rename).
    pub fn save(&self) -> anyhow::Result<()> {
        let path = get_remotes_file_path()
            .ok_or_else(|| anyhow::anyhow!("Cannot determine config directory"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(self)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, content)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_registry() {
        let registry: RemoteRegistry = toml::from_str("").unwrap();
        assert!(registry.remotes.is_empty());
    }

    #[test]
    fn test_parse_remotes_toml() {
        let toml_str = r#"
[remotes.wpc]
ssh_host = "wpc"
label = "Work PC"
emoji = "🖥️"

[remotes.server]
ssh_host = "dev-server"
label = "Dev Server"
emoji = "🏗️"
"#;
        let registry: RemoteRegistry = toml::from_str(toml_str).unwrap();
        assert_eq!(registry.remotes.len(), 2);
        assert_eq!(registry.remotes["wpc"].ssh_host, "wpc");
        assert_eq!(registry.remotes["wpc"].label, "Work PC");
        assert_eq!(registry.remotes["server"].emoji, "🏗️");
    }

    #[test]
    fn test_serialize_roundtrip() {
        let mut registry = RemoteRegistry::default();
        registry.remotes.insert(
            "test".to_string(),
            RemoteConfig {
                ssh_host: "test-host".to_string(),
                label: "Test".to_string(),
                emoji: "🧪".to_string(),
            },
        );
        let serialized = toml::to_string_pretty(&registry).unwrap();
        let deserialized: RemoteRegistry = toml::from_str(&serialized).unwrap();
        assert_eq!(deserialized.remotes.len(), 1);
        assert_eq!(deserialized.remotes["test"].ssh_host, "test-host");
    }
}
