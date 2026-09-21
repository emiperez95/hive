//! Serializable types for the web dashboard JSON API.
//!
//! `SessionView` is produced by `serve::server::gather_active_views()` (the
//! Active view projected from the conversation registry) and returned by
//! `/api/active`; `ConversationView` by `build_conversation_views()` for
//! `/api/conversations`. `ConversationMessage` is returned by `/api/messages`.

use crate::common::jsonl;
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
    /// No Claude is running AND the pane shows `claude -c`'s "No conversation found to
    /// continue" error — i.e. it's safe to offer starting a fresh Claude. False otherwise.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub claude_continue_failed: bool,
    /// Session is in the skipped list
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub skipped: bool,
    /// Active todo count for this session
    #[serde(default, skip_serializing_if = "is_zero")]
    pub todo_count: u32,
    /// Conversation messages for the dashboard (user + assistant)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<ConversationMessage>,
    /// One entry per Claude instance running in this tmux session. When more than
    /// one window is present the web dashboard renders the session as an accordion
    /// so a specific window can be selected. Empty when no Claude is running.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub windows: Vec<WindowView>,
}

/// One Claude instance (tmux window/pane) within a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowView {
    /// tmux global pane id (e.g. "%1") — the unique selector used by the web API.
    pub pane_id: String,
    /// tmux window index.
    pub window_index: String,
    /// tmux window name.
    pub window_name: String,
    /// Resolved Claude session id (jsonl basename), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Claude status for this specific window.
    pub status: Option<SessionStatus>,
    /// CPU usage across this window's Claude process tree.
    pub cpu: f32,
    /// Memory usage in KB across this window's Claude process tree.
    pub mem_kb: u64,
    /// Listening TCP ports for this window's processes.
    pub ports: Vec<u16>,
    /// Working directory of the window's pane.
    pub cwd: Option<String>,
    /// Last activity timestamp (ISO 8601) for this window.
    pub last_activity: Option<String>,
    /// (session, window, pane) for routing send-keys to this window.
    pub pane: Option<(String, String, String)>,
    /// Attention tier: 0 blocked · 1 ready for review · 2 working · 3 idle.
    ///
    /// Computed in Rust rather than in the frontend so there is exactly one
    /// definition of "needs you" — the JS has no test harness, and a second
    /// implementation would drift from `SessionStatus::blocks_human`.
    #[serde(default)]
    pub attention: u8,
    /// This window's worktree holds work nobody has looked at. Only ever computed
    /// for idle windows (see `annotate_attention`).
    #[serde(default)]
    pub unreviewed_work: bool,
    /// Seconds this window has held its current status, or `None` when it was
    /// already in that status the first time hive saw it (i.e. since the web server
    /// last started). A sort key only — never displayed, because after a restart a
    /// conversation blocked for an hour would otherwise read as brand new.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_secs: Option<u32>,
}

/// One conversation as exposed to the web dashboard's new conversation-first API
/// (`/api/conversations`). A faithful serialization of a [`crate::common::registry::Conversation`]
/// plus a few derived conveniences (short id, project emoji/key, auth profile,
/// frozen relative time) the frontend would otherwise recompute.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationView {
    /// Claude conversation UUID (the transcript basename) — the stable key.
    pub id: String,
    /// First 8 chars of the id ("—" for legacy composite `session#window` keys).
    pub short_id: String,
    /// User/AI conversation title, when the transcript carries one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Working directory the conversation runs/ran in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// "live" (running here now) or "closed" (known, resumable, not running here).
    pub lifecycle: String,
    /// Activity status (Working/Waiting/NeedsPermission/…); None for closed with
    /// no recovered status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<SessionStatus>,
    /// Whether the status is one requiring the user to act.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub needs_attention: bool,
    /// Last activity timestamp (ISO 8601).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity: Option<String>,
    /// Where it currently runs (live only) — session/window/pane for routing actions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<PlacementView>,
    /// Logical parent key (a project key like "hive" or a worktree key "hive/CSD-1").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Registered project key this belongs to (parent's project part), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_key: Option<String>,
    /// Project emoji for display (empty when no registered project).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub emoji: String,
    /// Frozen facet: this is a hibernated (pinned+noted Closed) conversation.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub frozen: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frozen_note: Option<String>,
    /// Human "3h ago" relative to when it was frozen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frozen_relative: Option<String>,
    /// Free-text overlay note (from conversations.json).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pinned: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub archived: bool,
    /// Auth profile to resume under ("work" etc.); None = default `~/.claude`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_profile: Option<String>,
    /// Live CPU% across the conversation's process tree (0 for closed).
    #[serde(default, skip_serializing_if = "is_zero_f32")]
    pub cpu: f32,
    /// Live memory (KB) across the conversation's process tree (0 for closed).
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub mem_kb: u64,
    /// Listening TCP ports for the live process tree (empty for closed).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<u16>,
}

fn is_zero_f32(v: &f32) -> bool {
    *v == 0.0
}
fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

