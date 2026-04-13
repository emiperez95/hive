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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn new_state() -> HookState {
        HookState::default()
    }

    // --- truncate / extract_filename ---

    #[test]
    fn test_truncate_short() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_exact() {
        assert_eq!(truncate("hello", 5), "hello");
    }

    #[test]
    fn test_truncate_long() {
        assert_eq!(truncate("hello world", 8), "hello...");
    }

    #[test]
    fn test_extract_filename_with_path() {
        assert_eq!(extract_filename("/home/user/file.rs"), "file.rs");
    }

    #[test]
    fn test_extract_filename_just_name() {
        assert_eq!(extract_filename("file.rs"), "file.rs");
    }

    #[test]
    fn test_extract_filename_nested() {
        assert_eq!(extract_filename("/a/b/c/d.txt"), "d.txt");
    }

    // --- Stop event ---

    #[test]
    fn test_stop_sets_waiting() {
        let mut state = new_state();
        let event = HookEvent::Stop {
            session_id: "s1".into(),
            cwd: "/tmp".into(),
        };
        let result = handle_hook_event(&mut state, event).unwrap();
        assert_eq!(result.status, SessionStatus::Waiting);
        assert!(!result.needs_attention);
        assert!(result.last_activity.is_some());
    }

    // --- PreToolUse events ---

    #[test]
    fn test_pre_tool_use_regular_tool_sets_working() {
        let mut state = new_state();
        let event = HookEvent::PreToolUse {
            session_id: "s1".into(),
            cwd: "/tmp".into(),
            tool_name: "Bash".into(),
            tool_input: None,
        };
        let result = handle_hook_event(&mut state, event).unwrap();
        assert_eq!(result.status, SessionStatus::Working);
        assert!(!result.needs_attention);
    }

    #[test]
    fn test_pre_tool_use_exit_plan_mode() {
        let mut state = new_state();
        let event = HookEvent::PreToolUse {
            session_id: "s1".into(),
            cwd: "/tmp".into(),
            tool_name: "ExitPlanMode".into(),
            tool_input: None,
        };
        let result = handle_hook_event(&mut state, event).unwrap();
        assert_eq!(result.status, SessionStatus::PlanReview);
        assert!(result.needs_attention);
    }

    #[test]
    fn test_pre_tool_use_ask_user_question() {
        let mut state = new_state();
        let event = HookEvent::PreToolUse {
            session_id: "s1".into(),
            cwd: "/tmp".into(),
            tool_name: "AskUserQuestion".into(),
            tool_input: None,
        };
        let result = handle_hook_event(&mut state, event).unwrap();
        assert_eq!(result.status, SessionStatus::QuestionAsked);
        assert!(result.needs_attention);
    }

    // --- PostToolUse events ---

    #[test]
    fn test_post_tool_use_sets_working() {
        let mut state = new_state();
        let event = HookEvent::PostToolUse {
            session_id: "s1".into(),
            cwd: "/tmp".into(),
            tool_name: "Bash".into(),
        };
        let result = handle_hook_event(&mut state, event).unwrap();
        assert_eq!(result.status, SessionStatus::Working);
        assert!(!result.needs_attention);
    }

    // --- PermissionRequest events ---

    #[test]
    fn test_permission_request_bash_with_command() {
        let mut state = new_state();
        let event = HookEvent::PermissionRequest {
            session_id: "s1".into(),
            cwd: "/tmp".into(),
            tool_name: "Bash".into(),
            tool_input: Some(json!({
                "command": "cargo test",
                "description": "Run tests"
            })),
        };
        let result = handle_hook_event(&mut state, event).unwrap();
        assert!(result.needs_attention);
        match &result.status {
            SessionStatus::NeedsPermission {
                tool_name,
                description,
            } => {
                assert_eq!(tool_name, "Bash: cargo test");
                assert_eq!(description.as_deref(), Some("Run tests"));
            }
            other => panic!("Expected NeedsPermission, got {:?}", other),
        }
    }

    #[test]
    fn test_permission_request_bash_no_input() {
        let mut state = new_state();
        let event = HookEvent::PermissionRequest {
            session_id: "s1".into(),
            cwd: "/tmp".into(),
            tool_name: "Bash".into(),
            tool_input: None,
        };
        let result = handle_hook_event(&mut state, event).unwrap();
        match &result.status {
            SessionStatus::NeedsPermission { tool_name, .. } => {
                assert_eq!(tool_name, "Bash: ...");
            }
            other => panic!("Expected NeedsPermission, got {:?}", other),
        }
    }

    #[test]
    fn test_permission_request_bash_long_command_truncated() {
        let mut state = new_state();
        let long_cmd = "a".repeat(100);
        let event = HookEvent::PermissionRequest {
            session_id: "s1".into(),
            cwd: "/tmp".into(),
            tool_name: "Bash".into(),
            tool_input: Some(json!({ "command": long_cmd })),
        };
        let result = handle_hook_event(&mut state, event).unwrap();
        match &result.status {
            SessionStatus::NeedsPermission { tool_name, .. } => {
                // "Bash: " + truncated command (60 chars with "...")
                assert!(tool_name.len() < 6 + 100); // shorter than full
                assert!(tool_name.ends_with("..."));
            }
            other => panic!("Expected NeedsPermission, got {:?}", other),
        }
    }

    #[test]
    fn test_permission_request_write() {
        let mut state = new_state();
        let event = HookEvent::PermissionRequest {
            session_id: "s1".into(),
            cwd: "/tmp".into(),
            tool_name: "Write".into(),
            tool_input: Some(json!({ "file_path": "/home/user/src/main.rs" })),
        };
        let result = handle_hook_event(&mut state, event).unwrap();
        assert!(result.needs_attention);
        match &result.status {
            SessionStatus::EditApproval { filename } => {
                assert_eq!(filename, "main.rs");
            }
            other => panic!("Expected EditApproval, got {:?}", other),
        }
    }

    #[test]
    fn test_permission_request_edit() {
        let mut state = new_state();
        let event = HookEvent::PermissionRequest {
            session_id: "s1".into(),
            cwd: "/tmp".into(),
            tool_name: "Edit".into(),
            tool_input: Some(json!({ "file_path": "/a/b/config.toml" })),
        };
        let result = handle_hook_event(&mut state, event).unwrap();
        match &result.status {
            SessionStatus::EditApproval { filename } => {
                assert_eq!(filename, "config.toml");
            }
            other => panic!("Expected EditApproval, got {:?}", other),
        }
    }

    #[test]
    fn test_permission_request_edit_no_input() {
        let mut state = new_state();
        let event = HookEvent::PermissionRequest {
            session_id: "s1".into(),
            cwd: "/tmp".into(),
            tool_name: "Write".into(),
            tool_input: None,
        };
        let result = handle_hook_event(&mut state, event).unwrap();
        match &result.status {
            SessionStatus::EditApproval { filename } => {
                assert_eq!(filename, "file");
            }
            other => panic!("Expected EditApproval, got {:?}", other),
        }
    }

    #[test]
    fn test_permission_request_exit_plan_mode() {
        let mut state = new_state();
        let event = HookEvent::PermissionRequest {
            session_id: "s1".into(),
            cwd: "/tmp".into(),
            tool_name: "ExitPlanMode".into(),
            tool_input: None,
        };
        let result = handle_hook_event(&mut state, event).unwrap();
        assert_eq!(result.status, SessionStatus::PlanReview);
        assert!(result.needs_attention);
    }

    #[test]
    fn test_permission_request_ask_user_question() {
        let mut state = new_state();
        let event = HookEvent::PermissionRequest {
            session_id: "s1".into(),
            cwd: "/tmp".into(),
            tool_name: "AskUserQuestion".into(),
            tool_input: None,
        };
        let result = handle_hook_event(&mut state, event).unwrap();
        assert_eq!(result.status, SessionStatus::QuestionAsked);
        assert!(result.needs_attention);
    }

    #[test]
    fn test_permission_request_unknown_tool() {
        let mut state = new_state();
        let event = HookEvent::PermissionRequest {
            session_id: "s1".into(),
            cwd: "/tmp".into(),
            tool_name: "WebSearch".into(),
            tool_input: None,
        };
        let result = handle_hook_event(&mut state, event).unwrap();
        assert!(result.needs_attention);
        match &result.status {
            SessionStatus::NeedsPermission { tool_name, .. } => {
                assert_eq!(tool_name, "WebSearch: ...");
            }
            other => panic!("Expected NeedsPermission, got {:?}", other),
        }
    }

    #[test]
    fn test_permission_request_task_tool() {
        let mut state = new_state();
        let event = HookEvent::PermissionRequest {
            session_id: "s1".into(),
            cwd: "/tmp".into(),
            tool_name: "Task".into(),
            tool_input: Some(json!({ "command": "npm run build" })),
        };
        let result = handle_hook_event(&mut state, event).unwrap();
        match &result.status {
            SessionStatus::NeedsPermission { tool_name, .. } => {
                assert!(tool_name.starts_with("Bash: "));
            }
            other => panic!("Expected NeedsPermission, got {:?}", other),
        }
    }

    // --- UserPromptSubmit ---

    #[test]
    fn test_user_prompt_submit_sets_working() {
        let mut state = new_state();
        let event = HookEvent::UserPromptSubmit {
            session_id: "s1".into(),
            cwd: "/tmp".into(),
        };
        let result = handle_hook_event(&mut state, event).unwrap();
        assert_eq!(result.status, SessionStatus::Working);
        assert!(!result.needs_attention);
    }

    // --- Notification ---

    #[test]
    fn test_notification_does_not_change_status() {
        let mut state = new_state();
        // First set a known status
        handle_hook_event(
            &mut state,
            HookEvent::Stop {
                session_id: "s1".into(),
                cwd: "/tmp".into(),
            },
        );

        // Now send notification
        let event = HookEvent::Notification {
            session_id: "s1".into(),
            cwd: "/tmp".into(),
            message: "Done!".into(),
        };
        let result = handle_hook_event(&mut state, event).unwrap();
        // Status should remain Waiting (from Stop), not change
        assert_eq!(result.status, SessionStatus::Waiting);
    }

    // --- Session lifecycle ---

    #[test]
    fn test_creates_session_on_first_event() {
        let mut state = new_state();
        assert!(state.sessions.is_empty());
        handle_hook_event(
            &mut state,
            HookEvent::Stop {
                session_id: "new-session".into(),
                cwd: "/project".into(),
            },
        );
        assert_eq!(state.sessions.len(), 1);
        assert!(state.sessions.contains_key("new-session"));
    }

    #[test]
    fn test_updates_cwd_on_event() {
        let mut state = new_state();
        handle_hook_event(
            &mut state,
            HookEvent::Stop {
                session_id: "s1".into(),
                cwd: "/old".into(),
            },
        );
        handle_hook_event(
            &mut state,
            HookEvent::Stop {
                session_id: "s1".into(),
                cwd: "/new".into(),
            },
        );
        assert_eq!(state.sessions["s1"].cwd, "/new");
    }

    #[test]
    fn test_multiple_sessions_independent() {
        let mut state = new_state();
        handle_hook_event(
            &mut state,
            HookEvent::Stop {
                session_id: "s1".into(),
                cwd: "/a".into(),
            },
        );
        handle_hook_event(
            &mut state,
            HookEvent::PreToolUse {
                session_id: "s2".into(),
                cwd: "/b".into(),
                tool_name: "Bash".into(),
                tool_input: None,
            },
        );
        assert_eq!(state.sessions.len(), 2);
        assert_eq!(state.sessions["s1"].status, SessionStatus::Waiting);
        assert_eq!(state.sessions["s2"].status, SessionStatus::Working);
    }

    #[test]
    fn test_status_transitions_full_cycle() {
        let mut state = new_state();
        let sid = "s1";
        let cwd = "/tmp";

        // User submits prompt → Working
        handle_hook_event(
            &mut state,
            HookEvent::UserPromptSubmit {
                session_id: sid.into(),
                cwd: cwd.into(),
            },
        );
        assert_eq!(state.sessions[sid].status, SessionStatus::Working);

        // Tool starts → still Working
        handle_hook_event(
            &mut state,
            HookEvent::PreToolUse {
                session_id: sid.into(),
                cwd: cwd.into(),
                tool_name: "Read".into(),
                tool_input: None,
            },
        );
        assert_eq!(state.sessions[sid].status, SessionStatus::Working);

        // Tool finishes → still Working
        handle_hook_event(
            &mut state,
            HookEvent::PostToolUse {
                session_id: sid.into(),
                cwd: cwd.into(),
                tool_name: "Read".into(),
            },
        );
        assert_eq!(state.sessions[sid].status, SessionStatus::Working);

        // Permission requested → NeedsPermission
        handle_hook_event(
            &mut state,
            HookEvent::PermissionRequest {
                session_id: sid.into(),
                cwd: cwd.into(),
                tool_name: "Bash".into(),
                tool_input: Some(json!({ "command": "rm -rf /" })),
            },
        );
        assert!(state.sessions[sid].needs_attention);
        assert!(matches!(
            state.sessions[sid].status,
            SessionStatus::NeedsPermission { .. }
        ));

        // Post tool (approved) → Working
        handle_hook_event(
            &mut state,
            HookEvent::PostToolUse {
                session_id: sid.into(),
                cwd: cwd.into(),
                tool_name: "Bash".into(),
            },
        );
        assert_eq!(state.sessions[sid].status, SessionStatus::Working);
        assert!(!state.sessions[sid].needs_attention);

        // Claude done → Waiting
        handle_hook_event(
            &mut state,
            HookEvent::Stop {
                session_id: sid.into(),
                cwd: cwd.into(),
            },
        );
        assert_eq!(state.sessions[sid].status, SessionStatus::Waiting);
    }
}
