//! Worktree lifecycle management — types, state persistence, git operations, file operations,
//! hook runner with metadata protocol, and Claude memory seeding.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::common::persistence::cache_dir;
use crate::common::projects::{expand_tilde, ProjectConfig};

// ─── Types ───────────────────────────────────────────────────────────────────

/// A single worktree entry persisted in worktrees.json
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeEntry {
    pub project_key: String,
    pub branch: String,
    pub worktree_type: String,
    pub path: String,
    pub session_name: String,
    #[serde(default)]
    pub metadata: serde_json::Value,
    pub created_at: String,
}

/// Top-level state file for all worktrees
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorktreeState {
    #[serde(default)]
    pub worktrees: HashMap<String, WorktreeEntry>,
}

impl WorktreeState {
    /// Build a lookup key from project key and branch
    pub fn make_key(project: &str, branch: &str) -> String {
        format!("{}/{}", project, branch)
    }

    /// Load worktree state from disk. Returns empty state on any error.
    pub fn load() -> Self {
        let Some(path) = Self::file_path() else {
            return Self::default();
        };
        let Ok(content) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        serde_json::from_str(&content).unwrap_or_else(|e| {
            eprintln!("Warning: failed to parse {}: {}", path.display(), e);
            Self::default()
        })
    }

    /// Save worktree state to disk atomically (write .tmp, rename).
    pub fn save(&self) -> Result<()> {
        let path = Self::file_path()
            .ok_or_else(|| anyhow::anyhow!("Cannot determine cache directory"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &content)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Add a worktree entry
    pub fn add(&mut self, entry: WorktreeEntry) {
        let key = Self::make_key(&entry.project_key, &entry.branch);
        self.worktrees.insert(key, entry);
    }

    /// Remove a worktree entry by project/branch. Returns the removed entry if it existed.
    pub fn remove(&mut self, project: &str, branch: &str) -> Option<WorktreeEntry> {
        let key = Self::make_key(project, branch);
        self.worktrees.remove(&key)
    }

    /// Get a worktree entry by project/branch
    pub fn get(&self, project: &str, branch: &str) -> Option<&WorktreeEntry> {
        let key = Self::make_key(project, branch);
        self.worktrees.get(&key)
    }

    /// List all worktrees for a given project
    #[allow(dead_code)]
    pub fn list_for_project(&self, project: &str) -> Vec<&WorktreeEntry> {
        self.worktrees
            .values()
            .filter(|e| e.project_key == project)
            .collect()
    }

    fn file_path() -> Option<PathBuf> {
        cache_dir().map(|p| p.join("worktrees.json"))
    }
}

// ─── Session name builder ────────────────────────────────────────────────────

/// Build a default session name: "{emoji} {type}-{branch}"
pub fn build_session_name(config: &ProjectConfig, wt_type: &str, branch: &str) -> String {
    format!("{} {}-{}", config.emoji, wt_type, branch)
}

// ─── Worktree lookup helpers ─────────────────────────────────────────────────

/// Find a worktree entry by its session name. Returns None if not found.
pub fn find_worktree_by_session_name(session_name: &str) -> Option<WorktreeEntry> {
    WorktreeState::load()
        .worktrees
        .values()
        .find(|e| e.session_name == session_name)
        .cloned()
}

/// Connect/create a tmux session for a worktree (similar to connect_project).
/// Creates the session at the worktree path if it doesn't exist.
pub fn connect_worktree(session_name: &str) -> bool {
    let Some(entry) = find_worktree_by_session_name(session_name) else {
        return false;
    };

    // Look up startup command from parent project config
    let registry = crate::common::projects::ProjectRegistry::load();
    let startup_cmd = registry
        .projects
        .get(&entry.project_key)
        .and_then(|c| c.startup_command.as_deref());

    crate::common::projects::ensure_tmux_session(
        &entry.session_name,
        &entry.path,
        startup_cmd,
    )
}

// ─── Import ─────────────────────────────────────────────────────────────────

/// Import existing git worktrees into worktrees.json.
/// Scans `git worktree list` from project_root, matches paths against worktrees_dir,
/// and registers any that aren't already tracked.
/// Returns the list of newly imported entries.
pub fn import_worktrees(
    project_key: &str,
    config: &ProjectConfig,
    worktrees_dir: &Path,
    tmux_sessions: &[String],
) -> Result<Vec<WorktreeEntry>> {
    let project_root = expand_tilde(&config.project_root);

    // Run git worktree list --porcelain for reliable parsing
    let output = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(&project_root)
        .output()
        .context("Failed to run git worktree list")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git worktree list failed: {}", stderr.trim());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut state = WorktreeState::load();
    let mut imported = Vec::new();

    // Parse porcelain output: blocks separated by blank lines
    // Each block has: worktree <path>\nHEAD <sha>\nbranch refs/heads/<name>\n
    let worktrees_dir_str = worktrees_dir.to_string_lossy();

    let mut current_path: Option<String> = None;
    let mut current_branch: Option<String> = None;

    for line in stdout.lines().chain(std::iter::once("")) {
        if line.is_empty() {
            // End of block — process if we have path + branch
            if let (Some(ref wt_path), Some(ref branch)) = (&current_path, &current_branch) {
                // Only import worktrees under the worktrees_dir (not the main project root)
                if wt_path.starts_with(worktrees_dir_str.as_ref())
                    && wt_path != project_root.to_string_lossy().as_ref()
                    && state.get(project_key, branch).is_none()
                {
                    // Try to find a matching tmux session
                    let session_name = tmux_sessions
                        .iter()
                        .find(|s| s.contains(branch))
                        .cloned()
                        .unwrap_or_else(|| build_session_name(config, "worktree", branch));

                    let entry = WorktreeEntry {
                        project_key: project_key.to_string(),
                        branch: branch.to_string(),
                        worktree_type: "worktree".to_string(),
                        path: wt_path.to_string(),
                        session_name,
                        metadata: serde_json::json!({}),
                        created_at: chrono::Utc::now().to_rfc3339(),
                    };
                    state.add(entry.clone());
                    imported.push(entry);
                }
            }
            current_path = None;
            current_branch = None;
            continue;
        }

        if let Some(path) = line.strip_prefix("worktree ") {
            current_path = Some(path.to_string());
        } else if let Some(branch_ref) = line.strip_prefix("branch refs/heads/") {
            current_branch = Some(branch_ref.to_string());
        }
    }

    if !imported.is_empty() {
        state.save()?;
    }

    Ok(imported)
}

// ─── Git operations ──────────────────────────────────────────────────────────

/// Create a git worktree.
/// If `existing` is true, attaches to an existing branch.
/// Otherwise creates a new branch from `base`.
pub fn create_git_worktree(
    project_root: &Path,
    worktrees_dir: &Path,
    branch: &str,
    base: &str,
    existing: bool,
) -> Result<PathBuf> {
    let worktree_path = worktrees_dir.join(branch);

    if worktree_path.exists() {
        bail!(
            "Worktree directory already exists: {}",
            worktree_path.display()
        );
    }

    // Ensure worktrees_dir exists
    std::fs::create_dir_all(worktrees_dir)
        .with_context(|| format!("Failed to create worktrees directory: {}", worktrees_dir.display()))?;

    let mut args = vec![
        "worktree".to_string(),
        "add".to_string(),
    ];

    if existing {
        args.push(worktree_path.to_string_lossy().to_string());
        args.push(branch.to_string());
    } else {
        args.push("-b".to_string());
        args.push(branch.to_string());
        args.push(worktree_path.to_string_lossy().to_string());
        args.push(base.to_string());
    }

    let output = Command::new("git")
        .args(&args)
        .current_dir(project_root)
        .output()
        .context("Failed to run git worktree add")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git worktree add failed: {}", stderr.trim());
    }

    Ok(worktree_path)
}

/// Delete a git worktree and optionally its branch.
pub fn delete_git_worktree(
    project_root: &Path,
    worktree_path: &Path,
    branch: &str,
    keep_branch: bool,
    force: bool,
) -> Result<()> {
    // Remove the worktree
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    let path_str = worktree_path.to_string_lossy();
    args.push(&path_str);

    let output = Command::new("git")
        .args(&args)
        .current_dir(project_root)
        .output()
        .context("Failed to run git worktree remove")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git worktree remove failed: {}", stderr.trim());
    }

