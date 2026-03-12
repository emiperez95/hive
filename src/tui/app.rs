//! TUI application state and logic.

use crate::common::debug::debug_log;
use crate::common::persistence::{
    is_globally_muted, load_auto_approve_sessions, load_favorite_sessions, load_muted_sessions,
    load_session_todos, load_skipped_sessions, save_auto_approve_sessions, save_favorite_sessions,
    save_muted_sessions, save_restorable_sessions, save_session_todos, save_skipped_sessions,
    set_global_mute,
};
use crate::common::ports::get_listening_ports_for_pids;
use crate::common::process::{get_all_descendants, get_process_info, is_claude_process};
use crate::common::projects::{has_project_config, ProjectRegistry};
use crate::common::worktree::sanitize_branch_name;
use crate::common::tmux::{get_current_session, get_other_client_sessions, get_tmux_sessions};
use crate::common::types::{
    lines_for_session, matches_filter, ClaudeStatus, ProcessInfo, SessionInfo, PERMISSION_KEYS,
};
use crate::ipc::messages::{HookState, SessionState, SessionStatus};
use anyhow::Result;
use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use sysinfo::System;

/// Text input mode for the TUI
#[derive(Debug, PartialEq)]
pub enum InputMode {
    Normal,
    AddTodo,         // Adding a todo in detail view
    Search,          // Interactive session search
    SpreadPrompt,    // Waiting for digit 1-9 to spread iTerm2 panes
    WorktreeBranch,        // Typing branch name for new worktree
    WorktreeBase,          // Picking base branch for new worktree
    WorktreeConfirmDelete, // Confirming worktree deletion
}

/// Search result item - active session, inactive project, or worktree
#[derive(Clone)]
pub enum SearchResult {
    Active(String),   // Session name
    Project(String),  // Project name from registry (not active)
    Worktree(String), // Worktree session name from worktrees.json (not active)
}

/// TUI application state
pub struct App {
    pub sys: System,
    pub session_infos: Vec<SessionInfo>,
    pub filter: Option<String>,
    pub interval: u64,
    pub selected: usize,
    pub scroll_offset: usize,
    pub show_selection: bool,
    // Favorites
    pub favorite_sessions: HashSet<String>,
    pub error_message: Option<(String, Instant)>,
    // Text input
    pub input_mode: InputMode,
    pub input_buffer: String,
    // Session todos
    pub session_todos: HashMap<String, Vec<String>>, // name -> list of todos
    // Detail view
    pub showing_detail: Option<String>, // session name being viewed
    pub detail_selected: Option<usize>, // selected todo/port index in detail view (None = no selection)
    pub detail_scroll_offset: usize,    // scroll offset for detail view content
    // Session restore
    pub last_save: Instant, // Track last save time for periodic saves
    // Stable permission key assignments (session name -> key)
    pub permission_key_map: HashMap<String, char>,
    // Sessions where we've sent permission approval but jsonl hasn't updated yet
    pub pending_approvals: HashSet<String>,
    // Search mode
    pub search_query: String,
    pub search_results: Vec<SearchResult>,
    pub search_scroll_offset: usize, // Scroll offset for search results
    pub project_names: Vec<String>,  // Cached list of all project session names
    pub worktree_names: Vec<String>, // Flat list of all worktree session names
    pub worktrees_by_project: HashMap<String, Vec<String>>, // project_key → worktree session names
    // Per-session auto-approve toggle
    pub auto_approve_sessions: HashSet<String>,
    // Per-session notification mute
    pub muted_sessions: HashSet<String>,
    pub global_mute: bool,
    // Per-session skip from cycling
    pub skipped_sessions: HashSet<String>,
    // Auto-open detail view on first refresh
    pub auto_detail: bool,
    // Chrome tabs matched to the currently viewed detail session's ports
    pub detail_chrome_tabs: Vec<(crate::common::chrome::ChromeTab, u16)>,
    // Branch commits ahead of base (for worktree sessions)
    pub detail_commits: Vec<String>,
    // Help screen visible
    pub showing_help: bool,
    // Auto-picker mode: started with --picker, Esc exits app
    pub auto_picker: bool,
    // Detail view: line index of each selectable item, total rendered lines
    pub detail_item_lines: Vec<usize>,
    pub detail_total_lines: usize,
    // Worktree wizard: resolved project key
    pub wt_project_key: Option<String>,
    // Worktree wizard: branch name from step 1
    pub wt_branch_name: Option<String>,
    // Worktree wizard: base branch choices for picker
    pub wt_base_choices: Vec<String>,
    // Worktree wizard: selected index in base picker
    pub wt_base_selected: usize,
    // Worktree delete: project key and branch for pending deletion
    pub wt_delete_project: Option<String>,
    pub wt_delete_branch: Option<String>,
}

