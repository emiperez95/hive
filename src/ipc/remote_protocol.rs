//! Wire protocol types for remote session communication.
//!
//! Server writes JSON lines to stdout, client writes JSON lines to stdin.
//! Transport is SSH stdio — one JSON object per line, newline-delimited.

use crate::ipc::messages::SessionStatus;
use serde::{Deserialize, Serialize};

/// Server → Client messages (written as JSON lines to stdout)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ServerMessage {
    /// Full snapshot of all sessions on this machine
    State {
        sessions: Vec<RemoteSessionData>,
    },
    /// Keep-alive signal (sent every 3s if no state change)
    Heartbeat,
}

/// Session data sent from the remote server to the client
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteSessionData {
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
    pub processes: Vec<RemoteProcessInfo>,
    /// Working directory (from first pane)
    pub cwd: Option<String>,
    /// Last activity timestamp (ISO 8601)
    pub last_activity: Option<String>,
    /// Session is attached to another tmux client
    pub attached: bool,
    /// (session, window, pane) for routing send-keys
    pub pane: Option<(String, String, String)>,
}

/// Minimal process info for remote display
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteProcessInfo {
    pub pid: u32,
    pub name: String,
    pub cpu_percent: f32,
    pub memory_kb: u64,
    pub command: String,
}

/// Client → Server messages (written as JSON lines to stdin)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientMessage {
    /// Send keys to a specific tmux pane on the remote
    SendKeys {
        session: String,
        window: String,
        pane: String,
        keys: Vec<String>,
    },
}