    // Optionally delete the branch
    if !keep_branch {
        let delete_flag = if force { "-D" } else { "-d" };
        let output = Command::new("git")
            .args(["branch", delete_flag, branch])
            .current_dir(project_root)
            .output()
            .context("Failed to run git branch delete")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Don't fail hard — branch might be checked out elsewhere or already deleted
            eprintln!("Warning: could not delete branch '{}': {}", branch, stderr.trim());
        }
    }

    Ok(())
}

// ─── File operations ─────────────────────────────────────────────────────────

/// Copy file patterns from project_root to worktree_path.
/// Patterns can be files or directories (copied recursively).
pub fn copy_file_patterns(
    project_root: &Path,
    worktree_path: &Path,
    patterns: &[String],
) -> Result<()> {
    for pattern in patterns {
        let src = project_root.join(pattern);
        let dst = worktree_path.join(pattern);

        if !src.exists() {
            eprintln!("Warning: copy pattern '{}' not found, skipping", pattern);
            continue;
        }

        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }

        if src.is_dir() {
            copy_dir_recursive(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst).with_context(|| {
                format!("Failed to copy {} -> {}", src.display(), dst.display())
            })?;
        }
    }
    Ok(())
}

/// Recursively copy a directory
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

/// Create symlinks for file patterns from project_root into worktree_path.
pub fn symlink_file_patterns(
    project_root: &Path,
    worktree_path: &Path,
    patterns: &[String],
) -> Result<()> {
    for pattern in patterns {
        let src = project_root.join(pattern);
        let dst = worktree_path.join(pattern);

        if !src.exists() {
            eprintln!("Warning: symlink pattern '{}' not found, skipping", pattern);
            continue;
        }

        if dst.exists() || dst.symlink_metadata().is_ok() {
            eprintln!("Warning: symlink target '{}' already exists, skipping", pattern);
            continue;
        }

        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }

        #[cfg(unix)]
        std::os::unix::fs::symlink(&src, &dst).with_context(|| {
            format!("Failed to symlink {} -> {}", src.display(), dst.display())
        })?;

        #[cfg(not(unix))]
        {
            // On non-Unix, fall back to copy
            if src.is_dir() {
                copy_dir_recursive(&src, &dst)?;
            } else {
                std::fs::copy(&src, &dst)?;
            }
        }
    }
    Ok(())
}

