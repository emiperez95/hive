//! `hive conversations` — browse every known Claude conversation (live AND
//! closed), grouped by its resolved parent (project / worktree), and jump to or
//! resume one. A conversation-focused counterpart to the classic session TUI;
//! classic `hive` is left untouched, so you can run either and go back and forth.
//!
//! Built as a read-only shadow over the re-rooted [`ConversationRegistry`]
//! (existing `state.json` + a disk scan + the `conversations.json` overlay +
//! live tmux placements). Only the switch/resume actions touch tmux.

use anyhow::{anyhow, Context, Result};
use std::collections::{HashMap, HashSet};
use std::io::IsTerminal;
use std::process::Command;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::common::frozen::{freeze_window, relative_time, FreezeTarget};
use crate::common::instances;
use crate::common::jsonl;
use crate::common::persistence::{
    load_auto_approve_sessions, load_favorite_sessions, load_muted_sessions, load_skipped_sessions,
    save_auto_approve_sessions, save_favorite_sessions, save_muted_sessions, save_skipped_sessions,
};
use crate::common::projects::{ensure_tmux_session, expand_tilde, ProjectRegistry};
use crate::common::registry::{
    self, Conversation, ConversationRegistry, ConversationSidecar, TmuxPlacement,
};
use crate::common::tmux::{get_current_tmux_session, select_window, switch_to_session};
use crate::common::worktree::WorktreeState;
use crate::ipc::messages::{HookState, SessionStatus};

/// Entry point. Interactive TUI on a terminal; static listing with `--list` or
/// when output is piped/redirected.
pub fn run_conversations(list: bool) -> Result<()> {
    if list || !std::io::stdout().is_terminal() {
        print!("{}", render_conversations(&gather_conversations()));
        return Ok(());
    }
    run_conversations_tui()
}

/// Build the conversation registry from live state (READ-ONLY): hook status +
/// disk existence + live placements + overlay, with parents resolved and the
/// Closed set bounded.
pub fn gather_conversations() -> ConversationRegistry {
    let hook = HookState::load();
    let disk = jsonl::scan_all_disk_conversations();
    let disk_ids: Vec<String> = disk.iter().map(|d| d.id.clone()).collect();
    let sidecar = ConversationSidecar::load();

    // Live placements: every currently-running Claude instance we can tie to a
    // conversation id becomes the SOLE Live discriminator for that id.
    let mut live_placements: HashMap<String, TmuxPlacement> = HashMap::new();
    for inst in instances::detect_all_instances() {
        // Prefer the hook-resolved conversation id. If a live Claude window has no
        // hook entry (state.json only tracks recently-active conversations), fall
        // back to the newest transcript in its cwd — but ONLY when the cwd isn't
        // shared by multiple windows, where that fallback would be ambiguous
        // (the S4/S5 seam). Without this, live windows absent from state.json are
        // invisible in the Active view even though classic `prefix + s` shows them.
        let sid = inst.session_id.clone().or_else(|| {
            if inst.cwd_shared {
                None
            } else {
                jsonl::find_latest_jsonl_for_cwd(&inst.cwd)
                    .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
            }
        });
        if let Some(sid) = sid {
            live_placements.insert(
                sid,
                TmuxPlacement {
                    session_name: inst.session_name,
                    window_index: inst.window_index,
                    window_name: inst.window_name,
                    pane_id: Some(inst.pane_id),
                },
            );
        }
    }

    let mut reg = ConversationRegistry::from_shadow(&hook, &disk_ids, &live_placements, &sidecar);

    // Enrich disk-only (Closed) conversations with cwd + last-activity + title
    // from the transcript — the hook side had none, so without this they can't
    // be placed or bounded.
    let disk_map: HashMap<&str, &jsonl::DiskConversation> =
        disk.iter().map(|d| (d.id.as_str(), d)).collect();
    for (id, c) in reg.conversations.iter_mut() {
        if let Some(d) = disk_map.get(id.as_str()) {
            if c.cwd.is_empty() {
                if let Some(cwd) = &d.cwd {
                    c.cwd = cwd.clone();
                }
            }
            if c.last_activity.is_none() {
                c.last_activity = d.last_activity.clone();
            }
            if c.title.is_none() {
                c.title = d.title.clone();
            }
            if c.auth_config_dir.is_none() {
                c.auth_config_dir = d.config_dir.clone();
            }
        }
    }

    // Resolve a logical parent for any conversation without one cached.
    let worktrees = WorktreeState::load();
    let projects = ProjectRegistry::load();
    for c in reg.conversations.values_mut() {
        if c.parent.is_none() {
            c.parent = registry::resolve_parent(&c.cwd, &worktrees, &projects);
        }
    }

    // Overlay the frozen facet (note + timestamp) so frozen windows read as
    // Closed+frozen (💤). Must run BEFORE bounding, since a pinned freeze is one
    // of the reasons a Closed conversation is surfaced.
    reg.apply_frozen(&crate::common::frozen::FrozenState::load());

    // Bound the (unbounded) on-disk Closed set: Live is always shown; a Closed
    // conversation is kept only if recently active, parented, or a pinned freeze.
    let now = chrono::Utc::now();
    let cfg = registry::BoundingCfg { max_age_days: 14 };
    reg.conversations.retain(|_, c| {
        c.lifecycle.is_actionable_here() || registry::should_surface_closed(c, now, &cfg)
    });

    reg
}

// ── Interactive TUI ─────────────────────────────────────────────────────────

/// What the user chose in the TUI, performed after the terminal is restored.
enum Action {
    Quit,
    Switch(Box<Conversation>),
    Reopen(Box<Conversation>),
    /// Start a fresh conversation in the given project (key).
    NewInProject(String),
}

/// Which list the TUI is showing.
enum View {
    /// Live conversations grouped by the tmux session running them (like `prefix + s`).
    Active,
    /// Every conversation (live + closed) grouped by project/worktree.
    Browse,
}

/// A group of conversations sharing a parent (project/worktree), in display order.
struct Group {
    key: String,
    convs: Vec<usize>, // indices into the `convs` vec
    path: String,      // common directory prefix of the group's conversations
    emoji: String,     // project icon for the header (empty if none)
}

/// The project emoji for a Browse group key: a project key ("hive") or a worktree
/// key ("hive/CSD-1" → the project part). Empty for special/unknown groups.
fn project_emoji(key: &str, projects: &ProjectRegistry) -> String {
    if key == FROZEN_GROUP {
        return String::new(); // already carries 💤
    }
    let pkey = key.split('/').next().unwrap_or(key);
    projects
        .projects
        .get(pkey)
        .map(|c| c.emoji.clone())
        .unwrap_or_default()
}

/// Resolve a Browse group key to a registered project key (the part before `/`
/// for a worktree key, or the key itself for a project). None for the special
/// groups (frozen, unassigned) or keys with no matching project.
fn project_key_of(group_key: &str, projects: &ProjectRegistry) -> Option<String> {
    if group_key == FROZEN_GROUP {
        return None;
    }
    let pkey = group_key.split('/').next().unwrap_or(group_key);
    projects
        .projects
        .contains_key(pkey)
        .then(|| pkey.to_string())
}

/// A visible row: a group header, or a conversation under an expanded group.
enum Row {
    Header(usize),                 // index into `groups`
    Conv { ci: usize, gi: usize }, // conversation index + its group index
}

/// Header key for the pinned group that enumerates all frozen conversations.
const FROZEN_GROUP: &str = "💤 frozen";

/// The session-level flag sets (all keyed by tmux session name), loaded together
/// so a conversation row can show ★ / [muted] / [auto] / [skip] and toggle them.
struct Flags {
    favorite: HashSet<String>,
    muted: HashSet<String>,
    auto_approve: HashSet<String>,
    skipped: HashSet<String>,
}

