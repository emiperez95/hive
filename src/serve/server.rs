//! Data gathering for the web dashboard's JSON API.
//!
//! Projects the shared [`crate::common::conversations`] registry into the wire
//! shapes the dashboard consumes: [`gather_active_views`] (the session-grouped
//! live view, `/api/active`) and [`build_conversation_views`] (the closed/frozen
//! set, `/api/conversations`). A single tmux session can host several Claude
//! windows; each becomes a [`WindowView`] and they aggregate into a
//! [`SessionView`], which the dashboard renders as an accordion.

use crate::common::persistence::{load_session_todos, load_skipped_sessions};
use crate::common::process::get_process_info;
use crate::common::projects::ProjectRegistry;
use crate::common::registry::{Conversation, ConversationRegistry};
use crate::common::tmux::{get_other_client_sessions, get_tmux_sessions};
use crate::ipc::messages::SessionStatus;
use crate::serve::web_types::{
    ConversationView, PlacementView, ProcessView, SessionView, WindowView,
};

use std::collections::HashMap;
use sysinfo::System;

/// Auto-approved sessions should surface as Working, never as a permission/edit prompt.
fn mask_auto_approve(
    status: Option<SessionStatus>,
    is_auto_approve: bool,
) -> Option<SessionStatus> {
    if !is_auto_approve {
        return status;
    }
    match status {
        Some(SessionStatus::NeedsPermission { .. }) | Some(SessionStatus::EditApproval { .. }) => {
            Some(SessionStatus::Working)
        }
        other => other,
    }
}

/// Pick the session-level status from its windows: a window needing attention wins,
/// then any working window, then waiting, falling back to the first window's status.
fn aggregate_status(windows: &[WindowView]) -> Option<SessionStatus> {
    if windows.is_empty() {
        return None;
    }
    let needs_attention = |s: &SessionStatus| {
        matches!(
            s,
            SessionStatus::NeedsPermission { .. }
                | SessionStatus::EditApproval { .. }
                | SessionStatus::PlanReview
                | SessionStatus::QuestionAsked
        )
    };
    if let Some(w) = windows
        .iter()
        .find(|w| w.status.as_ref().is_some_and(needs_attention))
    {
        return w.status.clone();
    }
    if windows
        .iter()
        .any(|w| matches!(w.status, Some(SessionStatus::Working)))
    {
        return Some(SessionStatus::Working);
    }
    // A running workflow on any window is busy — surface it above an idle window.
    if let Some(w) = windows
        .iter()
        .find(|w| matches!(w.status, Some(SessionStatus::RunningWorkflow { .. })))
    {
        return w.status.clone();
    }
    if windows
        .iter()
        .any(|w| matches!(w.status, Some(SessionStatus::Waiting)))
    {
        return Some(SessionStatus::Waiting);
    }
    windows.first().and_then(|w| w.status.clone())
}

// ── Conversation-first view (new `/api/conversations`) ──────────────────────

/// Build the conversation-first view for `/api/conversations` from an already-
/// gathered registry (live + closed + frozen, UUID-keyed) so the web dashboard
/// sees the same model as the TUI. The caller gathers the registry once and feeds
/// both this and [`gather_active_views`].
pub(crate) fn build_conversation_views(reg: &ConversationRegistry) -> Vec<ConversationView> {
    let projects = ProjectRegistry::load();
    let mut views: Vec<ConversationView> = reg
        .conversations
        .values()
        // Archived conversations stay in the registry (the TUI's project detail
        // lists them with their archive reason) but are set aside by definition —
        // keep them off the Resume view unless they're actually running.
        .filter(|c| !c.archived || c.lifecycle.is_actionable_here())
        .map(|c| build_conversation_view(c, &projects))
        .collect();
    // Live first, then most-recently-active first — a stable, sensible default;
    // the frontend regroups by project/session as needed.
    views.sort_by(|a, b| {
        let a_live = a.lifecycle == "live";
        let b_live = b.lifecycle == "live";
        b_live
            .cmp(&a_live)
            .then_with(|| b.last_activity.cmp(&a.last_activity))
            .then_with(|| a.id.cmp(&b.id))
    });
    views
}

