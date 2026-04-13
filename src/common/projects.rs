//! Project registry — TOML-based project configuration replacing sesh dependency.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

/// Port configuration for a project
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PortConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub base_port: u16,
    #[serde(default = "default_port_increment")]
    pub increment: u16,
}

impl PortConfig {
    pub fn is_default(&self) -> bool {
        !self.enabled && self.base_port == 0 && self.increment == 0
    }
}

fn default_port_increment() -> u16 {
    1
}

/// Database configuration for a project
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DatabaseConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub prefix: Option<String>,
}

impl DatabaseConfig {
    pub fn is_default(&self) -> bool {
        !self.enabled && self.prefix.is_none()
    }
}

/// File patterns for worktree setup
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FilePatterns {
    #[serde(default)]
    pub copy: Vec<String>,
    #[serde(default)]
    pub symlink: Vec<String>,
}

impl FilePatterns {
    pub fn is_default(&self) -> bool {
        self.copy.is_empty() && self.symlink.is_empty()
    }
}

/// A single project configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectConfig {
    /// Emoji identifier for session names
    pub emoji: String,
    /// Project root path (supports ~ expansion)
    pub project_root: String,
    /// Display name override (defaults to table key)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Command to run on session startup
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_command: Option<String>,
    /// Directory for worktrees
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktrees_dir: Option<String>,
    /// Default git base branch for worktrees
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_base_branch: Option<String>,
    /// Worktree types
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub worktree_types: Vec<String>,
    /// Package manager (npm, pnpm, yarn, etc.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package_manager: Option<String>,
    /// Port configuration
    #[serde(default, skip_serializing_if = "PortConfig::is_default")]
    pub ports: PortConfig,
    /// Database configuration
    #[serde(default, skip_serializing_if = "DatabaseConfig::is_default")]
    pub database: DatabaseConfig,
    /// File patterns for worktree setup
    #[serde(default, skip_serializing_if = "FilePatterns::is_default")]
    pub files: FilePatterns,
    /// Custom hooks directory (defaults to ~/.hive/projects/{key}/hooks/)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks_dir: Option<String>,
    /// Claude auth profile name (sets CLAUDE_CONFIG_DIR to ~/.claude-{name})
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_profile: Option<String>,
}

impl ProjectConfig {
    /// Build env vars to inject into tmux sessions for this project.
    /// Currently only sets `CLAUDE_CONFIG_DIR` when `auth_profile` is set.
    pub fn tmux_env(&self) -> Vec<(String, String)> {
        let mut env = Vec::new();
        if let Some(profile) = &self.auth_profile {
            if let Some(home) = dirs::home_dir() {
                let dir = home.join(format!(".claude-{}", profile));
                env.push((
                    "CLAUDE_CONFIG_DIR".into(),
                    dir.to_string_lossy().into_owned(),
                ));
            }
        }
        env
    }
}

/// Root configuration containing all projects
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectRegistry {
    /// Global default root for worktrees: {worktrees_root}/{project_key}/
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktrees_root: Option<String>,
    #[serde(default)]
    pub projects: HashMap<String, ProjectConfig>,
}

/// Get the path to projects.toml
pub fn get_projects_file_path() -> Option<PathBuf> {
    crate::common::persistence::hive_home().map(|p| p.join("projects.toml"))
}

/// Expand ~ to home directory in a path string
pub fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

