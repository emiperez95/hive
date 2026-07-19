//! Core types used throughout the application.

/// tmux pane information
#[derive(Debug, Clone)]
pub struct TmuxPane {
    pub index: String,
    /// tmux global pane id, e.g. "%1" (stable for the pane's lifetime).
    /// Used to correlate a pane with the Claude `session_id` recorded by hooks.
    pub id: String,
    pub pid: u32,
    pub cwd: String,
}

/// tmux window information
#[derive(Debug, Clone)]
pub struct TmuxWindow {
    pub index: String,
    #[allow(dead_code)]
    pub name: String,
    pub panes: Vec<TmuxPane>,
}

/// tmux session information
#[derive(Debug, Clone)]
pub struct TmuxSession {
    pub name: String,
    pub windows: Vec<TmuxWindow>,
}

/// Process resource information
#[derive(Debug, Clone)]
pub struct ProcessInfo {
    #[allow(dead_code)]
    pub pid: u32,
    pub name: String,
    pub cpu_percent: f32,
    pub memory_kb: u64,
    pub command: String,
}

/// Claude Code status states
#[derive(Debug, Clone)]
pub enum ClaudeStatus {
    /// Idle, waiting for user input
    Waiting,
    /// Needs permission to run a command (command, optional description)
    NeedsPermission(String, Option<String>),
    /// Edit file approval dialog (filename)
    EditApproval(String),
    /// Claude has a plan waiting for approval
    PlanReview,
    /// Claude asked a question via AskUserQuestion
    QuestionAsked,
    /// Working or unknown state
    Unknown,
}

impl std::fmt::Display for ClaudeStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClaudeStatus::Waiting => write!(f, "waiting for input"),
            ClaudeStatus::NeedsPermission(_, _) => write!(f, "needs permission"),
            ClaudeStatus::EditApproval(file) => write!(f, "edit: {}", file),
            ClaudeStatus::PlanReview => write!(f, "plan ready"),
            ClaudeStatus::QuestionAsked => write!(f, "question asked"),
            ClaudeStatus::Unknown => write!(f, "working"),
        }
    }
}

/// Truncate a command string for display
pub fn truncate_command(cmd: &str, max_len: usize) -> String {
    if cmd.len() <= max_len {
        cmd.to_string()
    } else {
        format!("{}...", &cmd[..max_len - 3])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_command_short() {
        assert_eq!(truncate_command("short", 10), "short");
    }

    #[test]
    fn test_truncate_command_exact() {
        assert_eq!(truncate_command("exactly 10", 10), "exactly 10");
    }

    #[test]
    fn test_truncate_command_long() {
        assert_eq!(truncate_command("this is too long", 10), "this is...");
    }

    #[test]
    fn test_truncate_command_empty() {
        assert_eq!(truncate_command("", 10), "");
    }
}
