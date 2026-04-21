//! Session data gathering for the web dashboard.
//!
//! Provides `gather_session_data()` — a TUI-state-free version of `App::refresh()`
//! that collects local tmux/sysinfo/hook data into serializable structs for the
//! web API.

use crate::common::persistence::{load_session_todos, load_skipped_sessions};
use crate::common::ports::get_listening_ports_for_pids;
use crate::common::process::{get_all_descendants, get_process_info, is_claude_process};
use crate::common::tmux::{get_other_client_sessions, get_tmux_sessions};
use crate::ipc::messages::{HookState, SessionStatus};
use crate::serve::protocol::{RemoteProcessInfo, RemoteSessionData};

use std::collections::HashMap;
use sysinfo::System;

/// Gather session data from local tmux + sysinfo + hook state.
/// This is a simplified version of App::refresh() that doesn't need TUI state.
pub(crate) fn gather_session_data(sys: &System, hook_state: &HookState) -> Vec<RemoteSessionData> {
    let sessions = match get_tmux_sessions() {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    let other_client_sessions = get_other_client_sessions();

    // Index hook sessions by cwd (most recent per cwd)
    let hook_sessions: HashMap<String, &crate::ipc::messages::SessionState> = {
        let mut by_cwd: HashMap<String, &crate::ipc::messages::SessionState> = HashMap::new();
        for session in hook_state.sessions.values() {
            let key = session.cwd.clone();
            let is_newer = by_cwd
                .get(&key)
                .is_none_or(|existing| session.last_activity > existing.last_activity);
            if is_newer {
                by_cwd.insert(key, session);
            }
        }
        by_cwd
    };

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

        // Find Claude pane first, then only count resources from that pane's tree.
        // This avoids counting hive web (which may run in another pane) and its descendants.
        let mut status: Option<SessionStatus> = None;
        let mut claude_pane: Option<(String, String, String)> = None;
        let mut last_activity: Option<String> = None;
        let mut claude_pids: Vec<u32> = Vec::new();
        'outer: for window in &session.windows {
            for p in &window.panes {
                let mut pane_pids = vec![p.pid];
                get_all_descendants(sys, p.pid, &mut pane_pids);

                let has_claude = pane_pids.iter().any(|&pid| {
                    get_process_info(sys, pid)
                        .map(|info| is_claude_process(&info))
                        .unwrap_or(false)
                });

                if has_claude {
                    claude_pids = pane_pids;
                    if let Some(hook_session) = hook_sessions.get(&p.cwd) {
                        status = Some(hook_session.status.clone());
                        last_activity = hook_session.last_activity.clone();
                    } else if let Some(jsonl_status) =
                        crate::common::jsonl::get_claude_status_from_jsonl(&p.cwd)
                    {
                        status = Some(convert_claude_to_session_status(&jsonl_status.status));
                        last_activity = jsonl_status
                            .timestamp
                            .map(|t| t.to_rfc3339());
                    } else {
                        status = Some(SessionStatus::Unknown);
                    }
                    claude_pane = Some((
                        session.name.clone(),
                        window.index.clone(),
                        p.index.clone(),
                    ));
                    break 'outer;
                }
            }
        }

        // Count resources only from the Claude pane's process tree
        let mut total_cpu = 0.0f32;
        let mut total_mem_kb = 0u64;
        let mut processes = Vec::new();

        for &pid in &claude_pids {
            if let Some(info) = get_process_info(sys, pid) {
                total_cpu += info.cpu_percent;
                total_mem_kb += info.memory_kb;
                processes.push(RemoteProcessInfo {
                    pid: info.pid,
                    name: info.name.clone(),
                    cpu_percent: info.cpu_percent,
                    memory_kb: info.memory_kb,
                    command: info.command.clone(),
                });
            }
        }

        processes.sort_by(|a, b| {
            b.cpu_percent
                .partial_cmp(&a.cpu_percent)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Detect listening ports from Claude pane processes
        let listening_ports = get_listening_ports_for_pids(&claude_pids, sys);
        let ports: Vec<u16> = listening_ports.iter().map(|lp| lp.port).collect();

        // Auto-approved sessions should only show Working, not NeedsPermission/EditApproval
        let status = if auto_approve_sessions.contains(&session.name) {
            match &status {
                Some(SessionStatus::NeedsPermission { .. })
                | Some(SessionStatus::EditApproval { .. }) => Some(SessionStatus::Working),
                other => other.clone(),
            }
        } else {
            status
        };

        results.push(RemoteSessionData {
            name: session.name.clone(),
            status,
            cpu: total_cpu,
            mem_kb: total_mem_kb,
            ports,
            processes,
            cwd: session_cwd,
            last_activity,
            attached: other_client_sessions.contains(&session.name),
            pane: claude_pane,
            skipped: skipped_sessions.contains(&session.name),
            todo_count: session_todos
                .get(&session.name)
                .map(|t| t.len() as u32)
                .unwrap_or(0),
            messages: Vec::new(),
        });
    }

    results
}

/// Convert a TUI `ClaudeStatus` (parsed from JSONL) back to wire `SessionStatus`.
fn convert_claude_to_session_status(
    status: &crate::common::types::ClaudeStatus,
) -> SessionStatus {
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
        ClaudeStatus::Unknown => SessionStatus::Working,
    }
}
