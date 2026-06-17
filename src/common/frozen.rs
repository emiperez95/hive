//! Frozen (hibernated) Claude windows — types, state persistence, and freeze/thaw lifecycle.
//!
//! A *frozen* entry is a single Claude window (one conversation) whose tmux window — and the
//! Claude process in it — has been killed to free resources, but whose conversation is
//! recoverable. We capture the Claude `session_id` so thaw can `claude --resume <id>` straight
//! back into the conversation.
//!
//! Granularity is the **window**, not the tmux session: a project session can host several
//! Claude windows, and each can be frozen/thawed independently. Freezing one window leaves the
//! session (and its other windows) alive; freezing the last window lets the session die
//! naturally. This is distinct from *skip* (skipped.txt), which keeps a session live and merely
//! excludes it from cycling.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

use crate::common::persistence::cache_dir;
use crate::common::projects::ensure_tmux_session;

// ─── Types ───────────────────────────────────────────────────────────────────

/// A single frozen Claude window persisted in frozen.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrozenEntry {
    /// Parent tmux/display session name (e.g. `🌳 Clear Session`).
    pub session_name: String,
    /// tmux window name (for display and to recreate the window).
    #[serde(default)]
    pub window_name: String,
    /// Original tmux window index (informational; ordering may differ on thaw).
    #[serde(default)]
    pub window_index: String,
    /// Working directory of the window.
    pub cwd: String,
    /// Claude conversation id to `--resume`. `None` falls back to `claude -c`.
    #[serde(default)]
    pub claude_session_id: Option<String>,
    /// `CLAUDE_CONFIG_DIR` captured from the live session (auth profile), if any.
    #[serde(default)]
    pub claude_config_dir: Option<String>,
    /// Freeform note/theme so you know why you parked it.
    #[serde(default)]
    pub note: String,
    /// RFC3339 timestamp of when it was frozen.
    pub frozen_at: String,
}

impl FrozenEntry {
    /// Stable, unique registry key. A Claude `session_id` is a UUID (globally unique); when
    /// it's missing we fall back to session+window, which is unique among live windows.
    pub fn key(&self) -> String {
        match &self.claude_session_id {
            Some(id) => id.clone(),
            None => format!("{}#{}", self.session_name, self.window_index),
        }
    }

    /// Short label for the window within its session (window name, else `win N`).
    /// Empty when no window info was captured (pre-window-level entries).
    pub fn window_label(&self) -> String {
        if !self.window_name.is_empty() {
            self.window_name.clone()
        } else if !self.window_index.is_empty() {
            format!("win {}", self.window_index)
        } else {
            String::new()
        }
    }
}

/// Top-level state file for all frozen windows, keyed by [`FrozenEntry::key`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FrozenState {
    #[serde(default)]
    pub frozen: HashMap<String, FrozenEntry>,
}

impl FrozenState {
    fn file_path() -> Option<PathBuf> {
        cache_dir().map(|p| p.join("frozen.json"))
    }

    /// Load frozen state from disk. Returns empty state on any error.
    pub fn load() -> Self {
        let Some(path) = Self::file_path() else {
            return Self::default();
        };
        let Ok(content) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        let mut state: Self = serde_json::from_str(&content).unwrap_or_else(|e| {
            eprintln!("Warning: failed to parse {}: {}", path.display(), e);
            Self::default()
        });
        // Migrate: older entries were keyed by session name; re-key by entry.key() so the
        // map key always matches what `key()` returns (lookups + display rely on this).
        state.normalize_keys();
        state
    }

    /// Re-key every entry by [`FrozenEntry::key`]. Idempotent; cheap no-op when already
    /// normalized. The re-keyed map is persisted on the next freeze/thaw/discard save.
    fn normalize_keys(&mut self) {
        let needs = self.frozen.iter().any(|(k, e)| k != &e.key());
        if !needs {
            return;
        }
        let entries: Vec<FrozenEntry> = self.frozen.drain().map(|(_, e)| e).collect();
        for e in entries {
            self.frozen.insert(e.key(), e);
        }
    }

