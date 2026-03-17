//! hive: Interactive Claude Code session dashboard for tmux.

mod common;
mod daemon;
mod ipc;
mod tui;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use crossterm::event::{poll, read, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::common::debug::{debug_log, init_debug};
use crate::common::persistence::{
    is_globally_muted, load_auto_approve_sessions, load_completed_todos, load_muted_sessions,
    load_session_todos, load_skipped_sessions, save_completed_todos, save_session_todos,
    save_skipped_sessions,
};
use crate::common::projects::{
    connect_project, connect_session, DatabaseConfig, FilePatterns, PortConfig, ProjectConfig,
    ProjectRegistry,
};
use crate::common::tmux::{
    get_current_tmux_session, get_current_tmux_session_names, get_other_client_sessions,
    resolve_tmux_path, switch_to_session,
};
use crate::common::types::PERMISSION_KEYS;
use crate::tui::app::{find_session_by_permission_key, App, InputMode, SearchResult};
use crate::tui::ui::ui;

/// Bundled janus-wt-portal agent definition, embedded at compile time.
const JANUS_AGENT_CONTENT: &str = include_str!("../.claude/agents/janus-wt-portal.md");

/// Bundled create-project command, embedded at compile time.
const CREATE_PROJECT_CMD_CONTENT: &str = include_str!("../.claude/commands/hive/create-project.md");

#[derive(Parser, Debug)]
#[command(name = "hive")]
#[command(version)]
#[command(about = "Interactive Claude Code session dashboard for tmux")]
struct Args {
    /// Subcommand to run
    #[command(subcommand)]
    command: Option<Command>,

    /// Filter sessions by name pattern (case-insensitive)
    #[arg(short, long, global = true)]
    filter: Option<String>,

    /// Refresh interval in seconds (default: 1)
    #[arg(short, long, default_value = "1", global = true)]
    watch: u64,

    /// Open detail view for the current tmux session on startup
    #[arg(short = 'D', long, global = true)]
    detail: bool,

    /// Enable debug logging to ~/.cache/hive/debug.log
    #[arg(long, global = true)]
    debug: bool,

