//! Message types for hook-to-TUI communication.
//!
//! The `hive hook` subcommand writes HookState to a JSON file.
//! The TUI reads it on each refresh cycle.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

/// Hook events sent from Claude Code hooks
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HookEvent {
    /// Claude turn ended, waiting for user input
    Stop { session_id: String, cwd: String },
    /// Tool is about to be executed (may or may not need permission)
    PreToolUse {
        session_id: String,
        cwd: String,
        tool_name: String,
        tool_input: Option<serde_json::Value>,
    },
    /// Tool execution completed
    PostToolUse {
        session_id: String,
        cwd: String,
        tool_name: String,
    },
    /// Permission is being requested (user must approve)
    PermissionRequest {
        session_id: String,
        cwd: String,
        tool_name: String,
        tool_input: Option<serde_json::Value>,
    },
    /// User submitted a prompt (used for external input detection)
    UserPromptSubmit { session_id: String, cwd: String },
    /// Notification event from Claude
    Notification {
        session_id: String,
        cwd: String,
        message: String,
    },
}

impl HookEvent {
    /// Get the session_id from any hook event
    pub fn session_id(&self) -> &str {
        match self {
            HookEvent::Stop { session_id, .. } => session_id,
            HookEvent::PreToolUse { session_id, .. } => session_id,
            HookEvent::PostToolUse { session_id, .. } => session_id,
            HookEvent::PermissionRequest { session_id, .. } => session_id,
            HookEvent::UserPromptSubmit { session_id, .. } => session_id,
            HookEvent::Notification { session_id, .. } => session_id,
        }
    }

    /// Get the cwd from any hook event
    pub fn cwd(&self) -> &str {
        match self {
            HookEvent::Stop { cwd, .. } => cwd,
            HookEvent::PreToolUse { cwd, .. } => cwd,
            HookEvent::PostToolUse { cwd, .. } => cwd,
            HookEvent::PermissionRequest { cwd, .. } => cwd,
            HookEvent::UserPromptSubmit { cwd, .. } => cwd,
            HookEvent::Notification { cwd, .. } => cwd,
        }
    }
}

/// Claude status as tracked by hooks
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SessionStatus {
    /// Waiting for user input
    Waiting,
    /// Needs permission for a command
    NeedsPermission {
        tool_name: String,
        description: Option<String>,
    },
    /// Edit approval needed
    EditApproval { filename: String },
    /// Plan ready for review
    PlanReview,
    /// Question asked via AskUserQuestion
    QuestionAsked,
    /// Working/processing
    Working,
    /// Unknown state
    Unknown,
}

/// State of a single Claude session
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    /// Unique session identifier (from Claude Code)
    pub session_id: String,
    /// Working directory
    pub cwd: String,
    /// Current Claude status
    pub status: SessionStatus,
    /// Whether this session needs user attention
    pub needs_attention: bool,
    /// Timestamp of last activity (ISO 8601)
    pub last_activity: Option<String>,
}

/// File-based state shared between `hive hook` and the TUI.
///
/// The hook subcommand loads this, updates the relevant session, and saves it back.
/// The TUI reads it on each refresh cycle.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct HookState {
    /// Active sessions by session_id
    pub sessions: HashMap<String, SessionState>,
}

impl HookState {
    /// Load state from disk
    pub fn load() -> Self {
        let path = get_state_file_path();
        if path.exists() {
            if let Ok(content) = fs::read_to_string(&path) {
                if let Ok(state) = serde_json::from_str(&content) {
                    return state;
                }
            }
        }
        Self::default()
    }

    /// Save state to disk atomically (write to temp file then rename)
    pub fn save(&self) -> std::io::Result<()> {
        let path = get_state_file_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(&self)?;
        let tmp_path = path.with_extension("json.tmp");
        fs::write(&tmp_path, &content)?;
        fs::rename(&tmp_path, &path)?;
        Ok(())
    }

    /// Get a mutable session by ID, creating if needed
    pub fn get_or_create_session(&mut self, session_id: &str, cwd: &str) -> &mut SessionState {
        self.sessions
            .entry(session_id.to_string())
            .or_insert_with(|| SessionState {
                session_id: session_id.to_string(),
                cwd: cwd.to_string(),
                status: SessionStatus::Unknown,
                needs_attention: false,
                last_activity: None,
            })
    }