// ─── Claude memory seeding ──────────────────────────────────────────────────

/// Seed Claude memory by copying .md files from the main project's Claude data dir
/// to the worktree's Claude data dir.
///
/// Claude stores per-project data in `~/.claude/projects/` using the project path
/// with `-` separators (e.g. `/Users/foo/project` → `-Users-foo-project`).
pub fn seed_memory(project_root: &Path, worktree_path: &Path) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?;
    let claude_projects = home.join(".claude").join("projects");

    let src_dir_name = path_to_claude_dir_name(project_root);
    let dst_dir_name = path_to_claude_dir_name(worktree_path);

    let src_dir = claude_projects.join(&src_dir_name);
    let dst_dir = claude_projects.join(&dst_dir_name);

    if !src_dir.exists() {
        return Ok(()); // No memory to seed
    }

    std::fs::create_dir_all(&dst_dir)?;

    // Copy .md files from source to destination
    for entry in std::fs::read_dir(&src_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().map(|e| e == "md").unwrap_or(false) && path.is_file() {
            let dst_file = dst_dir.join(entry.file_name());
            if !dst_file.exists() {
                std::fs::copy(&path, &dst_file)?;
            }
        }
    }

    Ok(())
}

/// Convert an absolute path to the Claude project directory name format.
/// `/Users/foo/project` → `-Users-foo-project`
fn path_to_claude_dir_name(path: &Path) -> String {
    let path_str = path.to_string_lossy();
    path_str.replace('/', "-")
}

// ─── Trust ───────────────────────────────────────────────────────────────────

/// Pre-trust a worktree directory in Claude's ~/.claude.json so the trust
/// dialog is skipped when Claude starts in the new session.
pub fn pretrust_claude_project(worktree_path: &Path) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?;
    let claude_json = home.join(".claude.json");

    let mut config: serde_json::Value = if claude_json.exists() {
        let content = std::fs::read_to_string(&claude_json)?;
        serde_json::from_str(&content)?
    } else {
        serde_json::json!({})
    };

    let path_key = worktree_path.to_string_lossy().to_string();
    let projects = config
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("~/.claude.json is not a JSON object"))?
        .entry("projects")
        .or_insert_with(|| serde_json::json!({}));

    let project = projects
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("projects is not a JSON object"))?
        .entry(&path_key)
        .or_insert_with(|| serde_json::json!({}));

    if let Some(obj) = project.as_object_mut() {
        obj.insert(
            "hasTrustDialogAccepted".to_string(),
            serde_json::Value::Bool(true),
        );
    }

    let content = serde_json::to_string_pretty(&config)?;
    std::fs::write(&claude_json, content)?;

    Ok(())
}

