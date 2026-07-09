//! Hook event processing from stdin.

use anyhow::Result;

use crate::common::persistence::{
    is_globally_muted, load_auto_approve_sessions, load_muted_projects, load_muted_sessions,
};
use crate::common::tmux::get_current_tmux_session;
use crate::daemon::hooks::handle_hook_event;
use crate::daemon::notifier::notify_needs_attention;
use crate::ipc::messages::{HookEvent, HookState, SessionStatus};

/// Process a hook event from stdin
pub fn run_hook(event_type: &str) -> Result<()> {
    use std::io::BufRead;

    // Read JSON from stdin
    let stdin = std::io::stdin();
    let mut input = String::new();
    let reader = stdin.lock();
    if let Some(line) = reader.lines().next() {
        let line = line?;
        input.push_str(&line);
    }

    if input.trim().is_empty() {
        return Ok(());
    }

    // Parse the input JSON
    let json: serde_json::Value =
        serde_json::from_str(&input).unwrap_or_else(|_| serde_json::json!({}));

    let session_id = json
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let cwd = json
        .get("cwd")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // The hook runs inside the Claude pane, so TMUX_PANE identifies which tmux pane
    // this session_id lives in. Recording it lets the TUI/web tell apart multiple
    // Claude instances that share a working directory (one per window/pane).
    let session_id_for_pane = session_id.clone();
    let cwd_for_record = cwd.clone();
    let tmux_pane = std::env::var("TMUX_PANE").ok().filter(|s| !s.is_empty());

    // SessionEnd is a lifecycle event, not a status change: drop the window from the recovery
    // snapshot and log a clean close. Handled here since it has no HookEvent variant.
    if event_type == "SessionEnd" {
        let mut open = crate::common::activity::OpenWindowsState::load();
        let session_name = open
            .windows
            .get(&session_id)
            .map(|w| w.session_name.clone())
            .unwrap_or_default();
        if open.remove(&session_id) {
            let _ = open.save();
        }
        crate::common::activity::log_window_close(&session_id, &session_name);
        return Ok(());
    }

    // Build HookEvent based on event type
    let hook_event = match event_type {
        "Stop" => HookEvent::Stop { session_id, cwd },
        "PreToolUse" => {
            let tool_name = json
                .get("tool_name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let tool_input = json.get("tool_input").cloned();
            HookEvent::PreToolUse {
                session_id,
                cwd,
                tool_name,
                tool_input,
            }
        }
        "PostToolUse" => {
            let tool_name = json
                .get("tool_name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            HookEvent::PostToolUse {
                session_id,
                cwd,
                tool_name,
            }
        }
        "PermissionRequest" => {
            let tool_name = json
                .get("tool_name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let tool_input = json.get("tool_input").cloned();
            HookEvent::PermissionRequest {
                session_id,
                cwd,
                tool_name,
                tool_input,
            }
        }
        "UserPromptSubmit" => HookEvent::UserPromptSubmit { session_id, cwd },
        "Notification" => {
            let message = json
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            HookEvent::Notification {
                session_id,
                cwd,
                message,
            }
        }
        _ => {
            eprintln!("Unknown hook event type: {}", event_type);
            return Ok(());
        }
    };

    // Load state, process event, save state
    let mut state = HookState::load();

    // Check auto-approve before notifications so we can skip alerting for auto-approved requests
    // Skip auto-approve for plans (ExitPlanMode) and questions (AskUserQuestion) — those need human input
    let mut auto_approved = false;
    let tool_name_str = json.get("tool_name").and_then(|v| v.as_str()).unwrap_or("");
    let is_human_input = tool_name_str == "ExitPlanMode" || tool_name_str == "AskUserQuestion";
    if event_type == "PermissionRequest" && !is_human_input {
        if let Some(tmux_session) = get_current_tmux_session() {
            let auto_approve = load_auto_approve_sessions();
            if auto_approve.contains(&tmux_session) {
                auto_approved = true;
                println!(
                    "{}",
                    serde_json::json!({
                        "hookSpecificOutput": {
                            "hookEventName": "PermissionRequest",
                            "decision": {
                                "behavior": "allow"
                            }
                        }
                    })
                );
            }
        }
    }

    let updated = handle_hook_event(&mut state, hook_event);

    // Record which tmux pane this session is running in (pane-id → session_id link).
    if let Some(pane) = &tmux_pane {
        if let Some(session) = state.sessions.get_mut(&session_id_for_pane) {
            session.tmux_pane = Some(pane.clone());
        }
        // Mirror the Claude conversation title (set by `/rename` or auto-generated) into the
        // tmux window name. The hook runs inside the Claude pane, so this is event-driven —
        // every hook fire keeps the window name current without polling.
        crate::common::tmux::sync_window_name_for_pane(pane);

        // Record this window into the "currently open" snapshot so it can be recovered after
        // a machine restart. Query the pane (post-sync, so #{window_name} carries the latest
        // Claude title) for its session + window identity; the auth profile comes from this
        // hook process's own env (inherited from the Claude pane).
        if let Some(info) = crate::common::tmux::display_message_for_pane(
            pane,
            "#{session_name}\t#{window_index}\t#{window_name}",
        ) {
            let mut parts = info.splitn(3, '\t');
            if let (Some(session_name), Some(window_index), Some(window_name)) =
                (parts.next(), parts.next(), parts.next())
            {
                crate::common::activity::record_window_seen(&crate::common::activity::WindowSeen {
                    claude_session_id: session_id_for_pane.clone(),
                    session_name: session_name.to_string(),
                    window_index: window_index.to_string(),
                    window_name: window_name.to_string(),
                    cwd: cwd_for_record.clone(),
                    claude_config_dir: std::env::var("CLAUDE_CONFIG_DIR")
                        .ok()
                        .filter(|s| !s.is_empty()),
                });
            }
        }
    }

    if let Some(updated_session) = updated {
        // Send notification if session needs attention, not muted, and not auto-approved
        if updated_session.needs_attention && !auto_approved {
            let muted = load_muted_sessions();
            let global_mute = is_globally_muted();

            // Try to find the tmux session name by matching cwd
            let session_name = updated_session
                .cwd
                .rsplit('/')
                .next()
                .unwrap_or(&updated_session.session_id);

            // Project-level mute (a remembered preference): resolve the cwd's
            // project and suppress if it's muted. Guarded so the common case (no
            // muted projects) pays nothing beyond a tiny file read.
            let project_muted = {
                let muted_projects = load_muted_projects();
                !muted_projects.is_empty()
                    && crate::common::registry::resolve_parent(
                        &updated_session.cwd,
                        &crate::common::worktree::WorktreeState::load(),
                        &crate::common::projects::ProjectRegistry::load(),
                    )
                    .map(|p| p.split('/').next().unwrap_or(&p).to_string())
                    .map(|k| muted_projects.contains(&k))
                    .unwrap_or(false)
            };

            if !global_mute && !project_muted && !muted.contains(session_name) {
                let status_text = match &updated_session.status {
                    SessionStatus::NeedsPermission { tool_name, .. } => {
                        format!("needs permission: {}", tool_name)
                    }
                    SessionStatus::EditApproval { filename } => {
                        format!("edit approval: {}", filename)
                    }
                    SessionStatus::PlanReview => "plan ready".to_string(),
                    SessionStatus::QuestionAsked => "question asked".to_string(),
                    _ => "needs attention".to_string(),
                };
                notify_needs_attention(session_name, &status_text);
            }
        }
    }

    // Clean up stale sessions (>10 minutes inactive)
    state.cleanup_stale_sessions(600);

    // Save state atomically
    state.save()?;

    Ok(())
}
