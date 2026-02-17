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
