//! `hive sessions` — a read-only listing of every known Claude session (live AND
//! closed), grouped by its resolved parent (project / worktree).
//!
//! This is the first surface onto the re-rooted [`SessionRegistry`] model: it does
//! NOT touch the TUI, the web view, or the hook writer. It builds the registry as a
//! shadow (existing `state.json` + a disk scan + the `sessions.json` overlay +
//! live tmux placements), resolves parents, and prints the grouping — so the
//! closed/resumable sessions the current views can't show become visible.

use anyhow::Result;
use std::collections::HashMap;

use crate::common::instances;
use crate::common::jsonl;
use crate::common::projects::ProjectRegistry;
use crate::common::registry::{
    self, ClaudeSession, SessionRegistry, SessionSidecar, TmuxPlacement,
};
use crate::common::worktree::WorktreeState;
use crate::ipc::messages::{HookState, SessionStatus};

/// Build the registry from live state (read-only) and print the grouped listing.
pub fn run_sessions() -> Result<()> {
    let hook = HookState::load();
    let disk = jsonl::scan_all_disk_sessions();
    let disk_ids: Vec<String> = disk.iter().map(|d| d.id.clone()).collect();
    let sidecar = SessionSidecar::load();

    // Live placements: every currently-running Claude instance we can tie to a
    // conversation id becomes the SOLE Live discriminator for that id.
    let mut live_placements: HashMap<String, TmuxPlacement> = HashMap::new();
    for inst in instances::detect_all_instances() {
        if let Some(sid) = inst.session_id {
            live_placements.insert(
                sid,
                TmuxPlacement {
                    session_name: inst.session_name,
                    window_index: inst.window_index,
                    window_name: inst.window_name,
                    pane_id: Some(inst.pane_id),
                },
            );
        }
    }

    let mut reg = SessionRegistry::from_shadow(&hook, &disk_ids, &live_placements, &sidecar);

    // Enrich disk-only (Closed) sessions with cwd + last-activity from the
    // transcript — the hook side had neither, so without this they can't be
    // placed or bounded.
    let disk_map: HashMap<&str, &jsonl::DiskSession> =
        disk.iter().map(|d| (d.id.as_str(), d)).collect();
    for (id, s) in reg.sessions.iter_mut() {
        if let Some(d) = disk_map.get(id.as_str()) {
            if s.cwd.is_empty() {
                if let Some(cwd) = &d.cwd {
                    s.cwd = cwd.clone();
                }
            }
            if s.last_activity.is_none() {
                s.last_activity = d.last_activity.clone();
            }
            if s.title.is_none() {
                s.title = d.title.clone();
            }
        }
    }

    // Resolve a logical parent for any session that doesn't already have one cached.
    let worktrees = WorktreeState::load();
    let projects = ProjectRegistry::load();
    for s in reg.sessions.values_mut() {
        if s.parent.is_none() {
            s.parent = registry::resolve_parent(&s.cwd, &worktrees, &projects);
        }
    }

    // Bound the (unbounded) on-disk Closed set: Live is always shown; a Closed
    // session is kept only if recently active, parented, or a pinned freeze.
    let now = chrono::Utc::now();
    let cfg = registry::BoundingCfg { max_age_days: 14 };
    reg.sessions.retain(|_, s| {
        s.lifecycle.is_actionable_here() || registry::should_surface_closed(s, now, &cfg)
    });

    print!("{}", render_registry(&reg));
    Ok(())
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

fn status_label(session: &ClaudeSession) -> &'static str {
    match session.status.as_ref().map(|s| &s.status) {
        None => "-",
        Some(SessionStatus::Working) => "working",
        Some(SessionStatus::Waiting) => "idle",
        Some(SessionStatus::NeedsPermission { .. }) => "needs-perm",
        Some(SessionStatus::EditApproval { .. }) => "edit-approval",
        Some(SessionStatus::PlanReview) => "plan-review",
        Some(SessionStatus::QuestionAsked) => "question",
        Some(SessionStatus::RunningWorkflow { .. }) => "workflow",
        Some(SessionStatus::Unknown) => "unknown",
    }
}

