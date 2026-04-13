//! TUI event loop — key handling and post-action dispatch.

use anyhow::{bail, Result};
use crossterm::event::{poll, read, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::cli::remote::{ensure_remote_sync, send_keys_to_remote};
use crate::cli::session::{run_collapse, run_spread};
use crate::cli::worktree::{run_wt_delete, run_wt_new};
use crate::cli::{Args, PostAction};
use crate::common::debug::debug_log;
use crate::common::projects::{connect_session, ProjectRegistry};
use crate::common::tmux::{get_current_tmux_session, resolve_tmux_path, switch_to_session};
use crate::common::types::PERMISSION_KEYS;
use crate::tui::app::{find_session_by_permission_key, App, InputMode, SearchResult};
use crate::tui::ui::ui;

/// Switch to a session or return a ConnectRemote post-action for remote sessions.
/// For local sessions, performs the switch immediately and returns None.
/// For remote sessions, returns Some(PostAction::ConnectRemote).
fn switch_or_connect(
    session_info: &crate::common::types::SessionInfo,
    app: &mut App,
) -> Option<PostAction> {
    use crate::common::types::SessionSource;
    match &session_info.source {
        SessionSource::Local => {
            app.unskip(&session_info.name);
            switch_to_session(&session_info.name);
            app.save_restorable();
            None
        }
        SessionSource::Remote {
            remote_key,
            remote_label,
            remote_emoji,
        } => {
            let ssh_host = crate::common::remotes::RemoteRegistry::load()
                .remotes
                .get(remote_key)
                .map(|c| c.ssh_host.clone())
                .unwrap_or_else(|| remote_key.clone());
            Some(PostAction::ConnectRemote {
                ssh_host,
                label: remote_label.clone(),
                emoji: remote_emoji.clone(),
                session_name: session_info.name.clone(),
            })
        }
    }
}

pub fn run_tui(
    terminal: &mut ratatui::DefaultTerminal,
    args: &Args,
    running: Arc<AtomicBool>,
) -> Result<PostAction> {
    // Auto-start remote sync if remotes are configured and sync isn't running
    ensure_remote_sync();

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
                                    } else if let Some(info) = app.detail_session_info().cloned() {
                                        if let Some(action) = switch_or_connect(&info, &mut app) {
                                            return Ok(action);
                                        }
                                        return Ok(PostAction::None);
                                    }
                                } else if let Some(info) = app.detail_session_info().cloned() {
                                    if let Some(action) = switch_or_connect(&info, &mut app) {
                                        return Ok(action);
                                    }
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
                                if let Some(session_info) = app.session_infos.get(idx).cloned() {
                                    if let Some(action) =
                                        switch_or_connect(&session_info, &mut app)
                                    {
                                        return Ok(action);
                                    }
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
                                        use crate::common::types::ClaudeStatus;
                                        let has_approve_always = matches!(
                                            session_info.claude_status,
                                            Some(ClaudeStatus::NeedsPermission(_, _))
                                        );
                                        let keys = if is_uppercase && has_approve_always {
                                            vec!["2".to_string(), "Enter".to_string()]
                                        } else {
                                            vec!["1".to_string(), "Enter".to_string()]
                                        };

                                        match &session_info.source {
                                            crate::common::types::SessionSource::Local => {
                                                use crate::common::tmux::send_key_to_pane;
                                                for key in &keys {
                                                    send_key_to_pane(sess, win, pane, key);
                                                }
                                            }
                                            crate::common::types::SessionSource::Remote {
                                                remote_key,
                                                ..
                                            } => {
                                                let registry =
                                                    crate::common::remotes::RemoteRegistry::load();
                                                if let Some(config) =
                                                    registry.remotes.get(remote_key)
                                                {
                                                    send_keys_to_remote(
                                                        &config.ssh_host,
                                                        sess,
                                                        win,
                                                        pane,
                                                        &keys,
                                                    );
                                                }
                                            }
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

pub fn handle_post_action(action: PostAction) -> Result<()> {
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
        PostAction::ConnectRemote {
            ssh_host,
            label,
            emoji,
            session_name,
        } => {
            let wrapper = format!("{} {} | {}", emoji, label, session_name);

            // Check if wrapper session already exists
            let exists = std::process::Command::new("tmux")
                .args(["has-session", "-t", &wrapper])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);

            if !exists {
                let ssh_cmd = format!(
                    "ssh -t -o RemoteCommand=none {} 'export PATH=/opt/homebrew/bin:/usr/local/bin:$PATH; exec tmux attach -t \"{}\"'",
                    ssh_host, session_name
                );
                std::process::Command::new("tmux")
                    .args(["new-session", "-d", "-s", &wrapper, &ssh_cmd])
                    .output()?;
            }

            switch_to_session(&wrapper);
            Ok(())
        }
        PostAction::None => Ok(()),
    }
}