// ─── Hooks ──────────────────────────────────────────────────────────────────

/// Resolve the hooks directory for a project config.
/// Uses `hooks_dir` if set, otherwise defaults to `~/.hive/projects/{project_key}/hooks/`.
pub fn resolve_hooks_dir(config: &ProjectConfig, project_key: &str) -> PathBuf {
    if let Some(ref dir) = config.hooks_dir {
        expand_tilde(dir)
    } else {
        dirs::home_dir()
            .map(|p| p.join(".hive").join("projects").join(project_key).join("hooks"))
            .unwrap_or_else(|| PathBuf::from("/tmp/.hive/projects").join(project_key).join("hooks"))
    }
}

/// Run a hook script if it exists. Returns the (possibly updated) metadata.
///
/// Hook scripts are shell scripts named `<hook_name>.sh` in the hooks directory.
/// Environment variables are set for the hook, and a metadata file is provided
/// for the hook to write output data.
pub fn run_hook(
    hooks_dir: &Path,
    name: &str,
    env_vars: &HashMap<String, String>,
    metadata: &serde_json::Value,
) -> Result<serde_json::Value> {
    let script = hooks_dir.join(format!("{}.sh", name));
    if !script.exists() {
        return Ok(metadata.clone());
    }

    // Create a temp file for metadata exchange (timestamp suffix to avoid predictable paths)
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let metadata_file = std::env::temp_dir().join(format!(
        "hive-hook-metadata-{}-{}-{}",
        name,
        std::process::id(),
        unique
    ));

    let mut cmd = Command::new("bash");
    cmd.arg(&script);
    // Run hooks in the worktree directory (or project root for pre-create when worktree doesn't exist)
    let worktree_cwd = env_vars.get("HIVE_WORKTREE_PATH").map(Path::new);
    let project_cwd = env_vars.get("HIVE_PROJECT_ROOT").map(Path::new);
    if let Some(cwd) = worktree_cwd.filter(|p| p.exists()).or(project_cwd) {
        cmd.current_dir(cwd);
    } else {
        cmd.current_dir(hooks_dir);
    }

    // Set all provided env vars
    for (key, value) in env_vars {
        cmd.env(key, value);
    }

    // Set metadata env vars
    cmd.env("HIVE_METADATA", serde_json::to_string(metadata)?);
    cmd.env("HIVE_METADATA_FILE", &metadata_file);

    let output = cmd.output().with_context(|| format!("Failed to run hook: {}", name))?;

    // Print hook output so log messages are visible
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stdout.trim().is_empty() {
        eprint!("{}", stdout);
    }
    if !stderr.trim().is_empty() {
        eprint!("{}", stderr);
    }

    if !output.status.success() {
        let _ = std::fs::remove_file(&metadata_file);
        bail!("Hook '{}' failed (exit {})", name, output.status);
    }

    // Read metadata file if the hook wrote one
    let updated_metadata = if metadata_file.exists() {
        let content = std::fs::read_to_string(&metadata_file)?;
        let _ = std::fs::remove_file(&metadata_file);
        if content.trim().is_empty() {
            metadata.clone()
        } else {
            let hook_output: serde_json::Value = serde_json::from_str(&content)
                .with_context(|| format!("Hook '{}' wrote invalid JSON to metadata file", name))?;
            merge_metadata(metadata, &hook_output)
        }
    } else {
        metadata.clone()
    };

    Ok(updated_metadata)
}

/// Merge two metadata JSON objects. Values from `overlay` take precedence.
pub fn merge_metadata(base: &serde_json::Value, overlay: &serde_json::Value) -> serde_json::Value {
    match (base, overlay) {
        (serde_json::Value::Object(base_map), serde_json::Value::Object(overlay_map)) => {
            let mut merged = base_map.clone();
            for (key, value) in overlay_map {
                merged.insert(key.clone(), value.clone());
            }
            serde_json::Value::Object(merged)
        }
        // If overlay is not an object, just return it
        (_, overlay) => overlay.clone(),
    }
}