/// Where a live conversation currently runs, for routing send/switch/freeze.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlacementView {
    pub session_name: String,
    pub window_index: String,
    pub window_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
}

/// One message in the conversation (user or assistant text).
///
/// `role` is `user` | `assistant`, plus one synthetic value: **`workflow`**, carrying
/// `running` — a background launch that hasn't reported back. Those have no transcript
/// entry of their own (the launch is wherever Claude called the tool, often hundreds of
/// messages earlier), so the server appends them after the last real message and the
/// chat renders them as live cards at the end.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationMessage {
    pub role: String,
    /// Text content (may be empty if message is tool-use only)
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    /// Tool uses in this assistant message
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolSummary>,
    /// Set when this entry is a `<task-notification>`: the parsed completion report.
    /// The raw markup is stripped from `text`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<TaskNotificationView>,
    /// Set on synthetic `role: "workflow"` entries: a launch still in flight.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running: Option<RunningTaskView>,
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
    /// `Workflow` launches only: the script's `meta`, so the card can show the plan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<WorkflowMetaView>,
}

/// A workflow's declared `meta` — name and the phases it will run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowMetaView {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub phases: Vec<WorkflowPhaseView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowPhaseView {
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

/// A background launch's completion report (`<task-notification>`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskNotificationView {
    pub status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub summary: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub result: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub output_file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TaskUsageView>,
}

/// Per-run totals from a workflow notification. `agents_error` is the one that
/// matters most and has no other surface: a workflow reports `completed` while
/// some of its agents failed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskUsageView {
    pub agent_count: u32,
    pub agents_done: u32,
    pub agents_error: u32,
    pub agents_skipped: u32,
    pub subagent_tokens: u64,
    pub tool_uses: u32,
    pub duration_ms: u64,
}

/// An in-flight background launch, rendered as a live card at the end of the chat.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunningTaskView {
    /// `workflow` | `agent` | `bash`
    pub kind: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// ISO 8601. The elapsed clock ticks client-side from this: a server-rendered
    /// duration would differ on every 2s poll and defeat the chat's re-render guard.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub phases: Vec<WorkflowPhaseView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents_started: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents_done: Option<u32>,
}

// ── jsonl → wire conversions ────────────────────────────────────────────────
// The parsing lives in `common::jsonl` (shared with the TUI); these are the
// serializable projections of it.

impl From<jsonl::WorkflowPhase> for WorkflowPhaseView {
    fn from(p: jsonl::WorkflowPhase) -> Self {
        Self {
            title: p.title,
            detail: p.detail,
        }
    }
}

impl From<jsonl::WorkflowMeta> for WorkflowMetaView {
    fn from(m: jsonl::WorkflowMeta) -> Self {
        Self {
            name: m.name,
            description: m.description,
            phases: m.phases.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<jsonl::TaskUsage> for TaskUsageView {
    fn from(u: jsonl::TaskUsage) -> Self {
        Self {
            agent_count: u.agent_count,
            agents_done: u.agents_done,
            agents_error: u.agents_error,
            agents_skipped: u.agents_skipped,
            subagent_tokens: u.subagent_tokens,
            tool_uses: u.tool_uses,
            duration_ms: u.duration_ms,
        }
    }
}

impl From<jsonl::TaskNotification> for TaskNotificationView {
    fn from(n: jsonl::TaskNotification) -> Self {
        Self {
            status: n.status,
            summary: n.summary,
            result: n.result,
            output_file: n.output_file,
            usage: n.usage.map(Into::into),
        }
    }
}

impl From<jsonl::ToolSummary> for ToolSummary {
    fn from(t: jsonl::ToolSummary) -> Self {
        Self {
            name: t.name,
            summary: t.summary,
            detail: t.detail,
            workflow: t.workflow.map(Into::into),
        }
    }
}

impl From<jsonl::ConversationMessage> for ConversationMessage {
    fn from(m: jsonl::ConversationMessage) -> Self {
        Self {
            role: m.role,
            text: m.text,
            tools: m.tools.into_iter().map(Into::into).collect(),
            task: m.task.map(Into::into),
            running: None,
        }
    }
}

impl From<jsonl::RunningTask> for RunningTaskView {
    fn from(t: jsonl::RunningTask) -> Self {
        Self {
            kind: match t.kind {
                jsonl::BackgroundKind::Workflow => "workflow",
                jsonl::BackgroundKind::Agent => "agent",
                jsonl::BackgroundKind::Bash => "bash",
            }
            .to_string(),
            label: t.label,
            description: t.description,
            started_at: t.started_at,
            phases: t.phases.into_iter().map(Into::into).collect(),
            agents_started: t.agents_started,
            agents_done: t.agents_done,
        }
    }
}

impl ConversationMessage {
    /// Wrap an in-flight launch as the synthetic trailing entry the chat renders as a
    /// live card. Not a transcript message — see the `role` note on this struct.
    pub fn running_card(task: jsonl::RunningTask) -> Self {
        Self {
            role: "workflow".to_string(),
            text: String::new(),
            tools: Vec::new(),
            task: None,
            running: Some(task.into()),
        }
    }
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
