//! TUI application state and logic.

use crate::common::debug::debug_log;
use crate::common::persistence::{
    is_globally_muted, load_auto_approve_sessions, load_favorite_sessions, load_muted_sessions,
    load_session_todos, load_skipped_sessions, save_auto_approve_sessions, save_favorite_sessions,
    save_muted_sessions, save_restorable_sessions, save_session_todos, save_skipped_sessions,
    set_global_mute,
};
use crate::common::ports::get_listening_ports_for_pids;
use crate::common::process::{
    build_children_map, collect_descendants, get_process_info, is_claude_process,
};
use crate::common::instances::{detect_claude_instances, ClaudeInstance, HookIndex};
use crate::common::projects::{has_project_config, ProjectConfig, ProjectRegistry};
use crate::common::tmux::{get_current_session, get_other_client_sessions, get_tmux_sessions};
use crate::common::types::{
    lines_for_session, matches_filter, ClaudeStatus, ClaudeWindowInfo, ProcessInfo, SessionInfo,
    PERMISSION_KEYS,
};
use crate::common::worktree::sanitize_branch_name;
use crate::ipc::messages::{HookState, SessionState, SessionStatus};
use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use sysinfo::System;

/// Text input mode for the TUI
#[derive(Debug, PartialEq)]
pub enum InputMode {
    Normal,
    AddTodo,               // Adding a todo in detail view
    FreezeWindowPick,      // Choosing which Claude window to freeze (multi-window session)
    FreezeNote,            // Typing a note while freezing a window
    Search,                // Interactive session search
    SpreadPrompt,          // Waiting for digit 1-9 to spread iTerm2 panes
    WorktreeBranch,        // Typing branch name for new worktree
    WorktreeBase,          // Picking base branch for new worktree
    WorktreeConfirmDelete, // Confirming worktree deletion
    NewProjectKey,         // Typing project key in new project wizard
    NewProjectEmoji,       // Typing emoji in new project wizard
    Hint,                  // Vim-style quick-jump: type a 2-char label to switch
}

/// Search result item - active session, inactive project, or worktree
#[derive(Clone)]
pub enum SearchResult {
    Active(String),   // Session name
    Project(String),  // Project name from registry (not active)
    Worktree(String), // Worktree session name from worktrees.json (not active)
    Frozen(String),   // Frozen (hibernated) session name from frozen.json (not live)
}

/// A quick-jump target in hint mode: either a whole session or a specific
/// window within a multi-window session.
#[derive(Clone, Debug, PartialEq)]
pub enum HintTarget {
    Session(String),        // session name
    Window(String, String), // (session name, window index)
}

/// Home-row alphabet for hint labels. 9 keys → 81 two-char labels.
const HINT_ALPHABET: &[u8] = b"asdfghjkl";

/// Generate `count` unique fixed-length (2-char) labels from the home-row
/// alphabet, in a stable order: aa, as, ad, … Caps out at 81 labels; any
/// targets beyond that get no label (unreachable, but never happens in practice).
fn gen_hint_labels(count: usize) -> Vec<String> {
    let mut out = Vec::with_capacity(count);
    'outer: for &a in HINT_ALPHABET {
        for &b in HINT_ALPHABET {
            if out.len() >= count {
                break 'outer;
            }
            out.push(format!("{}{}", a as char, b as char));
        }
    }
    out
}

/// TUI application state
pub struct App {
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
    pub show_archived: bool, // Reveal archived projects in the picker
    pub archived_session_names: HashSet<String>, // Archived project session names (for styling)
    pub archived_worktree_names: HashSet<String>, // Worktrees of archived projects (inherit status)
    pub orphan_worktree_names: HashSet<String>, // Worktrees whose parent project was deleted
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
    // New project wizard: stored key from step 1
    pub np_key: Option<String>,
    // Hint mode: (label, target) pairs in display order, and keys typed so far
    pub hint_targets: Vec<(String, HintTarget)>,
    pub hint_buffer: String,
    // Frozen (hibernated) Claude windows, reloaded each refresh
    pub frozen_state: crate::common::frozen::FrozenState,
    // Window chosen to freeze, carried into the note prompt
    pub pending_freeze: Option<crate::common::frozen::FreezeTarget>,
    // Window picker (multi-Claude session): candidate windows + highlighted index
    pub freeze_choices: Vec<crate::common::frozen::FreezeTarget>,
    pub freeze_choice_selected: usize,
}

