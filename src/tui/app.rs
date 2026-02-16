//! TUI application state and logic.

use crate::common::debug::debug_log;
use crate::common::persistence::{
    is_globally_muted, load_auto_approve_sessions, load_muted_sessions, load_parked_sessions,
    load_session_todos, load_skipped_sessions, save_auto_approve_sessions, save_muted_sessions,
    save_parked_sessions, save_restorable_sessions, save_session_todos, save_skipped_sessions,
    set_global_mute,
};
use crate::common::projects::{
    connect_project, has_project_config, list_project_names, ProjectRegistry,
};
use crate::common::ports::get_listening_ports_for_pids;
use crate::common::process::{get_all_descendants, get_process_info, is_claude_process};
use crate::common::tmux::{get_tmux_sessions, kill_tmux_session};
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
    ParkNote, // Entering note for parking
    AddTodo,  // Adding a todo in detail view
    Search,   // Interactive session search
}

/// Search result item - active session, parked one, or inactive sesh project
#[derive(Clone)]
pub enum SearchResult {
    Active(usize),       // Index into session_infos
    Parked(String),      // Session name from parked_sessions
    Project(String), // Project name from registry (not active, not parked)
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
    // Parking feature
    pub parked_sessions: HashMap<String, String>, // name -> note
    pub showing_parked: bool,
    pub parked_selected: usize,
    pub error_message: Option<(String, Instant)>,
    // Text input (park note or add todo)
    pub input_mode: InputMode,
    pub input_buffer: String,
    pub pending_park_session: Option<usize>, // session index to park after note entry
    // Session todos
    pub session_todos: HashMap<String, Vec<String>>, // name -> list of todos
    // Detail view
    pub showing_detail: Option<usize>, // session index being viewed
    pub detail_selected: usize,        // selected todo index in detail view
    pub detail_scroll_offset: usize,   // scroll offset for detail view content
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
    pub project_names: Vec<String>, // Cached list of all project session names
    // Parked session detail view
    pub showing_parked_detail: Option<String>, // parked session name being viewed
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
    // Help screen visible
    pub showing_help: bool,
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
            parked_sessions: load_parked_sessions(),
            showing_parked: false,
            parked_selected: 0,
            error_message: None,
            input_mode: InputMode::Normal,
            input_buffer: String::new(),
            pending_park_session: None,
            session_todos: load_session_todos(),
            showing_detail: None,
            detail_selected: 0,
            detail_scroll_offset: 0,
            last_save: Instant::now(),
            permission_key_map: HashMap::new(),
            pending_approvals: HashSet::new(),
            search_query: String::new(),
            search_results: Vec::new(),
            search_scroll_offset: 0,
            project_names: Vec::new(),
            showing_parked_detail: None,
            auto_approve_sessions: load_auto_approve_sessions(),
            muted_sessions: load_muted_sessions(),
            global_mute: is_globally_muted(),
            skipped_sessions: load_skipped_sessions(),
            auto_detail: false,
            detail_chrome_tabs: Vec::new(),
            showing_help: false,
        }
    }

    /// Update search results based on current query
    pub fn update_search_results(&mut self) {
        self.search_results.clear();
        let query = self.search_query.to_lowercase();

        // Collect active session names for deduplication
        let active_names: HashSet<String> =
            self.session_infos.iter().map(|s| s.name.clone()).collect();

        // Add matching active sessions
        for (i, info) in self.session_infos.iter().enumerate() {
            if query.is_empty() || info.name.to_lowercase().contains(&query) {
                self.search_results.push(SearchResult::Active(i));
            }
        }

        // Add matching parked sessions
        for name in self.parked_sessions.keys() {
            if query.is_empty() || name.to_lowercase().contains(&query) {
                self.search_results.push(SearchResult::Parked(name.clone()));
            }
        }

        // Add matching projects that are not active and not parked
        for name in &self.project_names {
            if active_names.contains(name) || self.parked_sessions.contains_key(name) {
                continue;
            }
            if query.is_empty() || name.to_lowercase().contains(&query) {
                self.search_results
                    .push(SearchResult::Project(name.clone()));
            }
        }

        // Reset selection if out of bounds
        if self.selected >= self.search_results.len() {
            self.selected = 0;
        }
    }

    /// Load project names list (called when entering search mode)
    pub fn load_project_names(&mut self) {
        self.project_names = list_project_names();
    }

    /// Calculate lines needed to display a search result
    fn lines_for_search_result(&self, result: &SearchResult) -> usize {
        match result {
            SearchResult::Active(_) => 1,
            SearchResult::Parked(name) => {
                if let Some(note) = self.parked_sessions.get(name) {
                    if !note.is_empty() {
                        return 2;
                    }
                }
                1
            }
            SearchResult::Project(_) => 1,
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
            });
        }

        // Sort: Claude (non-skipped) -> non-Claude (non-skipped) -> skipped
        session_infos.sort_by_key(|s| {
            let is_skipped = self.skipped_sessions.contains(&s.name);
            (is_skipped, s.claude_status.is_none())
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

        self.session_infos = session_infos;

        // Fetch Chrome tabs for detail view
        if let Some(idx) = self.showing_detail {
            if let Some(session) = self.session_infos.get(idx) {
                if !session.listening_ports.is_empty() {
                    let all_tabs = crate::common::chrome::get_chrome_tabs();
                    self.detail_chrome_tabs = crate::common::chrome::match_tabs_to_ports(
                        &all_tabs,
                        &session.listening_ports,
                    );
                } else {
                    self.detail_chrome_tabs.clear();
                }
            }
        } else {
            self.detail_chrome_tabs.clear();
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

        // Update search results if in search mode
        if self.input_mode == InputMode::Search {
            self.update_search_results();
        } else if !self.session_infos.is_empty() {
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

    /// Get sorted list of parked sessions (name, note)
    pub fn parked_list(&self) -> Vec<(String, String)> {
        let mut list: Vec<_> = self
            .parked_sessions
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        list.sort_by(|a, b| a.0.cmp(&b.0));
        list
    }

    /// Start parking a session
    pub fn start_park_session(&mut self, idx: usize) {
        if let Some(session_info) = self.session_infos.get(idx) {
            let name = session_info.name.clone();
            if !has_project_config(&name) {
                self.error_message = Some((
                    format!("Cannot park '{}': no project config", name),
                    Instant::now(),
                ));
                return;
            }
            self.input_mode = InputMode::ParkNote;
            self.input_buffer.clear();
            self.pending_park_session = Some(idx);
        }
    }

    /// Complete parking a session with the given note
    pub fn complete_park_session(&mut self) {
        if let Some(idx) = self.pending_park_session.take() {
            if let Some(session_info) = self.session_infos.get(idx) {
                let name = session_info.name.clone();
                let note = self.input_buffer.trim().to_string();
                if kill_tmux_session(&name) {
                    self.parked_sessions.insert(name.clone(), note);
                    save_parked_sessions(&self.parked_sessions);
                    self.showing_detail = None;
                } else {
                    self.error_message =
                        Some((format!("Failed to kill session '{}'", name), Instant::now()));
                }
            }
        }
        self.input_mode = InputMode::Normal;
        self.input_buffer.clear();
    }

    /// Cancel note input
    pub fn cancel_park_input(&mut self) {
        self.input_mode = InputMode::Normal;
        self.input_buffer.clear();
        self.pending_park_session = None;
    }

    /// Unpark the selected parked session
    pub fn unpark_selected(&mut self) {
        let list = self.parked_list();
        if let Some((name, _note)) = list.get(self.parked_selected) {
            let name = name.clone();
            if connect_project(&name) {
                self.parked_sessions.remove(&name);
                save_parked_sessions(&self.parked_sessions);
                self.showing_parked = false;
                self.parked_selected = 0;
            } else {
                self.error_message = Some((format!("Failed to unpark '{}'", name), Instant::now()));
            }
        }
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

    // --- Detail view methods ---

    pub fn open_detail(&mut self, idx: usize) {
        if idx < self.session_infos.len() {
            self.showing_detail = Some(idx);
            self.detail_selected = 0;
            self.detail_scroll_offset = 0;
        }
    }

    pub fn detail_session_name(&self) -> Option<String> {
        self.showing_detail
            .and_then(|idx| self.session_infos.get(idx))
            .map(|s| s.name.clone())
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
        let Some(name) = self.detail_session_name() else {
            return;
        };

        let should_save = if let Some(todos) = self.session_todos.get_mut(&name) {
            if self.detail_selected < todos.len() {
                todos.remove(self.detail_selected);
                if self.detail_selected >= todos.len() && self.detail_selected > 0 {
                    self.detail_selected -= 1;
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
        let registry = ProjectRegistry::load();
        let restorable: Vec<String> = self
            .session_infos
            .iter()
            .filter(|s| registry.has_project(&s.name))
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

    pub fn toggle_auto_approve(&mut self, idx: usize) {
        let Some(session_info) = self.session_infos.get(idx) else {
            return;
        };
        let name = session_info.name.clone();
        if self.auto_approve_sessions.contains(&name) {
            self.auto_approve_sessions.remove(&name);
            self.error_message = Some((format!("Auto-approve OFF for '{}'", name), Instant::now()));
        } else {
            self.auto_approve_sessions.insert(name.clone());
            self.error_message = Some((format!("Auto-approve ON for '{}'", name), Instant::now()));
        }
        save_auto_approve_sessions(&self.auto_approve_sessions);
    }

    pub fn is_auto_approved(&self, name: &str) -> bool {
        self.auto_approve_sessions.contains(name)
    }

    pub fn toggle_mute(&mut self, idx: usize) {
        let Some(session_info) = self.session_infos.get(idx) else {
            return;
        };
        let name = session_info.name.clone();
        if self.muted_sessions.contains(&name) {
            self.muted_sessions.remove(&name);
            self.error_message = Some((format!("Notifications ON for '{}'", name), Instant::now()));
        } else {
            self.muted_sessions.insert(name.clone());
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

    pub fn toggle_skip(&mut self, idx: usize) {
        let Some(session_info) = self.session_infos.get(idx) else {
            return;
        };
        let name = session_info.name.clone();
        if self.skipped_sessions.contains(&name) {
            self.skipped_sessions.remove(&name);
            self.error_message = Some((format!("Cycling ON for '{}'", name), Instant::now()));
        } else {
            self.skipped_sessions.insert(name.clone());
            self.error_message = Some((format!("Cycling OFF for '{}'", name), Instant::now()));
        }
        save_skipped_sessions(&self.skipped_sessions);
    }

    pub fn is_skipped(&self, name: &str) -> bool {
        self.skipped_sessions.contains(name)
    }
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