impl App {
    pub fn new(filter: Option<String>, interval: u64) -> Self {
        // Create persistent System instance - needs two refresh_all() calls
        // to establish baseline for CPU delta measurements
        let mut sys = System::new_all();
        sys.refresh_all();

        Self {
            sys,
            session_infos: Vec::new(),
            filter,
            interval,
            selected: 0,
            scroll_offset: 0,
            show_selection: false,
            favorite_sessions: load_favorite_sessions(),
            error_message: None,
            input_mode: InputMode::Normal,
            input_buffer: String::new(),
            session_todos: load_session_todos(),
            showing_detail: None,
            detail_selected: None,
            detail_scroll_offset: 0,
            last_save: Instant::now(),
            permission_key_map: HashMap::new(),
            pending_approvals: HashSet::new(),
            search_query: String::new(),
            search_results: Vec::new(),
            search_scroll_offset: 0,
            project_names: Vec::new(),
            worktree_names: Vec::new(),
            worktrees_by_project: HashMap::new(),
            auto_approve_sessions: load_auto_approve_sessions(),
            muted_sessions: load_muted_sessions(),
            global_mute: is_globally_muted(),
            skipped_sessions: load_skipped_sessions(),
            auto_detail: false,
            detail_chrome_tabs: Vec::new(),
            detail_commits: Vec::new(),
            showing_help: false,
            auto_picker: false,
            detail_item_lines: Vec::new(),
            detail_total_lines: 0,
            wt_project_key: None,
            wt_branch_name: None,
            wt_base_choices: Vec::new(),
            wt_base_selected: 0,
            wt_delete_project: None,
            wt_delete_branch: None,
        }
    }

    /// Update search results based on current query.
    /// Projects and their worktrees are grouped together: project first, then its worktrees.
    /// Searching for a project name also shows its worktrees.
    /// Uses cached project_names and worktrees_by_project (loaded in load_project_names).
    pub fn update_search_results(&mut self) {
        self.search_results.clear();
        let query = self.search_query.to_lowercase();

        // Collect active session names for deduplication
        let active_names: HashSet<String> =
            self.session_infos.iter().map(|s| s.name.clone()).collect();

        let worktree_names_set: HashSet<String> = self.worktree_names.iter().cloned().collect();

        // Track which worktrees were already added (to avoid duplicates)
        let mut added_worktrees: HashSet<String> = HashSet::new();

        // Add matching active sessions
        for info in self.session_infos.iter() {
            if query.is_empty() || info.name.to_lowercase().contains(&query) {
                self.search_results
                    .push(SearchResult::Active(info.name.clone()));
            }
        }

        // Add projects with their worktrees grouped underneath.
        // Uses cached project_names (loaded once when entering search mode).
        for session_name in &self.project_names {
            // Skip if this is a worktree name
            if worktree_names_set.contains(session_name) {
                continue;
            }

            let project_matches = query.is_empty() || session_name.to_lowercase().contains(&query);

            // Find worktrees for this project by checking worktrees_by_project keys
            // The key in worktrees_by_project is the project_key, not session_name.
            // We need to check all project keys whose worktrees might match.
            let mut matching_worktrees: Vec<&String> = Vec::new();
            let mut any_worktree_matches = false;
            for (proj_key, wt_names) in &self.worktrees_by_project {
                // Check if this project key is associated with this session_name
                // by checking if the session_name contains the project key
                if !session_name
                    .to_lowercase()
                    .contains(&proj_key.to_lowercase())
                {
                    continue;
                }
                for wt in wt_names {
                    if wt.to_lowercase().contains(&query) {
                        any_worktree_matches = true;
                    }
                    matching_worktrees.push(wt);
                }
            }

            let show_project = project_matches || any_worktree_matches;

            if show_project {
                if !active_names.contains(session_name) {
                    self.search_results
                        .push(SearchResult::Project(session_name.clone()));
                }

                for wt_name in &matching_worktrees {
                    if added_worktrees.contains(*wt_name) {
                        continue;
                    }
                    if active_names.contains(*wt_name) {
                        continue;
                    }
                    if project_matches || wt_name.to_lowercase().contains(&query) {
                        self.search_results
                            .push(SearchResult::Worktree((*wt_name).clone()));
                        added_worktrees.insert((*wt_name).clone());
                    }
                }
            }
        }

        // Add any orphan worktrees (no matching project in registry)
        for name in &self.worktree_names {
            if added_worktrees.contains(name) || active_names.contains(name) {
                continue;
            }
            if query.is_empty() || name.to_lowercase().contains(&query) {
                self.search_results
                    .push(SearchResult::Worktree(name.clone()));
            }
        }

        // Sort non-active results: favorites first, preserving relative order
        let active_count = self
            .search_results
            .iter()
            .take_while(|r| matches!(r, SearchResult::Active(_)))
            .count();
        if active_count < self.search_results.len() {
            let non_active = self.search_results.split_off(active_count);
            let mut fav_results = Vec::new();
            let mut rest_results = Vec::new();
            for r in non_active {
                let name = match &r {
                    SearchResult::Project(n) | SearchResult::Worktree(n) => n,
                    SearchResult::Active(_) => unreachable!(),
                };
                if self.favorite_sessions.contains(name) {
                    fav_results.push(r);
                } else {
                    rest_results.push(r);
                }
            }
            self.search_results.append(&mut fav_results);
            self.search_results.append(&mut rest_results);
        }

        // Reset selection if out of bounds
        if self.selected >= self.search_results.len() {
            self.selected = 0;
        }
    }

