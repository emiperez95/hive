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

/// File patterns for worktree setup
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FilePatterns {
    #[serde(default)]
    pub copy: Vec<String>,
    #[serde(default)]
    pub symlink: Vec<String>,
}

/// A single project configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectConfig {
    /// Emoji identifier for session names
    pub emoji: String,
    /// Project root path (supports ~ expansion)
    pub project_root: String,
    /// Display name override (defaults to table key)
    #[serde(default)]
    pub display_name: Option<String>,
    /// Command to run on session startup
    #[serde(default)]
    pub startup_command: Option<String>,
    /// Directory for worktrees (Phase 3)
    #[serde(default)]
    pub worktrees_dir: Option<String>,
    /// Default git base branch for worktrees (Phase 3)
    #[serde(default)]
    pub default_base_branch: Option<String>,
    /// Worktree types (Phase 3)
    #[serde(default)]
    pub worktree_types: Vec<String>,
    /// Package manager (npm, pnpm, yarn, etc.)
    #[serde(default)]
    pub package_manager: Option<String>,
    /// Port configuration (Phase 3)
    #[serde(default)]
    pub ports: PortConfig,
    /// Database configuration (Phase 3)
    #[serde(default)]
    pub database: DatabaseConfig,
    /// File patterns for worktree setup (Phase 3)
    #[serde(default)]
    pub files: FilePatterns,
}

/// Root configuration containing all projects
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectRegistry {
    #[serde(default)]
    pub projects: HashMap<String, ProjectConfig>,
}

/// Get config directory for hive
fn config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|p| p.join("hive"))
}

/// Get the path to projects.toml
pub fn get_projects_file_path() -> Option<PathBuf> {
    config_dir().map(|p| p.join("projects.toml"))
}

/// Expand ~ to home directory in a path string
fn expand_tilde(path: &str) -> PathBuf {
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
        toml::from_str(&content).unwrap_or_default()
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

    /// List all project session names
    pub fn list_session_names(&self) -> Vec<String> {
        self.projects
            .iter()
            .map(|(key, config)| Self::session_name(key, config))
            .collect()
    }
}

/// Check if a session name has a matching project config (replaces has_sesh_config)
pub fn has_project_config(session_name: &str) -> bool {
    ProjectRegistry::load().has_project(session_name)
}

/// Connect/create a tmux session for a project (replaces sesh_connect)
pub fn connect_project(session_name: &str) -> bool {
    let registry = ProjectRegistry::load();
    let Some((key, config)) = registry.find_by_session_name(session_name) else {
        return false;
    };

    let sess_name = ProjectRegistry::session_name(key, config);
    let root = expand_tilde(&config.project_root);

    // Check if session already exists
    let exists = Command::new("tmux")
        .args(["has-session", "-t", &sess_name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if !exists {
        // Create new detached session at project root
        let success = Command::new("tmux")
            .args([
                "new-session",
                "-d",
                "-s",
                &sess_name,
                "-c",
                &root.to_string_lossy(),
            ])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);

        if !success {
            return false;
        }

        // Run startup command if configured
        if let Some(ref cmd) = config.startup_command {
            let _ = Command::new("tmux")
                .args(["send-keys", "-t", &sess_name, cmd, "Enter"])
                .output();
        }
    }

    true
}

/// List all project session names (replaces list_sesh_projects)
pub fn list_project_names() -> Vec<String> {
    ProjectRegistry::load().list_session_names()
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
}