impl ProjectRegistry {
    /// Load the project registry from disk. Returns empty registry on any error.
    pub fn load() -> Self {
        let Some(path) = get_projects_file_path() else {
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
        let path = get_projects_file_path()
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

    /// Add a project to the registry.
    pub fn add_project(&mut self, key: String, config: ProjectConfig) {
        self.projects.insert(key, config);
    }

    /// Remove a project from the registry. Returns true if it existed.
    pub fn remove_project(&mut self, key: &str) -> bool {
        self.projects.remove(key).is_some()
    }

    /// Derive the tmux session name for a standalone project
    pub fn session_name(key: &str, config: &ProjectConfig) -> String {
        let name = config.display_name.as_deref().unwrap_or(key);
        format!("{} {}", config.emoji, name)
    }

    /// Check if any project matches the given session name
    pub fn has_project(&self, session_name: &str) -> bool {
        self.projects
            .iter()
            .any(|(key, config)| Self::session_name(key, config) == session_name)
    }

    /// Find a project by its derived session name. Returns (key, config).
    pub fn find_by_session_name(&self, session_name: &str) -> Option<(&str, &ProjectConfig)> {
        self.projects
            .iter()
            .find(|(key, config)| Self::session_name(key, config) == session_name)
            .map(|(key, config)| (key.as_str(), config))
    }

    /// Resolve the worktrees directory for a project.
    /// 1. project.worktrees_dir (explicit override)
    /// 2. registry.worktrees_root / project_key (global default)
    /// 3. None → error
    pub fn resolve_worktrees_dir(&self, key: &str, config: &ProjectConfig) -> Option<PathBuf> {
        if let Some(ref dir) = config.worktrees_dir {
            return Some(expand_tilde(dir));
        }
        if let Some(ref root) = self.worktrees_root {
            return Some(expand_tilde(root).join(key));
        }
        None
    }

    /// List all project session names
    pub fn list_session_names(&self) -> Vec<String> {
        self.projects
            .iter()
            .map(|(key, config)| Self::session_name(key, config))
            .collect()
    }
}

/// Check if a session name has a matching project config or worktree entry
pub fn has_project_config(session_name: &str) -> bool {
    ProjectRegistry::load().has_project(session_name)
        || crate::common::worktree::find_worktree_by_session_name(session_name).is_some()
}

/// Connect/create a tmux session for a project or worktree.
/// Tries project registry first, then worktrees.json.
pub fn connect_session(session_name: &str) -> bool {
    if connect_project(session_name) {
        return true;
    }
    crate::common::worktree::connect_worktree(session_name)
}

/// Ensure a tmux session exists, creating it at the given path if needed.
/// Optionally runs a startup command in the new session.
/// `env` is passed via `tmux new-session -e KEY=VAL` so the initial shell inherits it.
/// Returns true on success, false on failure.
pub fn ensure_tmux_session(
    session_name: &str,
    cwd: &str,
    startup_cmd: Option<&str>,
    env: &[(String, String)],
) -> bool {
    let exists = Command::new("tmux")
        .args(["has-session", "-t", session_name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if !exists {
        let mut cmd = Command::new("tmux");
        cmd.args(["new-session", "-d"]);
        let env_strings: Vec<String> =
            env.iter().map(|(k, v)| format!("{}={}", k, v)).collect();
        for es in &env_strings {
            cmd.arg("-e").arg(es);
        }
        cmd.args(["-s", session_name, "-c", cwd]);
        let success = cmd.output().map(|o| o.status.success()).unwrap_or(false);

        if !success {
            return false;
        }

        if let Some(startup) = startup_cmd {
            let _ = Command::new("tmux")
                .args(["send-keys", "-t", session_name, startup, "Enter"])
                .output();
        }
    }

    true
}

/// Connect/create a tmux session for a project (replaces sesh_connect)
pub fn connect_project(session_name: &str) -> bool {
    let registry = ProjectRegistry::load();
    let Some((key, config)) = registry.find_by_session_name(session_name) else {
        return false;
    };

    let sess_name = ProjectRegistry::session_name(key, config);
    let root = expand_tilde(&config.project_root);
    ensure_tmux_session(
        &sess_name,
        &root.to_string_lossy(),
        config.startup_command.as_deref(),
        &config.tmux_env(),
    )
}

/// Sesh session entry for parsing sesh.toml
#[derive(Debug, Deserialize)]
struct SeshSession {
    name: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    startup_command: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SeshConfig {
    #[serde(default)]
    session: Vec<SeshSession>,
}

/// Derive a registry key from a sesh session name.
/// Strips leading emoji, lowercases, replaces spaces with hyphens.
fn derive_key_from_name(name: &str) -> String {
    // Strip leading emoji: skip chars until we hit an ASCII alphanumeric
    let stripped = name
        .trim()
        .trim_start_matches(|c: char| !c.is_ascii_alphanumeric())
        .trim();

    if stripped.is_empty() {
        // Fallback: use the whole name, cleaned up
        return name
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == ' ' || *c == '-')
            .collect::<String>()
            .trim()
            .to_lowercase()
            .replace(' ', "-");
    }

    stripped.to_lowercase().replace(' ', "-")
}

/// Extract the leading emoji from a session name.
fn extract_emoji(name: &str) -> Option<String> {
    let trimmed = name.trim();
    let rest = trimmed
        .trim_start_matches(|c: char| !c.is_ascii_alphanumeric())
        .trim_start();
    let emoji_part = trimmed[..trimmed.len() - rest.len()].trim();
    if emoji_part.is_empty() {
        None
    } else {
        Some(emoji_part.to_string())
    }
}

/// Parse a sesh.toml file into project entries.
pub fn parse_sesh_toml(path: &std::path::Path) -> anyhow::Result<Vec<(String, ProjectConfig)>> {
    let content = std::fs::read_to_string(path)?;
    let sesh: SeshConfig = toml::from_str(&content)?;

    let mut results = Vec::new();
    for session in sesh.session {
        let key = derive_key_from_name(&session.name);
        if key.is_empty() {
            continue;
        }
        let emoji = extract_emoji(&session.name).unwrap_or_else(|| "📁".to_string());
        let project_root = session.path.unwrap_or_else(|| "~".to_string());

        // Derive display_name: the text part after the emoji
        let display_name = session
            .name
            .trim()
            .trim_start_matches(|c: char| !c.is_ascii_alphanumeric())
            .trim()
            .to_string();
        let display_name = if display_name == key {
            None
        } else {
            Some(display_name)
        };

        results.push((
            key,
            ProjectConfig {
                emoji,
                project_root,
                display_name,
                startup_command: session.startup_command,
                worktrees_dir: None,
                default_base_branch: None,
                worktree_types: Vec::new(),
                package_manager: None,
                ports: PortConfig::default(),
                database: DatabaseConfig::default(),
                files: FilePatterns::default(),
                hooks_dir: None,
                auth_profile: None,
            },
        ));
    }

    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_name_default() {
        let config = ProjectConfig {
            emoji: "🐝".to_string(),
            project_root: "~/projects/hive".to_string(),
            display_name: None,
            startup_command: None,
            worktrees_dir: None,
            default_base_branch: None,
            worktree_types: Vec::new(),
            package_manager: None,
            ports: PortConfig::default(),
            database: DatabaseConfig::default(),
            files: FilePatterns::default(),
            hooks_dir: None,
            auth_profile: None,
        };
        assert_eq!(ProjectRegistry::session_name("hive", &config), "🐝 hive");
    }

    #[test]
    fn test_session_name_display_name() {
        let config = ProjectConfig {
            emoji: "🌐".to_string(),
            project_root: "~/projects/my-app".to_string(),
            display_name: Some("My App".to_string()),
            startup_command: None,
            worktrees_dir: None,
            default_base_branch: None,
            worktree_types: Vec::new(),
            package_manager: None,
            ports: PortConfig::default(),
            database: DatabaseConfig::default(),
            files: FilePatterns::default(),
            hooks_dir: None,
            auth_profile: None,
        };
        assert_eq!(
            ProjectRegistry::session_name("my-app", &config),
            "🌐 My App"
        );
    }

    #[test]
    fn test_expand_tilde() {
        let result = expand_tilde("~/projects/hive");
        assert!(result.to_string_lossy().contains("projects/hive"));
        assert!(!result.to_string_lossy().starts_with("~"));
    }

    #[test]
    fn test_expand_tilde_absolute() {
        let result = expand_tilde("/usr/local/bin");
        assert_eq!(result, PathBuf::from("/usr/local/bin"));
    }

    #[test]
    fn test_parse_minimal_toml() {
        let toml_str = r#"
[projects.hive]
emoji = "🐝"
project_root = "~/projects/hive"
"#;
        let registry: ProjectRegistry = toml::from_str(toml_str).unwrap();
        assert_eq!(registry.projects.len(), 1);
        assert!(registry.projects.contains_key("hive"));
        assert_eq!(registry.projects["hive"].emoji, "🐝");
        assert!(registry.projects["hive"].startup_command.is_none());
    }

    #[test]
    fn test_parse_full_toml() {
        let toml_str = r#"
[projects.my-app]
emoji = "🌐"
display_name = "My App"
project_root = "~/projects/my-app"
default_base_branch = "main"
package_manager = "pnpm"
startup_command = "claude"

[projects.my-app.ports]
enabled = true
base_port = 3000
increment = 1

[projects.my-app.database]
enabled = true
prefix = "myapp"

[projects.my-app.files]
copy = ["package.json"]
symlink = [".env"]
"#;
        let registry: ProjectRegistry = toml::from_str(toml_str).unwrap();
        let config = &registry.projects["my-app"];
        assert_eq!(config.display_name.as_deref(), Some("My App"));
        assert!(config.ports.enabled);
        assert_eq!(config.ports.base_port, 3000);
        assert!(config.database.enabled);
        assert_eq!(config.database.prefix.as_deref(), Some("myapp"));
        assert_eq!(config.files.copy, vec!["package.json"]);
        assert_eq!(config.files.symlink, vec![".env"]);
    }

    #[test]
    fn test_has_project() {
        let toml_str = r#"
[projects.hive]
emoji = "🐝"
project_root = "~/projects/hive"
"#;
        let registry: ProjectRegistry = toml::from_str(toml_str).unwrap();
        assert!(registry.has_project("🐝 hive"));
        assert!(!registry.has_project("hive"));
        assert!(!registry.has_project("nonexistent"));
    }

    #[test]
    fn test_find_by_session_name() {
        let toml_str = r#"
[projects.hive]
emoji = "🐝"
project_root = "~/projects/hive"
startup_command = "claude"
"#;
        let registry: ProjectRegistry = toml::from_str(toml_str).unwrap();
        let result = registry.find_by_session_name("🐝 hive");
        assert!(result.is_some());
        let (key, config) = result.unwrap();
        assert_eq!(key, "hive");
        assert_eq!(config.startup_command.as_deref(), Some("claude"));
    }

    #[test]
    fn test_find_by_session_name_not_found() {
        let toml_str = r#"
[projects.hive]
emoji = "🐝"
project_root = "~/projects/hive"
"#;
        let registry: ProjectRegistry = toml::from_str(toml_str).unwrap();
        assert!(registry.find_by_session_name("nonexistent").is_none());
    }

    #[test]
    fn test_empty_registry() {
        let registry: ProjectRegistry = toml::from_str("").unwrap();
        assert!(registry.projects.is_empty());
        assert!(!registry.has_project("anything"));
        assert!(registry.list_session_names().is_empty());
    }

    #[test]
    fn test_list_session_names() {
        let toml_str = r#"
[projects.hive]
emoji = "🐝"
project_root = "~/projects/hive"

[projects.my-app]
emoji = "🌐"
display_name = "My App"
project_root = "~/projects/my-app"
"#;
        let registry: ProjectRegistry = toml::from_str(toml_str).unwrap();
        let names = registry.list_session_names();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"🐝 hive".to_string()));
        assert!(names.contains(&"🌐 My App".to_string()));
    }

    #[test]
    fn test_port_config_defaults_absent() {
        let toml_str = r#"
[projects.test]
emoji = "📦"
project_root = "~/test"
"#;
        let registry: ProjectRegistry = toml::from_str(toml_str).unwrap();
        let config = &registry.projects["test"];
        assert!(!config.ports.enabled);
        assert_eq!(config.ports.base_port, 0);
        // When ports table is absent, Default trait gives 0; serde default fn only applies to explicit table
        assert_eq!(config.ports.increment, 0);
    }

    #[test]
    fn test_port_config_defaults_explicit() {
        let toml_str = r#"
[projects.test]
emoji = "📦"
project_root = "~/test"

[projects.test.ports]
enabled = true
base_port = 3000
"#;
        let registry: ProjectRegistry = toml::from_str(toml_str).unwrap();
        let config = &registry.projects["test"];
        assert!(config.ports.enabled);
        assert_eq!(config.ports.base_port, 3000);
        assert_eq!(config.ports.increment, 1);
    }

    #[test]
    fn test_add_project() {
        let mut registry = ProjectRegistry::default();
        let config = ProjectConfig {
            emoji: "🧪".to_string(),
            project_root: "~/test".to_string(),
            display_name: None,
            startup_command: None,
            worktrees_dir: None,
            default_base_branch: None,
            worktree_types: Vec::new(),
            package_manager: None,
            ports: PortConfig::default(),
            database: DatabaseConfig::default(),
            files: FilePatterns::default(),
            hooks_dir: None,
            auth_profile: None,
        };
        registry.add_project("test".to_string(), config);
        assert_eq!(registry.projects.len(), 1);
        assert!(registry.has_project("🧪 test"));
    }

    #[test]
    fn test_remove_project() {
        let mut registry = ProjectRegistry::default();
        let config = ProjectConfig {
            emoji: "🧪".to_string(),
            project_root: "~/test".to_string(),
            display_name: None,
            startup_command: None,
            worktrees_dir: None,
            default_base_branch: None,
            worktree_types: Vec::new(),
            package_manager: None,
            ports: PortConfig::default(),
            database: DatabaseConfig::default(),
            files: FilePatterns::default(),
            hooks_dir: None,
            auth_profile: None,
        };
        registry.add_project("test".to_string(), config);
        assert!(registry.remove_project("test"));
        assert!(!registry.remove_project("test"));
        assert!(registry.projects.is_empty());
    }

    #[test]
    fn test_derive_key_from_name() {
        assert_eq!(derive_key_from_name("🐝 hive"), "hive");
        assert_eq!(derive_key_from_name("🌐 My App"), "my-app");
        assert_eq!(
            derive_key_from_name("📁 teleport-server"),
            "teleport-server"
        );
        assert_eq!(derive_key_from_name("⚖️ Legal Advisor"), "legal-advisor");
        assert_eq!(derive_key_from_name("00-Dashboard"), "00-dashboard");
        assert_eq!(derive_key_from_name("🛠️ Nvim config"), "nvim-config");
    }

    #[test]
    fn test_extract_emoji() {
        assert_eq!(extract_emoji("🐝 hive"), Some("🐝".to_string()));
        assert_eq!(extract_emoji("⚖️ Legal Advisor"), Some("⚖️".to_string()));
        assert_eq!(extract_emoji("00-Dashboard"), None);
        assert_eq!(extract_emoji("📁 test"), Some("📁".to_string()));
    }

    #[test]
    fn test_parse_sesh_toml() {
        let toml_str = r#"
[[session]]
name = "🐝 hive"
path = "/home/user/projects/hive"
startup_command = "claude -c"

[[session]]
name = "🌐 My App"
path = "/home/user/projects/my-app"

[[session]]
name = "00-Dashboard"
path = "/home/user"
startup_command = "btm"
"#;
        let tmp = std::env::temp_dir().join("test_sesh.toml");
        std::fs::write(&tmp, toml_str).unwrap();
        let results = parse_sesh_toml(&tmp).unwrap();
        std::fs::remove_file(&tmp).ok();

        assert_eq!(results.len(), 3);

        let (key, config) = &results[0];
        assert_eq!(key, "hive");
        assert_eq!(config.emoji, "🐝");
        assert_eq!(config.project_root, "/home/user/projects/hive");
        assert_eq!(config.startup_command.as_deref(), Some("claude -c"));

        let (key, config) = &results[1];
        assert_eq!(key, "my-app");
        assert_eq!(config.emoji, "🌐");
        assert_eq!(config.display_name.as_deref(), Some("My App"));

        let (key, config) = &results[2];
        assert_eq!(key, "00-dashboard");
        assert_eq!(config.display_name.as_deref(), Some("00-Dashboard"));
    }
}
