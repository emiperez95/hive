//! Serializable types for the web dashboard JSON API.
//!
//! `SessionView` is produced by `serve::server::gather_session_data()` and
//! returned by `/api/sessions`. `ConversationMessage` is returned by
//! `/api/messages`.

use crate::ipc::messages::SessionStatus;
use serde::{Deserialize, Serialize};

fn is_zero(v: &u32) -> bool {
    *v == 0
}

/// One session as exposed to the web dashboard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionView {
    /// tmux session name
    pub name: String,
    /// Claude status from hook state (None if no Claude process)
    pub status: Option<SessionStatus>,
    /// Total CPU usage across all processes
    pub cpu: f32,
    /// Total memory usage in KB
    pub mem_kb: u64,
    /// Listening TCP ports
    pub ports: Vec<u16>,
    /// Process info for display
    pub processes: Vec<ProcessView>,
    /// Working directory (from first pane)
    pub cwd: Option<String>,
    /// Last activity timestamp (ISO 8601)
    pub last_activity: Option<String>,
    /// Session is attached to another tmux client
    pub attached: bool,
    /// (session, window, pane) for routing send-keys
    pub pane: Option<(String, String, String)>,
    /// Session is in the skipped list
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub skipped: bool,
    /// Active todo count for this session
    #[serde(default, skip_serializing_if = "is_zero")]
    pub todo_count: u32,
    /// Conversation messages for the dashboard (user + assistant)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<ConversationMessage>,
}

/// One message in the conversation (user or assistant text).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationMessage {
    pub role: String,
    /// Text content (may be empty if message is tool-use only)
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    /// Tool uses in this assistant message
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolSummary>,
}

/// Compact summary of a tool use for the dashboard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSummary {
    /// Tool name (Bash, Write, Edit, Read, Grep, etc.)
    pub name: String,
    /// Short display text (command, file path, etc.)
    pub summary: String,
    /// Full detail for modal view (full command, content, etc.)
    pub detail: String,
}

/// Minimal process info for the dashboard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessView {
    pub pid: u32,
    pub name: String,
    pub cpu_percent: f32,
    pub memory_kb: u64,
    pub command: String,
}