    /// Load project and worktree names lists (called when entering search mode)
    pub fn load_project_names(&mut self) {
        self.project_names = ProjectRegistry::load().list_session_names();
        let wt_state = crate::common::worktree::WorktreeState::load();
        self.worktree_names = wt_state
            .worktrees
            .values()
            .map(|e| e.session_name.clone())
            .collect();
        let mut by_project: HashMap<String, Vec<String>> = HashMap::new();
        for entry in wt_state.worktrees.values() {
            by_project
                .entry(entry.project_key.clone())
                .or_default()
                .push(entry.session_name.clone());
        }
        self.worktrees_by_project = by_project;
    }

    /// Calculate lines needed to display a search result
    fn lines_for_search_result(&self, result: &SearchResult) -> usize {
        match result {
            SearchResult::Active(_) | SearchResult::Project(_) | SearchResult::Worktree(_) => 1,
        }
    }

    /// Ensure the selected search result is visible within the available height
    pub fn ensure_search_visible(&mut self, available_height: usize) {
        if available_height == 0 || self.search_results.is_empty() {
            return;
        }

        if self.selected < self.search_scroll_offset {
            self.search_scroll_offset = self.selected;
        }

        loop {
            let mut used = 0;
            for i in self.search_scroll_offset..=self.selected.min(self.search_results.len() - 1) {
                used += self.lines_for_search_result(&self.search_results[i]);
            }
            if used <= available_height {
                break;
            }
            self.search_scroll_offset += 1;
            if self.search_scroll_offset >= self.search_results.len() {
                break;
            }
        }
    }

