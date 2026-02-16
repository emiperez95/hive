//! hive: Interactive Claude Code session dashboard for tmux.

mod common;
mod daemon;
mod ipc;
mod tui;

use anyhow::Result;
use clap::{Parser, Subcommand};
use crossterm::event::{poll, read, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::common::debug::{debug_log, init_debug};
use crate::common::persistence::{
    is_globally_muted, load_muted_sessions, load_skipped_sessions, save_parked_sessions,
    sesh_connect,
};
use crate::common::tmux::{
    get_current_tmux_session, get_current_tmux_session_names, switch_to_session,
};
use crate::common::types::PERMISSION_KEYS;
use crate::tui::app::{find_session_by_permission_key, App, InputMode, SearchResult};
use crate::tui::ui::ui;

#[derive(Parser, Debug)]
#[command(name = "hive")]
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
}

fn run_tui(
    terminal: &mut ratatui::DefaultTerminal,
    args: &Args,
    running: Arc<AtomicBool>,
) -> Result<()> {
    let mut app = App::new(args.filter.clone(), args.watch);
    app.auto_detail = args.detail;

    loop {
        if !running.load(Ordering::SeqCst) {
            app.save_restorable();
            return Ok(());
        }

        if !app.showing_parked {
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
                return Ok(());
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
                        "KEY: {:?} (mode={:?}, showing_parked={}, showing_detail={:?}, showing_help={})",
                        code,
                        app.input_mode,
                        app.showing_parked,
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
                                return Ok(());
                            }
                            _ => {}
                        }
                    } else if app.showing_parked {
                        match code {
                            KeyCode::Char('u') | KeyCode::Char('U') | KeyCode::Esc => {
                                app.showing_parked = false;
                                app.parked_selected = 0;
                                needs_redraw = true;
                            }
                            KeyCode::Char('q') | KeyCode::Char('Q') => {
                                app.save_restorable();
                                return Ok(());
                            }
                            KeyCode::Up | KeyCode::Char('k') => {
                                if app.parked_selected > 0 {
                                    app.parked_selected -= 1;
                                }
                                needs_redraw = true;
                            }
                            KeyCode::Down | KeyCode::Char('j') => {
                                let count = app.parked_list().len();
                                if count > 0 && app.parked_selected < count - 1 {
                                    app.parked_selected += 1;
                                }
                                needs_redraw = true;
                            }
                            KeyCode::Enter => {
                                app.unpark_selected();
                                if !app.showing_parked {
                                    return Ok(());
                                }
                                should_refresh = true;
                                break;
                            }
                            KeyCode::Char(c) if c.is_ascii_lowercase() => {
                                let idx = (c as u8 - b'a') as usize;
                                let count = app.parked_list().len();
                                if idx < count {
                                    app.parked_selected = idx;
                                    needs_redraw = true;
                                }
                            }
                            _ => {}
                        }
                    } else if app.input_mode == InputMode::ParkNote {
                        match code {
                            KeyCode::Esc => {
                                app.cancel_park_input();
                                needs_redraw = true;
                            }
                            KeyCode::Enter if modifiers.contains(KeyModifiers::ALT) => {
                                app.input_buffer.push('\n');
                                needs_redraw = true;
                            }
                            KeyCode::Enter => {
                                app.complete_park_session();
                                should_refresh = true;
                                break;
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
                    } else if app.input_mode == InputMode::Search {
                        match code {
                            KeyCode::Esc => {
                                app.input_mode = InputMode::Normal;
                                app.search_query.clear();
                                app.search_results.clear();
                                needs_redraw = true;
                            }
                            KeyCode::Enter => {
                                if let Some(result) = app.search_results.get(app.selected).cloned()
                                {
                                    match result {
                                        SearchResult::Active(idx) => {
                                            app.open_detail(idx);
                                            app.input_mode = InputMode::Normal;
                                            app.search_query.clear();
                                            app.search_results.clear();
                                            needs_redraw = true;
                                        }
                                        SearchResult::Parked(name) => {
                                            app.showing_parked_detail = Some(name);
                                            app.input_mode = InputMode::Normal;
                                            app.search_query.clear();
                                            app.search_results.clear();
                                            needs_redraw = true;
                                        }
                                        SearchResult::SeshProject(name) => {
                                            app.input_mode = InputMode::Normal;
                                            app.search_query.clear();
                                            app.search_results.clear();
                                            if sesh_connect(&name) {
                                                app.save_restorable();
                                                return Ok(());
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
                            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => {
                                app.save_restorable();
                                return Ok(());
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
                                let todo_count = app.detail_todos().len();
                                if app.detail_selected < todo_count {
                                    app.delete_selected_todo();
                                }
                                needs_redraw = true;
                            }
                            KeyCode::Up | KeyCode::Char('k') => {
                                if app.detail_selected > 0 {
                                    app.detail_selected -= 1;
                                } else if app.detail_scroll_offset > 0 {
                                    app.detail_scroll_offset -= 1;
                                }
                                needs_redraw = true;
                            }
                            KeyCode::Down | KeyCode::Char('j') => {
                                let todo_count = app.detail_todos().len();
                                let port_count = app
                                    .showing_detail
                                    .and_then(|idx| app.session_infos.get(idx))
                                    .map(|s| s.listening_ports.len())
                                    .unwrap_or(0);
                                let total = todo_count + port_count;
                                if total > 0 && app.detail_selected < total - 1 {
                                    app.detail_selected += 1;
                                } else {
                                    app.detail_scroll_offset += 1;
                                }
                                needs_redraw = true;
                            }
                            KeyCode::Enter => {
                                let todo_count = app.detail_todos().len();
                                let port_count = app
                                    .showing_detail
                                    .and_then(|idx| app.session_infos.get(idx))
                                    .map(|s| s.listening_ports.len())
                                    .unwrap_or(0);
                                if app.detail_selected >= todo_count && port_count > 0 {
                                    let port_idx = app.detail_selected - todo_count;
                                    if let Some(session) = app
                                        .showing_detail
                                        .and_then(|idx| app.session_infos.get(idx))
                                    {
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
                                                let url =
                                                    format!("http://localhost:{}", port_info.port);
                                                crate::common::chrome::open_chrome_tab(&url);
                                            }
                                        }
                                    }
                                    needs_redraw = true;
                                } else if let Some(name) = app.detail_session_name() {
                                    switch_to_session(&name);
                                    app.save_restorable();
                                    return Ok(());
                                }
                            }
                            KeyCode::Char('p') | KeyCode::Char('P') => {
                                if let Some(idx) = app.showing_detail {
                                    app.start_park_session(idx);
                                    needs_redraw = true;
                                }
                            }
                            KeyCode::Char('!') => {
                                if let Some(idx) = app.showing_detail {
                                    app.toggle_auto_approve(idx);
                                    needs_redraw = true;
                                }
                            }
                            KeyCode::Char('m') | KeyCode::Char('M') => {
                                if let Some(idx) = app.showing_detail {
                                    app.toggle_mute(idx);
                                    needs_redraw = true;
                                }
                            }
                            KeyCode::Char('s') | KeyCode::Char('S') => {
                                if let Some(idx) = app.showing_detail {
                                    app.toggle_skip(idx);
                                    needs_redraw = true;
                                }
                            }
                            _ => {}
                        }
                    } else if app.showing_parked_detail.is_some() {
                        match code {
                            KeyCode::Esc => {
                                app.showing_parked_detail = None;
                                needs_redraw = true;
                            }
                            KeyCode::Char('q') | KeyCode::Char('Q') => {
                                app.save_restorable();
                                return Ok(());
                            }
                            KeyCode::Enter => {
                                if let Some(name) = app.showing_parked_detail.take() {
                                    if sesh_connect(&name) {
                                        app.parked_sessions.remove(&name);
                                        save_parked_sessions(&app.parked_sessions);
                                        should_refresh = true;
                                        break;
                                    } else {
                                        app.error_message = Some((
                                            format!("Failed to unpark '{}'", name),
                                            std::time::Instant::now(),
                                        ));
                                        app.showing_parked_detail = Some(name);
                                        needs_redraw = true;
                                    }
                                }
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
                            KeyCode::Char('u') | KeyCode::Char('U') => {
                                app.showing_parked = true;
                                app.parked_selected = 0;
                                needs_redraw = true;
                            }
                            KeyCode::Char('r') | KeyCode::Char('R') => {
                                should_refresh = true;
                                break;
                            }
                            KeyCode::Char('m') | KeyCode::Char('M') => {
                                app.toggle_global_mute();
                                needs_redraw = true;
                            }
                            KeyCode::Char('q') | KeyCode::Char('Q') => {
                                app.save_restorable();
                                return Ok(());
                            }
                            KeyCode::Esc => {
                                app.save_restorable();
                                return Ok(());
                            }
                            KeyCode::Char('c') if cfg!(unix) => {
                                app.save_restorable();
                                return Ok(());
                            }
                            KeyCode::Char('/') => {
                                app.input_mode = InputMode::Search;
                                app.search_query.clear();
                                app.search_scroll_offset = 0;
                                app.load_sesh_projects();
                                app.update_search_results();
                                app.selected = 0;
                                needs_redraw = true;
                            }
                            KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
                                let idx = c.to_digit(10).unwrap() as usize - 1;
                                if let Some(session_info) = app.session_infos.get(idx) {
                                    switch_to_session(&session_info.name);
                                    app.save_restorable();
                                    return Ok(());
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

    if let Some(updated_session) = handle_hook_event(&mut state, hook_event) {
        // Send notification if session needs attention and not muted
        if updated_session.needs_attention {
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

    let needs_hook_changes = !hooks_missing.is_empty() || !hooks_stale.is_empty();

    if !needs_hook_changes && tmux_s_bound && tmux_d_bound {
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
    let tmux_s_cmd = format!("display-popup -E -w 80% -h 70% \"{}\"", binary_str);
    let tmux_d_cmd = format!("display-popup -E -w 80% -h 70% \"{} --detail\"", binary_str);

    if !tmux_s_bound || !tmux_d_bound {
        println!();
        println!("Register tmux keybindings?");
        if !tmux_s_bound {
            println!("  prefix+s -> hive (list view)");
        }
        if !tmux_d_bound {
            println!("  prefix+d -> hive --detail (detail view)");
        }
        print!("[Y/n] ");
        std::io::Write::flush(&mut std::io::stdout()).ok();

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        let input = input.trim().to_lowercase();
        if input.is_empty() || input == "y" || input == "yes" {
            let mut registered = Vec::new();
            let mut failed = Vec::new();

            if !tmux_s_bound {
                match std::process::Command::new("tmux")
                    .args(["bind-key", "s", &tmux_s_cmd])
                    .status()
                {
                    Ok(s) if s.success() => registered.push("s"),
                    _ => failed.push("s"),
                }
            }
            if !tmux_d_bound {
                match std::process::Command::new("tmux")
                    .args(["bind-key", "d", &tmux_d_cmd])
                    .status()
                {
                    Ok(s) if s.success() => registered.push("d"),
                    _ => failed.push("d"),
                }
            }

            if !registered.is_empty() {
                println!("Tmux keybindings registered (current session only).");
                println!("Add to ~/.tmux.conf to persist:");
                if !tmux_s_bound && registered.contains(&"s") {
                    println!("  bind-key s {}", tmux_s_cmd);
                }
                if !tmux_d_bound && registered.contains(&"d") {
                    println!("  bind-key d {}", tmux_d_cmd);
                }
            }
            if !failed.is_empty() {
                println!("Could not register some keybindings (tmux not running?).");
            }
        } else {
            println!("Skipped tmux keybindings.");
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
    println!("Unbind tmux keybindings (prefix+s, prefix+d)?");
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
        println!("Tmux keybindings unbound (current session only).");
        println!("Remove from ~/.tmux.conf manually if present.");
    } else {
        println!("Skipped tmux keybinding removal.");
    }

    Ok(())
}

/// Cycle to next/prev tmux session, skipping skipped sessions
fn run_cycle(forward: bool) -> Result<()> {
    let skipped = load_skipped_sessions();
    let all_sessions = get_current_tmux_session_names();

    let filtered: Vec<&String> = all_sessions
        .iter()
        .filter(|name| !skipped.contains(*name))
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

fn main() -> Result<()> {
    let args = Args::parse();
    init_debug(args.debug);

    match args.command {
        Some(Command::Hook { event }) => run_hook(&event),
        Some(Command::Setup) => run_setup(),
        Some(Command::Uninstall) => run_uninstall(),
        Some(Command::CycleNext) => run_cycle(true),
        Some(Command::CyclePrev) => run_cycle(false),
        Some(Command::Tui) | None => {
            // Set up signal handler for graceful shutdown
            let running = Arc::new(AtomicBool::new(true));
            let r = running.clone();
            ctrlc::set_handler(move || {
                r.store(false, Ordering::SeqCst);
            })
            .expect("Error setting Ctrl-C handler");

            let mut terminal = ratatui::init();
            let result = run_tui(&mut terminal, &args, running);
            ratatui::restore();
            result
        }
    }
}
