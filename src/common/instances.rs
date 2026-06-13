//! Detection of individual Claude instances across tmux windows/panes.
//!
//! A single tmux session can host several Claude instances — one per window (or pane),
//! often all in the same working directory (e.g. multiple `claude` windows in one project
//! session). The rest of hive historically assumed one Claude per tmux session: both the
//! TUI and the web server walked the pane tree and stopped at the first Claude they found.
//!
//! This module is the shared source of truth that replaces those duplicated walks. It
//! enumerates *every* Claude-running pane and resolves each one to its conversation by
//! linking the tmux pane id to the Claude `session_id` that hooks recorded in
//! [`SessionState::tmux_pane`]. Since a `session_id` is exactly the `<session_id>.jsonl`
//! basename, that link lets consumers load the correct transcript and status per instance,
//! even when instances share a cwd.
//!
//! When a pane has no recorded pane id yet (e.g. a session that hasn't fired a hook since
//! upgrading), resolution falls back to the most-recently-active hook session for the cwd —
//! the same behaviour as before, with the same same-cwd ambiguity.

use std::collections::HashMap;

use sysinfo::System;

use crate::common::process::{
    build_children_map, collect_descendants, get_process_info, is_claude_process,
};
use crate::common::types::TmuxSession;
use crate::ipc::messages::{HookState, SessionState};

/// A single Claude instance running in a specific tmux pane.
#[derive(Debug, Clone)]
pub struct ClaudeInstance {
    /// tmux session name the pane belongs to.
    pub session_name: String,
    /// tmux window index (string, as tmux reports it).
    pub window_index: String,
    /// tmux window name.
    pub window_name: String,
    /// tmux pane index within the window.
    pub pane_index: String,
    /// tmux global pane id (e.g. "%1").
    pub pane_id: String,
    /// Working directory of the pane.
    pub cwd: String,
    /// Resolved Claude session id (the `<session_id>.jsonl` basename), when known.
    pub session_id: Option<String>,
    /// PIDs of the Claude pane's process tree (the pane pid + descendants).
    pub pids: Vec<u32>,
}

impl ClaudeInstance {
    /// `(session, window, pane)` tuple used for tmux `send-keys` / targeting.
    pub fn target(&self) -> (String, String, String) {
        (
            self.session_name.clone(),
            self.window_index.clone(),
            self.pane_index.clone(),
        )
    }
}

/// Index over hook state for resolving a pane (or cwd) to its recorded Claude session.
///
/// Two indexes are built once per gather pass: by tmux pane id (the precise link) and by
/// cwd (the legacy fallback). Both keep the most-recently-active session when keys collide.
pub struct HookIndex<'a> {
    by_pane: HashMap<String, &'a SessionState>,
    by_cwd: HashMap<String, &'a SessionState>,
}

impl<'a> HookIndex<'a> {
    /// Build the pane and cwd indexes from hook state.
    pub fn build(hook_state: &'a HookState) -> Self {
        let mut by_pane: HashMap<String, &SessionState> = HashMap::new();
        let mut by_cwd: HashMap<String, &SessionState> = HashMap::new();

        for session in hook_state.sessions.values() {
            if let Some(pane) = &session.tmux_pane {
                let is_newer = by_pane
                    .get(pane)
                    .is_none_or(|existing| session.last_activity > existing.last_activity);
                if is_newer {
                    by_pane.insert(pane.clone(), session);
                }
            }

            let is_newer = by_cwd
                .get(&session.cwd)
                .is_none_or(|existing| session.last_activity > existing.last_activity);
            if is_newer {
                by_cwd.insert(session.cwd.clone(), session);
            }
        }

        Self { by_pane, by_cwd }
    }

    /// Resolve the hook session for a pane: prefer an exact pane-id match, fall back to cwd.
    pub fn resolve(&self, pane_id: &str, cwd: &str) -> Option<&'a SessionState> {
        self.by_pane
            .get(pane_id)
            .copied()
            .or_else(|| self.by_cwd.get(cwd).copied())
    }
}