    /// Remove sessions that haven't been active for more than the given duration
    pub fn cleanup_stale_sessions(&mut self, max_age_secs: i64) {
        let now = chrono::Utc::now();
        self.sessions.retain(|_, session| {
            session
                .last_activity
                .as_ref()
                .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
                .map(|ts| {
                    let age = now.signed_duration_since(ts.with_timezone(&chrono::Utc));
                    age.num_seconds() < max_age_secs
                })
                .unwrap_or(false) // Remove sessions with no timestamp
        });
    }
}

/// State file path for hook-to-TUI communication
pub fn get_state_file_path() -> PathBuf {
    crate::common::persistence::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp/.hive/cache"))
        .join("state.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- HookEvent accessors ---

    #[test]
    fn test_hook_event_session_id_stop() {
        let e = HookEvent::Stop {
            session_id: "s1".into(),
            cwd: "/tmp".into(),
        };
        assert_eq!(e.session_id(), "s1");
        assert_eq!(e.cwd(), "/tmp");
    }

    #[test]
    fn test_hook_event_session_id_all_variants() {
        let variants: Vec<HookEvent> = vec![
            HookEvent::Stop {
                session_id: "a".into(),
                cwd: "/a".into(),
            },
            HookEvent::PreToolUse {
                session_id: "b".into(),
                cwd: "/b".into(),
                tool_name: "Bash".into(),
                tool_input: None,
            },
            HookEvent::PostToolUse {
                session_id: "c".into(),
                cwd: "/c".into(),
                tool_name: "Bash".into(),
            },
            HookEvent::PermissionRequest {
                session_id: "d".into(),
                cwd: "/d".into(),
                tool_name: "Bash".into(),
                tool_input: None,
            },
            HookEvent::UserPromptSubmit {
                session_id: "e".into(),
                cwd: "/e".into(),
            },
            HookEvent::Notification {
                session_id: "f".into(),
                cwd: "/f".into(),
                message: "hi".into(),
            },
        ];

        let expected_ids = ["a", "b", "c", "d", "e", "f"];
        for (event, expected) in variants.iter().zip(expected_ids.iter()) {
            assert_eq!(event.session_id(), *expected);
        }
    }

    // --- HookState ---

    #[test]
    fn test_hook_state_default_empty() {
        let state = HookState::default();
        assert!(state.sessions.is_empty());
    }

    #[test]
    fn test_get_or_create_session_new() {
        let mut state = HookState::default();
        let session = state.get_or_create_session("s1", "/project");

        assert_eq!(session.session_id, "s1");
        assert_eq!(session.cwd, "/project");
        assert_eq!(session.status, SessionStatus::Unknown);
        assert!(!session.needs_attention);
        assert!(session.last_activity.is_none());
    }

    #[test]
    fn test_get_or_create_session_existing() {
        let mut state = HookState::default();
        state.get_or_create_session("s1", "/project");
        state.sessions.get_mut("s1").unwrap().status = SessionStatus::Working;

        // Calling again should return existing, not overwrite
        let session = state.get_or_create_session("s1", "/new-cwd");
        assert_eq!(session.status, SessionStatus::Working);
        // Note: cwd is NOT updated by get_or_create_session (it uses or_insert_with)
    }

    #[test]
    fn test_get_or_create_multiple_sessions() {
        let mut state = HookState::default();
        state.get_or_create_session("s1", "/a");
        state.get_or_create_session("s2", "/b");
        state.get_or_create_session("s3", "/c");

        assert_eq!(state.sessions.len(), 3);
    }

    // --- cleanup_stale_sessions ---

    #[test]
    fn test_cleanup_removes_old_sessions() {
        let mut state = HookState::default();

        // Session with old timestamp (2 hours ago)
        let old_time = (chrono::Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
        state.sessions.insert(
            "old".into(),
            SessionState {
                session_id: "old".into(),
                cwd: "/old".into(),
                status: SessionStatus::Waiting,
                needs_attention: false,
                last_activity: Some(old_time),
            },
        );

        // Session with recent timestamp
        let recent_time = chrono::Utc::now().to_rfc3339();
        state.sessions.insert(
            "recent".into(),
            SessionState {
                session_id: "recent".into(),
                cwd: "/recent".into(),
                status: SessionStatus::Working,
                needs_attention: false,
                last_activity: Some(recent_time),
            },
        );

        // Cleanup sessions older than 1 hour (3600 seconds)
        state.cleanup_stale_sessions(3600);

        assert_eq!(state.sessions.len(), 1);
        assert!(state.sessions.contains_key("recent"));
        assert!(!state.sessions.contains_key("old"));
    }

    #[test]
    fn test_cleanup_removes_sessions_with_no_timestamp() {
        let mut state = HookState::default();
        state.sessions.insert(
            "no-ts".into(),
            SessionState {
                session_id: "no-ts".into(),
                cwd: "/tmp".into(),
                status: SessionStatus::Unknown,
                needs_attention: false,
                last_activity: None,
            },
        );

        state.cleanup_stale_sessions(600);
        assert!(state.sessions.is_empty());
    }

    #[test]
    fn test_cleanup_keeps_all_recent() {
        let mut state = HookState::default();
        let now = chrono::Utc::now().to_rfc3339();

        for i in 0..5 {
            let id = format!("s{}", i);
            state.sessions.insert(
                id.clone(),
                SessionState {
                    session_id: id,
                    cwd: "/tmp".into(),
                    status: SessionStatus::Working,
                    needs_attention: false,
                    last_activity: Some(now.clone()),
                },
            );
        }

        state.cleanup_stale_sessions(600);
        assert_eq!(state.sessions.len(), 5);
    }

    // --- Serialization ---

    #[test]
    fn test_hook_state_serialization_roundtrip() {
        let mut state = HookState::default();
        state.sessions.insert(
            "s1".into(),
            SessionState {
                session_id: "s1".into(),
                cwd: "/project".into(),
                status: SessionStatus::NeedsPermission {
                    tool_name: "Bash: cargo test".into(),
                    description: Some("Run tests".into()),
                },
                needs_attention: true,
                last_activity: Some("2025-01-01T00:00:00Z".into()),
            },
        );

        let json = serde_json::to_string(&state).unwrap();
        let deserialized: HookState = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.sessions.len(), 1);
        let session = &deserialized.sessions["s1"];
        assert_eq!(session.cwd, "/project");
        assert!(session.needs_attention);
        assert!(matches!(
            &session.status,
            SessionStatus::NeedsPermission { tool_name, description }
            if tool_name == "Bash: cargo test" && description.as_deref() == Some("Run tests")
        ));
    }

    #[test]
    fn test_session_status_all_variants_serialize() {
        let variants = vec![
            SessionStatus::Waiting,
            SessionStatus::Working,
            SessionStatus::Unknown,
            SessionStatus::PlanReview,
            SessionStatus::QuestionAsked,
            SessionStatus::NeedsPermission {
                tool_name: "Bash: ls".into(),
                description: None,
            },
            SessionStatus::EditApproval {
                filename: "main.rs".into(),
            },
        ];

        for status in variants {
            let json = serde_json::to_string(&status).unwrap();
            let deserialized: SessionStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(deserialized, status);
        }
    }

    #[test]
    fn test_hook_event_serialization_roundtrip() {
        let event = HookEvent::PermissionRequest {
            session_id: "test".into(),
            cwd: "/home".into(),
            tool_name: "Bash".into(),
            tool_input: Some(serde_json::json!({"command": "ls -la"})),
        };

        let json = serde_json::to_string(&event).unwrap();
        let deserialized: HookEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.session_id(), "test");
        assert_eq!(deserialized.cwd(), "/home");
    }

    // --- Save/Load to disk ---

    #[test]
    fn test_hook_state_save_load_roundtrip() {
        let dir =
            std::env::temp_dir().join(format!("hive-msg-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("state.json");

        let mut state = HookState::default();
        state.sessions.insert(
            "s1".into(),
            SessionState {
                session_id: "s1".into(),
                cwd: "/test".into(),
                status: SessionStatus::Waiting,
                needs_attention: false,
                last_activity: Some(chrono::Utc::now().to_rfc3339()),
            },
        );

        // Write manually to temp path (save() uses hardcoded path)
        let content = serde_json::to_string_pretty(&state).unwrap();
        std::fs::write(&path, &content).unwrap();

        // Read back
        let loaded_content = std::fs::read_to_string(&path).unwrap();
        let loaded: HookState = serde_json::from_str(&loaded_content).unwrap();

        assert_eq!(loaded.sessions.len(), 1);
        assert_eq!(loaded.sessions["s1"].status, SessionStatus::Waiting);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
