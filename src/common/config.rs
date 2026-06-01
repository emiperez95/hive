//! Global hive configuration — user-tunable defaults stored in `~/.hive/config.toml`.
//!
//! Separate from the project registry (`projects.toml`); this holds defaults applied
//! when creating new projects (where they live, their startup command, their emoji).
//! Every field has a built-in fallback so a fresh install works with no config file
//! and never assumes any particular home-directory layout.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Built-in default base directory for new projects (used when unconfigured).
const DEFAULT_PROJECTS_DIR: &str = "~/projects";
/// Built-in default startup command for new projects.
const DEFAULT_STARTUP_COMMAND: &str = "claude -c";
/// Built-in default emoji for new projects.
const DEFAULT_EMOJI: &str = "📁";

/// Valid dotted config keys, for error messages and `list`.
pub const CONFIG_KEYS: &[&str] = &[
    "defaults.projects_dir",
    "defaults.startup_command",
    "defaults.emoji",
];

/// Defaults applied when creating a new project.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Defaults {
    /// Base directory new projects are created under (a new project `key` becomes
    /// `{projects_dir}/{key}`). Supports a leading `~`.
    #[serde(default = "default_projects_dir")]
    pub projects_dir: String,
    /// Command run when a new project's session starts.
    #[serde(default = "default_startup_command")]
    pub startup_command: String,
    /// Emoji used for new projects when none is given.
    #[serde(default = "default_emoji")]
    pub emoji: String,
}

fn default_projects_dir() -> String {
    DEFAULT_PROJECTS_DIR.to_string()
}
fn default_startup_command() -> String {
    DEFAULT_STARTUP_COMMAND.to_string()
}
fn default_emoji() -> String {
    DEFAULT_EMOJI.to_string()
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            projects_dir: default_projects_dir(),
            startup_command: default_startup_command(),
            emoji: default_emoji(),
        }
    }
}

/// Top-level hive configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HiveConfig {
    #[serde(default)]
    pub defaults: Defaults,
}

impl HiveConfig {
    /// Path to the config file (`~/.hive/config.toml`).
    pub fn config_path() -> PathBuf {
        crate::common::persistence::hive_home()
            .unwrap_or_else(|| PathBuf::from(".hive"))
            .join("config.toml")
    }

    /// Load config from disk. Missing file or parse error → built-in defaults
    /// (so hive always has a usable config and never assumes a personal layout).
    pub fn load() -> Self {
        let path = Self::config_path();
        let Ok(content) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        toml::from_str(&content).unwrap_or_default()
    }

    /// Save config to disk atomically (write .tmp, rename).
    pub fn save(&self) -> Result<()> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).context("creating config dir")?;
        }
        let toml_str = toml::to_string_pretty(self).context("serializing config")?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, toml_str).context("writing config")?;
        std::fs::rename(&tmp, &path).context("renaming config")?;
        Ok(())
    }

    /// Get a config value by dotted key (e.g. "defaults.projects_dir").
    pub fn get(&self, key: &str) -> Option<String> {
        match key {
            "defaults.projects_dir" => Some(self.defaults.projects_dir.clone()),
            "defaults.startup_command" => Some(self.defaults.startup_command.clone()),
            "defaults.emoji" => Some(self.defaults.emoji.clone()),
            _ => None,
        }
    }

    /// Set a config value by dotted key. Returns an error for unknown keys.
    pub fn set(&mut self, key: &str, value: &str) -> Result<()> {
        match key {
            "defaults.projects_dir" => self.defaults.projects_dir = value.to_string(),
            "defaults.startup_command" => self.defaults.startup_command = value.to_string(),
            "defaults.emoji" => self.defaults.emoji = value.to_string(),
            _ => anyhow::bail!(
                "unknown config key '{}' (valid keys: {})",
                key,
                CONFIG_KEYS.join(", ")
            ),
        }
        Ok(())
    }

    /// All (key, value) pairs in stable order, for `hive config list`.
    pub fn entries(&self) -> Vec<(&'static str, String)> {
        vec![
            ("defaults.projects_dir", self.defaults.projects_dir.clone()),
            ("defaults.startup_command", self.defaults.startup_command.clone()),
            ("defaults.emoji", self.defaults.emoji.clone()),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_generic() {
        let d = Defaults::default();
        assert_eq!(d.projects_dir, "~/projects");
        assert_eq!(d.startup_command, "claude -c");
        assert_eq!(d.emoji, "📁");
        // Must never assume the author's personal layout.
        assert!(!d.projects_dir.contains("00-Personal"));
    }

    #[test]
    fn empty_config_uses_defaults() {
        let cfg: HiveConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.defaults.projects_dir, "~/projects");
        assert_eq!(cfg.defaults.startup_command, "claude -c");
    }

    #[test]
    fn partial_config_fills_missing_with_defaults() {
        let cfg: HiveConfig =
            toml::from_str("[defaults]\nprojects_dir = \"~/code\"\n").unwrap();
        assert_eq!(cfg.defaults.projects_dir, "~/code");
        assert_eq!(cfg.defaults.startup_command, "claude -c");
        assert_eq!(cfg.defaults.emoji, "📁");
    }

    #[test]
    fn get_set_roundtrip() {
        let mut cfg = HiveConfig::default();
        assert_eq!(cfg.get("defaults.projects_dir").as_deref(), Some("~/projects"));
        cfg.set("defaults.projects_dir", "~/work").unwrap();
        assert_eq!(cfg.get("defaults.projects_dir").as_deref(), Some("~/work"));
    }

    #[test]
    fn unknown_key_errors() {
        let mut cfg = HiveConfig::default();
        assert!(cfg.set("defaults.nope", "x").is_err());
        assert!(cfg.get("defaults.nope").is_none());
    }

    #[test]
    fn entries_cover_all_keys() {
        let cfg = HiveConfig::default();
        let keys: Vec<&str> = cfg.entries().iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, CONFIG_KEYS);
    }
}