    /// Refresh session data (gather from tmux + sysinfo, with hook state overlay)
    pub fn refresh(&mut self) -> Result<()> {
        self.sys.refresh_all();

        // Load hook state from file (written by `hive hook`)
        let hook_state = HookState::load();

        // Index hook sessions by cwd — keep only most recently active per cwd
        let hook_sessions: HashMap<String, &SessionState> = {
            let mut by_cwd: HashMap<String, &SessionState> = HashMap::new();
            for session in hook_state.sessions.values() {
                let key = session.cwd.clone();
                let is_newer = by_cwd
                    .get(&key)
                    .is_none_or(|existing| session.last_activity > existing.last_activity);
                if is_newer {
                    by_cwd.insert(key, session);
                }
            }
            by_cwd
        };

        let using_hooks = !hook_sessions.is_empty();
        if using_hooks {
            debug_log(&format!(
                "REFRESH: Using hook state for {} sessions (by cwd)",
                hook_sessions.len()
            ));
        }

        let sessions = get_tmux_sessions()?;
        let other_client_sessions = get_other_client_sessions();
        let current_session = get_current_session();
        let mut session_infos = Vec::new();

        for session in sessions {
            if !matches_filter(&session.name, &self.filter) {
                continue;
            }

            // Get session CWD from first pane
            let session_cwd = session
                .windows
                .first()
                .and_then(|w| w.panes.first())
                .map(|p| p.cwd.clone());

            // Calculate session totals and collect per-process info
            let mut all_pids = Vec::new();
            for window in &session.windows {
                for pane in &window.panes {
                    all_pids.push(pane.pid);
                    get_all_descendants(&self.sys, pane.pid, &mut all_pids);
                }
            }

            let mut total_cpu = 0.0;
            let mut total_mem_kb = 0u64;
            let mut processes: Vec<ProcessInfo> = Vec::new();

            for &pid in &all_pids {
                if let Some(info) = get_process_info(&self.sys, pid) {
                    total_cpu += info.cpu_percent;
                    total_mem_kb += info.memory_kb;
                    if info.cpu_percent > 0.0 || info.memory_kb >= 1024 {
                        processes.push(info);
                    }
                }
            }

            processes.sort_by(|a, b| {
                b.cpu_percent
                    .partial_cmp(&a.cpu_percent)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });

            // Detect listening ports for all PIDs in this session
            let listening_ports = get_listening_ports_for_pids(&all_pids, &self.sys);

            // Find Claude pane: check hook state by cwd, or detect Claude process
            let mut claude_status: Option<ClaudeStatus> = None;
            let mut claude_pane: Option<(String, String, String)> = None;
            let mut last_activity = None;

            'outer: for window in &session.windows {
                for p in &window.panes {
                    let mut pane_pids = vec![p.pid];
                    get_all_descendants(&self.sys, p.pid, &mut pane_pids);

                    let has_claude_process = pane_pids.iter().any(|&pid| {
                        get_process_info(&self.sys, pid)
                            .map(|info| is_claude_process(&info))
                            .unwrap_or(false)
                    });

                    if has_claude_process {
                        // Use hook state if available (richer status info)
                        if let Some(hook_session) = hook_sessions.get(&p.cwd) {
                            claude_status = Some(convert_hook_status(&hook_session.status));
                            last_activity = hook_session
                                .last_activity
                                .as_ref()
                                .and_then(|s| parse_timestamp(s));
                        } else {
                            claude_status = Some(ClaudeStatus::Unknown);
                        }
                        claude_pane =
                            Some((session.name.clone(), window.index.clone(), p.index.clone()));
                        break 'outer;
                    }
                }
            }

            session_infos.push(SessionInfo {
                name: session.name.clone(),
                claude_status,
                claude_pane,
                permission_key: None,
                total_cpu,
                total_mem_kb,
                last_activity,
                processes,
                cwd: session_cwd,
                listening_ports,
                attached_other_client: other_client_sessions.contains(&session.name),
                is_current_session: current_session.as_deref() == Some(session.name.as_str()),
            });
        }

        // Sort: skipped last, Claude before non-Claude, favorites first within each group
        session_infos.sort_by_key(|s| {
            let is_favorite = self.favorite_sessions.contains(&s.name);
            let is_skipped = self.skipped_sessions.contains(&s.name);
            (is_skipped, s.claude_status.is_none(), !is_favorite)
        });

        // Stable permission key assignment
        let sessions_needing_permission: HashSet<String> = session_infos
            .iter()
            .filter(|s| {
                !self.pending_approvals.contains(&s.name)
                    && matches!(
                        s.claude_status,
                        Some(ClaudeStatus::NeedsPermission(_, _))
                            | Some(ClaudeStatus::EditApproval(_))
                    )
            })
            .map(|s| s.name.clone())
            .collect();

        self.pending_approvals.retain(|name| {
            session_infos.iter().any(|s| {
                &s.name == name
                    && matches!(
                        s.claude_status,
                        Some(ClaudeStatus::NeedsPermission(_, _))
                            | Some(ClaudeStatus::EditApproval(_))
                    )
            })
        });

        self.permission_key_map
            .retain(|name, _| sessions_needing_permission.contains(name));