    /// Save frozen state to disk atomically (write .tmp, rename).
    pub fn save(&self) -> Result<()> {
        let path = Self::file_path().ok_or_else(|| anyhow!("Cannot determine cache directory"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &content)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn get(&self, key: &str) -> Option<&FrozenEntry> {
        self.frozen.get(key)
    }

    /// Entries sorted newest-frozen first, for display.
    pub fn sorted(&self) -> Vec<&FrozenEntry> {
        let mut entries: Vec<&FrozenEntry> = self.frozen.values().collect();
        entries.sort_by(|a, b| b.frozen_at.cmp(&a.frozen_at));
        entries
    }
}

// ─── Lifecycle ───────────────────────────────────────────────────────────────

/// Read `CLAUDE_CONFIG_DIR` from a live tmux session's environment, if set.
fn capture_config_dir(session_name: &str) -> Option<String> {
    let out = Command::new("tmux")
        .args(["show-environment", "-t", session_name, "CLAUDE_CONFIG_DIR"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout);
    // Either "CLAUDE_CONFIG_DIR=/path" or "-CLAUDE_CONFIG_DIR" (unset).
    line.trim()
        .strip_prefix("CLAUDE_CONFIG_DIR=")
        .map(|v| v.to_string())
}

/// What to freeze: a single Claude window identified within its parent session.
#[derive(Debug, Clone)]
pub struct FreezeTarget {
    pub session_name: String,
    pub window_index: String,
    pub window_name: String,
    pub cwd: String,
    pub claude_session_id: Option<String>,
}

/// Freeze one Claude window: record its resume metadata, then kill just that tmux window.
///
/// The parent session and its other windows are left running; if this was the session's last
/// window, tmux destroys the (now empty) session on its own. Returns the new entry's key.
pub fn freeze_window(target: &FreezeTarget, note: &str) -> Result<String> {
    // Capture the auth profile before we touch the window.
    let claude_config_dir = capture_config_dir(&target.session_name);

    let entry = FrozenEntry {
        session_name: target.session_name.clone(),
        window_name: target.window_name.clone(),
        window_index: target.window_index.clone(),
        cwd: target.cwd.clone(),
        claude_session_id: target.claude_session_id.clone(),
        claude_config_dir,
        note: note.trim().to_string(),
        frozen_at: chrono::Utc::now().to_rfc3339(),
    };
    let key = entry.key();

    let mut state = FrozenState::load();
    state.frozen.insert(key.clone(), entry);
    state.save()?;

    // Kill just this window — frees the Claude process. The conversation JSONL on disk is
    // untouched, so it stays resumable.
    let win_target = format!("{}:{}", target.session_name, target.window_index);
    let killed = Command::new("tmux")
        .args(["kill-window", "-t", &win_target])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !killed {
        crate::common::debug::debug_log(&format!(
            "freeze: kill-window returned non-zero for '{win_target}' (may already be gone)"
        ));
    }
    Ok(key)
}

/// Thaw a frozen window: re-add it to its session (or recreate the session) and resume.
///
/// Returns the session name to switch to. The entry is removed only on success.
pub fn thaw_window(key: &str) -> Result<String> {
    let mut state = FrozenState::load();
    let entry = state
        .frozen
        .get(key)
        .cloned()
        .ok_or_else(|| anyhow!("no frozen window for key '{key}'"))?;

    let startup = match &entry.claude_session_id {
        Some(id) => format!("claude --resume {id}"),
        None => "claude -c".to_string(),
    };

    let session_alive = Command::new("tmux")
        .args(["has-session", "-t", &entry.session_name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if session_alive {
        // Add a new window to the existing session and resume in it.
        let mut cmd = Command::new("tmux");
        cmd.args(["new-window", "-t", &entry.session_name]);
        if !entry.window_name.is_empty() {
            cmd.args(["-n", &entry.window_name]);
        }
        cmd.args(["-c", &entry.cwd]);
        // New windows inherit the session environment, but pass it explicitly too in case the
        // live session was created without it.
        if let Some(dir) = &entry.claude_config_dir {
            cmd.arg("-e").arg(format!("CLAUDE_CONFIG_DIR={dir}"));
        }
        let ok = cmd.output().map(|o| o.status.success()).unwrap_or(false);
        if !ok {
            return Err(anyhow!(
                "Failed to add window to session '{}'",
                entry.session_name
            ));
        }
        // new-window makes the new window active; send the resume command to it.
        let _ = Command::new("tmux")
            .args(["send-keys", "-t", &entry.session_name, &startup, "Enter"])
            .output();
    } else {
        // Session is gone (last window was frozen) — recreate it with this window.
        let env: Vec<(String, String)> = match &entry.claude_config_dir {
            Some(dir) => vec![("CLAUDE_CONFIG_DIR".to_string(), dir.clone())],
            None => Vec::new(),
        };
        if !ensure_tmux_session(&entry.session_name, &entry.cwd, Some(&startup), &env) {
            return Err(anyhow!(
                "Failed to recreate session '{}'",
                entry.session_name
            ));
        }
    }

    state.frozen.remove(key);
    state.save()?;
    Ok(entry.session_name)
}

/// Discard a frozen window without restoring it (conversation history stays on disk).
pub fn discard_frozen(key: &str) -> Result<bool> {
    let mut state = FrozenState::load();
    let removed = state.frozen.remove(key).is_some();
    if removed {
        state.save()?;
    }
    Ok(removed)
}

/// Human-friendly "2h ago" style relative time from an RFC3339 timestamp.
pub fn relative_time(rfc3339: &str) -> String {
    let Ok(then) = chrono::DateTime::parse_from_rfc3339(rfc3339) else {
        return String::new();
    };
    let now = chrono::Utc::now();
    let secs = (now - then.with_timezone(&chrono::Utc))
        .num_seconds()
        .max(0);
    match secs {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(session: &str, window_idx: &str, sid: Option<&str>, ts: &str) -> FrozenEntry {
        FrozenEntry {
            session_name: session.to_string(),
            window_name: "claude".to_string(),
            window_index: window_idx.to_string(),
            cwd: "/tmp/proj".to_string(),
            claude_session_id: sid.map(|s| s.to_string()),
            claude_config_dir: None,
            note: "blocked on API".to_string(),
            frozen_at: ts.to_string(),
        }
    }

    #[test]
    fn key_prefers_session_id_else_session_window() {
        let with_id = entry(
            "🌳 Clear Session",
            "1",
            Some("abc-123"),
            "2026-06-12T10:00:00Z",
        );
        assert_eq!(with_id.key(), "abc-123");
        let no_id = entry("🌳 Clear Session", "2", None, "2026-06-12T10:00:00Z");
        assert_eq!(no_id.key(), "🌳 Clear Session#2");
    }

    #[test]
    fn frozen_state_roundtrip_keyed_by_window() {
        let mut state = FrozenState::default();
        // Two windows of the SAME session frozen independently.
        for (idx, sid) in [("1", "sid-1"), ("2", "sid-2")] {
            let e = entry("🌳 Clear Session", idx, Some(sid), "2026-06-12T10:00:00Z");
            state.frozen.insert(e.key(), e);
        }
        let json = serde_json::to_string(&state).unwrap();
        let back: FrozenState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.frozen.len(), 2);
        assert_eq!(back.get("sid-1").unwrap().window_index, "1");
        assert_eq!(back.get("sid-2").unwrap().window_index, "2");
    }

    #[test]
    fn normalize_rekeys_legacy_session_keyed_entries() {
        // Legacy file: map keyed by session name, entry carries a conversation id and no
        // window info. After normalize, the map key must equal entry.key() (the sid).
        let legacy = r#"{
            "frozen": {
                "🌳 [clear-session] CSD-2629": {
                    "session_name": "🌳 [clear-session] CSD-2629",
                    "cwd": "/proj",
                    "claude_session_id": "a41e043a",
                    "note": "After prod deploy",
                    "frozen_at": "2026-06-12T14:22:32Z"
                }
            }
        }"#;
        let mut state: FrozenState = serde_json::from_str(legacy).unwrap();
        // Before normalize: keyed by session name, lookup by sid misses.
        assert!(state.get("a41e043a").is_none());
        state.normalize_keys();
        // After: keyed by sid, so the picker's get(entry.key()) resolves.
        let e = state.get("a41e043a").expect("re-keyed by sid");
        assert_eq!(e.note, "After prod deploy");
        assert_eq!(e.window_label(), ""); // no window info → no suffix
        assert_eq!(state.frozen.len(), 1);
    }

    #[test]
    fn sorted_is_newest_first() {
        let mut state = FrozenState::default();
        for (sid, ts) in [("a", "2026-06-10T10:00:00Z"), ("b", "2026-06-12T10:00:00Z")] {
            let e = entry("s", "1", Some(sid), ts);
            state.frozen.insert(e.key(), e);
        }
        let sorted = state.sorted();
        assert_eq!(sorted[0].claude_session_id.as_deref(), Some("b"));
        assert_eq!(sorted[1].claude_session_id.as_deref(), Some("a"));
    }

    #[test]
    fn relative_time_buckets() {
        let now = chrono::Utc::now();
        let five_min = (now - chrono::Duration::minutes(5)).to_rfc3339();
        assert_eq!(relative_time(&five_min), "5m ago");
        let three_hours = (now - chrono::Duration::hours(3)).to_rfc3339();
        assert_eq!(relative_time(&three_hours), "3h ago");
        let two_days = (now - chrono::Duration::days(2)).to_rfc3339();
        assert_eq!(relative_time(&two_days), "2d ago");
        assert_eq!(relative_time("garbage"), "");
    }
}
