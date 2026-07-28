//! `~/.hive/config.toml` — global hive settings.
//!
//! Currently just the `[web]` block that drives web-server autostart. Kept
//! separate from `projects.toml` (the project registry) so unrelated settings
//! don't churn that file. Loading is best-effort: a missing or malformed file
//! yields defaults (every feature off), never an error.

use serde::Deserialize;

use crate::common::persistence::hive_home;

/// The reserved tmux session name hive uses to host an autostarted web server.
/// Filtered out of every session listing (see `common::tmux`) so it never shows
/// up as a switch/cycle target in the TUI, the web dashboard, or `hive start`.
pub const WEB_SESSION: &str = "__hive_web";

fn default_web_port() -> u16 {
    8375
}

/// The `[web]` block of `config.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct WebConfig {
    /// Auto-start `hive web` (as the detached `WEB_SESSION` tmux session) the
    /// first time the TUI opens and nothing is already listening on `port`.
    /// Default **off**: it binds `0.0.0.0:<port>`, a LAN-exposed surface, so it
    /// must be opted into explicitly.
    #[serde(default)]
    pub autostart: bool,
    /// Port the autostarted server listens on (matches `hive web --port`).
    #[serde(default = "default_web_port")]
    pub port: u16,
    /// TTS read-aloud host, passed through as `--tts-host`. Off when unset.
    #[serde(default)]
    pub tts_host: Option<String>,
}

impl Default for WebConfig {
    fn default() -> Self {
        WebConfig {
            autostart: false,
            port: default_web_port(),
            tts_host: None,
        }
    }
}

/// The parsed `config.toml`. Unknown tables (e.g. a legacy `[defaults]`) are
/// ignored, so this can grow without disturbing what's already on disk.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct HiveConfig {
    #[serde(default)]
    pub web: WebConfig,
}

impl HiveConfig {
    /// Load `~/.hive/config.toml`. Missing file or parse error → defaults.
    pub fn load() -> Self {
        let Some(path) = hive_home().map(|p| p.join("config.toml")) else {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_feature_off() {
        let c = HiveConfig::default();
        assert!(!c.web.autostart, "autostart is opt-in");
        assert_eq!(c.web.port, 8375);
        assert!(c.web.tts_host.is_none());
    }

    #[test]
    fn empty_toml_parses_to_defaults() {
        let c: HiveConfig = toml::from_str("").unwrap();
        assert!(!c.web.autostart);
        assert_eq!(c.web.port, 8375);
    }

    #[test]
    fn ignores_unrelated_legacy_tables() {
        // The `[defaults]` block older installs carry must not break parsing.
        let c: HiveConfig = toml::from_str(
            "[defaults]\nprojects_dir = \"~/x\"\nemoji = \"📁\"\n\n[web]\nautostart = true\n",
        )
        .unwrap();
        assert!(c.web.autostart);
        assert_eq!(c.web.port, 8375, "port falls back to its default");
    }

    #[test]
    fn web_block_roundtrips() {
        let c: HiveConfig = toml::from_str(
            "[web]\nautostart = true\nport = 9000\ntts_host = \"http://10.0.0.2:9800\"\n",
        )
        .unwrap();
        assert!(c.web.autostart);
        assert_eq!(c.web.port, 9000);
        assert_eq!(c.web.tts_host.as_deref(), Some("http://10.0.0.2:9800"));
    }
}