        let used_keys: HashSet<char> = self.permission_key_map.values().copied().collect();
        let mut available_keys: Vec<char> = PERMISSION_KEYS
            .iter()
            .filter(|k| !used_keys.contains(k))
            .copied()
            .collect();

        for session in &mut session_infos {
            if sessions_needing_permission.contains(&session.name) {
                if let Some(&existing_key) = self.permission_key_map.get(&session.name) {
                    session.permission_key = Some(existing_key);
                } else if let Some(new_key) = available_keys.pop() {
                    self.permission_key_map
                        .insert(session.name.clone(), new_key);
                    session.permission_key = Some(new_key);
                }
            }
        }

        // Stabilize selected index across re-sort
        let selected_name = self
            .session_infos
            .get(self.selected)
            .map(|s| s.name.clone());

        self.session_infos = session_infos;

        // Restore selected index by name
        if let Some(ref name) = selected_name {
            if let Some(new_idx) = self.session_infos.iter().position(|s| &s.name == name) {
                self.selected = new_idx;
            }
        }

        // Chrome tabs are fetched on-demand (refresh_chrome_tabs), not every cycle

        // Fetch branch commits for worktree sessions in detail view
        if let Some(name) = self.detail_session_name() {
            if let Some(entry) = crate::common::worktree::find_worktree_by_session_name(&name) {
                let registry = ProjectRegistry::load();
                let base = registry
                    .projects
                    .get(&entry.project_key)
                    .and_then(|c| c.default_base_branch.clone())
                    .unwrap_or_else(|| "main".to_string());
                self.detail_commits = get_commits_ahead(&entry.path, &base);
            } else {
                self.detail_commits.clear();
            }
        } else {
            self.detail_commits.clear();
        }

        // Debug log refresh summary
        if crate::common::debug::is_debug_enabled() {
            let summary: Vec<String> = self
                .session_infos
                .iter()
                .map(|s| {
                    format!(
                        "{}:{:?}",
                        s.name,
                        s.claude_status.as_ref().map(|cs| format!("{}", cs))
                    )
                })
                .collect();
            debug_log(&format!(
                "REFRESH: {} sessions: [{}]",
                self.session_infos.len(),
                summary.join(", ")
            ));
        }

        if !self.session_infos.is_empty() {
            if self.selected >= self.session_infos.len() {
                self.selected = self.session_infos.len() - 1;
            }
        } else {
            self.selected = 0;
        }

