//! Hook event processing from stdin.

use anyhow::Result;

use crate::common::persistence::{
    is_globally_muted, load_auto_approve_sessions, load_muted_sessions,
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

    if let Some(updated_session) = handle_hook_event(&mut state, hook_event) {
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

            if !global_mute && !muted.contains(session_name) {
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