impl Flags {
    fn load() -> Self {
        Flags {
            favorite: load_favorite_sessions(),
            muted: load_muted_sessions(),
            auto_approve: load_auto_approve_sessions(),
            skipped: load_skipped_sessions(),
        }
    }
}

/// Which session-level flag a key toggles.
#[derive(Clone, Copy)]
enum Flag {
    Favorite,
    Mute,
    AutoApprove,
    Skip,
}

/// Toggle a session flag (add if absent, remove if present) and persist it.
fn toggle_flag(session: &str, flag: Flag) {
    let (mut set, save): (HashSet<String>, fn(&HashSet<String>)) = match flag {
        Flag::Favorite => (load_favorite_sessions(), save_favorite_sessions),
        Flag::Mute => (load_muted_sessions(), save_muted_sessions),
        Flag::AutoApprove => (load_auto_approve_sessions(), save_auto_approve_sessions),
        Flag::Skip => (load_skipped_sessions(), save_skipped_sessions),
    };
    if !set.remove(session) {
        set.insert(session.to_string());
    }
    save(&set);
}

/// Build a freeze target from a live conversation (its window placement + id).
fn freeze_target_of(c: &Conversation) -> Option<FreezeTarget> {
    let p = c.placement.as_ref()?;
    Some(FreezeTarget {
        session_name: p.session_name.clone(),
        window_index: p.window_index.clone(),
        window_name: p.window_name.clone(),
        cwd: c.cwd.clone(),
        claude_session_id: Some(c.id.as_str().to_string()),
    })
}

fn sort_convs(group: &mut [&Conversation]) {
    group.sort_by(|a, b| {
        b.lifecycle
            .is_actionable_here()
            .cmp(&a.lifecycle.is_actionable_here())
            .then_with(|| b.last_activity.cmp(&a.last_activity))
            .then_with(|| a.id.as_str().cmp(b.id.as_str()))
    });
}

/// Push a group (its conversations cloned into `convs`) and return it.
fn push_group(
    key: String,
    group: Vec<&Conversation>,
    convs: &mut Vec<Conversation>,
    emoji: String,
) -> Group {
    let path = common_prefix(&group.iter().map(|c| c.cwd.as_str()).collect::<Vec<_>>());
    let mut idxs = Vec::new();
    for c in group {
        idxs.push(convs.len());
        convs.push(c.clone());
    }
    Group {
        key,
        convs: idxs,
        path,
        emoji,
    }
}

/// Flatten the registry into grouped, sorted display order: a pinned "💤 frozen"
/// group first (all frozen conversations, enumerated), then the rest by parent.
fn build_groups(
    reg: &ConversationRegistry,
    projects: &ProjectRegistry,
) -> (Vec<Group>, Vec<Conversation>) {
    use std::collections::BTreeMap;
    let mut frozen: Vec<&Conversation> = Vec::new();
    let mut grouped: BTreeMap<String, Vec<&Conversation>> = BTreeMap::new();
    for c in reg.conversations.values() {
        if c.is_frozen() {
            frozen.push(c);
            continue;
        }
        let key = c
            .parent
            .clone()
            .unwrap_or_else(|| "(unassigned)".to_string());
        grouped.entry(key).or_default().push(c);
    }

    let mut groups = Vec::new();
    let mut convs = Vec::new();
    // Frozen pinned first so they're always enumerated at the top of Browse.
    if !frozen.is_empty() {
        sort_convs(&mut frozen);
        groups.push(push_group(
            FROZEN_GROUP.to_string(),
            frozen,
            &mut convs,
            String::new(),
        ));
    }
    for (key, mut group) in grouped {
        sort_convs(&mut group);
        let emoji = project_emoji(&key, projects);
        groups.push(push_group(key, group, &mut convs, emoji));
    }
    (groups, convs)
}

/// Case-insensitive match of a query against a conversation's title, id, parent
/// key, cwd, and frozen note — the fields a user would search by.
fn matches_query(c: &Conversation, q: &str) -> bool {
    let q = q.to_lowercase();
    let hay = |s: &str| s.to_lowercase().contains(&q);
    hay(c.id.as_str())
        || c.title.as_deref().map(hay).unwrap_or(false)
        || c.parent.as_deref().map(hay).unwrap_or(false)
        || hay(&c.cwd)
        || c.frozen.as_ref().map(|f| hay(&f.note)).unwrap_or(false)
}

/// A registry containing only the conversations matching `query` (all, if empty).
fn filter_registry(reg: &ConversationRegistry, query: &str) -> ConversationRegistry {
    if query.is_empty() {
        return reg.clone();
    }
    ConversationRegistry {
        conversations: reg
            .conversations
            .iter()
            .filter(|(_, c)| matches_query(c, query))
            .map(|(k, c)| (k.clone(), c.clone()))
            .collect(),
    }
}

/// The currently-visible rows: every header, plus the conversations of expanded
/// groups. Recomputed whenever the collapsed set changes.
fn visible_rows(groups: &[Group], collapsed: &HashSet<String>) -> Vec<Row> {
    let mut rows = Vec::new();
    for (gi, g) in groups.iter().enumerate() {
        rows.push(Row::Header(gi));
        if !collapsed.contains(&g.key) {
            for &ci in &g.convs {
                rows.push(Row::Conv { ci, gi });
            }
        }
    }
    rows
}

/// Active view: only LIVE conversations, grouped by the tmux session running
/// them — the conversation-aware analog of the classic `prefix + s` session list.
/// Skipped sessions are ordered last so they form a separate section.
fn build_active(
    reg: &ConversationRegistry,
    skipped: &HashSet<String>,
) -> (Vec<Group>, Vec<Conversation>) {
    use std::collections::BTreeMap;
    let mut grouped: BTreeMap<String, Vec<&Conversation>> = BTreeMap::new();
    for c in reg.conversations.values() {
        if !c.lifecycle.is_actionable_here() {
            continue;
        }
        let key = c
            .placement
            .as_ref()
            .map(|p| p.session_name.clone())
            .unwrap_or_else(|| "(detached)".to_string());
        grouped.entry(key).or_default().push(c);
    }
    // Partition into normal (first) and skipped (last) session groups.
    let mut normal = Vec::new();
    let mut skip = Vec::new();
    for (key, mut group) in grouped {
        group.sort_by(|a, b| {
            b.last_activity
                .cmp(&a.last_activity)
                .then_with(|| a.id.as_str().cmp(b.id.as_str()))
        });
        if skipped.contains(&key) {
            skip.push((key, group));
        } else {
            normal.push((key, group));
        }
    }
    let mut groups = Vec::new();
    let mut convs = Vec::new();
    for (key, group) in normal.into_iter().chain(skip) {
        // Session names already carry their project emoji, so no separate icon.
        groups.push(push_group(key, group, &mut convs, String::new()));
    }
    (groups, convs)
}

/// One worktree row in the project detail view: branch, its session name, and how
/// many of its conversations are live / frozen.
struct WtRow {
    branch: String,
    session: String,
    live: usize,
    frozen: usize,
}

/// The project detail sub-screen: a project's config, its worktrees, and every
/// conversation under it (live + closed + frozen), from which you can switch,
/// resume, or start a new one.
struct ProjectDetailState {
    key: String,
    worktrees: Vec<WtRow>,
    convs: Vec<Conversation>, // display order: live first, frozen last, else recency
    path: String,             // common cwd prefix (for subpath elision in rows)
    sel: usize,               // selected conversation index
}

impl ProjectDetailState {
    /// The most recently-active conversation — what `r` (resume last) opens.
    fn most_recent(&self) -> Option<&Conversation> {
        self.convs
            .iter()
            .max_by(|a, b| a.last_activity.cmp(&b.last_activity))
    }
}