impl App {
    pub fn new(filter: Option<String>, interval: u64) -> Self {
        Self {
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
            show_archived: false,
            archived_session_names: HashSet::new(),
            archived_worktree_names: HashSet::new(),
            orphan_worktree_names: HashSet::new(),
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
            np_key: None,
            hint_targets: Vec::new(),
            hint_buffer: String::new(),
            frozen_state: crate::common::frozen::FrozenState::load(),
            pending_freeze: None,
            freeze_choices: Vec::new(),
            freeze_choice_selected: 0,
        }
    }

    /// Update search results based on current query.
    /// Projects and their worktrees are grouped together: project first, then its worktrees.
    /// Searching for a project name also shows its worktrees.
    /// Uses cached project_names and worktrees_by_project (loaded in load_project_names).
    pub fn update_search_results(&mut self) {
        self.search_results.clear();
        let query = self.search_query.to_lowercase();

        // Frozen Claude windows are pinned at the top as their own group, each a window to
        // resume. They sit alongside (not in place of) the parent session row — the session
        // may still be alive with other windows, or gone entirely. Keyed by entry key so two
        // frozen windows of the same session are distinct, selectable rows.
        for entry in self.frozen_state.sorted() {
            let matches = query.is_empty()
                || entry.session_name.to_lowercase().contains(&query)
                || entry.window_name.to_lowercase().contains(&query)
                || entry.note.to_lowercase().contains(&query);
            if matches {
                self.search_results.push(SearchResult::Frozen(entry.key()));
            }
        }

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
            // Archived projects are hidden only on the full list (empty query);
            // they appear as soon as the user types, or when revealed via Ctrl+R.
            if query.is_empty()
                && !self.show_archived
                && self.archived_session_names.contains(session_name)
            {
                continue;
            }

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

        // Add worktrees not already grouped under a project above. Worktrees whose
        // parent project was deleted (true orphans) are hidden from the full list
        // (empty query) so they don't clutter it, but stay findable by typing.
        // Worktrees of an existing project that simply weren't grouped (fuzzy
        // name-match miss) still show on the full list as before.
        for name in &self.worktree_names {
            if added_worktrees.contains(name) || active_names.contains(name) {
                continue;
            }
            // Orphan (deleted project) and archived-project worktrees are hidden
            // from the full list but stay findable by typing.
            let hidden_on_full_list = self.orphan_worktree_names.contains(name)
                || (self.archived_worktree_names.contains(name) && !self.show_archived);
            let matches = if hidden_on_full_list {
                !query.is_empty() && name.to_lowercase().contains(&query)
            } else {
                query.is_empty() || name.to_lowercase().contains(&query)
            };
            if matches {
                self.search_results
                    .push(SearchResult::Worktree(name.clone()));
            }
        }

        // Sort the non-pinned results (projects/worktrees): favorites first, preserving
        // relative order. The pinned prefix is the leading run of Frozen then Active rows.
        let pinned_count = self
            .search_results
            .iter()
            .take_while(|r| matches!(r, SearchResult::Frozen(_) | SearchResult::Active(_)))
            .count();
        if pinned_count < self.search_results.len() {
            let non_active = self.search_results.split_off(pinned_count);
            let mut fav_results = Vec::new();
            let mut rest_results = Vec::new();
            for r in non_active {
                let name = match &r {
                    SearchResult::Project(n) | SearchResult::Worktree(n) => n,
                    SearchResult::Active(_) | SearchResult::Frozen(_) => unreachable!(),
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

    /// Load project and worktree names lists (called when entering search mode).
    /// `project_names` holds *all* projects; archived ones are recorded in
    /// `archived_session_names` and filtered out at search time (see
    /// `update_search_results`) only when the query is empty and not revealed.
    pub fn load_project_names(&mut self) {
        // Sort by session name so the picker order is stable across reloads
        // (HashMap iteration order is randomized per instance, so an unsorted
        // list reshuffles every time the registry is reloaded).
        let registry = ProjectRegistry::load();
        let mut with_archived = registry.list_session_names_with_archived();
        with_archived.sort_by(|a, b| a.0.cmp(&b.0));
        self.archived_session_names = with_archived
            .iter()
            .filter(|(_, archived)| *archived)
            .map(|(name, _)| name.clone())
            .collect();
        self.project_names = with_archived.into_iter().map(|(name, _)| name).collect();
        let wt_state = crate::common::worktree::WorktreeState::load();
        self.worktree_names = wt_state
            .worktrees
            .values()
            .map(|e| e.session_name.clone())
            .collect();
        // A worktree is an orphan when its parent project no longer exists in the
        // registry (project deleted). These are hidden from the full list.
        self.orphan_worktree_names = wt_state
            .worktrees
            .values()
            .filter(|e| !registry.projects.contains_key(&e.project_key))
            .map(|e| e.session_name.clone())
            .collect();
        // Archived is a project status: worktrees of an archived project inherit it
        // (hidden from the full list, shown dimmed with an [archived] tag).
        let archived_keys: HashSet<&String> = registry
            .projects
            .iter()
            .filter(|(_, c)| c.archived)
            .map(|(k, _)| k)
            .collect();
        self.archived_worktree_names = wt_state
            .worktrees
            .values()
            .filter(|e| archived_keys.contains(&e.project_key))
            .map(|e| e.session_name.clone())
            .collect();
        self.worktree_names.sort();
        let mut by_project: HashMap<String, Vec<String>> = HashMap::new();
        for entry in wt_state.worktrees.values() {
            by_project
                .entry(entry.project_key.clone())
                .or_default()
                .push(entry.session_name.clone());
        }
        for names in by_project.values_mut() {
            names.sort();
        }
        self.worktrees_by_project = by_project;
    }

    /// Toggle whether archived projects are revealed in the picker.
    pub fn toggle_show_archived(&mut self) {
        self.show_archived = !self.show_archived;
        self.load_project_names();
        self.update_search_results();
    }

    /// Archive/unarchive the highlighted project in the picker.
    /// No-op when the highlighted result is an active session or a worktree.
    pub fn toggle_archive_selected_project(&mut self) {
        let Some(SearchResult::Project(name)) = self.search_results.get(self.selected).cloned()
        else {
            return;
        };
        let mut registry = ProjectRegistry::load();
        let Some((key, config)) = registry.find_by_session_name(&name) else {
            return;
        };
        let key = key.to_string();
        let new_archived = !config.archived;
        if registry.set_archived(&key, new_archived) && registry.save().is_ok() {
            self.load_project_names();
            self.update_search_results();
        }
    }

    /// Calculate lines needed to display a search result
    fn lines_for_search_result(&self, result: &SearchResult) -> usize {
        match result {
            SearchResult::Active(_)
            | SearchResult::Project(_)
            | SearchResult::Worktree(_)
            | SearchResult::Frozen(_) => 1,
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
    /// Apply gathered session data to app state (cheap — runs on main thread).
    /// Handles sorting, permission key assignment, and selection stabilization.
    pub fn apply_refresh(&mut self, mut session_infos: Vec<SessionInfo>) {
        // Keep frozen state current so the picker group and footer count stay fresh.
        self.reload_frozen();

        // Sort: skipped last, Claude before non-Claude, favorites first
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

        if !self.session_infos.is_empty() {
            if self.selected >= self.session_infos.len() {
                self.selected = self.session_infos.len() - 1;
            }
        } else {
            self.selected = 0;
        }
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

        // Compute divider positions (must match render_session_list logic)
        let non_claude_start = self
            .session_infos
            .iter()
            .position(|s| !self.is_skipped(&s.name) && s.claude_status.is_none());
        let skipped_start = self
            .session_infos
            .iter()
            .position(|s| self.is_skipped(&s.name));

        loop {
            // Start with 1 for the initial blank line the renderer always adds
            let mut used: usize = 1;
            for i in self.scroll_offset..=self.selected {
                // Dividers take 2 lines each (blank + label)
                if Some(i) == non_claude_start {
                    used += 2;
                }
                if Some(i) == skipped_start {
                    used += 2;
                }
                used += lines_for_session(&self.session_infos[i], self.is_auto_approved(&self.session_infos[i].name));
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

    /// Begin freezing from the detail view. Enumerates the session's Claude windows:
    /// one window → straight to the note prompt; several → open the window picker first.
    pub fn start_freeze_from_detail(&mut self) {
        let Some(session_name) = self.detail_session_name() else {
            return;
        };
        use crate::common::frozen::FreezeTarget;
        let mut choices: Vec<FreezeTarget> = crate::common::instances::instances_for_session(
            &session_name,
        )
        .into_iter()
        .map(|inst| FreezeTarget {
            session_name: inst.session_name,
            window_index: inst.window_index,
            window_name: inst.window_name,
            cwd: inst.cwd,
            claude_session_id: inst.session_id,
        })
        .collect();
        choices.sort_by(|a, b| a.window_index.cmp(&b.window_index));

        match choices.len() {
            0 => {
                self.error_message = Some((
                    format!("No Claude window to freeze in '{session_name}'"),
                    Instant::now(),
                ));
            }
            1 => {
                self.pending_freeze = Some(choices.into_iter().next().unwrap());
                self.input_mode = InputMode::FreezeNote;
                self.input_buffer.clear();
            }
            _ => {
                self.freeze_choices = choices;
                self.freeze_choice_selected = 0;
                self.input_mode = InputMode::FreezeWindowPick;
            }
        }
    }

    pub fn freeze_pick_up(&mut self) {
        if self.freeze_choice_selected > 0 {
            self.freeze_choice_selected -= 1;
        }
    }

    pub fn freeze_pick_down(&mut self) {
        if self.freeze_choice_selected + 1 < self.freeze_choices.len() {
            self.freeze_choice_selected += 1;
        }
    }

    /// Confirm the highlighted window in the picker and move on to the note prompt.
    pub fn freeze_pick_confirm(&mut self) {
        if let Some(target) = self.freeze_choices.get(self.freeze_choice_selected).cloned() {
            self.pending_freeze = Some(target);
            self.freeze_choices.clear();
            self.input_mode = InputMode::FreezeNote;
            self.input_buffer.clear();
        }
    }

    /// Complete the freeze with the typed note. Kills just that window and records it.
    /// Returns true on success.
    pub fn complete_freeze(&mut self) -> bool {
        let Some(target) = self.pending_freeze.take() else {
            self.input_mode = InputMode::Normal;
            self.input_buffer.clear();
            return false;
        };
        let note = self.input_buffer.trim().to_string();
        self.input_mode = InputMode::Normal;
        self.input_buffer.clear();
        match crate::common::frozen::freeze_window(&target, &note) {
            Ok(_) => {
                self.reload_frozen();
                // Leave detail view: the window is gone, and the session may be too.
                self.showing_detail = None;
                self.detail_selected = None;
                true
            }
            Err(e) => {
                self.error_message = Some((
                    format!("Failed to freeze '{}': {e}", target.session_name),
                    Instant::now(),
                ));
                false
            }
        }
    }

    pub fn cancel_freeze(&mut self) {
        self.pending_freeze = None;
        self.freeze_choices.clear();
        self.input_mode = InputMode::Normal;
        self.input_buffer.clear();
    }

    /// Reload frozen state from disk (after freeze/thaw/discard or on refresh).
    pub fn reload_frozen(&mut self) {
        self.frozen_state = crate::common::frozen::FrozenState::load();
    }

    /// Discard the highlighted frozen entry in the picker without restoring it.
    pub fn discard_selected_frozen(&mut self) {
        let Some(SearchResult::Frozen(name)) = self.search_results.get(self.selected).cloned()
        else {
            return;
        };
        if crate::common::frozen::discard_frozen(&name).unwrap_or(false) {
            self.reload_frozen();
            self.update_search_results();
        }
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

    /// Remove a session from the skipped set if it's there (no-op otherwise).
    pub fn unskip(&mut self, name: &str) {
        if self.skipped_sessions.remove(name) {
            save_skipped_sessions(&self.skipped_sessions);
        }
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
                .resolve_worktrees_dir(
                    &entry.project_key,
                    registry.projects.get(&entry.project_key).unwrap(),
                )
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

    // --- New project wizard methods ---

    /// Start the new project wizard (step 1: key input)
    pub fn start_new_project_wizard(&mut self) {
        self.input_mode = InputMode::NewProjectKey;
        self.input_buffer.clear();
        self.np_key = None;
    }

    /// Validate key and advance to emoji step. Sets error_message on failure.
    pub fn np_enter_emoji_step(&mut self) {
        let key = self.input_buffer.trim().to_string();
        if key.is_empty() {
            self.cancel_new_project_wizard();
            return;
        }
        if key.contains(' ') || key.contains('/') {
            self.error_message = Some((
                "Key cannot contain spaces or slashes".to_string(),
                std::time::Instant::now(),
            ));
            return;
        }
        let registry = ProjectRegistry::load();
        if registry.projects.contains_key(&key) {
            self.error_message = Some((
                format!("Project '{}' already exists", key),
                std::time::Instant::now(),
            ));
            return;
        }
        self.np_key = Some(key);
        self.input_buffer.clear();
        self.input_mode = InputMode::NewProjectEmoji;
    }

    /// Complete the wizard: create project, return session name for connect+switch.
    pub fn np_complete(&mut self) -> Option<String> {
        let key = self.np_key.take()?;
        let emoji = {
            let e = self.input_buffer.trim().to_string();
            if e.is_empty() {
                "📁".to_string()
            } else {
                e
            }
        };
        let project_root = format!("~/Projects/00-Personal/{}", key);
        // Ensure the project directory exists before creating the tmux session
        let expanded = crate::common::projects::expand_tilde(&project_root);
        let _ = std::fs::create_dir_all(&expanded);
        let config = ProjectConfig {
            emoji: emoji.clone(),
            project_root,
            display_name: None,
            startup_command: Some("claude -c".to_string()),
            worktrees_dir: None,
            default_base_branch: None,
            worktree_types: Vec::new(),
            package_manager: None,
            ports: crate::common::projects::PortConfig::default(),
            database: crate::common::projects::DatabaseConfig::default(),
            files: crate::common::projects::FilePatterns::default(),
            hooks_dir: None,
            auth_profile: None,
            archived: false,
        };
        let session_name = ProjectRegistry::session_name(&key, &config);
        let mut registry = ProjectRegistry::load();
        registry.add_project(key, config);
        if let Err(e) = registry.save() {
            self.error_message = Some((
                format!("Failed to save project: {}", e),
                std::time::Instant::now(),
            ));
            self.cancel_new_project_wizard();
            return None;
        }
        self.cancel_new_project_wizard();
        Some(session_name)
    }

    /// Cancel the new project wizard, resetting state.
    pub fn cancel_new_project_wizard(&mut self) {
        self.input_mode = InputMode::Normal;
        self.input_buffer.clear();
        self.np_key = None;
    }

    // --- Hint mode (vim-style quick-jump) ---

    /// Enter hint mode: assign a 2-char label to every jump target (each session,
    /// or each window of a multi-window session) and freeze the list. No-op if
    /// there are no sessions to target.
    pub fn enter_hint_mode(&mut self) {
        let mut raw: Vec<HintTarget> = Vec::new();
        for s in &self.session_infos {
            if s.windows.len() > 1 {
                for w in &s.windows {
                    raw.push(HintTarget::Window(s.name.clone(), w.window_index.clone()));
                }
            } else {
                raw.push(HintTarget::Session(s.name.clone()));
            }
        }
        if raw.is_empty() {
            return;
        }
        self.hint_targets = gen_hint_labels(raw.len()).into_iter().zip(raw).collect();
        self.hint_buffer.clear();
        self.input_mode = InputMode::Hint;
    }

    /// Cancel hint mode, returning to the normal list.
    pub fn cancel_hint_mode(&mut self) {
        self.input_mode = InputMode::Normal;
        self.hint_buffer.clear();
        self.hint_targets.clear();
    }

    /// Feed a typed character into hint mode. Returns the matched target when the
    /// buffer completes a full label. Characters that don't extend any label's
    /// prefix are ignored (buffer unchanged).
    pub fn hint_input(&mut self, c: char) -> Option<HintTarget> {
        let mut candidate = self.hint_buffer.clone();
        candidate.push(c.to_ascii_lowercase());
        if !self
            .hint_targets
            .iter()
            .any(|(l, _)| l.starts_with(&candidate))
        {
            return None;
        }
        self.hint_buffer = candidate.clone();
        self.hint_targets
            .iter()
            .find(|(l, _)| *l == candidate)
            .map(|(_, t)| t.clone())
    }

    /// Label assigned to a whole-session target, if any (single-window sessions).
    pub fn hint_label_for_session(&self, name: &str) -> Option<&str> {
        self.hint_targets.iter().find_map(|(l, t)| match t {
            HintTarget::Session(n) if n == name => Some(l.as_str()),
            _ => None,
        })
    }

    /// Label assigned to a specific window target (multi-window sessions).
    pub fn hint_label_for_window(&self, name: &str, window_index: &str) -> Option<&str> {
        self.hint_targets.iter().find_map(|(l, t)| match t {
            HintTarget::Window(n, w) if n == name && w == window_index => Some(l.as_str()),
            _ => None,
        })
    }
}

/// Gather session data from tmux, sysinfo, and hooks.
/// This is the expensive operation (~700ms) that runs on a background thread.
/// Returns raw `Vec<SessionInfo>` for `App::apply_refresh()` to consume.
pub fn gather_sessions(sys: &mut System, filter: &Option<String>) -> Vec<SessionInfo> {
    let t0 = Instant::now();
    sys.refresh_all();
    let t_sysinfo = t0.elapsed();

    // Load hook state from file
    let t1 = Instant::now();
    let hook_state = HookState::load();
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
    let t_hooks = t1.elapsed();

    let t2 = Instant::now();
    let sessions = match get_tmux_sessions() {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let t_tmux = t2.elapsed();

    let t3 = Instant::now();
    let other_client_sessions = get_other_client_sessions();
    let current_session = get_current_session();
    let t_other = t3.elapsed();

    let mut session_infos = Vec::new();
    let t4 = Instant::now();

    // Build the process parent→children map once per gather pass so the per-pane
    // descendant walk is a HashMap lookup instead of a fresh `ps` subprocess each time.
    let children_map = build_children_map();

    // Detect every Claude instance across all sessions via the shared core, grouped by
    // session. Used to populate per-window display for sessions hosting multiple Claudes.
    let hook_index = HookIndex::build(&hook_state);
    let mut instances_by_session: HashMap<String, Vec<ClaudeInstance>> = HashMap::new();
    for inst in detect_claude_instances(&sessions, sys, &children_map, &hook_index) {
        instances_by_session
            .entry(inst.session_name.clone())
            .or_default()
            .push(inst);
    }

    for session in sessions {
        if !matches_filter(&session.name, filter) {
            continue;
        }

        let session_cwd = session
            .windows
            .first()
            .and_then(|w| w.panes.first())
            .map(|p| p.cwd.clone());

        let mut all_pids = Vec::new();
        let mut claude_status: Option<ClaudeStatus> = None;
        let mut claude_pane: Option<(String, String, String)> = None;
        let mut last_activity = None;

        // Single pass over panes: collect descendants and detect the Claude pane.
        for window in &session.windows {
            for pane in &window.panes {
                let pane_start = all_pids.len();
                all_pids.push(pane.pid);
                collect_descendants(&children_map, pane.pid, &mut all_pids);

                // Only check for Claude until we've found the pane.
                if claude_pane.is_none() {
                    let has_claude_process = all_pids[pane_start..].iter().any(|&pid| {
                        get_process_info(sys, pid)
                            .map(|info| is_claude_process(&info))
                            .unwrap_or(false)
                    });

                    if has_claude_process {
                        if let Some(hook_session) = hook_sessions.get(&pane.cwd) {
                            claude_status = Some(convert_hook_status(&hook_session.status));
                            last_activity = hook_session
                                .last_activity
                                .as_ref()
                                .and_then(|s| parse_timestamp(s));
                        } else if let Some(jsonl_status) =
                            crate::common::jsonl::get_claude_status_from_jsonl(&pane.cwd)
                        {
                            claude_status = Some(jsonl_status.status);
                            last_activity = jsonl_status.timestamp;
                        } else {
                            claude_status = Some(ClaudeStatus::Unknown);
                        }
                        claude_pane = Some((
                            session.name.clone(),
                            window.index.clone(),
                            pane.index.clone(),
                        ));
                    }
                }
            }
        }

        let mut total_cpu = 0.0;
        let mut total_mem_kb = 0u64;
        let mut processes: Vec<ProcessInfo> = Vec::new();

        for &pid in &all_pids {
            if let Some(info) = get_process_info(sys, pid) {
                total_cpu += info.cpu_percent;
                total_mem_kb += info.memory_kb;
                processes.push(info);
            }
        }

        processes.sort_by(|a, b| {
            b.cpu_percent
                .partial_cmp(&a.cpu_percent)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let listening_ports = get_listening_ports_for_pids(&all_pids, sys);

        // Per-window breakdown, only when the session hosts more than one Claude.
        // (Single-window sessions display via `claude_status`, avoiding extra jsonl reads.)
        let mut session_instances = instances_by_session
            .remove(&session.name)
            .unwrap_or_default();
        let windows: Vec<ClaudeWindowInfo> = if session_instances.len() > 1 {
            session_instances.sort_by(|a, b| a.window_index.cmp(&b.window_index));
            session_instances
                .iter()
                .map(|inst| {
                    let status = if let Some(h) = hook_index.resolve(&inst.pane_id, &inst.cwd) {
                        Some(convert_hook_status(&h.status))
                    } else if let Some(js) =
                        crate::common::jsonl::get_claude_status_from_jsonl_for(
                            &inst.cwd,
                            inst.session_id.as_deref(),
                        )
                    {
                        Some(js.status)
                    } else {
                        Some(ClaudeStatus::Unknown)
                    };
                    let cpu = inst
                        .pids
                        .iter()
                        .filter_map(|&pid| get_process_info(sys, pid))
                        .map(|i| i.cpu_percent)
                        .sum();
                    ClaudeWindowInfo {
                        window_index: inst.window_index.clone(),
                        window_name: inst.window_name.clone(),
                        status,
                        cpu,
                    }
                })
                .collect()
        } else {
            Vec::new()
        };

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
            windows,
        });
    }

    let t_loop = t4.elapsed();
    let t_total = t0.elapsed();
    debug_log(&format!(
        "GATHER TIMING: total={:?} sysinfo={:?} hooks={:?} tmux={:?} other={:?} loop={:?} sessions={}",
        t_total, t_sysinfo, t_hooks, t_tmux, t_other, t_loop, session_infos.len()
    ));

    session_infos
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gen_hint_labels_count_and_length() {
        let labels = gen_hint_labels(5);
        assert_eq!(labels.len(), 5);
        assert!(labels.iter().all(|l| l.chars().count() == 2));
    }

    #[test]
    fn test_gen_hint_labels_unique() {
        let labels = gen_hint_labels(40);
        let set: HashSet<&String> = labels.iter().collect();
        assert_eq!(set.len(), labels.len(), "labels must be unique");
    }

    #[test]
    fn test_gen_hint_labels_stable_order() {
        // First labels are deterministic: aa, as, ad, ...
        let labels = gen_hint_labels(3);
        assert_eq!(labels, vec!["aa", "as", "ad"]);
    }

    #[test]
    fn test_gen_hint_labels_caps_at_alphabet_squared() {
        // 9-letter alphabet → at most 81 two-char labels.
        let labels = gen_hint_labels(200);
        assert_eq!(labels.len(), HINT_ALPHABET.len() * HINT_ALPHABET.len());
    }
}
