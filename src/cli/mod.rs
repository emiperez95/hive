//! CLI argument parsing and subcommand dispatch.
//!
//! This module defines the CLI interface (via clap) and re-exports all
//! subcommand handlers. `main.rs` parses args and dispatches here.

pub mod hook;
pub mod project;
pub mod remote;
pub mod session;
pub mod setup;
pub mod todo;
pub mod update;
pub mod worktree;

use clap::{Parser, Subcommand};

/// Top-level CLI arguments for hive.
#[derive(Parser, Debug)]
#[command(name = "hive")]
#[command(version)]
#[command(about = "Interactive Claude Code session dashboard for tmux")]
pub struct Args {
    /// Subcommand to run
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Filter sessions by name pattern (case-insensitive)
    #[arg(short, long, global = true)]
    pub filter: Option<String>,

    /// Refresh interval in seconds (default: 1)
    #[arg(short, long, default_value = "1", global = true)]
    pub watch: u64,

    /// Open detail view for the current tmux session on startup
    #[arg(short = 'D', long, global = true)]
    pub detail: bool,

    /// Enable debug logging to ~/.cache/hive/debug.log
    #[arg(long, global = true)]
    pub debug: bool,

    /// Open directly in picker/search mode
    #[arg(long, global = true)]
    pub picker: bool,
}