        Ok(())
    }

    pub fn hide_selection(&mut self) {
        self.show_selection = false;
        self.selected = 0;
        self.scroll_offset = 0;
    }

    pub fn move_selection_up(&mut self) {
        if !self.show_selection {
            self.show_selection = true;
            return;
        }
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    pub fn move_selection_down(&mut self) {
        if !self.show_selection {
            self.show_selection = true;
            return;
        }
        if !self.session_infos.is_empty() && self.selected < self.session_infos.len() - 1 {
            self.selected += 1;
        }
    }

    /// Toggle favorite for a session by name
    pub fn toggle_favorite(&mut self, name: &str) {
        if self.favorite_sessions.contains(name) {
            self.favorite_sessions.remove(name);
            self.error_message = Some((format!("Unfavorited '{}'", name), Instant::now()));
        } else {
            self.favorite_sessions.insert(name.to_string());
            self.error_message = Some((format!("Favorited '{}'", name), Instant::now()));
        }
        save_favorite_sessions(&self.favorite_sessions);
    }

    /// Check if a session is favorited
    pub fn is_favorite(&self, name: &str) -> bool {
        self.favorite_sessions.contains(name)
    }

    /// Clear error message if it's older than 3 seconds
    pub fn clear_old_error(&mut self) {
        if let Some((_, instant)) = &self.error_message {
            if instant.elapsed() > Duration::from_secs(3) {
                self.error_message = None;
            }
        }
    }

    pub fn ensure_visible(&mut self, available_height: usize) {
        if available_height == 0 || self.session_infos.is_empty() {
            return;
        }
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        }
        loop {
            let mut used = 0;
            for i in self.scroll_offset..=self.selected {
                used += lines_for_session(&self.session_infos[i]);
            }
            if used <= available_height {
                break;
            }
            self.scroll_offset += 1;
        }
    }

    /// Auto-scroll detail view to keep the selected item visible
    pub fn ensure_detail_visible(&mut self, available_height: usize) {
        if available_height == 0 || self.detail_total_lines <= available_height {
            self.detail_scroll_offset = 0;
            return;
        }

        let max_scroll = self.detail_total_lines.saturating_sub(available_height);

        if let Some(sel) = self.detail_selected {
            if let Some(&item_line) = self.detail_item_lines.get(sel) {
                // Scroll up if item is above visible window
                if item_line < self.detail_scroll_offset {
                    self.detail_scroll_offset = item_line.saturating_sub(1);
                }
                // Scroll down if item is below visible window
                let visible_end = self.detail_scroll_offset + available_height;
                if item_line >= visible_end {
                    self.detail_scroll_offset = (item_line + 2).saturating_sub(available_height);
                }
            }
        }

        if self.detail_scroll_offset > max_scroll {
            self.detail_scroll_offset = max_scroll;
        }
    }

    // --- Detail view methods ---

    pub fn open_detail(&mut self, idx: usize) {
        if let Some(info) = self.session_infos.get(idx) {
            self.showing_detail = Some(info.name.clone());
            self.detail_selected = None;
            self.detail_scroll_offset = 0;
            self.detail_item_lines.clear();
            self.detail_total_lines = 0;
            self.refresh_chrome_tabs();
        }
    }

    pub fn detail_session_name(&self) -> Option<String> {
        self.showing_detail.clone()
    }

    pub fn detail_session_info(&self) -> Option<&SessionInfo> {
        self.showing_detail
            .as_ref()
            .and_then(|name| self.session_infos.iter().find(|s| &s.name == name))
    }

    /// Fetch Chrome tabs matching the current detail session's ports (on-demand).
    pub fn refresh_chrome_tabs(&mut self) {
        if let Some(session) = self.detail_session_info() {
            if !session.listening_ports.is_empty() {
                let ports = session.listening_ports.clone();
                let all_tabs = crate::common::chrome::get_chrome_tabs();
                self.detail_chrome_tabs =
                    crate::common::chrome::match_tabs_to_ports(&all_tabs, &ports);
            } else {
                self.detail_chrome_tabs.clear();
            }
        } else {
            self.detail_chrome_tabs.clear();
        }
    }

    pub fn detail_todos(&self) -> Vec<String> {
        self.detail_session_name()
            .and_then(|name| self.session_todos.get(&name))
            .cloned()
            .unwrap_or_default()
    }

    pub fn start_add_todo(&mut self) {
        self.input_mode = InputMode::AddTodo;
        self.input_buffer.clear();
    }

    pub fn complete_add_todo(&mut self) {
        if let Some(name) = self.detail_session_name() {
            let todo = self.input_buffer.trim().to_string();
            if !todo.is_empty() {
                self.session_todos.entry(name).or_default().push(todo);
                save_session_todos(&self.session_todos);
            }
        }
        self.input_mode = InputMode::Normal;
        self.input_buffer.clear();
    }

    pub fn cancel_add_todo(&mut self) {
        self.input_mode = InputMode::Normal;
        self.input_buffer.clear();
    }

    pub fn delete_selected_todo(&mut self) {
        let Some(sel) = self.detail_selected else {
            return;
        };
        let Some(name) = self.detail_session_name() else {
            return;
        };

        let should_save = if let Some(todos) = self.session_todos.get_mut(&name) {
            if sel < todos.len() {
                todos.remove(sel);
                if todos.is_empty() {
                    self.detail_selected = None;
                } else if sel >= todos.len() {
                    self.detail_selected = Some(sel - 1);
                }
                true
            } else {
                false
            }
        } else {
            false
        };

        if should_save {
            save_session_todos(&self.session_todos);
        }
    }

    pub fn todo_count(&self, session_name: &str) -> usize {
        self.session_todos
            .get(session_name)
            .map(|v| v.len())
            .unwrap_or(0)
    }

    pub fn save_restorable(&self) {
        let restorable: Vec<String> = self
            .session_infos
            .iter()
            .filter(|s| has_project_config(&s.name))
            .map(|s| s.name.clone())
            .collect();
        save_restorable_sessions(&restorable);
    }

    pub fn maybe_periodic_save(&mut self) {
        if self.last_save.elapsed() > Duration::from_secs(600) {
            self.save_restorable();
            self.last_save = Instant::now();
        }
    }

    pub fn toggle_auto_approve(&mut self, name: &str) {
        if self.auto_approve_sessions.contains(name) {
            self.auto_approve_sessions.remove(name);
            self.error_message = Some((format!("Auto-approve OFF for '{}'", name), Instant::now()));
        } else {
            self.auto_approve_sessions.insert(name.to_string());
            self.error_message = Some((format!("Auto-approve ON for '{}'", name), Instant::now()));
        }
        save_auto_approve_sessions(&self.auto_approve_sessions);
    }

    pub fn is_auto_approved(&self, name: &str) -> bool {
        self.auto_approve_sessions.contains(name)
    }

    pub fn toggle_mute(&mut self, name: &str) {
        if self.muted_sessions.contains(name) {
            self.muted_sessions.remove(name);
            self.error_message = Some((format!("Notifications ON for '{}'", name), Instant::now()));
        } else {
            self.muted_sessions.insert(name.to_string());
            self.error_message =
                Some((format!("Notifications OFF for '{}'", name), Instant::now()));
        }
        save_muted_sessions(&self.muted_sessions);
    }

    pub fn is_muted(&self, name: &str) -> bool {
        self.muted_sessions.contains(name)
    }

    pub fn toggle_global_mute(&mut self) {
        self.global_mute = !self.global_mute;
        set_global_mute(self.global_mute);
        if self.global_mute {
            self.error_message = Some(("Global mute ON".to_string(), Instant::now()));
        } else {
            self.error_message = Some(("Global mute OFF".to_string(), Instant::now()));
        }
    }

    pub fn toggle_skip(&mut self, name: &str) {
        if self.skipped_sessions.contains(name) {
            self.skipped_sessions.remove(name);
            self.error_message = Some((format!("Cycling ON for '{}'", name), Instant::now()));
        } else {
            self.skipped_sessions.insert(name.to_string());
            self.error_message = Some((format!("Cycling OFF for '{}'", name), Instant::now()));
        }
        save_skipped_sessions(&self.skipped_sessions);
    }

    pub fn is_skipped(&self, name: &str) -> bool {
        self.skipped_sessions.contains(name)
    }

    /// Start the worktree creation wizard from the current detail view session.
    /// Resolves the project key from the session (worktree entry or project registry).
    /// Returns true if the wizard was started, false with an error message if not.
    pub fn start_worktree_wizard(&mut self) -> bool {
        let Some(name) = self.detail_session_name() else {
            return false;
        };

        // Try worktree entry first (session is already a worktree → use its project)
        if let Some(entry) = crate::common::worktree::find_worktree_by_session_name(&name) {
            let registry = ProjectRegistry::load();
            if registry
                .resolve_worktrees_dir(&entry.project_key, registry.projects.get(&entry.project_key).unwrap())
                .is_some()
            {
                self.wt_project_key = Some(entry.project_key);
                self.input_mode = InputMode::WorktreeBranch;
                self.input_buffer.clear();
                return true;
            }
        }

        // Try project registry (session is the main project session)
        let registry = ProjectRegistry::load();
        if let Some((key, config)) = registry.find_by_session_name(&name) {
            if registry.resolve_worktrees_dir(key, config).is_some() {
                self.wt_project_key = Some(key.to_string());
                self.input_mode = InputMode::WorktreeBranch;
                self.input_buffer.clear();
                return true;
            }
        }

        self.error_message = Some((
            format!("No worktree config for '{}'", name),
            std::time::Instant::now(),
        ));
        false
    }

    /// Transition from branch name input to base branch picker.
    pub fn enter_base_picker(&mut self) {
        let branch = sanitize_branch_name(self.input_buffer.trim());
        if branch.is_empty() {
            self.cancel_worktree_wizard();
            return;
        }
        self.wt_branch_name = Some(branch);
        self.input_buffer.clear();

        // Resolve project_root from registry
        let registry = ProjectRegistry::load();
        let project_root = self
            .wt_project_key
            .as_ref()
            .and_then(|key| registry.projects.get(key))
            .map(|config| {
                crate::common::projects::expand_tilde(&config.project_root)
                    .to_string_lossy()
                    .to_string()
            });

        let repo_path = project_root.as_deref().unwrap_or(".");

        // Build choices: check existence of staging/main/master, then current branch
        let candidates = ["staging", "main", "master"];
        let mut choices: Vec<String> = Vec::new();
        for &name in &candidates {
            if branch_exists(repo_path, name) {
                choices.push(name.to_string());
            }
        }
        if let Some(current) = get_current_branch(repo_path) {
            if !choices.contains(&current) {
                choices.push(current);
            }
        }

        if choices.is_empty() {
            choices.push("main".to_string());
        }

        // Pre-select default_base_branch from config if it's in the list
        let default_base = self
            .wt_project_key
            .as_ref()
            .and_then(|key| registry.projects.get(key))
            .and_then(|config| config.default_base_branch.clone());

        self.wt_base_selected = default_base
            .and_then(|db| choices.iter().position(|c| c == &db))
            .unwrap_or(0);

        self.wt_base_choices = choices;
        self.input_mode = InputMode::WorktreeBase;
    }

    /// Start worktree deletion flow for the current detail session.
    /// Returns true if the confirmation modal was shown, false with an error if not a worktree.
    pub fn start_worktree_delete(&mut self) -> bool {
        let Some(name) = self.detail_session_name() else {
            return false;
        };

        if let Some(entry) = crate::common::worktree::find_worktree_by_session_name(&name) {
            self.wt_delete_project = Some(entry.project_key);
            self.wt_delete_branch = Some(entry.branch);
            self.input_mode = InputMode::WorktreeConfirmDelete;
            true
        } else {
            self.error_message = Some((
                "Not a worktree session".to_string(),
                std::time::Instant::now(),
            ));
            false
        }
    }

    /// Cancel worktree deletion, resetting state.
    pub fn cancel_worktree_delete(&mut self) {
        self.input_mode = InputMode::Normal;
        self.wt_delete_project = None;
        self.wt_delete_branch = None;
    }

    /// Cancel the worktree wizard, resetting state.
    pub fn cancel_worktree_wizard(&mut self) {
        self.input_mode = InputMode::Normal;
        self.input_buffer.clear();
        self.wt_project_key = None;
        self.wt_branch_name = None;
        self.wt_base_choices.clear();
        self.wt_base_selected = 0;
    }
}