/// Walk every pane of every tmux session and return one [`ClaudeInstance`] per pane that is
/// running a Claude process. Each instance's `session_id` is resolved via `hook_index`
/// (pane-id match preferred, cwd fallback).
///
/// `children_map` should be built once per gather pass (see
/// [`crate::common::process::build_children_map`]) so the descendant walk is in-memory.
pub fn detect_claude_instances(
    sessions: &[TmuxSession],
    sys: &System,
    children_map: &HashMap<u32, Vec<u32>>,
    hook_index: &HookIndex,
) -> Vec<ClaudeInstance> {
    let mut instances = Vec::new();

    for session in sessions {
        for window in &session.windows {
            for pane in &window.panes {
                let mut pids = vec![pane.pid];
                collect_descendants(children_map, pane.pid, &mut pids);

                let has_claude = pids.iter().any(|&pid| {
                    get_process_info(sys, pid)
                        .map(|info| is_claude_process(&info))
                        .unwrap_or(false)
                });
                if !has_claude {
                    continue;
                }

                let session_id = hook_index
                    .resolve(&pane.id, &pane.cwd)
                    .map(|s| s.session_id.clone());

                instances.push(ClaudeInstance {
                    session_name: session.name.clone(),
                    window_index: window.index.clone(),
                    window_name: window.name.clone(),
                    pane_index: pane.index.clone(),
                    pane_id: pane.id.clone(),
                    cwd: pane.cwd.clone(),
                    session_id,
                    pids,
                });
            }
        }
    }

    instances
}

/// Enumerate the Claude instances (one per Claude-running pane) in a single tmux session,
/// building the heavy inputs (process table, children map, hook index) on demand.
///
/// Intended for one-shot user actions like freeze — not per-refresh use, where the caller
/// should reuse a long-lived `System` and build the indexes once.
pub fn instances_for_session(session_name: &str) -> Vec<ClaudeInstance> {
    let Ok(sessions) = crate::common::tmux::get_tmux_sessions() else {
        return Vec::new();
    };
    let sys = System::new_all();
    let children_map = build_children_map();
    let hook_state = HookState::load();
    let hook_index = HookIndex::build(&hook_state);
    detect_claude_instances(&sessions, &sys, &children_map, &hook_index)
        .into_iter()
        .filter(|inst| inst.session_name == session_name)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::messages::SessionStatus;

    fn session(id: &str, cwd: &str, pane: Option<&str>, activity: &str) -> SessionState {
        SessionState {
            session_id: id.to_string(),
            cwd: cwd.to_string(),
            status: SessionStatus::Working,
            needs_attention: false,
            last_activity: Some(activity.to_string()),
            tmux_pane: pane.map(|p| p.to_string()),
        }
    }

    fn state_with(sessions: Vec<SessionState>) -> HookState {
        let mut state = HookState::default();
        for s in sessions {
            state.sessions.insert(s.session_id.clone(), s);
        }
        state
    }

    #[test]
    fn test_resolve_prefers_pane_id_over_cwd() {
        // Two sessions in the same cwd, distinguished only by their tmux pane.
        let state = state_with(vec![
            session("a", "/proj", Some("%1"), "2025-01-01T00:00:01Z"),
            session("b", "/proj", Some("%2"), "2025-01-01T00:00:02Z"),
        ]);
        let index = HookIndex::build(&state);

        assert_eq!(index.resolve("%1", "/proj").unwrap().session_id, "a");
        assert_eq!(index.resolve("%2", "/proj").unwrap().session_id, "b");
    }

    #[test]
    fn test_resolve_falls_back_to_cwd_when_pane_unknown() {
        let state = state_with(vec![session(
            "a",
            "/proj",
            Some("%1"),
            "2025-01-01T00:00:01Z",
        )]);
        let index = HookIndex::build(&state);

        // Unknown pane id, but the cwd is known → fall back to the cwd-indexed session.
        assert_eq!(index.resolve("%99", "/proj").unwrap().session_id, "a");
    }

    #[test]
    fn test_resolve_none_when_nothing_matches() {
        let state = state_with(vec![session(
            "a",
            "/proj",
            Some("%1"),
            "2025-01-01T00:00:01Z",
        )]);
        let index = HookIndex::build(&state);

        assert!(index.resolve("%99", "/other").is_none());
    }

    #[test]
    fn test_cwd_index_keeps_most_recent() {
        // No pane ids recorded (legacy sessions): cwd index should keep the newer one.
        let state = state_with(vec![
            session("old", "/proj", None, "2025-01-01T00:00:01Z"),
            session("new", "/proj", None, "2025-01-01T00:00:09Z"),
        ]);
        let index = HookIndex::build(&state);

        assert_eq!(index.resolve("%1", "/proj").unwrap().session_id, "new");
    }

    #[test]
    fn test_pane_index_keeps_most_recent_for_same_pane() {
        // Same pane id reused across resumed sessions: keep the most recently active.
        let state = state_with(vec![
            session("old", "/proj", Some("%1"), "2025-01-01T00:00:01Z"),
            session("new", "/proj", Some("%1"), "2025-01-01T00:00:09Z"),
        ]);
        let index = HookIndex::build(&state);

        assert_eq!(index.resolve("%1", "/proj").unwrap().session_id, "new");
    }
}