/// True if a conversation belongs to `key` — either the project root (`parent ==
/// key`) or one of its worktrees (`parent` starts with `key/`).
fn conv_in_project(c: &Conversation, key: &str) -> bool {
    match c.parent.as_deref() {
        Some(p) => p == key || p.starts_with(&format!("{key}/")),
        None => false,
    }
}

/// Build the project detail state from the registry (READ-ONLY): gather the
/// project's conversations, sort them for display, and summarize its worktrees.
fn build_project_detail(key: &str, reg: &ConversationRegistry) -> ProjectDetailState {
    let mut convs: Vec<Conversation> = reg
        .conversations
        .values()
        .filter(|c| conv_in_project(c, key))
        .cloned()
        .collect();
    // Live first, then non-frozen before frozen, then most-recent, then id.
    convs.sort_by(|a, b| {
        b.lifecycle
            .is_actionable_here()
            .cmp(&a.lifecycle.is_actionable_here())
            .then_with(|| a.is_frozen().cmp(&b.is_frozen()))
            .then_with(|| b.last_activity.cmp(&a.last_activity))
            .then_with(|| a.id.as_str().cmp(b.id.as_str()))
    });
    let path = common_prefix(&convs.iter().map(|c| c.cwd.as_str()).collect::<Vec<_>>());

    // Worktree summary rows, with per-worktree live/frozen conversation counts.
    let wts = WorktreeState::load();
    let mut worktrees: Vec<WtRow> = wts
        .worktrees
        .values()
        .filter(|e| e.project_key == key)
        .map(|e| {
            let pkey = WorktreeState::make_key(&e.project_key, &e.branch);
            let of = |c: &&Conversation| c.parent.as_deref() == Some(pkey.as_str());
            WtRow {
                branch: e.branch.clone(),
                session: e.session_name.clone(),
                live: convs
                    .iter()
                    .filter(|c| of(c) && c.lifecycle.is_actionable_here())
                    .count(),
                frozen: convs.iter().filter(|c| of(c) && c.is_frozen()).count(),
            }
        })
        .collect();
    worktrees.sort_by(|a, b| a.branch.cmp(&b.branch));

    ProjectDetailState {
        key: key.to_string(),
        worktrees,
        convs,
        path,
        sel: 0,
    }
}

/// Longest common absolute-directory prefix (component-wise) of the given cwds.
/// The path is a property of the project/worktree, not the individual
/// conversation, so it's shown once on the group header.
fn common_prefix(paths: &[&str]) -> String {
    let split = |s: &str| {
        s.split('/')
            .filter(|c| !c.is_empty())
            .map(String::from)
            .collect::<Vec<_>>()
    };
    let Some((first, rest)) = paths.split_first() else {
        return String::new();
    };
    let mut prefix = split(first);
    for p in rest {
        let comps = split(p);
        let n = prefix
            .iter()
            .zip(comps.iter())
            .take_while(|(a, b)| a == b)
            .count();
        prefix.truncate(n);
    }
    format!("/{}", prefix.join("/"))
}

/// Replace the home-dir prefix with `~`.
fn abbrev_home(path: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Some(rest) = path.strip_prefix(home.to_string_lossy().as_ref()) {
            return format!("~{rest}");
        }
    }
    path.to_string()
}

/// The part of `cwd` below the group's common `base` (empty when identical) —
/// the only path info that distinguishes a conversation from its group.
fn rel_below(cwd: &str, base: &str) -> String {
    // No meaningful common dir (root) → show the full path so it stays readable.
    if base.len() <= 1 {
        return abbrev_home(cwd);
    }
    cwd.strip_prefix(base)
        .map(|r| r.trim_start_matches('/').to_string())
        .unwrap_or_else(|| abbrev_home(cwd))
}

fn run_conversations_tui() -> Result<()> {
    let mut terminal =
        ratatui::try_init().context("hive conversations: needs a terminal (run interactively)")?;
    let action = conversations_loop(&mut terminal);
    ratatui::restore();

    match action? {
        Action::Quit => {}
        Action::Switch(c) => {
            if let Some(p) = &c.placement {
                switch_to_session(&p.session_name);
                if !p.window_index.is_empty() {
                    select_window(&p.session_name, &p.window_index);
                }
            }
        }
        Action::Reopen(c) => println!("{}", reopen(&c)?),
        Action::NewInProject(key) => println!("{}", new_conversation(&key)?),
    }
    Ok(())
}