/// Get the current branch of a git repo.
fn get_current_branch(repo_path: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(repo_path)
        .output()
        .ok()?;
    if output.status.success() {
        let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if branch.is_empty() || branch == "HEAD" {
            None
        } else {
            Some(branch)
        }
    } else {
        None
    }
}

/// Check if a branch exists in a git repo.
fn branch_exists(repo_path: &str, branch: &str) -> bool {
    std::process::Command::new("git")
        .args(["rev-parse", "--verify", branch])
        .current_dir(repo_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Find session by permission key
pub fn find_session_by_permission_key(sessions: &[SessionInfo], key: char) -> Option<&SessionInfo> {
    sessions
        .iter()
        .find(|s| s.permission_key == Some(key.to_ascii_lowercase()))
}

/// Convert hook SessionStatus to TUI ClaudeStatus
fn convert_hook_status(status: &SessionStatus) -> ClaudeStatus {
    match status {
        SessionStatus::Waiting => ClaudeStatus::Waiting,
        SessionStatus::NeedsPermission {
            tool_name,
            description,
        } => ClaudeStatus::NeedsPermission(tool_name.clone(), description.clone()),
        SessionStatus::EditApproval { filename } => ClaudeStatus::EditApproval(filename.clone()),
        SessionStatus::PlanReview => ClaudeStatus::PlanReview,
        SessionStatus::QuestionAsked => ClaudeStatus::QuestionAsked,
        SessionStatus::Working => ClaudeStatus::Unknown,
        SessionStatus::Unknown => ClaudeStatus::Unknown,
    }
}

/// Parse ISO 8601 timestamp to DateTime
fn parse_timestamp(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Get commits ahead of base branch in a git repo.
/// Returns commit summary lines (from `git log base..HEAD --oneline`).
fn get_commits_ahead(repo_path: &str, base_branch: &str) -> Vec<String> {
    let output = std::process::Command::new("git")
        .args(["log", &format!("{}..HEAD", base_branch), "--oneline", "-20"])
        .current_dir(repo_path)
        .output();
    match output {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            text.lines().map(|l| l.to_string()).collect()
        }
        _ => Vec::new(),
    }
}