/// Available subcommands.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Open the TUI (default behavior)
    Tui,
    /// Process a Claude Code hook event (reads JSON from stdin)
    Hook {
        /// Hook event type (Stop, PreToolUse, PostToolUse, PermissionRequest, UserPromptSubmit, Notification)
        event: String,
    },
    /// Register hooks in ~/.claude/settings.json and tmux keybinding
    Setup,
    /// Remove hive hooks from ~/.claude/settings.json and tmux keybinding
    Uninstall,
    /// Cycle to next tmux session (skipping skipped sessions)
    CycleNext,
    /// Cycle to previous tmux session (skipping skipped sessions)
    CyclePrev,
    /// Create/attach to a tmux session for a registered project
    Connect {
        /// Project key from the registry
        key: String,
    },
    /// Manage the project registry
    Project {
        #[command(subcommand)]
        command: Box<ProjectCommand>,
    },
    /// Update hive to the latest version from GitHub
    Update,
    /// Manage git worktrees for registered projects
    Wt {
        #[command(subcommand)]
        command: WtCommand,
    },
    /// Manage per-session todos
    Todo {
        #[command(subcommand)]
        command: TodoCommand,
    },
    /// Spread tmux sessions into N vertical iTerm2 panes
    Spread {
        /// Number of panes (1-9)
        count: usize,
    },
    /// Collapse iTerm2 panes back to a single pane
    Collapse,
    /// Auto-attach to the first available tmux session
    Start,
    /// Run as a remote session server (stdio transport for SSH)
    Serve {
        /// Use stdio transport (JSON lines on stdin/stdout)
        #[arg(long)]
        stdio: bool,
    },
    /// Manage remote machine connections
    Remote {
        #[command(subcommand)]
        command: RemoteCommand,
    },
    /// Start web dashboard for mobile access
    Web {
        /// Port to listen on
        #[arg(long, default_value = "8375")]
        port: u16,
        /// Dev mode: serve web.html from disk (live reload on browser refresh)
        #[arg(long)]
        dev: bool,
        /// TTS service URL (e.g. http://10.18.1.2:9800)
        #[arg(long)]
        tts_host: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum ProjectCommand {
    /// Add a project to the registry
    Add {
        /// Project key (used as identifier)
        key: String,
        /// Emoji identifier for session names
        #[arg(short, long)]
        emoji: String,
        /// Project root path (defaults to current directory)
        #[arg(short, long)]
        path: Option<String>,
        /// Display name override (defaults to key)
        #[arg(short = 'd', long)]
        display_name: Option<String>,
        /// Command to run on session startup
        #[arg(short = 's', long)]
        startup: Option<String>,
        /// Directory for worktrees
        #[arg(long)]
        worktrees_dir: Option<String>,
        /// Default git base branch for worktrees
        #[arg(long)]
        base_branch: Option<String>,
        /// Package manager (npm, pnpm, yarn, etc.)
        #[arg(long)]
        package_manager: Option<String>,
        /// Enable port management
        #[arg(long)]
        ports_enabled: bool,
        /// Base port number
        #[arg(long)]
        base_port: Option<u16>,
        /// Port increment between worktrees
        #[arg(long)]
        port_increment: Option<u16>,
        /// Enable database management
        #[arg(long)]
        db_enabled: bool,
        /// Database name prefix
        #[arg(long)]
        db_prefix: Option<String>,
        /// Files to copy into worktrees (repeatable)
        #[arg(long = "copy")]
        copy_files: Vec<String>,
        /// Files to symlink into worktrees (repeatable)
        #[arg(long = "symlink")]
        symlink_files: Vec<String>,
        /// Custom hooks directory (defaults to ~/.hive/projects/{key}/hooks/)
        #[arg(long)]
        hooks_dir: Option<String>,
    },
    /// Remove a project from the registry
    Remove {
        /// Project key to remove
        key: String,
    },
    /// List all configured projects
    List,
    /// Import projects from sesh.toml
    Import,
}

#[derive(Subcommand, Debug)]
pub enum WtCommand {
    /// Create a new worktree for a registered project
    New {
        /// Project key from the registry
        project: String,
        /// Branch name for the worktree
        branch: String,
        /// Base branch to create from (defaults to project's default_base_branch or "main")
        #[arg(long)]
        base: Option<String>,
        /// Attach to an existing branch instead of creating a new one
        #[arg(long)]
        existing: bool,
        /// Worktree type label (defaults to "worktree")
        #[arg(long = "type", default_value = "worktree")]
        wt_type: String,
        /// Send a prompt to Claude after startup
        #[arg(long)]
        prompt: Option<String>,
        /// Skip the project's startup command (create session without launching claude)
        #[arg(long)]
        no_startup: bool,
        /// Enable auto-approve for the new session
        #[arg(long)]
        auto_approve: bool,
    },
    /// Delete a worktree and its associated resources
    Delete {
        /// Project key from the registry
        project: String,
        /// Branch name of the worktree to delete
        branch: String,
        /// Keep the git branch after removing the worktree
        #[arg(long)]
        keep_branch: bool,
        /// Skip confirmation prompt
        #[arg(long)]
        force: bool,
    },
    /// List worktrees (all or for a specific project)
    List {
        /// Optional project key to filter by
        project: Option<String>,
    },
    /// Import existing git worktrees into worktrees.json
    Import {
        /// Project key to import worktrees for
        project: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum TodoCommand {
    /// List todos (active by default, --done for completed)
    List {
        #[arg(short, long)]
        session: Option<String>,
        /// Show completed todos instead of active
        #[arg(long)]
        done: bool,
    },
    /// Print the first active todo (exit 1 if none)
    Next {
        #[arg(short, long)]
        session: Option<String>,
    },
    /// Add a todo
    Add {
        text: String,
        #[arg(short, long)]
        session: Option<String>,
    },
    /// Mark a todo as done (moves to completed list)
    Done {
        /// 1-based index (default: 1)
        index: Option<usize>,
        #[arg(short, long)]
        session: Option<String>,
    },
    /// Clear completed todos
    Clear {
        #[arg(short, long)]
        session: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum RemoteCommand {
    /// Add a remote machine
    Add {
        /// Remote name (identifier)
        name: String,
        /// SSH host (from ~/.ssh/config)
        #[arg(long)]
        host: String,
        /// Display label (defaults to name)
        #[arg(long)]
        label: Option<String>,
        /// Emoji for remote sessions
        #[arg(long, default_value = "🖥️")]
        emoji: String,
    },
    /// Remove a remote machine
    Remove {
        /// Remote name to remove
        name: String,
    },
    /// List configured remotes
    List,
    /// Keep SSH connections alive and cache remote sessions in the background
    Sync,
}

/// Action to perform after the TUI exits and the terminal is restored.
///
/// Some actions (like spreading iTerm panes or exec-ing into tmux) must
/// happen after ratatui has cleaned up the alternate screen.
pub enum PostAction {
    None,
    Spread(usize),
    Collapse,
    /// Attach to a tmux session via exec (used by `hive start` outside tmux)
    Attach(String),
    /// Create a worktree and switch to its session
    CreateWorktree {
        project: String,
        branch: String,
        base: String,
    },
    /// Delete a worktree (confirmed in TUI)
    DeleteWorktree {
        project: String,
        branch: String,
    },
    /// Connect to a remote session by creating a local wrapper tmux session
    ConnectRemote {
        ssh_host: String,
        label: String,
        emoji: String,
        session_name: String,
    },
}
