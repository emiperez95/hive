//! Hook event handlers — update HookState based on incoming events.

use crate::ipc::messages::{HookEvent, HookState, SessionState, SessionStatus};
use chrono::Utc;

/// Handle a hook event and update state. Returns the updated session if any.
pub fn handle_hook_event(state: &mut HookState, event: HookEvent) -> Option<SessionState> {
    let session_id = event.session_id().to_string();
    let cwd = event.cwd().to_string();
    let now = Utc::now().to_rfc3339();

    // Ensure session exists
    let _ = state.get_or_create_session(&session_id, &cwd);

    // Compute new status and fields based on the event
    let (new_status, new_needs_attention) = match &event {
        HookEvent::Stop { .. } => (Some(SessionStatus::Waiting), Some(false)),

        HookEvent::PreToolUse { tool_name, .. } => {
            let status = match tool_name.as_str() {
                "ExitPlanMode" => SessionStatus::PlanReview,
                "AskUserQuestion" => SessionStatus::QuestionAsked,
                _ => SessionStatus::Working,
            };
            let needs_attention = matches!(
                status,
                SessionStatus::PlanReview | SessionStatus::QuestionAsked
            );
            (Some(status), Some(needs_attention))
        }

        HookEvent::PermissionRequest {
            tool_name,
            tool_input,
            ..
        } => {
            let status = match tool_name.as_str() {
                "Bash" | "Task" => {
                    let description = tool_input.as_ref().and_then(|input| {
                        input
                            .get("description")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                    });
                    let command = tool_input
                        .as_ref()
                        .and_then(|input| {
                            input
                                .get("command")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string())
                        })
                        .unwrap_or_else(|| "...".to_string());
                    SessionStatus::NeedsPermission {
                        tool_name: format!("Bash: {}", truncate(&command, 60)),
                        description,
                    }
                }
                "Write" | "Edit" => {
                    let filename = tool_input
                        .as_ref()
                        .and_then(|input| {
                            input
                                .get("file_path")
                                .and_then(|v| v.as_str())
                                .map(extract_filename)
                        })
                        .unwrap_or_else(|| "file".to_string());
                    SessionStatus::EditApproval { filename }
                }
                "ExitPlanMode" => SessionStatus::PlanReview,
                "AskUserQuestion" => SessionStatus::QuestionAsked,
                _ => SessionStatus::NeedsPermission {
                    tool_name: format!("{}: ...", tool_name),
                    description: None,
                },
            };
            (Some(status), Some(true))
        }

        HookEvent::PostToolUse { .. } => (Some(SessionStatus::Working), Some(false)),

        HookEvent::UserPromptSubmit { .. } => (Some(SessionStatus::Working), Some(false)),

        HookEvent::Notification { .. } => {
            // Notifications don't change status, but we update last activity
            (None, None)
        }
    };

    // Update the session
    let session = state.sessions.get_mut(&session_id)?;
    session.last_activity = Some(now);
    session.cwd = cwd;

    if let Some(status) = new_status {
        session.status = status;
    }
    if let Some(needs_attention) = new_needs_attention {
        session.needs_attention = needs_attention;
    }

    Some(session.clone())
}

/// Truncate a string to max length with ellipsis
fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len - 3])
    }
}

/// Extract filename from a full path
fn extract_filename(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}
