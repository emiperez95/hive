//! Session data gathering for the web dashboard.
//!
//! Provides `gather_session_data()` — a TUI-state-free version of `App::refresh()`
//! that collects local tmux/sysinfo/hook data into serializable structs for the
//! web API.
//!
//! A single tmux session can host several Claude instances (one per window). This
//! builds one [`WindowView`] per Claude instance via the shared
//! [`crate::common::instances`] core, then aggregates them into a session-level
//! [`SessionView`]. The web dashboard renders multi-window sessions as an accordion.

use crate::common::instances::{detect_claude_instances, ClaudeInstance, HookIndex};
use crate::common::persistence::{load_session_todos, load_skipped_sessions};
use crate::common::ports::get_listening_ports_for_pids;
use crate::common::process::{build_children_map, get_process_info};
use crate::common::tmux::{get_other_client_sessions, get_tmux_sessions};
use crate::ipc::messages::{HookState, SessionStatus};
use crate::serve::web_types::{ProcessView, SessionView, WindowView};

use std::collections::HashMap;
use sysinfo::System;

/// Gather session data from local tmux + sysinfo + hook state.
/// This is a simplified version of App::refresh() that doesn't need TUI state.
pub(crate) fn gather_session_data(sys: &System, hook_state: &HookState) -> Vec<SessionView> {
    let sessions = match get_tmux_sessions() {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    let other_client_sessions = get_other_client_sessions();
    let hook_index = HookIndex::build(hook_state);

    // Detect every Claude instance across all sessions in one pass, then group by session.
    let children_map = build_children_map();
    let instances = detect_claude_instances(&sessions, sys, &children_map, &hook_index);
    let mut instances_by_session: HashMap<String, Vec<ClaudeInstance>> = HashMap::new();
    for inst in instances {
        instances_by_session
            .entry(inst.session_name.clone())
            .or_default()
            .push(inst);
    }

    let skipped_sessions = load_skipped_sessions();
    let auto_approve_sessions = crate::common::persistence::load_auto_approve_sessions();
    let session_todos = load_session_todos();
    let mut results = Vec::new();

    for session in &sessions {
        let session_cwd = session
            .windows
            .first()
            .and_then(|w| w.panes.first())
            .map(|p| p.cwd.clone());

        let is_auto_approve = auto_approve_sessions.contains(&session.name);

        // Build one window per Claude instance in this session (ordered by window index).
        let mut session_instances = instances_by_session
            .remove(&session.name)
            .unwrap_or_default();
        session_instances.sort_by(|a, b| a.window_index.cmp(&b.window_index));

        let windows: Vec<WindowView> = session_instances
            .iter()
            .map(|inst| build_window_view(inst, sys, &hook_index, is_auto_approve))
            .collect();

        // Aggregate the windows into session-level fields. Single-window sessions
        // mirror their one window so existing single-Claude behaviour is unchanged.
        let cpu = windows.iter().map(|w| w.cpu).sum();
        let mem_kb = windows.iter().map(|w| w.mem_kb).sum();
        let mut ports: Vec<u16> = windows.iter().flat_map(|w| w.ports.iter().copied()).collect();
        ports.sort_unstable();
        ports.dedup();
        let last_activity = windows.iter().filter_map(|w| w.last_activity.clone()).max();
        let pane = windows.first().and_then(|w| w.pane.clone());
        let status = aggregate_status(&windows);

        // Resources are counted from the Claude panes' process trees only.
        let mut processes: Vec<ProcessView> = session_instances
            .iter()
            .flat_map(|inst| inst.pids.iter().copied())
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

/// Build a [`WindowView`] for a single Claude instance: per-window status, CPU/mem, ports.
fn build_window_view(
    inst: &ClaudeInstance,
    sys: &System,
    hook_index: &HookIndex,
    is_auto_approve: bool,
) -> WindowView {
    // Status: prefer the hook session bound to this pane; fall back to the instance's
    // own jsonl transcript (by session_id when known); else Unknown.
    let (status, last_activity) = if let Some(hook) = hook_index.resolve(&inst.pane_id, &inst.cwd) {
        (Some(hook.status.clone()), hook.last_activity.clone())
    } else if let Some(jsonl) = crate::common::jsonl::get_claude_status_from_jsonl_for(
        &inst.cwd,
        inst.session_id.as_deref(),
    ) {
        (
            Some(convert_claude_to_session_status(&jsonl.status)),
            jsonl.timestamp.map(|t| t.to_rfc3339()),
        )
    } else {
        (Some(SessionStatus::Unknown), None)
    };

    // An idle main thread may still have a workflow / background agent running.
    // Override Waiting with the in-flight summary so the dashboard shows it as busy.
    let status = if matches!(status, Some(SessionStatus::Waiting)) {
        crate::common::jsonl::background_running_summary(&inst.cwd, inst.session_id.as_deref())
            .map(|summary| SessionStatus::RunningWorkflow { summary })
            .or(status)
    } else {
        status
    };

    let status = mask_auto_approve(status, is_auto_approve);

    let mut cpu = 0.0f32;
    let mut mem_kb = 0u64;
    for &pid in &inst.pids {
        if let Some(info) = get_process_info(sys, pid) {
            cpu += info.cpu_percent;
            mem_kb += info.memory_kb;
        }
    }

    let ports: Vec<u16> = get_listening_ports_for_pids(&inst.pids, sys)
        .iter()
        .map(|lp| lp.port)
        .collect();

    let (s, w, p) = inst.target();
    WindowView {
        pane_id: inst.pane_id.clone(),
        window_index: inst.window_index.clone(),
        window_name: inst.window_name.clone(),
        session_id: inst.session_id.clone(),
        status,
        cpu,
        mem_kb,
        ports,
        cwd: Some(inst.cwd.clone()),
        last_activity,
        pane: Some((s, w, p)),
    }
}

/// Auto-approved sessions should surface as Working, never as a permission/edit prompt.
fn mask_auto_approve(status: Option<SessionStatus>, is_auto_approve: bool) -> Option<SessionStatus> {
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

/// Convert a TUI `ClaudeStatus` (parsed from JSONL) back to wire `SessionStatus`.
fn convert_claude_to_session_status(status: &crate::common::types::ClaudeStatus) -> SessionStatus {
    use crate::common::types::ClaudeStatus;
    match status {
        ClaudeStatus::Waiting => SessionStatus::Waiting,
        ClaudeStatus::NeedsPermission(tool, desc) => SessionStatus::NeedsPermission {
            tool_name: tool.clone(),
            description: desc.clone(),
        },
        ClaudeStatus::EditApproval(filename) => SessionStatus::EditApproval {
            filename: filename.clone(),
        },
        ClaudeStatus::PlanReview => SessionStatus::PlanReview,
        ClaudeStatus::QuestionAsked => SessionStatus::QuestionAsked,
        ClaudeStatus::RunningWorkflow(summary) => SessionStatus::RunningWorkflow {
            summary: summary.clone(),
        },
        ClaudeStatus::Unknown => SessionStatus::Working,
    }
}