/// Project the registry's LIVE conversations into the session-grouped
/// [`SessionView`] shape the Active view consumes (`/api/active`), plus every
/// live tmux session as a bare/startable entry. The conversation-model
/// replacement for the old session gather: each window carries a stable
/// conversation id and the registry's robust window→conversation resolution +
/// enriched status (so shared-cwd multi-Claude sessions report correct per-window
/// status, which the hook-only path could not). CPU/mem/ports come from the
/// registry sample; `sys` (kept alive by the caller for CPU deltas) is used only
/// to build the per-process breakdown for the info modal.
pub(crate) fn gather_active_views(reg: &ConversationRegistry, sys: &System) -> Vec<SessionView> {
    let sessions = match get_tmux_sessions() {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let other_client_sessions = get_other_client_sessions();
    let skipped_sessions = load_skipped_sessions();
    let auto_approve_sessions = crate::common::persistence::load_auto_approve_sessions();
    let session_todos = load_session_todos();

    // Group live conversations by the tmux session running them.
    let mut convs_by_session: HashMap<String, Vec<&Conversation>> = HashMap::new();
    for c in reg.conversations.values() {
        if !c.lifecycle.is_actionable_here() {
            continue;
        }
        if let Some(p) = &c.placement {
            convs_by_session
                .entry(p.session_name.clone())
                .or_default()
                .push(c);
        }
    }

    let mut results = Vec::new();
    for session in &sessions {
        let is_auto_approve = auto_approve_sessions.contains(&session.name);

        let mut session_convs = convs_by_session.remove(&session.name).unwrap_or_default();
        session_convs.sort_by(|a, b| window_index_of(a).cmp(window_index_of(b)));

        let windows: Vec<WindowView> = session_convs
            .iter()
            .map(|c| conv_to_window_view(c, is_auto_approve))
            .collect();

        let cpu = windows.iter().map(|w| w.cpu).sum();
        let mem_kb = windows.iter().map(|w| w.mem_kb).sum();
        let mut ports: Vec<u16> = windows
            .iter()
            .flat_map(|w| w.ports.iter().copied())
            .collect();
        ports.sort_unstable();
        ports.dedup();
        let last_activity = windows.iter().filter_map(|w| w.last_activity.clone()).max();
        let status = aggregate_status(&windows);

        let session_cwd = session
            .windows
            .first()
            .and_then(|w| w.panes.first())
            .map(|p| p.cwd.clone());

        // Bare session with no Claude: expose the pane only when `claude -c` failed
        // with "No conversation found to continue" (safe to offer a fresh start).
        let mut pane = windows.first().and_then(|w| w.pane.clone());
        let mut claude_continue_failed = false;
        if pane.is_none() {
            if let Some((s, w, p)) = session.windows.first().and_then(|win| {
                win.panes
                    .first()
                    .map(|p| (session.name.clone(), win.index.clone(), p.index.clone()))
            }) {
                if crate::common::tmux::capture_pane(&s, &w, &p)
                    .is_some_and(|t| t.contains("No conversation found to continue"))
                {
                    claude_continue_failed = true;
                    pane = Some((s, w, p));
                }
            }
        }

        // Per-process breakdown (info modal), from this session's Claude pids.
        let mut processes: Vec<ProcessView> = session_convs
            .iter()
            .flat_map(|c| c.pids.iter().copied())
            .filter_map(|pid| {
                get_process_info(sys, pid).map(|info| ProcessView {
                    pid: info.pid,
                    name: info.name,
                    cpu_percent: info.cpu_percent,
                    memory_kb: info.memory_kb,
                    command: info.command,
                })
            })
            .collect();
        processes.sort_by(|a, b| {
            b.cpu_percent
                .partial_cmp(&a.cpu_percent)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        results.push(SessionView {
            name: session.name.clone(),
            status,
            cpu,
            mem_kb,
            ports,
            processes,
            cwd: session_cwd,
            last_activity,
            attached: other_client_sessions.contains(&session.name),
            pane,
            claude_continue_failed,
            skipped: skipped_sessions.contains(&session.name),
            todo_count: session_todos
                .get(&session.name)
                .map(|t| t.len() as u32)
                .unwrap_or(0),
            messages: Vec::new(),
            windows,
        });
    }

    results
}

/// A live conversation's tmux window index (for ordering windows within a session).
fn window_index_of(c: &Conversation) -> &str {
    c.placement
        .as_ref()
        .map(|p| p.window_index.as_str())
        .unwrap_or("")
}

/// Build one [`WindowView`] from a live conversation: its placement + the
/// registry-resolved status/resources, with auto-approve masking applied.
fn conv_to_window_view(c: &Conversation, is_auto_approve: bool) -> WindowView {
    let (session, window_index, window_name, pane_id) = match &c.placement {
        Some(p) => (
            p.session_name.clone(),
            p.window_index.clone(),
            p.window_name.clone(),
            p.pane_id.clone().unwrap_or_default(),
        ),
        None => (String::new(), String::new(), String::new(), String::new()),
    };
    let status = mask_auto_approve(c.status.as_ref().map(|s| s.status.clone()), is_auto_approve);
    WindowView {
        pane_id: pane_id.clone(),
        window_index: window_index.clone(),
        window_name,
        session_id: Some(c.id.as_str().to_string()),
        status,
        cpu: c.cpu,
        mem_kb: c.mem_kb,
        ports: c.ports.clone(),
        cwd: (!c.cwd.is_empty()).then(|| c.cwd.clone()),
        last_activity: c.last_activity.clone(),
        pane: Some((session, window_index, pane_id)),
    }
}

fn build_conversation_view(c: &Conversation, projects: &ProjectRegistry) -> ConversationView {
    let short_id = if c.id.as_str().contains('#') {
        "—".to_string()
    } else {
        c.id.as_str().chars().take(8).collect()
    };
    // parent is "project" or "project/branch"; the project part drives emoji/grouping.
    let project_key = c.parent.as_deref().and_then(|p| {
        let pk = p.split('/').next().unwrap_or(p);
        projects.projects.contains_key(pk).then(|| pk.to_string())
    });
    let emoji = project_key
        .as_deref()
        .and_then(|pk| projects.projects.get(pk))
        .map(|cfg| cfg.emoji.clone())
        .unwrap_or_default();
    // Auth profile = the `.claude-<name>` suffix of the config dir; None = default.
    let auth_profile = c.auth_config_dir.as_ref().and_then(|dir| {
        std::path::Path::new(dir)
            .file_name()
            .and_then(|s| s.to_str())
            .and_then(|b| b.strip_prefix(".claude-"))
            .map(|s| s.to_string())
    });
    let (frozen, frozen_note, frozen_relative) = match &c.frozen {
        Some(f) => (
            true,
            Some(f.note.clone()).filter(|s| !s.is_empty()),
            Some(crate::common::frozen::relative_time(&f.frozen_at)),
        ),
        None => (false, None, None),
    };
    let placement = c.placement.as_ref().map(|p| PlacementView {
        session_name: p.session_name.clone(),
        window_index: p.window_index.clone(),
        window_name: p.window_name.clone(),
        pane_id: p.pane_id.clone(),
    });
    ConversationView {
        id: c.id.as_str().to_string(),
        short_id,
        title: c.title.clone(),
        cwd: (!c.cwd.is_empty()).then(|| c.cwd.clone()),
        lifecycle: if c.lifecycle.is_actionable_here() {
            "live"
        } else {
            "closed"
        }
        .to_string(),
        status: c.status.as_ref().map(|s| s.status.clone()),
        needs_attention: c
            .status
            .as_ref()
            .map(|s| s.needs_attention)
            .unwrap_or(false),
        last_activity: c.last_activity.clone(),
        placement,
        parent: c.parent.clone(),
        project_key,
        emoji,
        frozen,
        frozen_note,
        frozen_relative,
        note: c.note.clone(),
        pinned: c.pinned,
        archived: c.archived,
        auth_profile,
        cpu: c.cpu,
        mem_kb: c.mem_kb,
        ports: c.ports.clone(),
    }
}