fn conversations_loop(terminal: &mut ratatui::DefaultTerminal) -> Result<Action> {
    let mut reg = gather_conversations();
    let mut flags = Flags::load();
    let projects = ProjectRegistry::load();
    // Default to the Active view (live conversations by session); `/` browses all.
    let mut view = View::Active;

    // Switch the current view, rebuilding groups + collapse state.
    let rebuild = |view: &View,
                   reg: &ConversationRegistry,
                   skipped: &HashSet<String>,
                   query: &str|
     -> (Vec<Group>, Vec<Conversation>, HashSet<String>) {
        let filtered = filter_registry(reg, query);
        let src = &filtered;
        match view {
            View::Active => {
                let (g, c) = build_active(src, skipped);
                (g, c, HashSet::new())
            }
            View::Browse => {
                let (g, c) = build_groups(src, &projects);
                // When searching, expand everything so matches are visible;
                // otherwise collapse all but the pinned frozen group.
                let collapsed = if query.is_empty() {
                    g.iter()
                        .map(|g| g.key.clone())
                        .filter(|k| k != FROZEN_GROUP)
                        .collect()
                } else {
                    HashSet::new()
                };
                (g, c, collapsed)
            }
        }
    };

    let mut query = String::new();
    let mut searching = false;
    // Freeze-note input: Some(target) while typing the note for a pending freeze.
    let mut freezing: Option<FreezeTarget> = None;
    let mut freeze_note = String::new();
    let (mut groups, mut convs, mut collapsed) = rebuild(&view, &reg, &flags.skipped, &query);
    let mut sel: usize = 0;
    // Project detail sub-screen: Some(state) while drilled into a project.
    let mut detail: Option<ProjectDetailState> = None;

    // Enter/number activation: switch to a live conversation, else reopen it.
    let activate = |c: &Conversation| -> Action {
        if c.lifecycle.is_actionable_here() {
            Action::Switch(Box::new(c.clone()))
        } else {
            Action::Reopen(Box::new(c.clone()))
        }
    };

    let mut showing_help = false;

    loop {
        // ── Project detail sub-screen ───────────────────────────────────────
        // When drilled into a project, this fully owns the frame + input; Esc/q
        // backs out to the list, Enter/n/r return an Action that exits the TUI.
        if detail.is_some() {
            {
                let d = detail.as_mut().unwrap();
                terminal.draw(|frame| draw_project_detail(frame, d, &projects, &flags))?;
            }
            if !event::poll(Duration::from_millis(250))? {
                continue;
            }
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => detail = None,
                KeyCode::Down | KeyCode::Char('j') => {
                    let d = detail.as_mut().unwrap();
                    if d.sel + 1 < d.convs.len() {
                        d.sel += 1;
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    let d = detail.as_mut().unwrap();
                    d.sel = d.sel.saturating_sub(1);
                }
                KeyCode::Enter => {
                    let d = detail.as_ref().unwrap();
                    if let Some(c) = d.convs.get(d.sel) {
                        return Ok(activate(c));
                    }
                }
                KeyCode::Char(dch @ '1'..='9') => {
                    let d = detail.as_ref().unwrap();
                    if let Some(c) = d.convs.get(dch as usize - '1' as usize) {
                        return Ok(activate(c));
                    }
                }
                // `n` starts a fresh conversation; `r` resumes the last one.
                KeyCode::Char('n') => {
                    return Ok(Action::NewInProject(detail.as_ref().unwrap().key.clone()))
                }
                KeyCode::Char('r') => {
                    if let Some(c) = detail.as_ref().unwrap().most_recent().cloned() {
                        return Ok(activate(&c));
                    }
                }
                _ => {}
            }
            continue;
        }

        let rows = visible_rows(&groups, &collapsed);
        if sel >= rows.len() {
            sel = rows.len().saturating_sub(1);
        }
        let search = searching.then(|| query.clone());
        let freeze = freezing.as_ref().map(|_| freeze_note.clone());
        terminal.draw(|frame| {
            draw(
                frame,
                &view,
                &groups,
                &convs,
                &rows,
                &collapsed,
                sel,
                showing_help,
                &flags,
                search.as_deref(),
                freeze.as_deref(),
            )
        })?;

        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        // Help overlay: any key dismisses it (? / Esc / q explicitly).
        if showing_help {
            showing_help = false;
            continue;
        }

        // Search input mode: type to filter; Esc exits, Enter opens the selection.
        if searching {
            match key.code {
                KeyCode::Esc => {
                    searching = false;
                    query.clear();
                    (groups, convs, collapsed) = rebuild(&view, &reg, &flags.skipped, &query);
                    sel = 0;
                }
                KeyCode::Enter => {
                    if let Some(Row::Conv { ci, .. }) = rows.get(sel) {
                        return Ok(activate(&convs[*ci]));
                    }
                }
                KeyCode::Down => {
                    if sel + 1 < rows.len() {
                        sel += 1;
                    }
                }
                KeyCode::Up => sel = sel.saturating_sub(1),
                KeyCode::Backspace => {
                    query.pop();
                    (groups, convs, collapsed) = rebuild(&view, &reg, &flags.skipped, &query);
                    sel = 0;
                }
                KeyCode::Char(c) => {
                    query.push(c);
                    (groups, convs, collapsed) = rebuild(&view, &reg, &flags.skipped, &query);
                    sel = 0;
                }
                _ => {}
            }
            continue;
        }

        // Freeze-note input: Enter freezes the pending window (kills it), Esc cancels.
        if let Some(target) = freezing.clone() {
            match key.code {
                KeyCode::Esc => {
                    freezing = None;
                    freeze_note.clear();
                }
                KeyCode::Enter => {
                    let _ = freeze_window(&target, &freeze_note);
                    freezing = None;
                    freeze_note.clear();
                    reg = gather_conversations();
                    flags = Flags::load();
                    (groups, convs, collapsed) = rebuild(&view, &reg, &flags.skipped, &query);
                    sel = 0;
                }
                KeyCode::Backspace => {
                    freeze_note.pop();
                }
                KeyCode::Char(c) => freeze_note.push(c),
                _ => {}
            }
            continue;
        }

        // The conversation under the cursor (for row-scoped actions).
        let selected_conv = |rows: &[Row], convs: &[Conversation]| -> Option<Conversation> {
            match rows.get(sel) {
                Some(Row::Conv { ci, .. }) => convs.get(*ci).cloned(),
                _ => None,
            }
        };

        match key.code {
            KeyCode::Char('?') => showing_help = true,
            KeyCode::Char('q') | KeyCode::Char('Q') => return Ok(Action::Quit),
            // Number keys 1-9 jump to the Nth visible conversation (like classic hive).
            KeyCode::Char(d @ '1'..='9') => {
                let n = d as usize - '1' as usize;
                if let Some(Row::Conv { ci, .. }) =
                    rows.iter().filter(|r| matches!(r, Row::Conv { .. })).nth(n)
                {
                    return Ok(activate(&convs[*ci]));
                }
            }
            // Esc backs out of Browse to Active; from Active it quits.
            KeyCode::Esc => match view {
                View::Browse => {
                    view = View::Active;
                    (groups, convs, collapsed) = rebuild(&view, &reg, &flags.skipped, &query);
                    sel = 0;
                }
                View::Active => return Ok(Action::Quit),
            },
            // `/` opens Browse from Active; in Browse it starts a search filter.
            KeyCode::Char('/') => match view {
                View::Active => {
                    view = View::Browse;
                    (groups, convs, collapsed) = rebuild(&view, &reg, &flags.skipped, &query);
                    sel = 0;
                }
                View::Browse => {
                    searching = true;
                    query.clear();
                    sel = 0;
                }
            },
            KeyCode::Down | KeyCode::Char('j') => {
                if sel + 1 < rows.len() {
                    sel += 1;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => sel = sel.saturating_sub(1),
            // Expand the selected group (Browse only — Active is always expanded).
            KeyCode::Right | KeyCode::Char('l') if matches!(view, View::Browse) => {
                if let Some(Row::Header(gi)) = rows.get(sel) {
                    collapsed.remove(&groups[*gi].key);
                }
            }
            // Collapse (Browse only): on a header, fold it; on a conversation, fold
            // its group and move the cursor up to that header.
            KeyCode::Left | KeyCode::Char('h') if matches!(view, View::Browse) => {
                match rows.get(sel) {
                    Some(Row::Header(gi)) => {
                        collapsed.insert(groups[*gi].key.clone());
                    }
                    Some(Row::Conv { gi, .. }) => {
                        let gi = *gi;
                        collapsed.insert(groups[gi].key.clone());
                        let new_rows = visible_rows(&groups, &collapsed);
                        sel = new_rows
                            .iter()
                            .position(|r| matches!(r, Row::Header(g) if *g == gi))
                            .unwrap_or(0);
                    }
                    None => {}
                }
            }
            KeyCode::Enter => match rows.get(sel) {
                Some(Row::Conv { ci, .. }) => return Ok(activate(&convs[*ci])),
                // Enter on a header toggles it (Browse only).
                Some(Row::Header(gi)) if matches!(view, View::Browse) => {
                    // Drill into the project's detail; special groups (frozen /
                    // unassigned) have no single project, so fall back to folding.
                    let key = groups[*gi].key.clone();
                    if let Some(pkey) = project_key_of(&key, &projects) {
                        detail = Some(build_project_detail(&pkey, &reg));
                    } else if !collapsed.remove(&key) {
                        collapsed.insert(key);
                    }
                }
                _ => {}
            },
            KeyCode::Char('r') => {
                reg = gather_conversations();
                flags = Flags::load();
                (groups, convs, collapsed) = rebuild(&view, &reg, &flags.skipped, &query);
                sel = 0;
            }
            // Session-level flag toggles on the selected LIVE conversation's session.
            KeyCode::Char('f') | KeyCode::Char('m') | KeyCode::Char('s') | KeyCode::Char('!') => {
                if let Some(c) = selected_conv(&rows, &convs) {
                    if let Some(p) = &c.placement {
                        let flag = match key.code {
                            KeyCode::Char('f') => Flag::Favorite,
                            KeyCode::Char('m') => Flag::Mute,
                            KeyCode::Char('!') => Flag::AutoApprove,
                            _ => Flag::Skip,
                        };
                        toggle_flag(&p.session_name, flag);
                        flags = Flags::load();
                        // Skip changes grouping/section, so rebuild.
                        (groups, convs, collapsed) = rebuild(&view, &reg, &flags.skipped, &query);
                    }
                }
            }
            // Freeze the selected live conversation's window (prompts for a note).
            KeyCode::Char('z') | KeyCode::Char('Z') => {
                if let Some(c) = selected_conv(&rows, &convs) {
                    if c.lifecycle.is_actionable_here() {
                        if let Some(t) = freeze_target_of(&c) {
                            freezing = Some(t);
                            freeze_note.clear();
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw(
    frame: &mut ratatui::Frame,
    view: &View,
    groups: &[Group],
    convs: &[Conversation],
    rows: &[Row],
    collapsed: &HashSet<String>,
    sel: usize,
    showing_help: bool,
    flags: &Flags,
    search: Option<&str>,
    freeze: Option<&str>,
) {
    let area = frame.area();
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(area);

    let total = convs.len();
    let live = convs
        .iter()
        .filter(|c| c.lifecycle.is_actionable_here())
        .count();
    let frozen = convs.iter().filter(|c| c.is_frozen()).count();
    // Classic-style header bar: bold `hive`, view label, then dim metadata + a
    // blue frozen count (matching the main list's header composition).
    let (view_label, counts, footer): (&str, String, &str) = match view {
        View::Active => (
            "active",
            format!("{live} running · {} sessions", groups.len()),
            " ↑/↓ move · Enter switch · z freeze · f★ m mute ! auto s skip · / browse · ? · q",
        ),
        View::Browse => (
            "conversations",
            format!(
                "{total} · {live} live · {} closed · {} groups",
                total - live,
                groups.len()
            ),
            " Enter open · z freeze · f★ m mute ! auto s skip · / search · ←/→ fold · Esc · ? · q",
        ),
    };
    let mut header_spans = vec![
        Span::styled(" hive", Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(
            format!(" · {view_label}"),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("   "),
        Span::styled(counts, Style::default().add_modifier(Modifier::DIM)),
    ];
    if frozen > 0 {
        header_spans.push(Span::raw("   "));
        header_spans.push(Span::styled(
            format!("💤 {frozen} frozen"),
            Style::default().fg(Color::Blue),
        ));
    }
    let hint = match view {
        View::Active => "  ( / browse )",
        View::Browse => "  ( Esc back )",
    };
    header_spans.push(Span::styled(
        hint,
        Style::default().add_modifier(Modifier::DIM),
    ));
    frame.render_widget(Paragraph::new(Line::from(header_spans)), chunks[0]);

    if showing_help {
        frame.render_widget(Paragraph::new(help_lines()), chunks[1]);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " any key to dismiss",
                Style::default().fg(Color::DarkGray),
            ))),
            chunks[2],
        );
        return;
    }

    // Blank line above the list, mirroring the classic view's top padding.
    let body = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(chunks[1]);

    // Build the full display line list: a blank line before each group (spacing),
    // a "── skipped ──" divider before the first skipped session (Active), and
    // one line per row. Blanks/dividers aren't navigable rows, so we track where
    // the selected row lands (`sel_display`) to window the scroll around it.
    let divider = |label: &str| {
        Line::from(Span::styled(
            format!("  ── {label} ──"),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ))
    };
    let mut display: Vec<Line> = Vec::new();
    let mut sel_display = 0usize;
    let mut conv_seen = 0usize;
    let mut shown_skip = false;
    for (ri, row) in rows.iter().enumerate() {
        if let Row::Header(gi) = row {
            let is_skip_group = flags.skipped.contains(&groups[*gi].key);
            if is_skip_group && !shown_skip {
                shown_skip = true;
                if !display.is_empty() {
                    display.push(Line::raw(""));
                }
                display.push(divider("skipped"));
            } else if !display.is_empty() && matches!(view, View::Active) {
                // Blank line between groups only in Active; Browse stays dense so
                // the many project headers are scannable at a glance.
                display.push(Line::raw(""));
            }
        }
        let num = if let Row::Conv { .. } = row {
            conv_seen += 1;
            (conv_seen <= 9).then_some(conv_seen)
        } else {
            None
        };
        if ri == sel {
            sel_display = display.len();
        }
        let selected = ri == sel;
        let collapsible = matches!(view, View::Browse);
        display.push(match row {
            Row::Header(gi) => header_line(&groups[*gi], convs, collapsed, selected, collapsible),
            Row::Conv { ci, gi } => conv_line(&convs[*ci], &groups[*gi].path, selected, num, flags),
        });
    }

    // Scroll so the selected row stays visible.
    let h = body[1].height as usize;
    let offset = if sel_display >= h {
        sel_display + 1 - h
    } else {
        0
    };
    let lines: Vec<Line> = display.into_iter().skip(offset).take(h).collect();
    frame.render_widget(Paragraph::new(lines), body[1]);

    // Footer: freeze-note prompt, else search query, else the key hints.
    let footer_line = if let Some(note) = freeze {
        Line::from(vec![
            Span::styled(" 💤 freeze note: ", Style::default().fg(Color::Blue)),
            Span::styled(note.to_string(), Style::default().fg(Color::Blue)),
            Span::styled("█", Style::default().add_modifier(Modifier::SLOW_BLINK)),
            Span::styled(
                "   Enter freeze · Esc cancel",
                Style::default().fg(Color::DarkGray),
            ),
        ])
    } else if let Some(q) = search {
        Line::from(vec![
            Span::styled(" /", Style::default().fg(Color::Yellow)),
            Span::styled(q.to_string(), Style::default().fg(Color::Yellow)),
            Span::styled("█", Style::default().add_modifier(Modifier::SLOW_BLINK)),
            Span::styled(
                "   Enter open · Esc cancel",
                Style::default().fg(Color::DarkGray),
            ),
        ])
    } else {
        Line::from(Span::styled(footer, Style::default().fg(Color::DarkGray)))
    };
    frame.render_widget(Paragraph::new(footer_line), chunks[2]);
}

/// Render the project detail sub-screen: config, worktrees, and the project's
/// conversations (the only navigable rows). `n` starts a new one, `r` resumes the
/// last, Enter switches/resumes the selected one.
fn draw_project_detail(
    frame: &mut ratatui::Frame,
    state: &ProjectDetailState,
    projects: &ProjectRegistry,
    flags: &Flags,
) {
    let area = frame.area();
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(area);

    let config = projects.projects.get(&state.key);
    let emoji = config.map(|c| c.emoji.clone()).unwrap_or_default();
    let name = config
        .and_then(|c| c.display_name.clone())
        .unwrap_or_else(|| state.key.clone());
    let live = state
        .convs
        .iter()
        .filter(|c| c.lifecycle.is_actionable_here())
        .count();
    let frozen = state.convs.iter().filter(|c| c.is_frozen()).count();
    let closed = state.convs.len() - live;

    // Title bar: icon + name, counts, profile tag, back hint.
    let icon = if emoji.is_empty() {
        String::new()
    } else {
        format!("{emoji} ")
    };
    let mut title_spans = vec![
        Span::styled(
            format!(" {icon}{name}"),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("   "),
        Span::styled(
            format!("{live} live · {closed} closed"),
            Style::default().add_modifier(Modifier::DIM),
        ),
    ];
    if frozen > 0 {
        title_spans.push(Span::raw("   "));
        title_spans.push(Span::styled(
            format!("💤 {frozen} frozen"),
            Style::default().fg(Color::Blue),
        ));
    }
    if let Some(profile) = config.and_then(|c| c.auth_profile.clone()) {
        title_spans.push(Span::raw("   "));
        title_spans.push(Span::styled(
            format!("[{profile}]"),
            Style::default().fg(Color::Magenta),
        ));
    }
    title_spans.push(Span::styled(
        "   ( Esc back )",
        Style::default().add_modifier(Modifier::DIM),
    ));
    frame.render_widget(Paragraph::new(Line::from(title_spans)), chunks[0]);

    // Body: config block, worktrees, then the navigable conversation list.
    let mut display: Vec<Line> = Vec::new();
    let mut conv_pos: Vec<usize> = Vec::new();
    display.push(Line::raw(""));

    let field = |label: &str, val: String| -> Line<'static> {
        Line::from(vec![
            Span::styled(
                format!("  {label:<10}"),
                Style::default().add_modifier(Modifier::DIM),
            ),
            Span::raw(val),
        ])
    };
    if let Some(c) = config {
        display.push(field(
            "path",
            abbrev_home(expand_tilde(&c.project_root).to_string_lossy().as_ref()),
        ));
        let profile = match &c.auth_profile {
            Some(p) => format!("{p}  (~/.claude-{p})"),
            None => "personal  (~/.claude)".to_string(),
        };
        display.push(field("profile", profile));
        if c.ports.enabled {
            display.push(field(
                "ports",
                format!("base {} (+{})", c.ports.base_port, c.ports.increment),
            ));
        }
        if let Some(s) = &c.startup_command {
            display.push(field("startup", s.clone()));
        }
    }

    display.push(Line::raw(""));
    display.push(Line::from(Span::styled(
        format!("  Worktrees ({})", state.worktrees.len()),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    if state.worktrees.is_empty() {
        display.push(Line::from(Span::styled(
            "    none",
            Style::default().add_modifier(Modifier::DIM),
        )));
    } else {
        for w in &state.worktrees {
            let mut tail = format!("{} live", w.live);
            if w.frozen > 0 {
                tail.push_str(&format!(" · {} frozen", w.frozen));
            }
            display.push(Line::from(vec![
                Span::styled(
                    format!("    {:<18}", w.branch),
                    Style::default().fg(if w.live > 0 {
                        Color::Green
                    } else {
                        Color::Gray
                    }),
                ),
                Span::styled(
                    format!("{:<24}", w.session),
                    Style::default().add_modifier(Modifier::DIM),
                ),
                Span::styled(tail, Style::default().add_modifier(Modifier::DIM)),
            ]));
        }
    }

    display.push(Line::raw(""));
    display.push(Line::from(Span::styled(
        "  Conversations",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    if state.convs.is_empty() {
        display.push(Line::from(Span::styled(
            "    (none — press n to start one)",
            Style::default().add_modifier(Modifier::DIM),
        )));
    } else {
        for (i, c) in state.convs.iter().enumerate() {
            conv_pos.push(display.len());
            let num = (i < 9).then_some(i + 1);
            display.push(conv_line(c, &state.path, i == state.sel, num, flags));
        }
    }

    // Scroll so the selected conversation stays visible.
    let h = chunks[1].height as usize;
    let sel_display = conv_pos.get(state.sel).copied().unwrap_or(0);
    let offset = if sel_display >= h {
        sel_display + 1 - h
    } else {
        0
    };
    let lines: Vec<Line> = display.into_iter().skip(offset).take(h).collect();
    frame.render_widget(Paragraph::new(lines), chunks[1]);

    let footer = " ↑/↓ move · Enter switch/resume · n new · r resume last · Esc back · q";
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            footer,
            Style::default().fg(Color::DarkGray),
        ))),
        chunks[2],
    );
}

/// The help overlay lines.
fn help_lines() -> Vec<Line<'static>> {
    let key = |k: &str, d: &str| -> Line<'static> {
        Line::from(vec![
            Span::styled(
                format!("  {k:<10}"),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(d.to_string()),
        ])
    };
    vec![
        Line::raw(""),
        Line::from(Span::styled(
            "  hive conversations — keys",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        key("1-9", "Jump to / switch the Nth conversation"),
        key("↑/↓ j/k", "Move selection"),
        key(
            "Enter",
            "Conversation: switch/resume · project header: detail",
        ),
        key(
            "/",
            "Active: browse all · Browse: search/filter (type to match)",
        ),
        key(
            "z",
            "Freeze the selected live conversation (prompts for a note)",
        ),
        key("f / m", "Favorite ★ / mute the conversation's session"),
        key(
            "! / s",
            "Toggle auto-approve / skip the conversation's session",
        ),
        key(
            "Esc",
            "Browse → Active · Active → quit · cancel search/freeze",
        ),
        key("←/→ h/l", "Collapse / expand a group (Browse)"),
        key("r", "Refresh"),
        key("?", "This help"),
        key("q", "Quit"),
        Line::raw(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("● free", Style::default().fg(Color::Green)),
            Span::raw("   "),
            Span::styled("● busy", Style::default().fg(Color::Blue)),
            Span::styled(
                "   ○ closed (resumable)   💤 frozen   ",
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled("skipped/archived", Style::default().fg(Color::DarkGray)),
        ]),
    ]
}

/// A group header row: a disclosure triangle (Browse only), the parent key, and
/// counts. In Active, groups can't be collapsed, so the triangle is dropped.
fn header_line(
    g: &Group,
    convs: &[Conversation],
    collapsed: &HashSet<String>,
    selected: bool,
    collapsible: bool,
) -> Line<'static> {
    let tri = if !collapsible {
        String::new()
    } else if collapsed.contains(&g.key) {
        "▸ ".to_string()
    } else {
        "▾ ".to_string()
    };
    let n = g.convs.len();
    let live = g
        .convs
        .iter()
        .filter(|&&ci| convs[ci].lifecycle.is_actionable_here())
        .count();
    // Project icon (Browse groups; Active session names already include it).
    let icon = if g.emoji.is_empty() {
        String::new()
    } else {
        format!("{} ", g.emoji)
    };
    let text = format!("{tri}{icon}{}  ({n}, {live} live)", g.key);
    let mut style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    if selected {
        style = style.add_modifier(Modifier::REVERSED);
    }
    Line::from(Span::styled(text, style))
}

/// The freeze note (why it was parked) for a frozen conversation, else empty.
fn frozen_note(c: &Conversation) -> String {
    match &c.frozen {
        Some(f) if !f.note.trim().is_empty() => {
            format!(
                "  📝 {}",
                f.note.trim().chars().take(40).collect::<String>()
            )
        }
        _ => String::new(),
    }
}

/// The auth-profile label for a conversation: `~/.claude-work` → "work". None ⇒
/// the default `~/.claude` (personal), which is left untagged.
fn env_label(c: &Conversation) -> Option<String> {
    let dir = c.auth_config_dir.as_ref()?;
    std::path::Path::new(dir)
        .file_name()
        .and_then(|s| s.to_str())
        .and_then(|b| b.strip_prefix(".claude-"))
        .map(|s| s.to_string())
}

/// Whether the conversation's tmux session is skipped (from cycling).
fn is_skipped(c: &Conversation, skipped: &HashSet<String>) -> bool {
    c.placement
        .as_ref()
        .map(|p| skipped.contains(&p.session_name))
        .unwrap_or(false)
}

/// Busy = the agent is actively running (working or a background workflow).
/// Everything else that's live (idle, waiting on you) reads as "free".
fn is_busy(c: &Conversation) -> bool {
    matches!(
        c.status.as_ref().map(|s| &s.status),
        Some(SessionStatus::Working) | Some(SessionStatus::RunningWorkflow { .. })
    )
}

/// At-a-glance row color for a live, non-archived conversation:
/// blue = busy (working/workflow), green = free (idle / waiting on you).
fn live_color(c: &Conversation) -> Color {
    if is_busy(c) {
        Color::Blue
    } else {
        Color::Green
    }
}

fn conv_line(
    c: &Conversation,
    group_path: &str,
    selected: bool,
    num: Option<usize>,
    flags: &Flags,
) -> Line<'static> {
    // Session-level flags apply to a live conversation's tmux session.
    let session = c.placement.as_ref().map(|p| p.session_name.as_str());
    let has = |set: &HashSet<String>| session.map(|s| set.contains(s)).unwrap_or(false);
    let favorite = has(&flags.favorite);
    let muted = has(&flags.muted);
    let auto = has(&flags.auto_approve);
    // Quick-jump number (1-9) or two spaces, mirroring the classic list.
    let num_prefix = match num {
        Some(n) => format!("{n} "),
        None => "  ".to_string(),
    };
    let marker = if c.is_frozen() {
        "💤"
    } else if c.lifecycle.is_actionable_here() {
        "●"
    } else {
        "○"
    };
    let title = c
        .title
        .as_deref()
        .map(|t| t.chars().take(36).collect::<String>())
        .unwrap_or_default();
    // "(3m ago)" relative time, mirroring classic's activity suffix.
    let ago = c
        .last_activity
        .as_deref()
        .map(|t| format!("  ({})", relative_time(t)))
        .unwrap_or_default();
    // Only the subpath that distinguishes this conversation from its group's
    // shared path (empty for the common case — the whole group shares one dir).
    let sub = rel_below(&c.cwd, group_path);
    let sub_part = if sub.is_empty() {
        String::new()
    } else {
        format!("  {sub}")
    };

    let skip = is_skipped(c, &flags.skipped);
    // At-a-glance color: live non-archived → blue (busy) / green (free); skipped
    // and archived → gray; closed → gray; selected → reversed highlight.
    let base = if selected {
        Style::default().add_modifier(Modifier::REVERSED)
    } else if skip || c.archived {
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM)
    } else if c.lifecycle.is_actionable_here() {
        Style::default().fg(live_color(c))
    } else {
        Style::default().fg(Color::Gray)
    };
    let dim = |base: Style| {
        if selected {
            base
        } else {
            Style::default().add_modifier(Modifier::DIM)
        }
    };

    // Leading favorite star (yellow) or a blank in the same 1-char slot.
    let fav_span = if favorite {
        Span::styled("★", Style::default().fg(Color::Yellow))
    } else {
        Span::styled(" ", base)
    };
    // Line 1 fragment: number · marker · id · title (classic name column).
    let mut spans = vec![
        fav_span,
        Span::styled(
            format!(
                " {} {} {:8}  {:<36}",
                num_prefix,
                marker,
                short_id(c.id.as_str()),
                title
            ),
            base,
        ),
    ];

    // Classic-style colored "→ status  (ago)".
    let (label, color) = status_span(c);
    if !label.is_empty() {
        let sc = if selected {
            base
        } else {
            Style::default().fg(color)
        };
        spans.push(Span::styled(format!("  → {label}"), sc));
    }
    if !ago.is_empty() {
        spans.push(Span::styled(ago, dim(base)));
    }
    // Frozen note, distinguishing subpath, and tags.
    let fnote = frozen_note(c);
    if !fnote.is_empty() {
        spans.push(Span::styled(fnote, dim(base)));
    }
    if !sub_part.is_empty() {
        spans.push(Span::styled(sub_part, dim(base)));
    }
    if let Some(env) = env_label(c) {
        let col = if selected {
            base
        } else {
            Style::default().fg(Color::Magenta)
        };
        spans.push(Span::styled(format!("  [{env}]"), col));
    }
    let tag = |on: bool, text: &'static str, fg: Color| -> Option<Span<'static>> {
        on.then(|| {
            let col = if selected {
                base
            } else {
                Style::default().fg(fg)
            };
            Span::styled(text, col)
        })
    };
    spans.extend(tag(auto, "  [auto]", Color::Green));
    spans.extend(tag(muted, "  [muted]", Color::DarkGray));
    spans.extend(tag(skip, "  [skip]", Color::DarkGray));
    Line::from(spans)
}

/// The tmux session that owns this conversation's project/worktree, if resolvable
/// from its logical `parent` key.
fn target_session(c: &Conversation) -> Option<String> {
    let parent = c.parent.as_ref()?;
    // A worktree parent ("project/branch") carries its own recorded session name.
    if parent.contains('/') {
        let wts = WorktreeState::load();
        let name = wts.worktrees.get(parent).map(|e| e.session_name.clone())?;
        return (!name.is_empty()).then_some(name);
    }
    // A project parent maps to the project's generated session name.
    let projects = ProjectRegistry::load();
    let config = projects.projects.get(parent)?;
    Some(ProjectRegistry::session_name(parent, config))
}

/// Reopen a closed conversation: resume it (`claude --resume <id>`) in its own
/// project/worktree session under its original auth profile — creating the
/// session if needed — then switch to it. Falls back to the current session when
/// the conversation has no resolvable parent (e.g. the "unassigned" group).
fn reopen(c: &Conversation) -> Result<String> {
    // Frozen conversations thaw through the existing frozen path, which recreates
    // the window/session, resumes (`--resume <id>` or `claude -c` for id-less
    // legacy entries), and removes the frozen.json entry. Then switch to it.
    if c.is_frozen() {
        let session = crate::common::frozen::thaw_window(c.id.as_str())?;
        switch_to_session(&session);
        return Ok(format!("Thawed {} in {session}", short_id(c.id.as_str())));
    }

    let startup = format!("claude --resume {}", c.id);
    // Resume under the same auth profile the conversation was created in.
    let env: Vec<(String, String)> = match &c.auth_config_dir {
        Some(dir) => vec![("CLAUDE_CONFIG_DIR".to_string(), dir.clone())],
        None => Vec::new(),
    };

    let target = target_session(c)
        .or_else(get_current_tmux_session)
        .ok_or_else(|| anyhow!("no target session (not inside tmux and no project match)"))?;

    let alive = Command::new("tmux")
        .args(["has-session", "-t", &target])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if alive {
        // Add a window to the existing session and resume in it.
        let mut cmd = Command::new("tmux");
        cmd.args(["new-window", "-t", &target, "-c", &c.cwd]);
        for (k, v) in &env {
            cmd.arg("-e").arg(format!("{k}={v}"));
        }
        if let Some(title) = &c.title {
            if !title.is_empty() {
                cmd.args(["-n", title]);
            }
        }
        if !cmd.output().map(|o| o.status.success()).unwrap_or(false) {
            return Err(anyhow!("failed to open a new window in '{target}'"));
        }
        let _ = Command::new("tmux")
            .args(["send-keys", "-t", &target, &startup, "Enter"])
            .output();
    } else if !ensure_tmux_session(&target, &c.cwd, Some(&startup), &env) {
        return Err(anyhow!("failed to create session '{target}'"));
    }

    switch_to_session(&target);
    Ok(format!(
        "Reopened {} in {target} — {startup}",
        short_id(c.id.as_str())
    ))
}

/// Start a fresh conversation in a project: if its session is alive, open a new
/// window running `claude`; otherwise create the session running `claude`. Uses
/// the project's auth profile (CLAUDE_CONFIG_DIR). Then switch to it.
fn new_conversation(key: &str) -> Result<String> {
    let projects = ProjectRegistry::load();
    let config = projects
        .projects
        .get(key)
        .ok_or_else(|| anyhow!("unknown project '{key}'"))?;
    let session = ProjectRegistry::session_name(key, config);
    let root = expand_tilde(&config.project_root)
        .to_string_lossy()
        .into_owned();
    let env = config.tmux_env();

    let alive = Command::new("tmux")
        .args(["has-session", "-t", &session])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if alive {
        let mut cmd = Command::new("tmux");
        cmd.args(["new-window", "-t", &session, "-c", &root]);
        for (k, v) in &env {
            cmd.arg("-e").arg(format!("{k}={v}"));
        }
        if !cmd.output().map(|o| o.status.success()).unwrap_or(false) {
            return Err(anyhow!("failed to open a new window in '{session}'"));
        }
        let _ = Command::new("tmux")
            .args(["send-keys", "-t", &session, "claude", "Enter"])
            .output();
    } else if !ensure_tmux_session(&session, &root, Some("claude"), &env) {
        return Err(anyhow!("failed to create session '{session}'"));
    }

    switch_to_session(&session);
    Ok(format!("New conversation in {session}"))
}

fn short_id(id: &str) -> String {
    // Synthetic frozen rows are keyed by a composite "session#window" (no real
    // conversation id) — show a dash rather than a truncated tmux name.
    if id.contains('#') {
        return "—".to_string();
    }
    id.chars().take(8).collect()
}

fn status_label(session: &Conversation) -> &'static str {
    match session.status.as_ref().map(|s| &s.status) {
        None => "-",
        Some(SessionStatus::Working) => "working",
        Some(SessionStatus::Waiting) => "idle",
        Some(SessionStatus::NeedsPermission { .. }) => "needs-perm",
        Some(SessionStatus::EditApproval { .. }) => "edit-approval",
        Some(SessionStatus::PlanReview) => "plan-review",
        Some(SessionStatus::QuestionAsked) => "question",
        Some(SessionStatus::RunningWorkflow { .. }) => "workflow",
        Some(SessionStatus::Unknown) => "unknown",
    }
}

/// Short status label + classic-hive color (idle=cyan, plan/ask=magenta,
/// perm/edit=yellow, flow=blue, work=darkgray). Empty label ⇒ closed (no status).
fn status_span(c: &Conversation) -> (&'static str, Color) {
    match c.status.as_ref().map(|s| &s.status) {
        None => ("", Color::DarkGray),
        Some(SessionStatus::Waiting) => ("idle", Color::Cyan),
        Some(SessionStatus::PlanReview) => ("plan", Color::Magenta),
        Some(SessionStatus::QuestionAsked) => ("ask?", Color::Magenta),
        Some(SessionStatus::NeedsPermission { .. }) => ("needs-perm", Color::Yellow),
        Some(SessionStatus::EditApproval { .. }) => ("edit", Color::Yellow),
        Some(SessionStatus::RunningWorkflow { .. }) => ("flow", Color::Blue),
        Some(SessionStatus::Working) => ("work", Color::DarkGray),
        Some(SessionStatus::Unknown) => ("…", Color::DarkGray),
    }
}

/// Render the registry grouped by parent key (deterministic ordering) — pure, so
/// it is unit-testable without tmux/disk.
pub fn render_conversations(reg: &ConversationRegistry) -> String {
    use std::collections::BTreeMap;

    let skipped = load_skipped_sessions();
    let projects = ProjectRegistry::load();
    let total = reg.conversations.len();
    let live = reg
        .conversations
        .values()
        .filter(|s| s.lifecycle.is_actionable_here())
        .count();
    let closed = total - live;
    let frozen = reg.conversations.values().filter(|c| c.is_frozen()).count();

    // Frozen conversations are enumerated first under a pinned "💤 frozen" header;
    // the rest are grouped by parent (BTreeMap gives stable ordering).
    let mut groups: BTreeMap<String, Vec<&Conversation>> = BTreeMap::new();
    let mut frozen_group: Vec<&Conversation> = Vec::new();
    for s in reg.conversations.values() {
        if s.is_frozen() {
            frozen_group.push(s);
            continue;
        }
        let key = s
            .parent
            .clone()
            .unwrap_or_else(|| "(unassigned)".to_string());
        groups.entry(key).or_default().push(s);
    }

    let mut out = String::new();
    let frozen_tag = if frozen > 0 {
        format!(", {frozen} frozen")
    } else {
        String::new()
    };
    out.push_str(&format!(
        "hive conversations — {total} known ({live} live · {closed} closed{frozen_tag}) across {} groups\n",
        groups.len()
    ));

    // Emit the pinned frozen group first, then the parent groups.
    let ordered = std::iter::once((FROZEN_GROUP.to_string(), frozen_group))
        .filter(|(_, g)| !g.is_empty())
        .chain(groups);
    for (parent, mut sessions) in ordered {
        // Live first, then most-recently-active, then id for stability.
        sessions.sort_by(|a, b| {
            b.lifecycle
                .is_actionable_here()
                .cmp(&a.lifecycle.is_actionable_here())
                .then_with(|| b.last_activity.cmp(&a.last_activity))
                .then_with(|| a.id.as_str().cmp(b.id.as_str()))
        });
        out.push('\n');
        let icon = project_emoji(&parent, &projects);
        if icon.is_empty() {
            out.push_str(&format!("{parent}\n"));
        } else {
            out.push_str(&format!("{icon} {parent}\n"));
        }
        // The path belongs to the project/worktree (implied by the group name), so
        // it's not shown; only a distinguishing subpath is kept for rows below.
        let path = common_prefix(&sessions.iter().map(|s| s.cwd.as_str()).collect::<Vec<_>>());
        for s in sessions {
            let marker = if s.is_frozen() {
                "💤"
            } else if s.lifecycle.is_actionable_here() {
                "●"
            } else {
                "○"
            };
            let last = s
                .last_activity
                .as_deref()
                .map(|t| t.chars().take(10).collect::<String>())
                .unwrap_or_default();
            let title = s
                .title
                .as_deref()
                .map(|t| t.chars().take(28).collect::<String>())
                .unwrap_or_default();
            // Only the subpath that distinguishes this conversation from its group.
            let sub = rel_below(&s.cwd, &path);
            let env = env_label(s).map(|e| format!("  [{e}]")).unwrap_or_default();
            let skip = if is_skipped(s, &skipped) {
                "  [skip]"
            } else {
                ""
            };
            out.push_str(&format!(
                "  {} {:8}  {:<28}  {:<13}  {}  {}{}{}{}\n",
                marker,
                short_id(s.id.as_str()),
                title,
                status_label(s),
                last,
                sub,
                frozen_note(s),
                env,
                skip,
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::registry::{ConversationId, Lifecycle};

    fn mk(id: &str, lc: Lifecycle, parent: Option<&str>, last: Option<&str>) -> Conversation {
        Conversation {
            id: ConversationId::from(id),
            cwd: "/home/u/hive".to_string(),
            lifecycle: lc,
            status: None,
            last_activity: last.map(|s| s.to_string()),
            placement: None,
            parent: parent.map(|s| s.to_string()),
            frozen: None,
            note: String::new(),
            pinned: false,
            archived: false,
            title: None,
            auth_config_dir: None,
        }
    }

    #[test]
    fn test_render_groups_and_counts() {
        let mut reg = ConversationRegistry::default();
        let mut live = mk(
            "live1",
            Lifecycle::Live,
            Some("hive"),
            Some("2026-07-02T00:00:00Z"),
        );
        live.title = Some("My Task".to_string());
        reg.conversations.insert("live1".to_string(), live);
        reg.conversations.insert(
            "closed1".to_string(),
            mk(
                "closed1",
                Lifecycle::Closed,
                Some("hive"),
                Some("2026-07-01T00:00:00Z"),
            ),
        );
        reg.conversations.insert(
            "orphan".to_string(),
            mk("orphan", Lifecycle::Closed, None, None),
        );

        let out = render_conversations(&reg);

        assert!(out.contains("3 known (1 live · 2 closed) across 2 groups"));
        assert!(out.contains("hive\n"));
        assert!(out.contains("(unassigned)"));
        assert!(out.contains('●')); // a live marker
        assert!(out.contains('○')); // a closed marker
        assert!(out.contains("My Task")); // named session's title is shown
    }

    #[test]
    fn test_render_empty_registry() {
        let reg = ConversationRegistry::default();
        let out = render_conversations(&reg);
        assert!(out.contains("0 known (0 live · 0 closed) across 0 groups"));
    }
}