    /// Open directly in picker/search mode
    #[arg(long, global = true)]
    picker: bool,
}

#[derive(Subcommand, Debug)]
enum Command {
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
}

#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)]
enum ProjectCommand {
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
enum WtCommand {
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
enum TodoCommand {
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

/// Action to perform after TUI exits (post terminal restore)
enum PostAction {
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
}

fn run_tui(
    terminal: &mut ratatui::DefaultTerminal,
    args: &Args,
    running: Arc<AtomicBool>,
) -> Result<PostAction> {
    let mut app = App::new(args.filter.clone(), args.watch);
    app.auto_detail = args.detail;

    // Migrate old worktree session names to new [project_key] format
    {
        let registry = ProjectRegistry::load();
        let mut wt_state = crate::common::worktree::WorktreeState::load();
        wt_state.migrate_session_names(&registry);
    }

    if args.picker {
        app.auto_picker = true;
        app.input_mode = InputMode::Search;
        app.load_project_names();
        app.update_search_results();
    }

    loop {
        if !running.load(Ordering::SeqCst) {
            app.save_restorable();
            return Ok(PostAction::None);
        }

        if app.input_mode != InputMode::Search
            && app.input_mode != InputMode::WorktreeBase
            && app.input_mode != InputMode::WorktreeConfirmDelete
            && app.input_mode != InputMode::NewProjectKey
            && app.input_mode != InputMode::NewProjectEmoji
        {
            app.refresh()?;
            app.maybe_periodic_save();
        }

        // Auto-open detail view for current tmux session (once, after first refresh)
        if app.auto_detail {
            app.auto_detail = false;
            if let Some(current) = get_current_tmux_session() {
                if let Some(idx) = app.session_infos.iter().position(|s| s.name == current) {
                    app.open_detail(idx);
                } else {
                    app.error_message = Some((
                        format!("Session '{}' not found in list", current),
                        std::time::Instant::now(),
                    ));
                }
            } else {
                app.error_message = Some((
                    "Could not detect current tmux session".to_string(),
                    std::time::Instant::now(),
                ));
            }
        }

        terminal.draw(|frame| ui(frame, &mut app))?;

        let sleep_ms = 100u64;
        let iterations = (app.interval * 1000) / sleep_ms;
        let mut should_refresh = false;
        let mut needs_redraw = false;

        for _ in 0..iterations {
            if !running.load(Ordering::SeqCst) {
                app.save_restorable();
                return Ok(PostAction::None);
            }

            if poll(Duration::from_millis(sleep_ms))? {
                if let Event::Key(KeyEvent {
                    code,
                    modifiers,
                    kind: KeyEventKind::Press,
                    ..
                }) = read()?
                {
                    debug_log(&format!(
                        "KEY: {:?} (mode={:?}, showing_detail={:?}, showing_help={})",
                        code,
                        app.input_mode,
                        app.showing_detail.is_some(),
                        app.showing_help,
                    ));

                    // Help screen takes priority
                    if app.showing_help {
                        match code {
                            KeyCode::Char('?') | KeyCode::Esc => {
                                app.showing_help = false;
                                needs_redraw = true;
                            }
                            KeyCode::Char('q') | KeyCode::Char('Q') => {
                                app.save_restorable();
                                return Ok(PostAction::None);
                            }
                            _ => {}
                        }
                    } else if app.input_mode == InputMode::AddTodo {
                        match code {
                            KeyCode::Esc => {
                                app.cancel_add_todo();
                                needs_redraw = true;
                            }
                            KeyCode::Enter if modifiers.contains(KeyModifiers::ALT) => {
                                app.input_buffer.push('\n');
                                needs_redraw = true;
                            }
                            KeyCode::Enter => {
                                app.complete_add_todo();
                                needs_redraw = true;
                            }
                            KeyCode::Backspace => {
                                app.input_buffer.pop();
                                needs_redraw = true;
                            }
                            KeyCode::Char(c) => {
                                app.input_buffer.push(c);
                                needs_redraw = true;
                            }
                            _ => {}
                        }
                    } else if app.input_mode == InputMode::SpreadPrompt {
                        match code {
                            KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
                                let n = c.to_digit(10).unwrap() as usize;
                                app.save_restorable();
                                // Return spread count to run after TUI cleanup
                                return Ok(PostAction::Spread(n));
                            }
                            KeyCode::Esc => {
                                app.input_mode = InputMode::Normal;
                                needs_redraw = true;
                            }
                            _ => {}
                        }
                    } else if app.input_mode == InputMode::WorktreeBranch {
                        match code {
                            KeyCode::Enter => {
                                app.enter_base_picker();
                                needs_redraw = true;
                            }
                            KeyCode::Esc => {
                                app.cancel_worktree_wizard();
                                needs_redraw = true;
                            }
                            KeyCode::Backspace => {
                                app.input_buffer.pop();
                                needs_redraw = true;
                            }
                            KeyCode::Char(c) => {
                                app.input_buffer.push(c);
                                needs_redraw = true;
                            }
                            _ => {}
                        }
                    } else if app.input_mode == InputMode::WorktreeBase {
                        match code {
                            KeyCode::Enter => {
                                if let (Some(project), Some(branch)) =
                                    (app.wt_project_key.take(), app.wt_branch_name.take())
                                {
                                    let base = app
                                        .wt_base_choices
                                        .get(app.wt_base_selected)
                                        .cloned()
                                        .unwrap_or_else(|| "main".to_string());
                                    app.cancel_worktree_wizard();
                                    app.save_restorable();
                                    return Ok(PostAction::CreateWorktree {
                                        project,
                                        branch,
                                        base,
                                    });
                                }
                            }
                            KeyCode::Esc => {
                                app.cancel_worktree_wizard();
                                needs_redraw = true;
                            }
                            KeyCode::Up | KeyCode::Char('k') => {
                                if app.wt_base_selected > 0 {
                                    app.wt_base_selected -= 1;
                                }
                                needs_redraw = true;
                            }
                            KeyCode::Down | KeyCode::Char('j') => {
                                if app.wt_base_selected + 1 < app.wt_base_choices.len() {
                                    app.wt_base_selected += 1;
                                }
                                needs_redraw = true;
                            }
                            _ => {}
                        }
                    } else if app.input_mode == InputMode::WorktreeConfirmDelete {
                        match code {
                            KeyCode::Enter => {
                                if let (Some(project), Some(branch)) =
                                    (app.wt_delete_project.take(), app.wt_delete_branch.take())
                                {
                                    app.cancel_worktree_delete();
                                    app.save_restorable();
                                    return Ok(PostAction::DeleteWorktree { project, branch });
                                }
                            }
                            KeyCode::Esc => {
                                app.cancel_worktree_delete();
                                needs_redraw = true;
                            }
                            _ => {}
                        }
                    } else if app.input_mode == InputMode::NewProjectKey {
                        match code {
                            KeyCode::Enter => {
                                app.np_enter_emoji_step();
                                needs_redraw = true;
                            }
                            KeyCode::Esc => {
                                app.cancel_new_project_wizard();
                                needs_redraw = true;
                            }
                            KeyCode::Backspace => {
                                app.input_buffer.pop();
                                needs_redraw = true;
                            }
                            KeyCode::Char(c) => {
                                app.input_buffer.push(c);
                                needs_redraw = true;
                            }
                            _ => {}
                        }
                    } else if app.input_mode == InputMode::NewProjectEmoji {
                        match code {
                            KeyCode::Enter => {
                                if let Some(session_name) = app.np_complete() {
                                    if connect_session(&session_name) {
                                        app.unskip(&session_name);
                                        switch_to_session(&session_name);
                                        app.save_restorable();
                                        return Ok(PostAction::None);
                                    } else {
                                        app.error_message = Some((
                                            format!("Failed to connect to '{}'", session_name),
                                            std::time::Instant::now(),
                                        ));
                                    }
                                }
                                needs_redraw = true;
                            }
                            KeyCode::Esc => {
                                app.cancel_new_project_wizard();
                                needs_redraw = true;
                            }
                            KeyCode::Backspace => {
                                app.input_buffer.pop();
                                needs_redraw = true;
                            }
                            KeyCode::Char(c) => {
                                app.input_buffer.push(c);
                                needs_redraw = true;
                            }
                            _ => {}
                        }
                    } else if app.input_mode == InputMode::Search {
                        match code {
                            KeyCode::Esc => {
                                if app.auto_picker {
                                    app.save_restorable();
                                    return Ok(PostAction::None);
                                }
                                app.input_mode = InputMode::Normal;
                                app.search_query.clear();
                                app.search_results.clear();
                                needs_redraw = true;
                            }
                            KeyCode::Enter => {
                                if let Some(result) = app.search_results.get(app.selected).cloned()
                                {
                                    match result {
                                        SearchResult::Active(name) => {
                                            app.unskip(&name);
                                            app.save_restorable();
                                            if app.auto_picker {
                                                return Ok(PostAction::Attach(name));
                                            }
                                            switch_to_session(&name);
                                            return Ok(PostAction::None);
                                        }
                                        SearchResult::Project(name)
                                        | SearchResult::Worktree(name) => {
                                            app.input_mode = InputMode::Normal;
                                            app.search_query.clear();
                                            app.search_results.clear();
                                            if connect_session(&name) {
                                                app.unskip(&name);
                                                app.save_restorable();
                                                if app.auto_picker {
                                                    return Ok(PostAction::Attach(name));
                                                }
                                                switch_to_session(&name);
                                                return Ok(PostAction::None);
                                            } else {
                                                app.error_message = Some((
                                                    format!("Failed to connect to '{}'", name),
                                                    std::time::Instant::now(),
                                                ));
                                                needs_redraw = true;
                                            }
                                        }
                                    }
                                } else {
                                    app.input_mode = InputMode::Normal;
                                    app.search_query.clear();
                                    app.search_results.clear();
                                    needs_redraw = true;
                                }
                            }
                            KeyCode::Backspace => {
                                app.search_query.pop();
                                app.update_search_results();
                                needs_redraw = true;
                            }
                            KeyCode::Up => {
                                if app.selected > 0 {
                                    app.selected -= 1;
                                }
                                needs_redraw = true;
                            }
                            KeyCode::Down => {
                                if app.selected + 1 < app.search_results.len() {
                                    app.selected += 1;
                                }
                                needs_redraw = true;
                            }
                            KeyCode::Char(c) => {
                                app.search_query.push(c);
                                app.update_search_results();
                                needs_redraw = true;
                            }
                            _ => {}
                        }
                    } else if app.showing_detail.is_some() {
                        match code {
                            KeyCode::Esc => {
                                if app.detail_selected.is_some() {
                                    app.detail_selected = None;
                                    needs_redraw = true;
                                } else {
                                    app.save_restorable();
                                    return Ok(PostAction::None);
                                }
                            }
                            KeyCode::Char('q') | KeyCode::Char('Q') => {
                                app.save_restorable();
                                return Ok(PostAction::None);
                            }
                            KeyCode::Char('?') => {
                                app.showing_help = true;
                                needs_redraw = true;
                            }
                            KeyCode::Char('a') | KeyCode::Char('A') => {
                                app.start_add_todo();
                                needs_redraw = true;
                            }
                            KeyCode::Char('d') | KeyCode::Char('D') | KeyCode::Backspace => {
                                if app.detail_selected.is_some() {
                                    app.delete_selected_todo();
                                }
                                needs_redraw = true;
                            }
                            KeyCode::Up | KeyCode::Char('k') => {
                                let todo_count = app.detail_todos().len();
                                let port_count = app
                                    .detail_session_info()
                                    .map(|s| s.listening_ports.len())
                                    .unwrap_or(0);
                                let total = todo_count + port_count;
                                match app.detail_selected {
                                    None => {
                                        if app.detail_scroll_offset > 0 {
                                            app.detail_scroll_offset -= 1;
                                        } else if total > 0 {
                                            app.detail_selected = Some(total - 1);
                                        }
                                    }
                                    Some(0) => {
                                        if app.detail_scroll_offset > 0 {
                                            app.detail_scroll_offset -= 1;
                                        }
                                        app.detail_selected = None;
                                    }
                                    Some(sel) => {
                                        app.detail_selected = Some(sel - 1);
                                    }
                                }
                                needs_redraw = true;
                            }
                            KeyCode::Down | KeyCode::Char('j') => {
                                let todo_count = app.detail_todos().len();
                                let port_count = app
                                    .detail_session_info()
                                    .map(|s| s.listening_ports.len())
                                    .unwrap_or(0);
                                let total = todo_count + port_count;
                                match app.detail_selected {
                                    None => {
                                        if total > 0 {
                                            app.detail_selected = Some(0);
                                        } else {
                                            app.detail_scroll_offset += 1;
                                        }
                                    }
                                    Some(sel) if total > 0 && sel < total - 1 => {
                                        app.detail_selected = Some(sel + 1);
                                    }
                                    _ => {
                                        app.detail_scroll_offset += 1;
                                    }
                                }
                                needs_redraw = true;
                            }
                            KeyCode::Enter => {
                                if let Some(sel) = app.detail_selected {
                                    let todo_count = app.detail_todos().len();
                                    let port_count = app
                                        .detail_session_info()
                                        .map(|s| s.listening_ports.len())
                                        .unwrap_or(0);
                                    if sel >= todo_count && port_count > 0 {
                                        let port_idx = sel - todo_count;
                                        app.refresh_chrome_tabs();
                                        if let Some(session) = app.detail_session_info() {
                                            if let Some(port_info) =
                                                session.listening_ports.get(port_idx)
                                            {
                                                let matched_tab = app
                                                    .detail_chrome_tabs
                                                    .iter()
                                                    .find(|(_, p)| *p == port_info.port);
                                                if let Some((tab, _)) = matched_tab {
                                                    crate::common::chrome::focus_chrome_tab(tab);
                                                } else {
                                                    let url = format!(
                                                        "http://localhost:{}",
                                                        port_info.port
                                                    );
                                                    crate::common::chrome::open_chrome_tab(&url);
                                                }
                                            }
                                        }
                                        needs_redraw = true;
                                    } else if let Some(name) = app.detail_session_name() {
                                        app.unskip(&name);
                                        switch_to_session(&name);
                                        app.save_restorable();
                                        return Ok(PostAction::None);
                                    }
                                } else if let Some(name) = app.detail_session_name() {
                                    app.unskip(&name);
                                    switch_to_session(&name);
                                    app.save_restorable();
                                    return Ok(PostAction::None);
                                }
                            }
                            KeyCode::Char('f') | KeyCode::Char('F') => {
                                if let Some(name) = app.detail_session_name() {
                                    app.toggle_favorite(&name);
                                    needs_redraw = true;
                                }
                            }
                            KeyCode::Char('!') => {
                                if let Some(name) = app.detail_session_name() {
                                    app.toggle_auto_approve(&name);
                                    needs_redraw = true;
                                }
                            }
                            KeyCode::Char('m') | KeyCode::Char('M') => {
                                if let Some(name) = app.detail_session_name() {
                                    app.toggle_mute(&name);
                                    needs_redraw = true;
                                }
                            }
                            KeyCode::Char('s') | KeyCode::Char('S') => {
                                if let Some(name) = app.detail_session_name() {
                                    app.toggle_skip(&name);
                                    needs_redraw = true;
                                }
                            }
                            KeyCode::Char('o') | KeyCode::Char('O') => {
                                app.refresh_chrome_tabs();
                                crate::common::chrome::focus_all_matched_tabs(
                                    &app.detail_chrome_tabs,
                                );
                                needs_redraw = true;
                            }
                            KeyCode::Char('w') | KeyCode::Char('W') => {
                                app.start_worktree_wizard();
                                needs_redraw = true;
                            }
                            KeyCode::Char('x') | KeyCode::Char('X') => {
                                app.start_worktree_delete();
                                needs_redraw = true;
                            }
                            _ => {}
                        }
                    } else {
                        // Normal mode input
                        match code {
                            KeyCode::Char('?') => {
                                app.showing_help = true;
                                needs_redraw = true;
                            }
                            KeyCode::Up | KeyCode::Char('k') => {
                                app.move_selection_up();
                                needs_redraw = true;
                            }
                            KeyCode::Down | KeyCode::Char('j') => {
                                app.move_selection_down();
                                needs_redraw = true;
                            }
                            KeyCode::Enter => {
                                if app.show_selection {
                                    app.open_detail(app.selected);
                                    needs_redraw = true;
                                }
                            }
                            KeyCode::Char('r') | KeyCode::Char('R') => {
                                should_refresh = true;
                                break;
                            }
                            KeyCode::Char('m') | KeyCode::Char('M') => {
                                app.toggle_global_mute();
                                needs_redraw = true;
                            }
                            KeyCode::Char('l') | KeyCode::Char('L') => {
                                let pane_count = crate::common::iterm::get_iterm_pane_count();
                                if pane_count > 1 {
                                    app.save_restorable();
                                    return Ok(PostAction::Collapse);
                                } else {
                                    app.input_mode = InputMode::SpreadPrompt;
                                    needs_redraw = true;
                                }
                            }
                            KeyCode::Char('q') | KeyCode::Char('Q') => {
                                app.save_restorable();
                                return Ok(PostAction::None);
                            }
                            KeyCode::Esc => {
                                app.save_restorable();
                                return Ok(PostAction::None);
                            }
                            KeyCode::Char('c') if cfg!(unix) => {
                                app.save_restorable();
                                return Ok(PostAction::None);
                            }
                            KeyCode::Char('n') | KeyCode::Char('N') => {
                                app.start_new_project_wizard();
                                needs_redraw = true;
                            }
                            KeyCode::Char('/') => {
                                app.input_mode = InputMode::Search;
                                app.search_query.clear();
                                app.search_scroll_offset = 0;
                                app.load_project_names();
                                app.update_search_results();
                                app.selected = 0;
                                needs_redraw = true;
                            }
                            KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
                                let idx = c.to_digit(10).unwrap() as usize - 1;
                                if let Some(session_info) = app.session_infos.get(idx) {
                                    let name = session_info.name.clone();
                                    app.unskip(&name);
                                    switch_to_session(&name);
                                    app.save_restorable();
                                    return Ok(PostAction::None);
                                }
                            }
                            KeyCode::Char(c)
                                if PERMISSION_KEYS.contains(&c.to_ascii_lowercase()) =>
                            {
                                let is_uppercase = c.is_ascii_uppercase();
                                if let Some(session_info) =
                                    find_session_by_permission_key(&app.session_infos, c)
                                {
                                    if let Some((ref sess, ref win, ref pane)) =
                                        session_info.claude_pane
                                    {
                                        use crate::common::tmux::send_key_to_pane;
                                        use crate::common::types::ClaudeStatus;
                                        let has_approve_always = matches!(
                                            session_info.claude_status,
                                            Some(ClaudeStatus::NeedsPermission(_, _))
                                        );
                                        if is_uppercase && has_approve_always {
                                            send_key_to_pane(sess, win, pane, "2");
                                            send_key_to_pane(sess, win, pane, "Enter");
                                        } else {
                                            send_key_to_pane(sess, win, pane, "1");
                                            send_key_to_pane(sess, win, pane, "Enter");
                                        }
                                        app.pending_approvals.insert(session_info.name.clone());
                                        app.hide_selection();
                                        should_refresh = true;
                                        break;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }

                    if needs_redraw {
                        terminal.draw(|frame| ui(frame, &mut app))?;
                        needs_redraw = false;
                    }
                }
            }
        }

        if should_refresh {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Process a hook event from stdin
fn run_hook(event_type: &str) -> Result<()> {
    use crate::daemon::hooks::handle_hook_event;
    use crate::daemon::notifier::notify_needs_attention;
    use crate::ipc::messages::{HookEvent, HookState, SessionStatus};
    use std::io::BufRead;

    // Read JSON from stdin
    let stdin = std::io::stdin();
    let mut input = String::new();
    let reader = stdin.lock();
    if let Some(line) = reader.lines().next() {
        let line = line?;
        input.push_str(&line);
    }

    if input.trim().is_empty() {
        return Ok(());
    }

    // Parse the input JSON
    let json: serde_json::Value =
        serde_json::from_str(&input).unwrap_or_else(|_| serde_json::json!({}));

    let session_id = json
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let cwd = json
        .get("cwd")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Build HookEvent based on event type
    let hook_event = match event_type {
        "Stop" => HookEvent::Stop { session_id, cwd },
        "PreToolUse" => {
            let tool_name = json
                .get("tool_name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let tool_input = json.get("tool_input").cloned();
            HookEvent::PreToolUse {
                session_id,
                cwd,
                tool_name,
                tool_input,
            }
        }
        "PostToolUse" => {
            let tool_name = json
                .get("tool_name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            HookEvent::PostToolUse {
                session_id,
                cwd,
                tool_name,
            }
        }
        "PermissionRequest" => {
            let tool_name = json
                .get("tool_name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let tool_input = json.get("tool_input").cloned();
            HookEvent::PermissionRequest {
                session_id,
                cwd,
                tool_name,
                tool_input,
            }
        }
        "UserPromptSubmit" => HookEvent::UserPromptSubmit { session_id, cwd },
        "Notification" => {
            let message = json
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            HookEvent::Notification {
                session_id,
                cwd,
                message,
            }
        }
        _ => {
            eprintln!("Unknown hook event type: {}", event_type);
            return Ok(());
        }
    };

    // Load state, process event, save state
    let mut state = HookState::load();

    // Check auto-approve before notifications so we can skip alerting for auto-approved requests
    // Skip auto-approve for plans (ExitPlanMode) and questions (AskUserQuestion) — those need human input
    let mut auto_approved = false;
    let tool_name_str = json.get("tool_name").and_then(|v| v.as_str()).unwrap_or("");
    let is_human_input = tool_name_str == "ExitPlanMode" || tool_name_str == "AskUserQuestion";
    if event_type == "PermissionRequest" && !is_human_input {
        if let Some(tmux_session) = get_current_tmux_session() {
            let auto_approve = load_auto_approve_sessions();
            if auto_approve.contains(&tmux_session) {
                auto_approved = true;
                println!(
                    "{}",
                    serde_json::json!({
                        "hookSpecificOutput": {
                            "hookEventName": "PermissionRequest",
                            "decision": {
                                "behavior": "allow"
                            }
                        }
                    })
                );
            }
        }
    }

    if let Some(updated_session) = handle_hook_event(&mut state, hook_event) {
        // Send notification if session needs attention, not muted, and not auto-approved
        if updated_session.needs_attention && !auto_approved {
            let muted = load_muted_sessions();
            let global_mute = is_globally_muted();

            // Try to find the tmux session name by matching cwd
            let session_name = updated_session
                .cwd
                .rsplit('/')
                .next()
                .unwrap_or(&updated_session.session_id);

            if !global_mute && !muted.contains(session_name) {
                let status_text = match &updated_session.status {
                    SessionStatus::NeedsPermission { tool_name, .. } => {
                        format!("needs permission: {}", tool_name)
                    }
                    SessionStatus::EditApproval { filename } => {
                        format!("edit approval: {}", filename)
                    }
                    SessionStatus::PlanReview => "plan ready".to_string(),
                    SessionStatus::QuestionAsked => "question asked".to_string(),
                    _ => "needs attention".to_string(),
                };
                notify_needs_attention(session_name, &status_text);
            }
        }
    }

    // Clean up stale sessions (>10 minutes inactive)
    state.cleanup_stale_sessions(600);

    // Save state atomically
    state.save()?;

    Ok(())
}

/// Check if a hook command belongs to hive
fn is_hive_hook_command(cmd: &str) -> bool {
    let is_hive_event = cmd.ends_with(" hook Stop")
        || cmd.ends_with(" hook PreToolUse")
        || cmd.ends_with(" hook PostToolUse")
        || cmd.ends_with(" hook UserPromptSubmit")
        || cmd.ends_with(" hook PermissionRequest")
        || cmd.ends_with(" hook Notification");
    is_hive_event && (cmd.contains("/hive hook ") || cmd.starts_with("hive hook "))
}

/// Setup hooks in ~/.claude/settings.json
fn run_setup() -> Result<()> {
    use std::fs;

    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?;
    let settings_path = home.join(".claude").join("settings.json");

    // Find the binary path: prefer installed location, fall back to current exe
    let installed_path = home.join(".local").join("bin").join("hive");
    let binary_path = if installed_path.exists() {
        installed_path
    } else {
        std::env::current_exe().unwrap_or_else(|_| installed_path.clone())
    };

    let binary_str = binary_path.to_string_lossy();

    // Read existing settings
    let settings: serde_json::Value = if settings_path.exists() {
        let content = fs::read_to_string(&settings_path)?;
        serde_json::from_str(&content)?
    } else {
        serde_json::json!({})
    };

    let hook_events = [
        "Stop",
        "PreToolUse",
        "PostToolUse",
        "UserPromptSubmit",
        "PermissionRequest",
    ];

    // Check which hooks are already installed (with correct binary path)
    let mut hooks_missing: Vec<&str> = Vec::new();
    let mut hooks_stale: Vec<&str> = Vec::new(); // installed but wrong binary path
    let mut hooks_ok: Vec<&str> = Vec::new();

    if let Some(hooks_obj) = settings.get("hooks").and_then(|h| h.as_object()) {
        for event in &hook_events {
            let expected_cmd = format!("{} hook {}", binary_str, event);
            let mut found_exact = false;
            let mut found_other_hive = false;

            if let Some(groups) = hooks_obj.get(*event).and_then(|g| g.as_array()) {
                for group in groups {
                    if let Some(hooks) = group.get("hooks").and_then(|h| h.as_array()) {
                        for hook in hooks {
                            if let Some(cmd) = hook.get("command").and_then(|c| c.as_str()) {
                                if cmd == expected_cmd {
                                    found_exact = true;
                                } else if is_hive_hook_command(cmd) {
                                    found_other_hive = true;
                                }
                            }
                        }
                    }
                }
            }

            if found_exact {
                hooks_ok.push(event);
            } else if found_other_hive {
                hooks_stale.push(event);
            } else {
                hooks_missing.push(event);
            }
        }
    } else {
        hooks_missing.extend_from_slice(&hook_events);
    }

    // Check tmux keybindings
    let tmux_keys = std::process::Command::new("tmux")
        .args(["list-keys"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();

    let tmux_s_bound = tmux_keys
        .lines()
        .any(|line| line.contains("prefix") && line.contains(" s ") && line.contains("hive"));
    let tmux_d_bound = tmux_keys.lines().any(|line| {
        line.contains("prefix")
            && line.contains(" d ")
            && line.contains("hive")
            && line.contains("--detail")
    });
    let tmux_cn_bound = tmux_keys
        .lines()
        .any(|line| line.contains("C-n") && line.contains("hive") && line.contains("cycle-next"));
    let tmux_cp_bound = tmux_keys
        .lines()
        .any(|line| line.contains("C-p") && line.contains("hive") && line.contains("cycle-prev"));

    // Report status
    println!("hive setup status:");
    println!();

    if !hooks_ok.is_empty() {
        for event in &hooks_ok {
            println!("  [ok]      {} hook", event);
        }
    }
    if !hooks_stale.is_empty() {
        for event in &hooks_stale {
            println!("  [update]  {} hook (different binary path)", event);
        }
    }
    if !hooks_missing.is_empty() {
        for event in &hooks_missing {
            println!("  [missing] {} hook", event);
        }
    }

    if tmux_s_bound {
        println!("  [ok]      tmux prefix+s keybinding (list)");
    } else {
        println!("  [missing] tmux prefix+s keybinding (list)");
    }
    if tmux_d_bound {
        println!("  [ok]      tmux prefix+d keybinding (detail)");
    } else {
        println!("  [missing] tmux prefix+d keybinding (detail)");
    }
    if tmux_cn_bound {
        println!("  [ok]      tmux Ctrl+n keybinding (cycle next)");
    } else {
        println!("  [missing] tmux Ctrl+n keybinding (cycle next)");
    }
    if tmux_cp_bound {
        println!("  [ok]      tmux Ctrl+p keybinding (cycle prev)");
    } else {
        println!("  [missing] tmux Ctrl+p keybinding (cycle prev)");
    }

    // Check janus-wt-portal agent
    let agent_dest = home
        .join(".claude")
        .join("agents")
        .join("janus-wt-portal.md");
    let agent_status = if agent_dest.exists() {
        let existing = fs::read_to_string(&agent_dest).unwrap_or_default();
        if existing == JANUS_AGENT_CONTENT {
            "ok"
        } else {
            "update"
        }
    } else {
        "missing"
    };

    match agent_status {
        "ok" => println!("  [ok]      janus-wt-portal agent"),
        "update" => println!("  [update]  janus-wt-portal agent (content differs)"),
        _ => println!("  [missing] janus-wt-portal agent"),
    }

    // Check create-project command
    let cmd_dest = home
        .join(".claude")
        .join("commands")
        .join("hive")
        .join("create-project.md");
    let cmd_status = if cmd_dest.exists() {
        let existing = fs::read_to_string(&cmd_dest).unwrap_or_default();
        if existing == CREATE_PROJECT_CMD_CONTENT {
            "ok"
        } else {
            "update"
        }
    } else {
        "missing"
    };

    match cmd_status {
        "ok" => println!("  [ok]      create-project command"),
        "update" => println!("  [update]  create-project command (content differs)"),
        _ => println!("  [missing] create-project command"),
    }

    let needs_hook_changes = !hooks_missing.is_empty() || !hooks_stale.is_empty();
    let all_tmux_bound = tmux_s_bound && tmux_d_bound && tmux_cn_bound && tmux_cp_bound;
    let agent_needs_install = agent_status != "ok";
    let cmd_needs_install = cmd_status != "ok";

    if !needs_hook_changes && all_tmux_bound && !agent_needs_install && !cmd_needs_install {
        println!();
        println!("Everything is already set up!");
        return Ok(());
    }

    // Install missing/stale hooks
    if needs_hook_changes {
        let events_to_install: Vec<&str> = hooks_missing
            .iter()
            .chain(hooks_stale.iter())
            .copied()
            .collect();

        println!();
        println!(
            "Install {} hook(s)? Existing hooks for other tools will be preserved.",
            events_to_install.len()
        );
        print!("[Y/n] ");
        std::io::Write::flush(&mut std::io::stdout()).ok();

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        let input = input.trim().to_lowercase();
        if !input.is_empty() && input != "y" && input != "yes" {
            println!("Skipped hooks.");
        } else {
            let mut settings = settings.clone();
            let needs_matcher = ["PreToolUse", "PostToolUse", "PermissionRequest"];

            let hooks_obj = settings
                .as_object_mut()
                .ok_or_else(|| anyhow::anyhow!("settings.json is not a JSON object"))?
                .entry("hooks")
                .or_insert_with(|| serde_json::json!({}));

            let hooks_map = hooks_obj
                .as_object_mut()
                .ok_or_else(|| anyhow::anyhow!("hooks is not a JSON object"))?;

            for event in &events_to_install {
                let hive_command = format!("{} hook {}", binary_str, event);
                let hive_hook_entry = serde_json::json!({
                    "type": "command",
                    "command": hive_command
                });

                let event_array = hooks_map
                    .entry(event.to_string())
                    .or_insert_with(|| serde_json::json!([]));

                let groups = event_array
                    .as_array_mut()
                    .ok_or_else(|| anyhow::anyhow!("hooks.{} is not an array", event))?;

                // Remove any existing hive hooks from all groups
                for group in groups.iter_mut() {
                    if let Some(hooks_arr) = group.get_mut("hooks").and_then(|h| h.as_array_mut()) {
                        hooks_arr.retain(|h| {
                            h.get("command")
                                .and_then(|c| c.as_str())
                                .map(|c| !is_hive_hook_command(c))
                                .unwrap_or(true)
                        });
                    }
                }

                // Remove empty groups
                groups.retain(|group| {
                    group
                        .get("hooks")
                        .and_then(|h| h.as_array())
                        .map(|a| !a.is_empty())
                        .unwrap_or(true)
                });

                if needs_matcher.contains(event) {
                    let wildcard_group = groups.iter_mut().find(|g| {
                        g.get("matcher")
                            .and_then(|m| m.as_str())
                            .map(|m| m == "*")
                            .unwrap_or(false)
                    });

                    if let Some(group) = wildcard_group {
                        if let Some(hooks_arr) =
                            group.get_mut("hooks").and_then(|h| h.as_array_mut())
                        {
                            hooks_arr.push(hive_hook_entry);
                        }
                    } else {
                        groups.push(serde_json::json!({
                            "matcher": "*",
                            "hooks": [hive_hook_entry]
                        }));
                    }
                } else {
                    let no_matcher_group = groups.iter_mut().find(|g| g.get("matcher").is_none());

                    if let Some(group) = no_matcher_group {
                        if let Some(hooks_arr) =
                            group.get_mut("hooks").and_then(|h| h.as_array_mut())
                        {
                            hooks_arr.push(hive_hook_entry);
                        }
                    } else {
                        groups.push(serde_json::json!({
                            "hooks": [hive_hook_entry]
                        }));
                    }
                }
            }

            // Ensure .claude directory exists
            if let Some(parent) = settings_path.parent() {
                fs::create_dir_all(parent)?;
            }

            let content = serde_json::to_string_pretty(&settings)?;
            fs::write(&settings_path, content)?;

            println!("Hooks installed. Restart Claude Code sessions for hooks to take effect.");
        }
    }

    // Install tmux keybindings
    if !all_tmux_bound {
        let tmux_s_cmd = format!("display-popup -E -w 80% -h 70% \"{}\"", binary_str);
        let tmux_d_cmd = format!("display-popup -E -w 80% -h 70% \"{} --detail\"", binary_str);
        let tmux_cn_cmd = format!("run-shell \"{} cycle-next\"", binary_str);
        let tmux_cp_cmd = format!("run-shell \"{} cycle-prev\"", binary_str);

        let bindings: Vec<(&str, &str, &str, bool)> = vec![
            ("prefix", "s", &tmux_s_cmd, tmux_s_bound),
            ("prefix", "d", &tmux_d_cmd, tmux_d_bound),
            ("root", "C-n", &tmux_cn_cmd, tmux_cn_bound),
            ("root", "C-p", &tmux_cp_cmd, tmux_cp_bound),
        ];

        let missing: Vec<_> = bindings.iter().filter(|(_, _, _, bound)| !bound).collect();

        println!();
        println!("Register tmux keybindings?");
        for (table, key, _cmd, _) in &missing {
            let label = if *table == "root" {
                key.to_string()
            } else {
                format!("prefix+{}", key)
            };
            println!("  {} -> hive", label);
        }
        print!("[Y/n] ");
        std::io::Write::flush(&mut std::io::stdout()).ok();

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        let input = input.trim().to_lowercase();
        if input.is_empty() || input == "y" || input == "yes" {
            let mut registered = Vec::new();
            let mut failed = Vec::new();

            for (table, key, cmd, bound) in &bindings {
                if *bound {
                    continue;
                }
                let args = if *table == "root" {
                    vec!["bind-key", "-n", key, cmd]
                } else {
                    vec!["bind-key", key, cmd]
                };
                match std::process::Command::new("tmux").args(&args).status() {
                    Ok(s) if s.success() => registered.push(*key),
                    _ => failed.push(*key),
                }
            }

            if !registered.is_empty() {
                println!("Tmux keybindings registered (current session only).");
                println!("Add to ~/.tmux.conf to persist:");
                for (table, key, cmd, bound) in &bindings {
                    if *bound || !registered.contains(key) {
                        continue;
                    }
                    if *table == "root" {
                        println!("  bind-key -n {} {}", key, cmd);
                    } else {
                        println!("  bind-key {} {}", key, cmd);
                    }
                }
            }
            if !failed.is_empty() {
                println!("Could not register some keybindings (tmux not running?).");
            }
        } else {
            println!("Skipped tmux keybindings.");
        }
    }

    // Install janus-wt-portal agent
    if agent_needs_install {
        println!();
        println!("Install janus-wt-portal agent to ~/.claude/agents/?");
        print!("[Y/n] ");
        std::io::Write::flush(&mut std::io::stdout()).ok();

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        let input = input.trim().to_lowercase();
        if input.is_empty() || input == "y" || input == "yes" {
            if let Some(parent) = agent_dest.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&agent_dest, JANUS_AGENT_CONTENT)?;
            println!("Agent installed to {:?}", agent_dest);
        } else {
            println!("Skipped agent installation.");
        }
    }

    // Install create-project command
    if cmd_needs_install {
        println!();
        println!("Install create-project command to ~/.claude/commands/hive/?");
        print!("[Y/n] ");
        std::io::Write::flush(&mut std::io::stdout()).ok();

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        let input = input.trim().to_lowercase();
        if input.is_empty() || input == "y" || input == "yes" {
            if let Some(parent) = cmd_dest.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&cmd_dest, CREATE_PROJECT_CMD_CONTENT)?;
            println!("Command installed to {:?}", cmd_dest);
        } else {
            println!("Skipped command installation.");
        }
    }

    Ok(())
}

/// Remove hive hooks from ~/.claude/settings.json
fn run_uninstall() -> Result<()> {
    use std::fs;

    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?;
    let settings_path = home.join(".claude").join("settings.json");

    if !settings_path.exists() {
        println!(
            "No settings file found at {:?}, nothing to do.",
            settings_path
        );
        return Ok(());
    }

    let mut settings: serde_json::Value = {
        let content = fs::read_to_string(&settings_path)?;
        serde_json::from_str(&content)?
    };

    let hooks_map = match settings.get_mut("hooks").and_then(|h| h.as_object_mut()) {
        Some(map) => map,
        None => {
            println!("No hooks found in settings, nothing to do.");
            return Ok(());
        }
    };

    // Find which events have hive hooks
    let mut found_events: Vec<String> = Vec::new();
    for (event, groups) in hooks_map.iter() {
        if let Some(arr) = groups.as_array() {
            for group in arr {
                if let Some(hooks) = group.get("hooks").and_then(|h| h.as_array()) {
                    for hook in hooks {
                        if let Some(cmd) = hook.get("command").and_then(|c| c.as_str()) {
                            if is_hive_hook_command(cmd) {
                                found_events.push(event.clone());
                                break;
                            }
                        }
                    }
                }
            }
        }
    }

    if found_events.is_empty() {
        println!("No hive hooks found in settings, nothing to do.");
        return Ok(());
    }

    found_events.sort();
    found_events.dedup();

    println!("Found hive hooks for the following events:");
    for event in &found_events {
        println!("  {}", event);
    }
    println!();
    println!("Other hooks will be preserved.");
    print!("Remove hive hooks? [Y/n] ");
    std::io::Write::flush(&mut std::io::stdout()).ok();

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let input = input.trim().to_lowercase();
    if !input.is_empty() && input != "y" && input != "yes" {
        println!("Aborted.");
        return Ok(());
    }

    // Remove hive hooks from all groups
    for (_event, groups) in hooks_map.iter_mut() {
        if let Some(arr) = groups.as_array_mut() {
            for group in arr.iter_mut() {
                if let Some(hooks_arr) = group.get_mut("hooks").and_then(|h| h.as_array_mut()) {
                    hooks_arr.retain(|h| {
                        h.get("command")
                            .and_then(|c| c.as_str())
                            .map(|c| !is_hive_hook_command(c))
                            .unwrap_or(true)
                    });
                }
            }
            // Remove empty groups
            arr.retain(|group| {
                group
                    .get("hooks")
                    .and_then(|h| h.as_array())
                    .map(|a| !a.is_empty())
                    .unwrap_or(true)
            });
        }
    }

    // Remove event keys that now have empty arrays
    hooks_map.retain(|_, groups| groups.as_array().map(|a| !a.is_empty()).unwrap_or(true));

    let content = serde_json::to_string_pretty(&settings)?;
    fs::write(&settings_path, content)?;

    println!("Hive hooks removed from {:?}", settings_path);

    // Offer to unbind tmux keys
    println!();
    println!("Unbind tmux keybindings (prefix+s, prefix+d, Ctrl+n, Ctrl+p)?");
    print!("[Y/n] ");
    std::io::Write::flush(&mut std::io::stdout()).ok();

    let mut input2 = String::new();
    std::io::stdin().read_line(&mut input2)?;
    let input2 = input2.trim().to_lowercase();
    if input2.is_empty() || input2 == "y" || input2 == "yes" {
        for key in &["s", "d"] {
            let _ = std::process::Command::new("tmux")
                .args(["unbind-key", key])
                .status();
        }
        for key in &["C-n", "C-p"] {
            let _ = std::process::Command::new("tmux")
                .args(["unbind-key", "-n", key])
                .status();
        }
        println!("Tmux keybindings unbound (current session only).");
        println!("Remove from ~/.tmux.conf manually if present.");
    } else {
        println!("Skipped tmux keybinding removal.");
    }

    // Remove janus-wt-portal agent
    let agent_path = home
        .join(".claude")
        .join("agents")
        .join("janus-wt-portal.md");
    if agent_path.exists() {
        println!();
        print!("Remove janus-wt-portal agent? [Y/n] ");
        std::io::Write::flush(&mut std::io::stdout()).ok();

        let mut input3 = String::new();
        std::io::stdin().read_line(&mut input3)?;
        let input3 = input3.trim().to_lowercase();
        if input3.is_empty() || input3 == "y" || input3 == "yes" {
            fs::remove_file(&agent_path)?;
            println!("Removed {:?}", agent_path);
        } else {
            println!("Skipped agent removal.");
        }
    }

    Ok(())
}

/// Cycle to next/prev tmux session, skipping skipped sessions
fn run_cycle(forward: bool) -> Result<()> {
    let skipped = load_skipped_sessions();
    let other_clients = get_other_client_sessions();
    let all_sessions = get_current_tmux_session_names();

    let filtered: Vec<&String> = all_sessions
        .iter()
        .filter(|name| !skipped.contains(*name) && !other_clients.contains(*name))
        .collect();

    if filtered.is_empty() {
        return Ok(());
    }

    let current = get_current_tmux_session();

    let current_idx = current
        .as_ref()
        .and_then(|c| filtered.iter().position(|name| *name == c));

    let target = match current_idx {
        Some(idx) => {
            if filtered.len() <= 1 {
                return Ok(());
            }
            if forward {
                filtered[(idx + 1) % filtered.len()]
            } else {
                filtered[(idx + filtered.len() - 1) % filtered.len()]
            }
        }
        None => filtered[0],
    };

    switch_to_session(target);
    Ok(())
}

/// Spread tmux sessions into N vertical iTerm2 panes
fn run_spread(count: usize) -> Result<()> {
    if count <= 1 {
        return Ok(());
    }
    crate::common::tmux::set_all_sessions_layout("spread");
    crate::common::iterm::spread_panes(count - 1);
    Ok(())
}

/// Collapse iTerm2 panes back to a single pane
fn run_collapse() -> Result<()> {
    crate::common::iterm::collapse_panes();
    crate::common::tmux::set_all_sessions_layout("collapse");
    Ok(())
}

/// Connect to a registered project by key
fn run_connect(key: &str) -> Result<()> {
    let registry = ProjectRegistry::load();
    let config = registry
        .projects
        .get(key)
        .ok_or_else(|| anyhow::anyhow!("Project '{}' not found in registry", key))?;

    let session_name = ProjectRegistry::session_name(key, config);
    if !connect_project(&session_name) {
        anyhow::bail!("Failed to create/connect session for '{}'", key);
    }
    // Unskip if it was skipped — user explicitly chose to connect
    let mut skipped = load_skipped_sessions();
    if skipped.remove(&session_name) {
        save_skipped_sessions(&skipped);
    }
    switch_to_session(&session_name);
    Ok(())
}

/// Add a project to the registry
fn run_project_add(cmd: ProjectCommand) -> Result<()> {
    let ProjectCommand::Add {
        key,
        emoji,
        path,
        display_name,
        startup,
        worktrees_dir,
        base_branch,
        package_manager,
        ports_enabled,
        base_port,
        port_increment,
        db_enabled,
        db_prefix,
        copy_files,
        symlink_files,
        hooks_dir,
    } = cmd
    else {
        unreachable!()
    };

    let mut registry = ProjectRegistry::load();

    if registry.projects.contains_key(&key) {
        anyhow::bail!(
            "Project '{}' already exists. Remove it first to re-add.",
            key
        );
    }

    let project_root = path.unwrap_or_else(|| {
        std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| ".".to_string())
    });

    let config = ProjectConfig {
        emoji,
        project_root,
        display_name,
        startup_command: startup,
        worktrees_dir,
        default_base_branch: base_branch,
        worktree_types: Vec::new(),
        package_manager,
        ports: PortConfig {
            enabled: ports_enabled,
            base_port: base_port.unwrap_or(0),
            increment: port_increment.unwrap_or(1),
        },
        database: DatabaseConfig {
            enabled: db_enabled,
            prefix: db_prefix,
        },
        files: FilePatterns {
            copy: copy_files,
            symlink: symlink_files,
        },
        hooks_dir,
    };

    let session_name = ProjectRegistry::session_name(&key, &config);
    registry.add_project(key, config);
    registry.save()?;
    println!("Added project '{}'", session_name);
    Ok(())
}

/// Remove a project from the registry
fn run_project_remove(key: &str) -> Result<()> {
    let mut registry = ProjectRegistry::load();
    if !registry.remove_project(key) {
        anyhow::bail!("Project '{}' not found in registry", key);
    }
    registry.save()?;
    println!("Removed project '{}'", key);
    Ok(())
}

/// List all configured projects
fn run_project_list() -> Result<()> {
    let registry = ProjectRegistry::load();

    if registry.projects.is_empty() {
        println!("No projects configured. Use 'hive project add' or 'hive project import'.");
        return Ok(());
    }

    // Sort by key for consistent output
    let mut entries: Vec<_> = registry.projects.iter().collect();
    entries.sort_by_key(|(k, _)| k.as_str());

    // Calculate column widths
    let max_key = entries.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    let max_session = entries
        .iter()
        .map(|(k, c)| ProjectRegistry::session_name(k, c).len())
        .max()
        .unwrap_or(0);

    println!(
        "{:<width_k$}  {:<width_s$}  PATH",
        "KEY",
        "SESSION",
        width_k = max_key,
        width_s = max_session
    );
    println!(
        "{:<width_k$}  {:<width_s$}  ----",
        "-".repeat(max_key),
        "-".repeat(max_session),
        width_k = max_key,
        width_s = max_session
    );

    for (key, config) in &entries {
        let session = ProjectRegistry::session_name(key, config);
        println!(
            "{:<width_k$}  {:<width_s$}  {}",
            key,
            session,
            config.project_root,
            width_k = max_key,
            width_s = max_session
        );
    }

    println!("\n{} project(s)", entries.len());
    Ok(())
}

/// Import projects from sesh.toml
fn run_project_import() -> Result<()> {
    use crate::common::projects::parse_sesh_toml;

    // Check ~/.config/sesh/sesh.toml first (common on macOS), then XDG config dir
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?;
    let dot_config_path = home.join(".config").join("sesh").join("sesh.toml");
    let xdg_path = dirs::config_dir().map(|p| p.join("sesh").join("sesh.toml"));

    let sesh_path = if dot_config_path.exists() {
        dot_config_path
    } else if let Some(ref xdg) = xdg_path {
        if xdg.exists() {
            xdg.clone()
        } else {
            anyhow::bail!("sesh.toml not found at {:?} or {:?}", dot_config_path, xdg);
        }
    } else {
        anyhow::bail!("sesh.toml not found at {:?}", dot_config_path);
    };

    let entries = parse_sesh_toml(&sesh_path)?;
    let mut registry = ProjectRegistry::load();
    let mut added = 0;
    let mut skipped = 0;

    for (key, config) in entries {
        if registry.projects.contains_key(&key) {
            skipped += 1;
        } else {
            let name = ProjectRegistry::session_name(&key, &config);
            println!("  + {}", name);
            registry.add_project(key, config);
            added += 1;
        }
    }

    registry.save()?;
    println!(
        "\nImported {} project(s), skipped {} existing",
        added, skipped
    );
    Ok(())
}

/// Create a new worktree: full 12-step workflow with hooks
fn run_wt_new(
    project: &str,
    branch: &str,
    base: Option<&str>,
    existing: bool,
    wt_type: &str,
    prompt: Option<&str>,
    auto_approve: bool,
) -> Result<()> {
    use crate::common::persistence::{load_auto_approve_sessions, save_auto_approve_sessions};
    use crate::common::projects::expand_tilde;
    use crate::common::worktree::*;
    use anyhow::Context;

    // 1. Load project config, resolve worktrees dir, build default session name
    let registry = ProjectRegistry::load();
    let config = registry
        .projects
        .get(project)
        .ok_or_else(|| anyhow::anyhow!("Project '{}' not found in registry", project))?;

    let worktrees_dir = registry
        .resolve_worktrees_dir(project, config)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "No worktrees directory configured for '{}'. Set 'worktrees_dir' on the project or 'worktrees_root' globally in projects.toml.",
                project
            )
        })?;

    let project_root = expand_tilde(&config.project_root);
    let base_branch = base
        .map(|s| s.to_string())
        .or_else(|| config.default_base_branch.clone())
        .unwrap_or_else(|| "main".to_string());

    let mut session_name = build_session_name(config, project, wt_type, branch);
    let hooks_dir = resolve_hooks_dir(config, project);
    let mut metadata = serde_json::json!({});

    // Check if already registered (single load, reused for insertion later)
    let mut state = WorktreeState::load();
    if state.get(project, branch).is_some() {
        bail!(
            "Worktree '{}/{}' already exists in registry. Delete it first.",
            project,
            branch
        );
    }

    println!("Creating worktree {}/{}...", project, branch);

    // 2. pre-create hook (uses estimated worktree path since it doesn't exist yet)
    let pre_env = build_hook_env(
        project,
        branch,
        &worktrees_dir.join(branch),
        &project_root,
        &session_name,
        wt_type,
        &hooks_dir,
    );
    metadata = run_hook(&hooks_dir, "pre-create", &pre_env, &metadata)?;

    // 3. git worktree add
    let worktree_path = create_git_worktree(
        &project_root,
        &worktrees_dir,
        branch,
        &base_branch,
        existing,
    )?;
    println!("  Created worktree at {}", worktree_path.display());

    // Steps 4-12: any failure triggers cleanup of everything created so far.
    // Track what was created so cleanup knows what to tear down.
    let mut tmux_session_created = false;

    let result = (|| -> Result<()> {
        // Build hook env once (reused for post-worktree, post-copy, post-setup)
        let mut env = build_hook_env(
            project,
            branch,
            &worktree_path,
            &project_root,
            &session_name,
            wt_type,
            &hooks_dir,
        );

        // 4. post-worktree hook
        metadata = run_hook(&hooks_dir, "post-worktree", &env, &metadata)?;

        // 5. Copy/symlink file patterns
        if !config.files.copy.is_empty() {
            copy_file_patterns(&project_root, &worktree_path, &config.files.copy)?;
            println!("  Copied {} file pattern(s)", config.files.copy.len());
        }
        if !config.files.symlink.is_empty() {
            symlink_file_patterns(&project_root, &worktree_path, &config.files.symlink)?;
            println!("  Symlinked {} file pattern(s)", config.files.symlink.len());
        }

        // 6. Seed Claude memory + pre-trust
        seed_memory(&project_root, &worktree_path)?;
        pretrust_claude_project(&worktree_path)?;
        println!("  Seeded Claude memory (trusted)");

        // 7. post-copy hook
        metadata = run_hook(&hooks_dir, "post-copy", &env, &metadata)?;

        // 8. Check metadata for session name override, create tmux session
        if let Some(name_override) = metadata.get("session_name").and_then(|v| v.as_str()) {
            session_name = name_override.to_string();
            env.insert("HIVE_SESSION_NAME".to_string(), session_name.clone());
        }

        let tmux_output = std::process::Command::new("tmux")
            .args([
                "new-session",
                "-d",
                "-s",
                &session_name,
                "-c",
                &worktree_path.to_string_lossy(),
            ])
            .output()
            .context("Failed to run tmux new-session")?;

        if !tmux_output.status.success() {
            let stderr = String::from_utf8_lossy(&tmux_output.stderr);
            bail!(
                "Failed to create tmux session '{}': {}",
                session_name,
                stderr.trim()
            );
        }
        tmux_session_created = true;
        println!("  Created tmux session '{}'", session_name);

        // 9. Register in worktrees.json (reuse state loaded at step 1)
        state.add(WorktreeEntry {
            project_key: project.to_string(),
            branch: branch.to_string(),
            worktree_type: wt_type.to_string(),
            path: worktree_path.to_string_lossy().to_string(),
            session_name: session_name.clone(),
            metadata: metadata.clone(),
            created_at: chrono::Utc::now().to_rfc3339(),
        });
        state.save()?;

        // 10. Enable auto-approve if requested
        if auto_approve {
            let mut sessions = load_auto_approve_sessions();
            sessions.insert(session_name.clone());
            save_auto_approve_sessions(&sessions);
            println!("  Enabled auto-approve for '{}'", session_name);
        }

        // 11. post-setup hook
        run_hook(&hooks_dir, "post-setup", &env, &metadata)?;

        // 12. Run startup command (append prompt as CLI argument if provided)
        //     New worktrees have no conversation to continue, so strip `-c` from claude commands.
        if let Some(ref cmd) = config.startup_command {
            let base_cmd = cmd.replace("claude -c", "claude");
            let full_cmd = match prompt {
                Some(p) => format!("{} {:?}", base_cmd, p),
                None => base_cmd,
            };
            let _ = std::process::Command::new("tmux")
                .args(["send-keys", "-t", &session_name, &full_cmd, "Enter"])
                .output();
        }

        Ok(())
    })();

    if let Err(e) = result {
        eprintln!("  Error during worktree setup, cleaning up...");
        if tmux_session_created {
            use crate::common::tmux::kill_tmux_session;
            kill_tmux_session(&session_name);
        }
        let _ = delete_git_worktree(&project_root, &worktree_path, branch, true, true);
        return Err(e);
    }

    println!("Ready: session '{}'", session_name);

    Ok(())
}

/// Delete a worktree: full 7-step workflow with hooks
fn run_wt_delete(project: &str, branch: &str, keep_branch: bool, force: bool) -> Result<()> {
    use crate::common::projects::expand_tilde;
    use crate::common::tmux::kill_tmux_session;
    use crate::common::worktree::*;

    // 1. Look up entry in worktrees.json
    let state = WorktreeState::load();
    let entry = state
        .get(project, branch)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Worktree '{}/{}' not found in registry. Use 'hive wt list' to see registered worktrees.",
                project,
                branch
            )
        })?
        .clone();

    let registry = ProjectRegistry::load();
    let config = registry
        .projects
        .get(project)
        .ok_or_else(|| anyhow::anyhow!("Project '{}' not found in registry", project))?;

    let project_root = expand_tilde(&config.project_root);
    let worktree_path = std::path::PathBuf::from(&entry.path);

    // 2. Confirmation prompt
    if !force {
        println!(
            "Delete worktree '{}/{}' (session: '{}', path: {})?",
            project, branch, entry.session_name, entry.path
        );
        print!("[y/N] ");
        std::io::Write::flush(&mut std::io::stdout()).ok();

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        let input = input.trim().to_lowercase();
        if input != "y" && input != "yes" {
            println!("Aborted.");
            return Ok(());
        }
    }

    let hooks_dir = resolve_hooks_dir(config, project);
    let env = build_hook_env(
        project,
        branch,
        &worktree_path,
        &project_root,
        &entry.session_name,
        &entry.worktree_type,
        &hooks_dir,
    );

    // 3. pre-delete hook (receives full metadata)
    let _ = run_hook(&hooks_dir, "pre-delete", &env, &entry.metadata)?;

    // 4. Kill tmux session
    if kill_tmux_session(&entry.session_name) {
        println!("  Killed tmux session '{}'", entry.session_name);
    }

    // 5. git worktree remove + optionally delete branch
    let git_removed =
        match delete_git_worktree(&project_root, &worktree_path, branch, keep_branch, force) {
            Ok(()) => {
                println!("  Removed git worktree");
                true
            }
            Err(e) => {
                eprintln!("  Warning: git worktree removal failed: {}", e);
                false
            }
        };

    // 6. Remove from worktrees.json only if git worktree was actually removed
    if git_removed {
        let mut state = WorktreeState::load();
        state.remove(project, branch);
        state.save()?;
    } else {
        eprintln!("  Keeping registry entry (git worktree still exists on disk)");
    }

    // 7. post-delete hook
    if git_removed {
        let _ = run_hook(&hooks_dir, "post-delete", &env, &entry.metadata)?;
        println!("Deleted worktree '{}/{}'", project, branch);
    } else {
        eprintln!(
            "Partial delete: hooks ran and tmux killed, but worktree remains at {}",
            entry.path
        );
    }
    Ok(())
}

/// List worktrees with tmux session status
fn run_wt_list(project: Option<&str>) -> Result<()> {
    let state = crate::common::worktree::WorktreeState::load();
    let tmux_sessions = get_current_tmux_session_names();

    let mut entries: Vec<_> = if let Some(proj) = project {
        state
            .worktrees
            .values()
            .filter(|e| e.project_key == proj)
            .collect()
    } else {
        state.worktrees.values().collect()
    };

    if entries.is_empty() {
        if let Some(proj) = project {
            println!("No worktrees registered for project '{}'.", proj);
        } else {
            println!("No worktrees registered. Use 'hive wt new' to create one.");
        }
        return Ok(());
    }

    // Sort by project then branch
    entries.sort_by(|a, b| {
        a.project_key
            .cmp(&b.project_key)
            .then(a.branch.cmp(&b.branch))
    });

    // Calculate column widths
    let max_key = entries
        .iter()
        .map(|e| WorktreeState::make_key(&e.project_key, &e.branch).len())
        .max()
        .unwrap_or(0);
    let max_session = entries
        .iter()
        .map(|e| e.session_name.len())
        .max()
        .unwrap_or(0);

    use crate::common::worktree::WorktreeState;

    println!(
        "{:<width_k$}  {:<width_s$}  STATUS  PATH",
        "WORKTREE",
        "SESSION",
        width_k = max_key,
        width_s = max_session
    );
    println!(
        "{:<width_k$}  {:<width_s$}  ------  ----",
        "-".repeat(max_key),
        "-".repeat(max_session),
        width_k = max_key,
        width_s = max_session
    );

    for entry in &entries {
        let wt_key = WorktreeState::make_key(&entry.project_key, &entry.branch);
        let status = if tmux_sessions.contains(&entry.session_name) {
            "active"
        } else {
            "dead"
        };
        println!(
            "{:<width_k$}  {:<width_s$}  {:<6}  {}",
            wt_key,
            entry.session_name,
            status,
            entry.path,
            width_k = max_key,
            width_s = max_session
        );
    }

    println!("\n{} worktree(s)", entries.len());
    Ok(())
}

/// Import existing git worktrees into worktrees.json
fn run_wt_import(project: &str) -> Result<()> {
    use crate::common::worktree::import_worktrees;

    let registry = ProjectRegistry::load();
    let config = registry
        .projects
        .get(project)
        .ok_or_else(|| anyhow::anyhow!("Project '{}' not found in registry", project))?;

    let worktrees_dir = registry
        .resolve_worktrees_dir(project, config)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "No worktrees directory configured for '{}'. Set 'worktrees_dir' on the project or 'worktrees_root' globally.",
                project
            )
        })?;

