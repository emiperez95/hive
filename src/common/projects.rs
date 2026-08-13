//! Project registry — TOML-based project configuration.

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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    /// Archived: hidden from the picker and default `project list`
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub archived: bool,
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
    #[allow(dead_code)] // registry API, retained + tested; no live caller since classic went
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

    /// List (session_name, archived) for all projects.
    #[allow(dead_code)] // registry API, retained + tested; no live caller since classic went
    pub fn list_session_names_with_archived(&self) -> Vec<(String, bool)> {
        self.projects
            .iter()
            .map(|(key, config)| (Self::session_name(key, config), config.archived))
            .collect()
    }

    /// Set the archived flag by project key. Returns false if the key is not found.
    pub fn set_archived(&mut self, key: &str, archived: bool) -> bool {
        match self.projects.get_mut(key) {
            Some(config) => {
                config.archived = archived;
                true
            }
            None => false,
        }
    }

    /// Clear the archived flag for a project key OR a worktree key
    /// (`project/branch` — a worktree belongs to its project). Returns true only
    /// when something actually changed, so callers can skip the write on the
    /// common path.
    pub fn unarchive(&mut self, key: &str) -> bool {
        let key = key.split('/').next().unwrap_or(key);
        match self.projects.get_mut(key) {
            Some(config) if config.archived => {
                config.archived = false;
                true
            }
            _ => false,
        }
    }
}

/// Starting work in a project puts it back in play: clear its `archived` flag.
/// Mirrors the two rules hive already follows — opening an archived CONVERSATION
/// unarchives it, and switching to a skipped session un-skips it — so a project
/// can't stay hidden from Browse while you're actively working in it. Accepts a
/// worktree key (`project/branch`) too. Best-effort and silent: unknown key or
/// already-active project writes nothing.
pub fn activate_project(key: &str) {
    let mut registry = ProjectRegistry::load();
    if registry.unarchive(key) {
        let _ = registry.save();
    }
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
    // Exact match: a bare `-t` falls back to prefix matching, so "📊 Avateen" would
    // report "already exists" on the strength of "📊 Avateen Hub" and never create
    // the session (see `tmux::exact`).
    let exists = Command::new("tmux")
        .args([
            "has-session",
            "-t",
            &crate::common::tmux::exact(session_name),
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if !exists {
        let mut cmd = Command::new("tmux");
        cmd.args(["new-session", "-d"]);
        let env_strings: Vec<String> = env.iter().map(|(k, v)| format!("{}={}", k, v)).collect();
        for es in &env_strings {
            cmd.arg("-e").arg(es);
        }
        cmd.args(["-s", session_name, "-c", cwd]);
        let success = cmd.output().map(|o| o.status.success()).unwrap_or(false);

        if !success {
            return false;
        }

        if let Some(startup) = startup_cmd {
            // Active-pane target, not the bare session target: send-keys can't
            // resolve `=name` (see `tmux::exact_active_pane`), which left every
            // freshly-created session sitting at a shell with no startup command.
            let _ = Command::new("tmux")
                .args([
                    "send-keys",
                    "-t",
                    &crate::common::tmux::exact_active_pane(session_name),
                    startup,
                    "Enter",
                ])
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
            archived: false,
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
            archived: false,
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
        assert!(registry.list_session_names_with_archived().is_empty());
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
        let names: Vec<String> = registry
            .list_session_names_with_archived()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
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
            archived: false,
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
            archived: false,
        };
        registry.add_project("test".to_string(), config);
        assert!(registry.remove_project("test"));
        assert!(!registry.remove_project("test"));
        assert!(registry.projects.is_empty());
    }

    #[test]
    fn test_archived_omitted_when_false() {
        let toml_str = r#"
[projects.hive]
emoji = "🐝"
project_root = "~/projects/hive"
"#;
        let registry: ProjectRegistry = toml::from_str(toml_str).unwrap();
        assert!(!registry.projects["hive"].archived);
        // Round-trips without emitting an `archived` key.
        let out = toml::to_string_pretty(&registry).unwrap();
        assert!(!out.contains("archived"));
    }

    #[test]
    fn test_archived_roundtrip_when_true() {
        let toml_str = r#"
[projects.hive]
emoji = "🐝"
project_root = "~/projects/hive"
archived = true
"#;
        let registry: ProjectRegistry = toml::from_str(toml_str).unwrap();
        assert!(registry.projects["hive"].archived);
        let out = toml::to_string_pretty(&registry).unwrap();
        assert!(out.contains("archived = true"));
    }

    #[test]
    fn test_set_archived() {
        let toml_str = r#"
[projects.hive]
emoji = "🐝"
project_root = "~/projects/hive"
"#;
        let mut registry: ProjectRegistry = toml::from_str(toml_str).unwrap();
        assert!(registry.set_archived("hive", true));
        assert!(registry.projects["hive"].archived);
        assert!(!registry.set_archived("nonexistent", true));
    }

    #[test]
    fn test_unarchive_reports_only_real_changes() {
        // `unarchive` is what starting work in a project calls, on every new
        // conversation / resume / connect — so it must report "nothing changed" for
        // the common case (already active, or unknown key) to avoid a pointless
        // rewrite of projects.toml, and must accept a worktree key.
        let toml_str = r#"
[projects.hive]
emoji = "🐝"
project_root = "~/projects/hive"
archived = true
"#;
        let mut registry: ProjectRegistry = toml::from_str(toml_str).unwrap();
        assert!(!registry.unarchive("nonexistent"), "unknown key: no change");

        // A worktree key resolves to its project.
        assert!(registry.unarchive("hive/CSD-1"), "worktree key unarchives");
        assert!(!registry.projects["hive"].archived);

        // Already active → no change, so no write.
        assert!(!registry.unarchive("hive"), "already active: no change");
    }

    #[test]
    fn test_list_session_names_with_archived() {
        let toml_str = r#"
[projects.hive]
emoji = "🐝"
project_root = "~/projects/hive"

[projects.old]
emoji = "📦"
project_root = "~/projects/old"
archived = true
"#;
        let registry: ProjectRegistry = toml::from_str(toml_str).unwrap();
        let names = registry.list_session_names_with_archived();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&("🐝 hive".to_string(), false)));
        assert!(names.contains(&("📦 old".to_string(), true)));
    }
}
