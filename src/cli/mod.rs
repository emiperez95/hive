//! CLI argument parsing and subcommand dispatch.
//!
//! This module defines the CLI interface (via clap) and re-exports all
//! subcommand handlers. `main.rs` parses args and dispatches here.

pub mod conversations;
pub mod fork;
pub mod hook;
pub mod project;
pub mod session;
pub mod setup;
pub mod sidebar;
pub mod stage;
pub mod stats;
pub mod task;
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

    /// Filter conversations by pattern (opens Browse + search pre-filled)
    #[arg(short, long, global = true)]
    pub filter: Option<String>,

    /// Open the current window's conversation detail on startup
    #[arg(short = 'D', long, global = true)]
    pub detail: bool,

    /// Open the current window's project detail on startup
    #[arg(short = 'P', long, global = true)]
    pub project_detail: bool,

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
    /// Open the TUI (default behavior — the conversation-first view)
    Tui,
    /// Process a Claude Code hook event (reads JSON from stdin)
    Hook {
        /// Hook event type (Stop, PreToolUse, PostToolUse, PermissionRequest, UserPromptSubmit, Notification, SessionEnd)
        event: String,
    },
    /// Register hooks in ~/.claude/settings.json and tmux keybinding
    Setup {
        /// Auto-accept all prompts (for scripted installs)
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Remove hive hooks from ~/.claude/settings.json and tmux keybinding
    Uninstall {
        /// Auto-accept all prompts
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Cycle to next tmux session (skipping skipped sessions)
    CycleNext {
        /// Triggering pane id (#{pane_id}) used to locate the current session
        #[arg(long)]
        pane: Option<String>,
    },
    /// Cycle to previous tmux session (skipping skipped sessions)
    CyclePrev {
        /// Triggering pane id (#{pane_id}) used to locate the current session
        #[arg(long)]
        pane: Option<String>,
    },
    /// Jump to the next non-busy Claude window (current session first, then others)
    CycleFree {
        /// Triggering pane id (#{pane_id}) used to locate the current window
        #[arg(long)]
        pane: Option<String>,
    },
    /// Cycle to next tmux window in the current session
    WindowNext {
        /// Triggering pane id (#{pane_id}) used to locate the current session
        #[arg(long)]
        pane: Option<String>,
    },
    /// Cycle to previous tmux window in the current session
    WindowPrev {
        /// Triggering pane id (#{pane_id}) used to locate the current session
        #[arg(long)]
        pane: Option<String>,
    },
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
    /// A project's task board (Apple Reminders): list, add, move, complete. The
    /// project defaults to the one this directory belongs to (worktrees included).
    Task {
        #[command(subcommand)]
        command: TaskCommand,
    },
    /// Spread tmux sessions into N side-by-side terminal panes (iTerm2 or Ghostty)
    Spread {
        /// Number of panes (1-9)
        count: usize,
    },
    /// Collapse terminal panes back to a single pane
    Collapse,
    /// Auto-attach to the first available tmux session
    Start {
        /// In Ghostty, first open the hive sidebar as a split of this tab (if it has none)
        #[arg(long)]
        sidebar: bool,
    },
    /// Attention list as a terminal UI — meant for a Ghostty split beside tmux
    Sidebar {
        /// Open it as a split on the right of the current Ghostty tab instead
        #[arg(long)]
        split: bool,
        /// Shrink this split to the sidebar's width on startup (used by --split)
        #[arg(long, hide = true)]
        fit: bool,
    },
    /// Split a conversation in two: the fork continues the same history in another
    /// window — beside it, in a new worktree, or in another project. Run from inside
    /// Claude (`/hive:fork`) it forks the conversation running it.
    Fork {
        /// Fork into a NEW worktree of the conversation's project, branched off the
        /// branch it has checked out
        #[arg(long, value_name = "BRANCH", conflicts_with = "to")]
        wt: Option<String>,
        /// Base branch for --wt (default: the source's checked-out branch)
        #[arg(long, requires = "wt")]
        base: Option<String>,
        /// Fork into a registered project or worktree (`project` or `project/branch`)
        #[arg(long, value_name = "KEY")]
        to: Option<String>,
        /// Conversation to fork (default: the Claude conversation running this command)
        #[arg(long)]
        id: Option<String>,
        /// Switch the tmux client to the fork (default: stay — the original carries on)
        #[arg(long)]
        switch: bool,
        /// What the fork should do — sent as its first message
        #[arg(trailing_var_arg = true)]
        directive: Vec<String>,
    },
    /// Move a conversation on the board (`/board`): todo, planning, in-progress,
    /// blocked or done. Run from inside Claude (`/hive:stage`) it moves the
    /// conversation running it.
    Stage {
        /// todo | planning | in-progress | blocked | done
        stage: String,
        /// Also freeze the window — at the end of this turn when run from inside it
        #[arg(long)]
        freeze: bool,
        /// Conversation to move (default: the Claude conversation running this command)
        #[arg(long)]
        id: Option<String>,
        /// Freeze right away instead of at the end of the turn
        #[arg(long, requires = "freeze")]
        now: bool,
        /// What it's blocked on (kept only for `blocked`)
        #[arg(trailing_var_arg = true)]
        reason: Vec<String>,
    },
    /// Freeze a conversation's window: close it, keeping the conversation to thaw
    /// later. From inside Claude (`/hive:freeze`) it waits for the turn to end.
    Freeze {
        /// Conversation to freeze (default: the Claude conversation running this command)
        #[arg(long)]
        id: Option<String>,
        /// Act right away — from inside, this cuts the reply off
        #[arg(long)]
        now: bool,
        /// Why it's parked — shown on its frozen card
        #[arg(trailing_var_arg = true)]
        note: Vec<String>,
    },
    /// Close a conversation's window. The transcript stays on disk and it can be
    /// resumed. From inside Claude (`/hive:close`) it waits for the turn to end.
    Close {
        /// Conversation to close (default: the Claude conversation running this command)
        #[arg(long)]
        id: Option<String>,
        /// Act right away — from inside, this cuts the reply off
        #[arg(long)]
        now: bool,
    },
    /// Block until Claude process <pid> ends its turn (used by deferred freeze/close)
    #[command(hide = true)]
    WaitTurn {
        pid: u32,
        /// Give up after this many seconds (default 1800)
        #[arg(long)]
        timeout: Option<u64>,
    },
    /// Show usage stats from the activity log
    Stats {
        /// How many days back to summarize (default: 7)
        #[arg(long, default_value = "7")]
        days: i64,
    },
    /// Browse Claude conversations (live + closed) grouped by project/worktree;
    /// interactive TUI, or `--list` for a plain listing.
    Conversations {
        /// Print a plain listing instead of the interactive TUI
        #[arg(long)]
        list: bool,
    },
    /// Append an activity event (invoked by tmux focus hooks). Hidden from help.
    #[command(hide = true)]
    Event {
        /// Event kind: focus | blur
        kind: String,
        /// Session name (for focus events)
        session: Option<String>,
        /// Window index (for focus events)
        window: Option<String>,
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
        /// Claude auth profile (sets CLAUDE_CONFIG_DIR to ~/.claude-{name})
        #[arg(long)]
        auth_profile: Option<String>,
    },
    /// Remove a project from the registry
    Remove {
        /// Project key to remove
        key: String,
    },
    /// Archive a project (hide from picker and default list)
    Archive {
        /// Project key to archive
        key: String,
    },
    /// Unarchive a project
    Unarchive {
        /// Project key to unarchive
        key: String,
    },
    /// List all configured projects
    List {
        /// Include archived projects
        #[arg(long)]
        all: bool,
    },
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
        /// Stay where you are instead of switching the tmux client to the new session
        #[arg(long)]
        no_switch: bool,
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
    /// Remove what Claude's Workflows leave behind: agent worktrees under
    /// .claude/worktrees/ and their worktree-wf_* / wf/* branches. Only removes what is
    /// clean and fully on a remote; shows the plan and asks first.
    Prune {
        /// Only this project (default: every registered project)
        project: Option<String>,
        /// Also prune local branches matching this glob, e.g. 'review-*' (repeatable)
        #[arg(long = "branches", value_name = "GLOB")]
        branches: Vec<String>,
        /// Leave agent worktrees git has touched within this many hours
        #[arg(long, default_value_t = 24)]
        idle_hours: u64,
        /// Show the plan without removing anything
        #[arg(long)]
        dry_run: bool,
        /// Skip the confirmation prompt
        #[arg(short, long)]
        yes: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum TaskCommand {
    /// Projects that have a task board
    Boards,
    /// The board, by column
    List {
        /// Project key (default: the project of the current directory)
        #[arg(short, long)]
        project: Option<String>,
        /// Only this column
        #[arg(short, long)]
        column: Option<String>,
        /// Full JSON (id, status, header fields, body, linked conversation)
        #[arg(long)]
        json: bool,
    },
    /// One task in full: header fields and body
    Show {
        /// Task id — the 8 characters `list` prints are enough
        id: String,
        #[arg(short, long)]
        project: Option<String>,
    },
    /// Add a task (to the first column unless --column)
    Add {
        #[arg(short, long)]
        project: Option<String>,
        #[arg(short, long)]
        column: Option<String>,
        /// Header field, repeatable: --field area=infra
        #[arg(long, value_name = "KEY=VALUE")]
        field: Vec<String>,
        /// Markdown body under the header
        #[arg(long)]
        body: Option<String>,
        /// ISO date or datetime
        #[arg(long)]
        due: Option<String>,
        /// none | low | medium | high
        #[arg(long)]
        priority: Option<String>,
        /// The task title
        #[arg(required = true, trailing_var_arg = true)]
        title: Vec<String>,
    },
    /// Move a task to a column (rewrites its `status:`; `done` completes it)
    Move {
        id: String,
        column: String,
        #[arg(short, long)]
        project: Option<String>,
        /// What a task moved to `blocked` waits on (required there on a strict board)
        #[arg(long, value_name = "REASON")]
        by: Option<String>,
    },
    /// Set header fields (`key=value`; `key=` removes), the Reminders fields
    /// `priority=` (none|low|medium|high), `due=` (ISO date; empty clears) and `title=`,
    /// and the markdown body (`--append` adds to it, `--body` rewrites it)
    Set {
        id: String,
        #[arg(
            required_unless_present_any = ["body", "append"],
            value_name = "KEY=VALUE"
        )]
        fields: Vec<String>,
        /// Replace the body. `-` reads it from stdin; an empty value clears it. The old
        /// text is not kept anywhere — it is echoed to stderr, and that is the only copy
        #[arg(long, conflicts_with = "append", allow_hyphen_values = true)]
        body: Option<String>,
        /// Add to the end of the body, after a blank line. `-` reads it from stdin
        #[arg(long, allow_hyphen_values = true)]
        append: Option<String>,
        #[arg(short, long)]
        project: Option<String>,
    },
    /// The board's schema: every header field, its type, allowed values, what is required
    Schema {
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Report tasks that break the board's schema (read-only)
    Lint {
        #[arg(short, long)]
        project: Option<String>,
        /// Include cosmetic issues (field order)
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// Complete a task (move it to `done`)
    Done {
        id: String,
        #[arg(short, long)]
        project: Option<String>,
    },
    /// Record that a conversation is working on a task (`conversation:` in its header)
    Link {
        id: String,
        #[arg(short, long)]
        project: Option<String>,
        /// Conversation (default: the Claude conversation running this command)
        #[arg(long)]
        conv: Option<String>,
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
        /// Every session's todos, grouped by project
        #[arg(long, conflicts_with_all = ["session", "done"])]
        all: bool,
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