/// Render the registry grouped by parent key (deterministic ordering) — pure, so
/// it is unit-testable without tmux/disk.
pub fn render_registry(reg: &SessionRegistry) -> String {
    use std::collections::BTreeMap;

    let total = reg.sessions.len();
    let live = reg
        .sessions
        .values()
        .filter(|s| s.lifecycle.is_actionable_here())
        .count();
    let closed = total - live;

    // Group by parent; None → "(unassigned)". BTreeMap gives stable ordering.
    let mut groups: BTreeMap<String, Vec<&ClaudeSession>> = BTreeMap::new();
    for s in reg.sessions.values() {
        let key = s
            .parent
            .clone()
            .unwrap_or_else(|| "(unassigned)".to_string());
        groups.entry(key).or_default().push(s);
    }

    let mut out = String::new();
    out.push_str(&format!(
        "hive sessions — {total} known ({live} live · {closed} closed) across {} groups\n",
        groups.len()
    ));

    for (parent, mut sessions) in groups {
        // Live first, then most-recently-active, then id for stability.
        sessions.sort_by(|a, b| {
            b.lifecycle
                .is_actionable_here()
                .cmp(&a.lifecycle.is_actionable_here())
                .then_with(|| b.last_activity.cmp(&a.last_activity))
                .then_with(|| a.id.as_str().cmp(b.id.as_str()))
        });
        out.push('\n');
        out.push_str(&format!("{parent}\n"));
        for s in sessions {
            let marker = if s.is_frozen() {
                "💤"
            } else if s.lifecycle.is_actionable_here() {
                "●"
            } else {
                "○"
            };
            let last = s
                .last_activity
                .as_deref()
                .map(|t| t.chars().take(10).collect::<String>())
                .unwrap_or_default();
            let title = s
                .title
                .as_deref()
                .map(|t| t.chars().take(28).collect::<String>())
                .unwrap_or_default();
            out.push_str(&format!(
                "  {} {:8}  {:<28}  {:<13}  {}  {}\n",
                marker,
                short_id(s.id.as_str()),
                title,
                status_label(s),
                s.cwd,
                last,
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::registry::{ClaudeSessionId, Lifecycle};

    fn mk(id: &str, lc: Lifecycle, parent: Option<&str>, last: Option<&str>) -> ClaudeSession {
        ClaudeSession {
            id: ClaudeSessionId::from(id),
            cwd: "/home/u/hive".to_string(),
            lifecycle: lc,
            status: None,
            last_activity: last.map(|s| s.to_string()),
            placement: None,
            parent: parent.map(|s| s.to_string()),
            frozen: None,
            note: String::new(),
            pinned: false,
            archived: false,
            title: None,
        }
    }

    #[test]
    fn test_render_groups_and_counts() {
        let mut reg = SessionRegistry::default();
        let mut live = mk(
            "live1",
            Lifecycle::Live,
            Some("hive"),
            Some("2026-07-02T00:00:00Z"),
        );
        live.title = Some("My Task".to_string());
        reg.sessions.insert("live1".to_string(), live);
        reg.sessions.insert(
            "closed1".to_string(),
            mk(
                "closed1",
                Lifecycle::Closed,
                Some("hive"),
                Some("2026-07-01T00:00:00Z"),
            ),
        );
        reg.sessions.insert(
            "orphan".to_string(),
            mk("orphan", Lifecycle::Closed, None, None),
        );

        let out = render_registry(&reg);

        assert!(out.contains("3 known (1 live · 2 closed) across 2 groups"));
        assert!(out.contains("hive\n"));
        assert!(out.contains("(unassigned)"));
        assert!(out.contains('●')); // a live marker
        assert!(out.contains('○')); // a closed marker
        assert!(out.contains("My Task")); // named session's title is shown
    }

    #[test]
    fn test_render_empty_registry() {
        let reg = SessionRegistry::default();
        let out = render_registry(&reg);
        assert!(out.contains("0 known (0 live · 0 closed) across 0 groups"));
    }
}