/// Build the standard environment variables for hooks
pub fn build_hook_env(
    project_key: &str,
    branch: &str,
    worktree_path: &Path,
    project_root: &Path,
    session_name: &str,
    wt_type: &str,
    hooks_dir: &Path,
) -> HashMap<String, String> {
    let mut env = HashMap::new();
    env.insert("HIVE_PROJECT_KEY".to_string(), project_key.to_string());
    env.insert("HIVE_BRANCH".to_string(), branch.to_string());
    env.insert(
        "HIVE_WORKTREE_PATH".to_string(),
        worktree_path.to_string_lossy().to_string(),
    );
    env.insert(
        "HIVE_PROJECT_ROOT".to_string(),
        project_root.to_string_lossy().to_string(),
    );
    env.insert("HIVE_SESSION_NAME".to_string(), session_name.to_string());
    env.insert("HIVE_WORKTREE_TYPE".to_string(), wt_type.to_string());
    env.insert(
        "HIVE_HOOKS_DIR".to_string(),
        hooks_dir.to_string_lossy().to_string(),
    );
    env
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(emoji: &str) -> ProjectConfig {
        ProjectConfig {
            emoji: emoji.to_string(),
            project_root: "~/projects/test".to_string(),
            display_name: None,
            startup_command: None,
            worktrees_dir: None,
            default_base_branch: None,
            worktree_types: Vec::new(),
            package_manager: None,
            ports: crate::common::projects::PortConfig::default(),
            database: crate::common::projects::DatabaseConfig::default(),
            files: crate::common::projects::FilePatterns::default(),
            hooks_dir: None,
        }
    }

    #[test]
    fn test_build_session_name() {
        let config = test_config("🌳");
        assert_eq!(
            build_session_name(&config, "worktree", "CSD-2527"),
            "🌳 worktree-CSD-2527"
        );
    }

    #[test]
    fn test_build_session_name_feature() {
        let config = test_config("🐝");
        assert_eq!(
            build_session_name(&config, "feature", "my-branch"),
            "🐝 feature-my-branch"
        );
    }

    #[test]
    fn test_make_key() {
        assert_eq!(
            WorktreeState::make_key("clear-session", "CSD-2527"),
            "clear-session/CSD-2527"
        );
    }

    #[test]
    fn test_state_add_get() {
        let mut state = WorktreeState::default();
        let entry = WorktreeEntry {
            project_key: "hive".to_string(),
            branch: "test-branch".to_string(),
            worktree_type: "worktree".to_string(),
            path: "/tmp/worktrees/hive/test-branch".to_string(),
            session_name: "🐝 worktree-test-branch".to_string(),
            metadata: serde_json::json!({}),
            created_at: "2026-02-17T10:00:00Z".to_string(),
        };
        state.add(entry);

        let got = state.get("hive", "test-branch");
        assert!(got.is_some());
        assert_eq!(got.unwrap().session_name, "🐝 worktree-test-branch");
    }

    #[test]
    fn test_state_remove() {
        let mut state = WorktreeState::default();
        let entry = WorktreeEntry {
            project_key: "hive".to_string(),
            branch: "test-branch".to_string(),
            worktree_type: "worktree".to_string(),
            path: "/tmp/worktrees/hive/test-branch".to_string(),
            session_name: "🐝 worktree-test-branch".to_string(),
            metadata: serde_json::json!({}),
            created_at: "2026-02-17T10:00:00Z".to_string(),
        };
        state.add(entry);

        let removed = state.remove("hive", "test-branch");
        assert!(removed.is_some());
        assert!(state.get("hive", "test-branch").is_none());
    }

    #[test]
    fn test_state_remove_nonexistent() {
        let mut state = WorktreeState::default();
        assert!(state.remove("hive", "nope").is_none());
    }

    #[test]
    fn test_state_list_for_project() {
        let mut state = WorktreeState::default();
        for branch in ["br-1", "br-2"] {
            state.add(WorktreeEntry {
                project_key: "proj-a".to_string(),
                branch: branch.to_string(),
                worktree_type: "worktree".to_string(),
                path: format!("/tmp/{}", branch),
                session_name: format!("📦 worktree-{}", branch),
                metadata: serde_json::json!({}),
                created_at: "2026-02-17T10:00:00Z".to_string(),
            });
        }
        state.add(WorktreeEntry {
            project_key: "proj-b".to_string(),
            branch: "other".to_string(),
            worktree_type: "worktree".to_string(),
            path: "/tmp/other".to_string(),
            session_name: "🔧 worktree-other".to_string(),
            metadata: serde_json::json!({}),
            created_at: "2026-02-17T10:00:00Z".to_string(),
        });

        let list = state.list_for_project("proj-a");
        assert_eq!(list.len(), 2);
        assert!(state.list_for_project("proj-b").len() == 1);
        assert!(state.list_for_project("proj-c").is_empty());
    }

    #[test]
    fn test_state_serialization_roundtrip() {
        let mut state = WorktreeState::default();
        state.add(WorktreeEntry {
            project_key: "clear-session".to_string(),
            branch: "CSD-2527".to_string(),
            worktree_type: "worktree".to_string(),
            path: "/Users/test/02-features/CSD-2527".to_string(),
            session_name: "🌳 worktree-CSD-2527-3027".to_string(),
            metadata: serde_json::json!({
                "frontend_port": 3027,
                "backend_port": 3028,
                "db_name": "clearsession_csd2527"
            }),
            created_at: "2026-02-17T10:00:00Z".to_string(),
        });

        let json = serde_json::to_string(&state).unwrap();
        let deserialized: WorktreeState = serde_json::from_str(&json).unwrap();

        let entry = deserialized.get("clear-session", "CSD-2527").unwrap();
        assert_eq!(entry.metadata["frontend_port"], 3027);
        assert_eq!(entry.metadata["db_name"], "clearsession_csd2527");
    }

    #[test]
    fn test_resolve_hooks_dir_default() {
        let config = test_config("🐝");
        let hooks_dir = resolve_hooks_dir(&config, "hive");
        let path_str = hooks_dir.to_string_lossy();
        assert!(path_str.contains(".hive/projects/hive/hooks"));
    }

    #[test]
    fn test_resolve_hooks_dir_custom() {
        let mut config = test_config("🐝");
        config.hooks_dir = Some("~/my-hooks".to_string());
        let hooks_dir = resolve_hooks_dir(&config, "hive");
        assert!(hooks_dir.to_string_lossy().ends_with("my-hooks"));
    }

    #[test]
    fn test_merge_metadata_both_objects() {
        let base = serde_json::json!({"a": 1, "b": 2});
        let overlay = serde_json::json!({"b": 3, "c": 4});
        let merged = merge_metadata(&base, &overlay);
        assert_eq!(merged["a"], 1);
        assert_eq!(merged["b"], 3); // overlay wins
        assert_eq!(merged["c"], 4);
    }

    #[test]
    fn test_merge_metadata_empty_overlay() {
        let base = serde_json::json!({"a": 1});
        let overlay = serde_json::json!({});
        let merged = merge_metadata(&base, &overlay);
        assert_eq!(merged["a"], 1);
    }

    #[test]
    fn test_merge_metadata_session_name_override() {
        let base = serde_json::json!({});
        let overlay = serde_json::json!({"session_name": "custom-name", "port": 3000});
        let merged = merge_metadata(&base, &overlay);
        assert_eq!(merged["session_name"], "custom-name");
        assert_eq!(merged["port"], 3000);
    }

    #[test]
    fn test_build_hook_env() {
        let env = build_hook_env(
            "my-proj",
            "feat-1",
            Path::new("/tmp/worktrees/feat-1"),
            Path::new("/home/user/projects/my-proj"),
            "📦 worktree-feat-1",
            "worktree",
            Path::new("/home/user/.hive/projects/my-proj/hooks"),
        );
        assert_eq!(env["HIVE_PROJECT_KEY"], "my-proj");
        assert_eq!(env["HIVE_BRANCH"], "feat-1");
        assert_eq!(env["HIVE_WORKTREE_PATH"], "/tmp/worktrees/feat-1");
        assert_eq!(env["HIVE_PROJECT_ROOT"], "/home/user/projects/my-proj");
        assert_eq!(env["HIVE_SESSION_NAME"], "📦 worktree-feat-1");
        assert_eq!(env["HIVE_WORKTREE_TYPE"], "worktree");
        assert_eq!(env["HIVE_HOOKS_DIR"], "/home/user/.hive/projects/my-proj/hooks");
    }

    #[test]
    fn test_path_to_claude_dir_name() {
        assert_eq!(
            path_to_claude_dir_name(Path::new("/Users/foo/project")),
            "-Users-foo-project"
        );
    }

    #[test]
    fn test_run_hook_missing_script() {
        let hooks_dir = std::env::temp_dir().join("hive-test-no-hooks");
        let _ = std::fs::create_dir_all(&hooks_dir);
        let metadata = serde_json::json!({"existing": true});
        let result = run_hook(&hooks_dir, "nonexistent", &HashMap::new(), &metadata).unwrap();
        assert_eq!(result["existing"], true);
        let _ = std::fs::remove_dir_all(&hooks_dir);
    }
}