    let tmux_sessions = get_current_tmux_session_names();

    println!("Scanning git worktrees for '{}'...", project);
    let imported = import_worktrees(project, config, &worktrees_dir, &tmux_sessions)?;

    if imported.is_empty() {
        println!("No new worktrees found to import.");
    } else {
        for entry in &imported {
            println!(
                "  + {}/{} → session '{}'",
                entry.project_key, entry.branch, entry.session_name
            );
        }
        println!("\nImported {} worktree(s)", imported.len());
    }

    Ok(())
}

/// Update hive to the latest version from GitHub, then re-run setup
fn run_update() -> Result<()> {
    const REPO_URL: &str = "https://github.com/emiperez95/hive";

    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?;
    let install_root = home.join(".local");

    println!("Current version: {}", env!("CARGO_PKG_VERSION"));
    println!("Installing latest from {}...", REPO_URL);
    println!();

    let status = std::process::Command::new("cargo")
        .args([
            "install",
            "--git",
            REPO_URL,
            "--root",
            &install_root.to_string_lossy(),
        ])
        .status()?;

    if !status.success() {
        bail!("cargo install failed");
    }

    println!();
    println!("Running setup with new binary...");
    println!();

    let new_binary = install_root.join("bin").join("hive");
    let setup_status = std::process::Command::new(&new_binary)
        .arg("setup")
        .status()?;

    if !setup_status.success() {
        bail!("hive setup failed");
    }

    Ok(())
}

/// Resolve session name: explicit flag or auto-detect from tmux
fn resolve_session(explicit: Option<String>) -> Result<String> {
    match explicit {
        Some(name) => Ok(name),
        None => get_current_tmux_session()
            .ok_or_else(|| anyhow::anyhow!("Could not detect tmux session. Use --session <name>.")),
    }
}

/// Dispatch todo subcommands
fn run_todo(command: TodoCommand) -> Result<()> {
    match command {
        TodoCommand::List { session, done } => run_todo_list(session, done),
        TodoCommand::Next { session } => run_todo_next(session),
        TodoCommand::Add { text, session } => run_todo_add(text, session),
        TodoCommand::Done { index, session } => run_todo_done(index, session),
        TodoCommand::Clear { session } => run_todo_clear(session),
    }
}

/// List todos: active or completed, 1-based INDEX\tTEXT per line
fn run_todo_list(session: Option<String>, done: bool) -> Result<()> {
    let session = resolve_session(session)?;
    let todos = if done {
        load_completed_todos()
    } else {
        load_session_todos()
    };
    if let Some(items) = todos.get(&session) {
        for (i, item) in items.iter().enumerate() {
            println!("{}\t{}", i + 1, item);
        }
    }
    Ok(())
}

/// Print first active todo as raw text, exit 1 if none
fn run_todo_next(session: Option<String>) -> Result<()> {
    let session = resolve_session(session)?;
    let todos = load_session_todos();
    if let Some(items) = todos.get(&session) {
        if let Some(first) = items.first() {
            println!("{}", first);
            return Ok(());
        }
    }
    std::process::exit(1);
}

/// Add a todo to the active list
fn run_todo_add(text: String, session: Option<String>) -> Result<()> {
    let session = resolve_session(session)?;
    let mut todos = load_session_todos();
    todos.entry(session).or_default().push(text);
    save_session_todos(&todos);
    Ok(())
}

/// Mark a todo as done: remove from active, append to completed
fn run_todo_done(index: Option<usize>, session: Option<String>) -> Result<()> {
    let session = resolve_session(session)?;
    let mut todos = load_session_todos();
    let items = todos.entry(session.clone()).or_default();
    let idx = index.unwrap_or(1);
    if idx == 0 || idx > items.len() {
        bail!("Invalid todo index: {} (have {} todo(s))", idx, items.len());
    }
    let removed = items.remove(idx - 1);
    if items.is_empty() {
        todos.remove(&session);
    }
    save_session_todos(&todos);

    let mut completed = load_completed_todos();
    completed.entry(session).or_default().push(removed.clone());
    save_completed_todos(&completed);

    println!("{}", removed);
    Ok(())
}

/// Clear completed todos for a session
fn run_todo_clear(session: Option<String>) -> Result<()> {
    let session = resolve_session(session)?;
    let mut completed = load_completed_todos();
    completed.remove(&session);
    save_completed_todos(&completed);
    Ok(())
}

/// Handle post-TUI actions (spread, collapse, attach, create worktree)
fn handle_post_action(action: PostAction) -> Result<()> {
    match action {
        PostAction::Spread(n) => run_spread(n),
        PostAction::Collapse => run_collapse(),
        PostAction::Attach(name) => {
            use std::os::unix::process::CommandExt;
            let tmux = resolve_tmux_path();
            let err = std::process::Command::new(&tmux)
                .args(["attach-session", "-t", &name])
                .exec();
            bail!("exec failed: {}", err);
        }
        PostAction::CreateWorktree {
            project,
            branch,
            base,
        } => {
            eprintln!("Creating worktree {}/{}...", project, branch);
            run_wt_new(
                &project,
                &branch,
                Some(&base),
                false,
                "worktree",
                None,
                false,
            )?;
            // Look up final session name from WorktreeState (hooks may override)
            let state = crate::common::worktree::WorktreeState::load();
            let session_name = state
                .get(&project, &branch)
                .map(|e| e.session_name.clone())
                .unwrap_or_else(|| format!("{}/{}", project, branch));
            switch_to_session(&session_name);
            Ok(())
        }
        PostAction::DeleteWorktree { project, branch } => {
            eprintln!("Deleting worktree {}/{}...", project, branch);
            run_wt_delete(&project, &branch, false, true)?;
            Ok(())
        }
        PostAction::None => Ok(()),
    }
}

/// Find the first tmux session not skipped and not attached to another client.
fn run_start() -> Result<Option<String>> {
    let skipped = load_skipped_sessions();
    let other_clients = get_other_client_sessions();
    let sessions: Vec<String> = get_current_tmux_session_names()
        .into_iter()
        .filter(|name| !skipped.contains(name))
        .collect();

    // Prefer a session not attached elsewhere, fall back to any non-skipped session
    let target = sessions
        .iter()
        .find(|name| !other_clients.contains(*name))
        .or_else(|| sessions.first());

    Ok(target.cloned())
}

fn main() -> Result<()> {
    let mut args = Args::parse();
    init_debug(args.debug);

    match args.command {
        Some(Command::Hook { event }) => run_hook(&event),
        Some(Command::Setup) => run_setup(),
        Some(Command::Update) => run_update(),
        Some(Command::Uninstall) => run_uninstall(),
        Some(Command::CycleNext) => run_cycle(true),
        Some(Command::CyclePrev) => run_cycle(false),
        Some(Command::Connect { key }) => run_connect(&key),
        Some(Command::Project { command }) => match *command {
            cmd @ ProjectCommand::Add { .. } => run_project_add(cmd),
            ProjectCommand::Remove { key } => run_project_remove(&key),
            ProjectCommand::List => run_project_list(),
            ProjectCommand::Import => run_project_import(),
        },
        Some(Command::Todo { command }) => run_todo(command),
        Some(Command::Spread { count }) => run_spread(count),
        Some(Command::Collapse) => run_collapse(),
        Some(Command::Start) => {
            if let Some(target) = run_start()? {
                use std::os::unix::process::CommandExt;
                let tmux = resolve_tmux_path();
                let err = std::process::Command::new(&tmux)
                    .args(["attach-session", "-t", &target])
                    .exec();
                bail!("exec failed: {}", err);
            }
            // No available session — fall through to TUI picker
            args.picker = true;
            args.command = None;
            // fall through below
            let running = Arc::new(AtomicBool::new(true));
            let r = running.clone();
            ctrlc::set_handler(move || {
                r.store(false, Ordering::SeqCst);
            })
            .expect("Error setting Ctrl-C handler");

            let mut terminal = ratatui::init();
            let action = run_tui(&mut terminal, &args, running);
            ratatui::restore();
            handle_post_action(action?)
        }
        Some(Command::Wt { command }) => match command {
            WtCommand::New {
                project,
                branch,
                base,
                existing,
                wt_type,
                prompt,
                auto_approve,
            } => run_wt_new(
                &project,
                &branch,
                base.as_deref(),
                existing,
                &wt_type,
                prompt.as_deref(),
                auto_approve,
            ),
            WtCommand::Delete {
                project,
                branch,
                keep_branch,
                force,
            } => run_wt_delete(&project, &branch, keep_branch, force),
            WtCommand::List { project } => run_wt_list(project.as_deref()),
            WtCommand::Import { project } => run_wt_import(&project),
        },
        Some(Command::Tui) | None => {
            // Set up signal handler for graceful shutdown
            let running = Arc::new(AtomicBool::new(true));
            let r = running.clone();
            ctrlc::set_handler(move || {
                r.store(false, Ordering::SeqCst);
            })
            .expect("Error setting Ctrl-C handler");

            let mut terminal = ratatui::init();
            let action = run_tui(&mut terminal, &args, running);
            ratatui::restore();
            // Run spread/collapse after terminal is restored (popup closed)
            handle_post_action(action?)
        }
    }
}
