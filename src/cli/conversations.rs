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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sysinfo::System;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::common::activity::OpenWindowsState;
use crate::common::chrome::{
    focus_all_matched_tabs, get_chrome_tabs, match_tabs_to_ports, ChromeTab,
};
use crate::common::conversations::{
    gather_conversations, gather_conversations_stats, reopen_conversation,
};
use crate::common::frozen::{discard_frozen, freeze_window, relative_time, FreezeTarget};
use crate::common::instances;
use crate::common::jsonl;
use crate::common::persistence::{
    is_globally_muted, load_auto_approve_sessions, load_completed_todos, load_favorite_sessions,
    load_muted_projects, load_muted_sessions, load_session_todos, load_skipped_sessions,
    save_auto_approve_sessions, save_completed_todos, save_favorite_sessions, save_muted_projects,
    save_muted_sessions, save_session_todos, save_skipped_sessions, set_global_mute,
};
use crate::common::ports::{get_listening_ports_for_pids, ListeningPort};
use crate::common::process::get_process_info;
use crate::common::projects::{
    activate_project, ensure_tmux_session, expand_tilde, ProjectConfig, ProjectRegistry,
};
use crate::common::registry::{
    Conversation, ConversationOverlay, ConversationRegistry, ConversationSidecar, Lifecycle,
};
use crate::common::tmux::{
    exact, exact_active_pane, exact_window, get_all_windows, get_current_tmux_session,
    get_current_tmux_session_names, get_current_tmux_window, select_window, switch_to_session,
};
use crate::common::types::ProcessInfo;
use crate::common::worktree::WorktreeState;
use crate::ipc::messages::SessionStatus;

/// Entry point. Interactive TUI on a terminal; static listing with `--list` or
/// when output is piped/redirected.
/// Startup options for the conversations TUI, mapped from the CLI. Mirrors the
/// classic flags so the tmux bindings keep working after the default view flips to
/// this one (`prefix+d` → `--detail`, the picker fall-through, `--filter`).
#[derive(Default)]
pub struct ConvOptions {
    /// Print a static listing and exit (no TUI).
    pub list: bool,
    /// Open the current tmux window's conversation detail on startup (classic `--detail`).
    pub detail: bool,
    /// Open the current tmux window's PROJECT detail on startup (`prefix+a`).
    pub project_detail: bool,
    /// Start in Browse + search mode (classic `--picker`).
    pub picker: bool,
    /// Start in Browse + search pre-filled with this query (classic `--filter`).
    pub filter: Option<String>,
}

pub fn run_conversations(opts: ConvOptions) -> Result<()> {
    if opts.list || !std::io::stdout().is_terminal() {
        print!("{}", render_conversations(&gather_conversations()));
        return Ok(());
    }
    run_conversations_tui(&opts)
}

// ── Interactive TUI ─────────────────────────────────────────────────────────

/// What the user chose in the TUI, performed after the terminal is restored.
enum Action {
    Quit,
    Switch(Box<Conversation>),
    Reopen(Box<Conversation>),
    /// Start a fresh conversation in the given project (key).
    NewInProject(String),
    /// Start a fresh conversation in a tmux session (name) with an initial prompt —
    /// used to turn a todo into a `claude "task: …"` window.
    NewTask(String, String),
    /// Spread N sessions into iTerm2 panes / collapse back (classic `L`).
    Spread(usize),
    Collapse,
    /// Create a worktree (project key, branch) and switch to it.
    WtNew(String, String),
    /// Delete a worktree (project key, branch).
    WtDelete(String, String),
    /// Open a registered worktree's tmux session (project key, branch), starting
    /// the session if it isn't running.
    ConnectWorktree(String, String),
    /// Start a fresh conversation in a worktree (project key, branch).
    NewInWorktree(String, String),
    /// Switch to a bare tmux session (one with no live conversation).
    SwitchSession(String),
    /// Switch to a specific non-Claude tmux window (session name, window index).
    SwitchWindow(String, String),
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
    /// tmux windows of this session with NO Claude conversation (plain shells,
    /// servers, editors). Active view only; shown as dimmed "window" rows so the
    /// full session is visible, marked apart from real conversations.
    windows: Vec<WinRow>,
    /// Registered worktrees of this project. Browse only; shown as rows above the
    /// project's conversations so a worktree is a jump target in its own right.
    worktrees: Vec<BrowseWt>,
}

/// A non-Claude tmux window shown as a row in the Active view.
struct WinRow {
    index: String,
    name: String,
}

/// A registered worktree shown as a row in the Browse list. A worktree is a place
/// you go, independent of whether any conversation currently lives there: a fresh
/// one has none, and an old one's have aged out of the (14-day) registry bound —
/// so keying this off the worktree registry, not conversations, is what makes
/// every worktree reachable from `/`.
#[derive(Clone)]
struct BrowseWt {
    project: String,
    branch: String,
    /// Its tmux session is running right now (the name is re-resolved from the
    /// worktree registry when the row is acted on, so it isn't carried here).
    session_live: bool,
    /// Live conversations currently under this worktree.
    live: usize,
    /// When this worktree was last worked in: the newest activity across its
    /// conversations, falling back to when it was created (a brand-new worktree has
    /// no conversations yet, and should still rank as recent).
    last_activity: Option<String>,
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

/// A visible row: a group header, a conversation, a registered worktree, a
/// non-Claude tmux window, or the `… N more` toggle that ends a capped section
/// (everything but the header sits under an expanded group).
enum Row {
    Header(usize),                   // index into `groups`
    Conv { ci: usize, gi: usize },   // conversation index + its group index
    Window { gi: usize, wi: usize }, // group index + index into that group's `windows`
    Wt { gi: usize, wi: usize },     // group index + index into that group's `worktrees`
    More { gi: usize, section: Section },
}

/// The two capped sections of a group — what a `More` row expands.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Section {
    Worktrees,
    Convs,
}

/// How many worktrees / conversations a Browse group lists before it folds the
/// rest behind a `… N more` row.
const BROWSE_PAGE: usize = 5;

/// Header key for the pinned group that enumerates all frozen conversations.
const FROZEN_GROUP: &str = "💤 frozen";

/// The recovery screen: conversations the last frame says were open, that aren't now.
///
/// The set comes from `open-windows.json`, which the gather keeps reconciled to the live
/// windows (see [`crate::common::activity::sync_open_windows`]). While hive is watching, a
/// window that closes leaves the file; when hive dies *with* the machine nothing is removed,
/// so what is left is the frame that was open at that moment.
///
/// Rows are the registry's own `Conversation`s, not snapshot entries: that is what lets the
/// restore go through [`reopen_conversation`], which resolves the destination from the
/// conversation's PARENT rather than from a recorded session name. A name captured before a
/// reboot is a guess about a world that no longer exists — the mistake that once put a
/// thawed conversation in the wrong worktree's session (`f51860b`).
struct RecoverState {
    /// Restore candidates in (session, window index) order, so replaying them rebuilds the
    /// layout you left rather than an arbitrary permutation.
    rows: Vec<Conversation>,
    /// Ids ticked for restore. Everything starts ticked: after a reboot you usually want the
    /// whole frame back, and un-ticking three is less work than ticking eleven.
    checked: HashSet<String>,
    sel: usize,
    /// When the frame was last observed. Deliberately "last seen", not "when the machine
    /// died" — hive cannot know the latter, and claiming it would be a lie on any machine
    /// that sat idle before going down.
    last_seen: Option<String>,
    /// Restore failures from the last `Enter`, shown in place of the footer hint.
    errors: Vec<String>,
}

/// Build the recovery screen from the frame on disk, or `None` when there is nothing to
/// offer — which is the normal state, and not an error: it means everything that was open
/// still is.
fn build_recover(reg: &ConversationRegistry) -> Option<RecoverState> {
    let snap = OpenWindowsState::load();
    let rows = recover_rows(&snap.windows, reg);
    if rows.is_empty() {
        return None;
    }
    let last_seen = snap.windows.values().map(|w| w.last_seen.clone()).max();
    let checked: HashSet<String> = rows.iter().map(|c| c.id.as_str().to_string()).collect();
    Some(RecoverState {
        rows,
        checked,
        sel: 0,
        last_seen,
        errors: Vec::new(),
    })
}

/// Which of the ticked conversations recovery should drop you into: the most recently active
/// one, i.e. what you were last working on before the machine went down. Ties keep replay
/// order (the earlier row wins). Not the cursor row — every row starts ticked and the cursor
/// starts on row 1, so "the cursor" would mean "alphabetically first session" for anyone who
/// just pressed Enter.
fn landing_target(picked: &[Conversation]) -> Option<&Conversation> {
    picked.iter().reduce(|best, c| {
        if c.last_activity > best.last_activity {
            c
        } else {
            best
        }
    })
}

/// Pure core of [`build_recover`]: the restore candidates a frame implies, in the order they
/// should be replayed. Takes the entries so the rules stay unit-testable without an
/// `open-windows.json` on disk.
///
/// A candidate is an entry that is **not currently live**. Reconciling against the registry
/// — rather than, say, the `--resume` id in a process's argv — is what makes the screen
/// idempotent: open it twice and the second pass has nothing to do. argv would not serve, as
/// it holds the id a window was LAUNCHED with, which goes stale the moment that window
/// starts a different conversation.
fn recover_rows(
    windows: &HashMap<String, crate::common::activity::OpenWindow>,
    reg: &ConversationRegistry,
) -> Vec<Conversation> {
    let mut rows: Vec<(&str, u32, Conversation)> = Vec::new();
    for (id, w) in windows {
        let Some(c) = reg.conversations.get(id) else {
            // Bounded out of the registry (too old, no parent, unpinned) — there is nothing
            // to resume it *with*, so it cannot honestly be offered.
            continue;
        };
        if c.lifecycle == Lifecycle::Live {
            continue;
        }
        // Parse the index so "10" sorts after "9" rather than between "1" and "2".
        let idx = w.window_index.parse::<u32>().unwrap_or(u32::MAX);
        rows.push((w.session_name.as_str(), idx, c.clone()));
    }
    rows.sort_by(|a, b| a.0.cmp(b.0).then_with(|| a.1.cmp(&b.1)));
    rows.into_iter().map(|(_, _, c)| c).collect()
}

/// Home-row alphabet for hint-jump labels (9 keys → 81 two-char labels).
const HINT_ALPHABET: &[u8] = b"asdfghjkl";

/// `count` unique 2-char labels from the home row in stable order (aa, as, ad, …),
/// so any visible conversation is one hop away regardless of the 1-9 quick-jump cap.
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

/// The session-level flag sets (all keyed by tmux session name), loaded together
/// so a conversation row can show ★ / [muted] / [auto] / [skip] and toggle them.
struct Flags {
    favorite: HashSet<String>,
    muted: HashSet<String>,
    auto_approve: HashSet<String>,
    skipped: HashSet<String>,
    /// Muted PROJECT keys (a remembered per-project preference), not session names.
    muted_projects: HashSet<String>,
    /// Archived PROJECT keys (hidden from Browse unless revealed).
    archived_projects: HashSet<String>,
    /// Active todo count per tmux session — for the row badge (todos stay
    /// session-level; a conversation just surfaces its session's list).
    todo_counts: HashMap<String, usize>,
}

impl Flags {
    fn load() -> Self {
        Flags {
            favorite: load_favorite_sessions(),
            muted: load_muted_sessions(),
            auto_approve: load_auto_approve_sessions(),
            skipped: load_skipped_sessions(),
            muted_projects: load_muted_projects(),
            archived_projects: ProjectRegistry::load()
                .projects
                .iter()
                .filter(|(_, c)| c.archived)
                .map(|(k, _)| k.clone())
                .collect(),
            todo_counts: load_session_todos()
                .into_iter()
                .map(|(k, v)| (k, v.len()))
                .collect(),
        }
    }
}

/// A minimal 3-step new-project wizard state (`N` in the list): key → emoji → path.
struct NewProject {
    step: u8,
    key: String,
    emoji: String,
    path: String,
    /// Why the last Enter didn't go through. Rendered red in the footer and cleared
    /// on the next keystroke — the wizard used to discard everything in silence when
    /// a required field was blank, which read exactly like a successful create.
    error: Option<String>,
}

/// Why the wizard's key step can't advance, if it can't. `exists` is whether the key
/// is already registered — re-adding would clobber that project's config (auth
/// profile, ports, worktrees dir) with defaults.
fn wizard_key_error(key: &str, exists: bool) -> Option<String> {
    let k = key.trim();
    if k.is_empty() {
        Some("key is required".to_string())
    } else if exists {
        Some(format!("project '{k}' already exists"))
    } else {
        None
    }
}

/// Why the wizard's path step can't finish, if it can't.
fn wizard_path_error(path: &str) -> Option<String> {
    path.trim()
        .is_empty()
        .then(|| "path is required".to_string())
}

/// Register a new project in projects.toml (other fields default; edit the TOML or
/// use `hive project add` for the full set).
fn create_project(key: &str, emoji: &str, path: &str) -> Result<()> {
    let mut reg = ProjectRegistry::load();
    reg.add_project(
        key.to_string(),
        ProjectConfig {
            emoji: if emoji.is_empty() {
                "📁".to_string()
            } else {
                emoji.to_string()
            },
            project_root: path.to_string(),
            ..Default::default()
        },
    );
    reg.save()
}

/// Toggle a project's remembered mute preference and persist it.
fn toggle_muted_project(key: &str) {
    let mut set = load_muted_projects();
    if !set.remove(key) {
        set.insert(key.to_string());
    }
    save_muted_projects(&set);
}

/// Toggle a project's archived flag in projects.toml.
fn toggle_archived_project(key: &str) {
    let mut reg = ProjectRegistry::load();
    let now = reg.projects.get(key).map(|c| c.archived).unwrap_or(false);
    if reg.set_archived(key, !now) {
        let _ = reg.save();
    }
}

/// Mutate one conversation's overlay entry in `conversations.json`, dropping the
/// entry again if it's been reset to empty (keeps the sidecar tidy).
fn edit_overlay(id: &str, f: impl FnOnce(&mut ConversationOverlay)) {
    let mut sc = ConversationSidecar::load();
    f(sc.conversations.entry(id.to_string()).or_default());
    if sc.conversations.get(id) == Some(&ConversationOverlay::default()) {
        sc.conversations.remove(id);
    }
    let _ = sc.save();
}

/// Set (or clear) a conversation's pinned flag (persisted overlay).
fn set_conversation_pin(id: &str, pinned: bool) {
    edit_overlay(id, |o| o.pinned = pinned);
}

/// Set (or clear) a conversation's mute override (persisted overlay). With it on,
/// the hook notifier rings for this conversation even under global / project /
/// session mute — see `cli::hook::should_notify`.
fn set_conversation_notify_override(id: &str, on: bool) {
    edit_overlay(id, |o| o.notify_override = on);
}

/// Set (or clear) a conversation's free-text note (persisted overlay).
fn set_conversation_note(id: &str, note: &str) {
    edit_overlay(id, |o| o.note = note.trim().to_string());
}

/// Archive (with a reason) or unarchive a conversation — persisted overlay.
/// Archiving stamps the reason and the time so the project detail can say WHY a
/// hidden conversation is hidden; unarchiving clears both, so an unarchived
/// conversation carries no stale reason if it's archived again later.
fn set_conversation_archived(id: &str, archived: bool, reason: Option<&str>) {
    edit_overlay(id, |o| {
        o.archived = archived;
        if archived {
            let reason = reason.unwrap_or("").trim();
            o.archive_reason = (!reason.is_empty()).then(|| reason.to_string());
            o.archived_at = Some(chrono::Utc::now().to_rfc3339());
        } else {
            o.archive_reason = None;
            o.archived_at = None;
        }
    });
}

/// The active todo list for a tmux session (todos are session-level).
fn session_todos(session: &str) -> Vec<String> {
    load_session_todos().remove(session).unwrap_or_default()
}

/// Append a todo to a session's list.
fn add_session_todo(session: &str, text: &str) {
    let mut todos = load_session_todos();
    todos
        .entry(session.to_string())
        .or_default()
        .push(text.to_string());
    save_session_todos(&todos);
}

/// Mark the `idx`-th (0-based) todo done: move it from active to completed.
fn done_session_todo(session: &str, idx: usize) {
    let mut todos = load_session_todos();
    let Some(items) = todos.get_mut(session) else {
        return;
    };
    if idx >= items.len() {
        return;
    }
    let removed = items.remove(idx);
    if items.is_empty() {
        todos.remove(session);
    }
    save_session_todos(&todos);
    let mut completed = load_completed_todos();
    completed
        .entry(session.to_string())
        .or_default()
        .push(removed);
    save_completed_todos(&completed);
}

/// Delete the `idx`-th (0-based) todo outright — dropped, NOT moved to completed.
fn delete_session_todo(session: &str, idx: usize) {
    let mut todos = load_session_todos();
    let Some(items) = todos.get_mut(session) else {
        return;
    };
    if idx >= items.len() {
        return;
    }
    items.remove(idx);
    if items.is_empty() {
        todos.remove(session);
    }
    save_session_todos(&todos);
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

/// Un-skip a session (remove it from the skipped set) if present. Switching TO a
/// session means you're actively using it, so it should re-enter cycling — mirrors
/// classic's `unskip` on switch.
fn unskip_session(name: &str) {
    let mut skipped = load_skipped_sessions();
    if skipped.remove(name) {
        save_skipped_sessions(&skipped);
    }
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
        // Pinned first, then live, then most-recent, then id for stability.
        b.pinned
            .cmp(&a.pinned)
            .then_with(|| {
                b.lifecycle
                    .is_actionable_here()
                    .cmp(&a.lifecycle.is_actionable_here())
            })
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
        windows: Vec::new(),
        worktrees: Vec::new(),
    }
}

/// Browse grouping: one group per PROJECT (worktrees fold into their project),
/// preceded by a pinned "💤 frozen" bucket. `include_empty` adds every registered
/// non-archived project that has no conversations, so Browse doubles as a
/// launchpad (open any project, start fresh). Projects sort by recent activity
/// (most-recent first), empty ones last alphabetically. `worktrees` (keyed by
/// project, from [`browse_worktrees`]) adds each project's worktrees as rows and
/// surfaces the project itself when one of them is all the query matched.
fn build_browse(
    reg: &ConversationRegistry,
    projects: &ProjectRegistry,
    include_empty: bool,
    reveal_archived: bool,
    matched_projects: &HashSet<String>,
    worktrees: &HashMap<String, Vec<BrowseWt>>,
) -> (Vec<Group>, Vec<Conversation>) {
    let is_archived = |key: &str| {
        projects
            .projects
            .get(key)
            .map(|c| c.archived)
            .unwrap_or(false)
    };
    let mut frozen: Vec<&Conversation> = Vec::new();
    let mut grouped: HashMap<String, Vec<&Conversation>> = HashMap::new();
    for c in reg.conversations.values() {
        // Archiving means "out of the way": an archived conversation drops off
        // Browse but is NOT forgotten — its project's detail screen still lists
        // it, with the reason it was archived. (A live one always shows: you
        // can't hide something that's running.)
        if c.archived && !c.lifecycle.is_actionable_here() {
            continue;
        }
        if c.is_frozen() {
            frozen.push(c);
            continue;
        }
        // Collapse a worktree key ("proj/branch") to its project ("proj").
        let key = match c.parent.as_deref() {
            Some(p) => p.split('/').next().unwrap_or(p).to_string(),
            None => "(unassigned)".to_string(),
        };
        grouped.entry(key).or_default().push(c);
    }
    if include_empty {
        for key in projects.projects.keys() {
            grouped.entry(key.clone()).or_default();
        }
    }
    // Hide archived projects only on the FULL list (`include_empty` == no search),
    // matching the classic picker: they reappear as soon as you type a search, or
    // when revealed via Ctrl+R. A project with a LIVE conversation is never hidden —
    // same rule as the conversation-level archive above, one level up: you can't
    // hide something that's running. Without this, starting a conversation in an
    // archived project (from a bare `claude` in a tmux window, which no unarchive-on-
    // start hook can catch) dropped it off Browse entirely.
    if !reveal_archived && include_empty {
        grouped.retain(|key, convs| {
            !is_archived(key) || convs.iter().any(|c| c.lifecycle.is_actionable_here())
        });
    }
    // Searching: a project matched BY NAME surfaces even with zero conversations —
    // archived ones included, since the retain above is scoped to the full list.
    // Without this, a project whose conversations aged out of the registry (every
    // long-archived project) could not be found at all.
    for key in matched_projects {
        if projects.projects.contains_key(key) {
            grouped.entry(key.clone()).or_default();
        }
    }
    // Same for worktrees: a project surfaces because it HAS worktrees to show, even
    // with no conversations of its own. `worktrees` is already query-filtered by the
    // caller, so when searching this is exactly the branch-matched set; the archived
    // guard mirrors the retain above (which only runs on the full list).
    // `!grouped.contains_key` is what the retain above just decided: an archived
    // project that survived it (because something is live there) keeps its worktrees.
    for key in worktrees.keys() {
        if include_empty && !reveal_archived && is_archived(key) && !grouped.contains_key(key) {
            continue;
        }
        grouped.entry(key.clone()).or_default();
    }

    // Order: projects the query NAMED first — by project name or by one of their
    // branches (both are "you asked for this project"), so a searched-for project
    // outranks incidental conversation hits — then most-recent activity (desc),
    // then name; empties last. Neither set is populated when not searching, and the
    // branch half is scoped to a search so the flat list keeps its recency order.
    let named =
        |k: &String| matched_projects.contains(k) || (!include_empty && worktrees.contains_key(k));
    let recency = |g: &[&Conversation]| g.iter().filter_map(|c| c.last_activity.clone()).max();
    let mut keyed: Vec<(String, Vec<&Conversation>)> = grouped.into_iter().collect();
    keyed.sort_by(|(ka, a), (kb, b)| {
        named(kb)
            .cmp(&named(ka))
            .then_with(|| recency(b).cmp(&recency(a)))
            .then_with(|| ka.to_lowercase().cmp(&kb.to_lowercase()))
    });

    let mut groups = Vec::new();
    let mut convs = Vec::new();
    // Frozen pinned first so the whole freeze set is one row at the top.
    if !frozen.is_empty() {
        sort_convs(&mut frozen);
        groups.push(push_group(
            FROZEN_GROUP.to_string(),
            frozen,
            &mut convs,
            String::new(),
        ));
    }
    for (key, mut group) in keyed {
        sort_convs(&mut group);
        let emoji = project_emoji(&key, projects);
        let wts = worktrees.get(&key).cloned().unwrap_or_default();
        let mut g = push_group(key, group, &mut convs, emoji);
        g.worktrees = wts;
        groups.push(g);
    }
    (groups, convs)
}

/// Browse's worktree rows, keyed by project: every registered worktree when the
/// query is empty, only the matching ones while searching. A worktree matches on
/// its branch, its tmux session name, or its path — and every worktree of a
/// project the query NAMED comes along, mirroring how conversations follow their
/// project.
///
/// Pure (tmux and disk state come in as arguments) so the filtering stays
/// unit-testable. `reg` should be the FULL registry: the live counts describe the
/// worktree, not the search.
fn browse_worktrees(
    wts: &WorktreeState,
    reg: &ConversationRegistry,
    live_sessions: &HashSet<String>,
    query: &str,
    matched_projects: &HashSet<String>,
) -> HashMap<String, Vec<BrowseWt>> {
    let q = query.to_lowercase();
    let mut out: HashMap<String, Vec<BrowseWt>> = HashMap::new();
    for e in wts.worktrees.values() {
        let hay = |s: &str| s.to_lowercase().contains(&q);
        let keep = q.is_empty()
            || matched_projects.contains(&e.project_key)
            || hay(&e.branch)
            || hay(&e.session_name)
            || hay(&e.path);
        if !keep {
            continue;
        }
        let wt_key = WorktreeState::make_key(&e.project_key, &e.branch);
        let mine = || {
            reg.conversations
                .values()
                .filter(|c| c.parent.as_deref() == Some(wt_key.as_str()))
        };
        let live = mine().filter(|c| c.lifecycle.is_actionable_here()).count();
        // Newest conversation activity, else creation time — both RFC3339, so a
        // string compare orders them (the same idiom the conversation rows use).
        let last_activity = mine()
            .filter_map(|c| c.last_activity.clone())
            .max()
            .or_else(|| (!e.created_at.is_empty()).then(|| e.created_at.clone()));
        out.entry(e.project_key.clone())
            .or_default()
            .push(BrowseWt {
                project: e.project_key.clone(),
                branch: e.branch.clone(),
                session_live: live_sessions.contains(&e.session_name),
                live,
                last_activity,
            });
    }
    // Running worktrees first (where the work is), then most-recently-worked-in —
    // so a capped list shows the 5 that matter. Branch name only breaks ties.
    for rows in out.values_mut() {
        rows.sort_by(|a, b| {
            b.session_live
                .cmp(&a.session_live)
                .then_with(|| b.last_activity.cmp(&a.last_activity))
                .then_with(|| a.branch.to_lowercase().cmp(&b.branch.to_lowercase()))
        });
    }
    out
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

/// Registered project keys whose key or display name matches `q` (case-insensitive).
/// Browse searches projects FIRST: a project the user names has to surface even
/// with zero conversations in the registry (it never had any, or they aged out of
/// the bound) — which is the only way to find an archived one, since archiving is
/// what removes it from the unfiltered list.
fn projects_matching(projects: &ProjectRegistry, q: &str) -> HashSet<String> {
    if q.is_empty() {
        return HashSet::new();
    }
    let q = q.to_lowercase();
    projects
        .projects
        .iter()
        .filter(|(key, config)| {
            key.to_lowercase().contains(&q)
                || config
                    .display_name
                    .as_deref()
                    .map(|d| d.to_lowercase().contains(&q))
                    .unwrap_or(false)
        })
        .map(|(key, _)| key.clone())
        .collect()
}

/// The project a conversation belongs to: its `parent` collapsed at the first `/`
/// (so a worktree "proj/branch" reads as "proj"), matching Browse's grouping.
fn conv_project_key(c: &Conversation) -> Option<&str> {
    c.parent
        .as_deref()
        .map(|p| p.split('/').next().unwrap_or(p))
}

/// A registry containing only the conversations matching `query` (all, if empty).
/// A conversation also survives when its PROJECT is one of `matched_projects`, so
/// naming a project in the query brings its conversations along even when none of
/// them match the text themselves (e.g. searching a display name).
fn filter_registry(
    reg: &ConversationRegistry,
    query: &str,
    matched_projects: &HashSet<String>,
) -> ConversationRegistry {
    if query.is_empty() {
        return reg.clone();
    }
    ConversationRegistry {
        conversations: reg
            .conversations
            .iter()
            .filter(|(_, c)| {
                matches_query(c, query)
                    || conv_project_key(c)
                        .map(|k| matched_projects.contains(k))
                        .unwrap_or(false)
            })
            .map(|(k, c)| (k.clone(), c.clone()))
            .collect(),
    }
}

/// The currently-visible rows: every header, plus the contents of expanded groups.
/// Recomputed whenever the collapsed / expanded sets change.
///
/// `page` caps each section at N rows and follows it with a `More` row (Browse: a
/// project with 26 worktrees and 29 conversations is a wall otherwise). `None`
/// shows everything — the Active view stays complete, since a live window that
/// isn't listed is a window you can't get back to.
fn visible_rows(
    groups: &[Group],
    collapsed: &HashSet<String>,
    expanded: &HashSet<(String, Section)>,
    page: Option<usize>,
) -> Vec<Row> {
    let mut rows = Vec::new();
    for (gi, g) in groups.iter().enumerate() {
        rows.push(Row::Header(gi));
        if collapsed.contains(&g.key) {
            continue;
        }
        // How many of `total` to list, and whether the section needs a More row.
        let shown = |total: usize, section: Section| -> (usize, bool) {
            match page {
                Some(n) if total > n => {
                    if expanded.contains(&(g.key.clone(), section)) {
                        (total, true)
                    } else {
                        (n, true)
                    }
                }
                _ => (total, false),
            }
        };
        // Worktrees lead: they're places (a branch you can go work in), and
        // the conversations below are what has happened in them.
        let (n_wt, more_wt) = shown(g.worktrees.len(), Section::Worktrees);
        for wi in 0..n_wt {
            rows.push(Row::Wt { gi, wi });
        }
        if more_wt {
            rows.push(Row::More {
                gi,
                section: Section::Worktrees,
            });
        }
        let (n_conv, more_conv) = shown(g.convs.len(), Section::Convs);
        for &ci in g.convs.iter().take(n_conv) {
            rows.push(Row::Conv { ci, gi });
        }
        if more_conv {
            rows.push(Row::More {
                gi,
                section: Section::Convs,
            });
        }
        // Non-Claude windows come after the session's conversations.
        for wi in 0..g.windows.len() {
            rows.push(Row::Window { gi, wi });
        }
    }
    rows
}

/// Active view: only LIVE conversations, grouped by the tmux session running
/// them — the conversation-aware analog of the classic `prefix + s` session list.
/// Skipped sessions are ordered last so they form a separate section.
///
/// `session_windows` (session name → its tmux windows) does double duty: its keys
/// are the live sessions to surface even without a conversation, and each session's
/// windows that host no conversation become dimmed "window" rows so the whole
/// session is visible.
fn build_active(
    reg: &ConversationRegistry,
    skipped: &HashSet<String>,
    session_windows: &HashMap<String, Vec<(String, String)>>,
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
    // Surface EVERY live tmux session that has no live conversation as a bare
    // (0-conv) group, so the Active view matches classic's session-first list.
    // Otherwise a session where claude isn't running (a plain shell like `00-main`,
    // a skipped one, a server) is invisible here and can't be seen or un-skipped.
    for name in session_windows.keys() {
        grouped.entry(name.clone()).or_default();
    }
    // Partition into three sections, matching classic's order: normal claude
    // groups first, then "other" (bare live sessions with no claude), then skipped.
    let mut normal = Vec::new();
    let mut other = Vec::new();
    let mut skip = Vec::new();
    for (key, mut group) in grouped {
        group.sort_by(|a, b| {
            b.last_activity
                .cmp(&a.last_activity)
                .then_with(|| a.id.as_str().cmp(b.id.as_str()))
        });
        if skipped.contains(&key) {
            skip.push((key, group));
        } else if group.is_empty() {
            other.push((key, group));
        } else {
            normal.push((key, group));
        }
    }
    let mut groups = Vec::new();
    let mut convs = Vec::new();
    for (key, group) in normal.into_iter().chain(other).chain(skip) {
        // Session names already carry their project emoji, so no separate icon.
        let mut g = push_group(key, group, &mut convs, String::new());
        // Attach the session's windows that no conversation occupies — plain
        // shells/servers/editors — as dimmed rows so the full session is visible.
        if let Some(wins) = session_windows.get(&g.key) {
            let covered: HashSet<String> = g
                .convs
                .iter()
                .filter_map(|&ci| convs[ci].placement.as_ref())
                .map(|p| p.window_index.clone())
                .collect();
            g.windows = wins
                .iter()
                .filter(|(idx, _)| !covered.contains(idx))
                .map(|(index, name)| WinRow {
                    index: index.clone(),
                    name: name.clone(),
                })
                .collect();
        }
        groups.push(g);
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
    /// When this worktree was last worked in — orders the list so a capped view
    /// shows the latest few. See [`browse_worktrees`] for the same rule.
    last_activity: Option<String>,
}

/// A worktree detail's own identity — set when the screen is showing a WORKTREE
/// ("project/branch") rather than a project, which changes the header block (its
/// own path + session) and what `n` / `x` act on.
struct WtInfo {
    project: String,
    branch: String,
    path: String,
    session: String,
}

/// The project detail sub-screen: a project's config, its worktrees, and every
/// conversation under it (live + closed + frozen), from which you can switch,
/// resume, or start a new one. The same screen also renders a single WORKTREE
/// (drilled into from a worktree row) — then `wt_info` is set and the nested
/// worktree list is dropped.
struct ProjectDetailState {
    key: String,
    worktrees: Vec<WtRow>,
    convs: Vec<Conversation>, // display order: live first, frozen last, else recency
    path: String,             // common cwd prefix (for subpath elision in rows)
    /// Session-level todos, flat and in display order: `(session_name, text)`. The
    /// cursor selects todos FIRST, then conversations, then worktrees — matching
    /// the order they're drawn in.
    todos: Vec<(String, String)>,
    sel: usize,               // index into the combined [todos ++ convs ++ worktrees] list
    wt_input: Option<String>, // Some ⇒ typing a branch name for a new worktree
    /// Some ⇒ typing the archive reason for that conversation id (`A`).
    archive_input: Option<(String, String)>,
    /// `Tab`: list every worktree and conversation instead of the latest
    /// `BROWSE_PAGE`. A project with 26 worktrees and 29 conversations buries its
    /// todos and its recent work otherwise.
    show_all: bool,
    /// Some ⇒ this screen is one worktree, not a project.
    wt_info: Option<WtInfo>,
    /// This screen is a cross-project BUCKET (💤 frozen / "(unassigned)"), not a
    /// project: it has no config, no worktrees, and its conversations come from
    /// everywhere — so it labels each row with the project it belongs to.
    bucket: bool,
}

impl ProjectDetailState {
    /// The most recently-active conversation — what `r` (resume last) opens.
    /// Archived ones are skipped: you archived them to get them out of the way,
    /// so they must not become the thing `r` reopens.
    fn most_recent(&self) -> Option<&Conversation> {
        self.convs
            .iter()
            .filter(|c| !c.archived)
            .max_by(|a, b| a.last_activity.cmp(&b.last_activity))
    }

    /// Index of the first archived conversation — where the "Archived" section
    /// starts (the sort puts them last). `None` when there are none.
    fn archived_from(&self) -> Option<usize> {
        self.convs.iter().position(|c| c.archived)
    }

    /// Index of the first frozen conversation — where the "Frozen" section starts.
    /// The sort puts them directly after the live ones, so they're always on the
    /// first page: freezing is how you park work you mean to come back to, and it
    /// only works if you can still see what's parked. `None` when there are none —
    /// or when there is nothing else, since a header that sections off the whole
    /// list (the 💤 bucket) only restates the screen and every row's own marker.
    fn frozen_from(&self) -> Option<usize> {
        let at = self
            .convs
            .iter()
            .position(|c| c.is_frozen() && !c.archived)?;
        let separates = at > 0 || !self.convs.iter().all(|c| c.is_frozen() && !c.archived);
        separates.then_some(at)
    }

    /// Index of the first plain closed conversation, but only when a Frozen section
    /// precedes it — that's the one case where the closed run needs a header of its
    /// own, to mark where the frozen (pending) ones stop. Without a frozen section
    /// the closed rows just continue the list, unlabelled, as before.
    fn closed_from(&self) -> Option<usize> {
        self.frozen_from()?;
        self.convs
            .iter()
            .position(|c| !c.archived && !c.is_frozen() && !c.lifecycle.is_actionable_here())
    }

    /// How many conversations fall in each display section: `(frozen, closed)`,
    /// counting only the non-archived ones (the archived tail counts itself).
    fn section_counts(&self) -> (usize, usize) {
        let alive = self.convs.iter().filter(|c| !c.archived);
        let frozen = alive.clone().filter(|c| c.is_frozen()).count();
        let closed = alive
            .filter(|c| !c.is_frozen() && !c.lifecycle.is_actionable_here())
            .count();
        (frozen, closed)
    }

    /// The 💤 screen — the one detail that isn't a project's working set but a
    /// shelf of parked windows. It renders three-line cards instead of one-row
    /// `conv_line`s, and it never pages (see `visible_convs`).
    fn is_freeze_screen(&self) -> bool {
        self.key == FROZEN_GROUP
    }

    /// How many conversations the list shows: all of them, or the latest page.
    ///
    /// The 💤 screen is **never** capped. Paging exists so a project's config,
    /// todos and worktrees aren't pushed off by a long conversation list — but
    /// that screen has none of those competing for the space, the parked list is
    /// the entire reason to open it, and a `… N more` row would hide exactly the
    /// entry you froze in order not to forget it.
    fn visible_convs(&self) -> usize {
        if self.show_all || self.is_freeze_screen() {
            self.convs.len()
        } else {
            self.convs.len().min(BROWSE_PAGE)
        }
    }

    /// How many worktrees the list shows — same rule.
    fn visible_worktrees(&self) -> usize {
        if self.show_all {
            self.worktrees.len()
        } else {
            self.worktrees.len().min(BROWSE_PAGE)
        }
    }

    /// Total selectable rows, in cursor order: todos, then the VISIBLE
    /// conversations, then the VISIBLE worktrees — capped, so the cursor can't walk
    /// off into rows that aren't drawn.
    fn num_items(&self) -> usize {
        self.todos.len() + self.visible_convs() + self.visible_worktrees()
    }

    /// The selected todo `(session, text)` if the cursor is on a todo row.
    fn selected_todo(&self) -> Option<&(String, String)> {
        (self.sel < self.todos.len()).then(|| &self.todos[self.sel])
    }

    /// The selected conversation if the cursor is past the todos — and still within
    /// the VISIBLE conversations, since the worktrees follow them.
    fn selected_conv(&self) -> Option<&Conversation> {
        self.sel
            .checked_sub(self.todos.len())
            .filter(|i| *i < self.visible_convs())
            .and_then(|i| self.convs.get(i))
    }

    /// The selected worktree if the cursor is past the todos and conversations —
    /// the worktree list is drawn last, so it selects last.
    fn selected_worktree(&self) -> Option<&WtRow> {
        self.sel
            .checked_sub(self.todos.len() + self.visible_convs())
            .and_then(|i| self.worktrees.get(i))
    }
}

/// Where a conversation lives, for display: "🌳 Clear Session" — or
/// "🌳 Clear Session / CSD-2723" when it's in a worktree. `None` when it has no
/// resolvable parent. Used by the cross-project buckets, where every row comes
/// from somewhere different and the cwd's shared prefix says nothing.
fn project_label(c: &Conversation, projects: &ProjectRegistry) -> Option<String> {
    let parent = c.parent.as_deref()?;
    let (key, branch) = match parent.split_once('/') {
        Some((k, b)) => (k, Some(b)),
        None => (parent, None),
    };
    let config = projects.projects.get(key);
    let name = config
        .and_then(|p| p.display_name.clone())
        .unwrap_or_else(|| key.to_string());
    let emoji = config.map(|p| p.emoji.clone()).unwrap_or_default();
    let mut label = if emoji.is_empty() {
        name
    } else {
        format!("{emoji} {name}")
    };
    if let Some(b) = branch {
        label.push_str(&format!(" / {b}"));
    }
    Some(label)
}

/// True if a conversation belongs to `key` — either the project root (`parent ==
/// key`) or one of its worktrees (`parent` starts with `key/`).
fn conv_in_project(c: &Conversation, key: &str) -> bool {
    match c.parent.as_deref() {
        Some(p) => p == key || p.starts_with(&format!("{key}/")),
        None => false,
    }
}

/// Display order for a project detail's conversation list: archived last (this
/// is the ONE screen that still shows them, and they belong below the working
/// set), then live first, **frozen before plain closed**, then most-recent, then
/// id for stability. `archived_from()` / `frozen_from()` / `closed_from()` section
/// off the runs this produces.
///
/// Frozen sorts ABOVE closed because it means something different: a frozen
/// conversation is pending work you deliberately set aside to come back to, not
/// a conversation that merely ended. Sorted below the closed pile it fell off the
/// end of the capped list, so the one thing you froze *in order to remember it*
/// was the one thing you couldn't see.
fn sort_project_convs(convs: &mut [Conversation]) {
    convs.sort_by(|a, b| {
        a.archived
            .cmp(&b.archived)
            .then_with(|| {
                b.lifecycle
                    .is_actionable_here()
                    .cmp(&a.lifecycle.is_actionable_here())
            })
            .then_with(|| b.is_frozen().cmp(&a.is_frozen()))
            .then_with(|| b.last_activity.cmp(&a.last_activity))
            .then_with(|| a.id.as_str().cmp(b.id.as_str()))
    });
}

/// Build the detail sub-screen for a Browse group `key` (READ-ONLY): gather its
/// conversations, sort them for display, and (for a real project) summarize its
/// worktrees. `key` is a project key, a WORKTREE key ("project/branch" — drilled
/// into from a worktree row), the frozen bucket, or "(unassigned)".
///
/// A worktree key details the worktree itself: `conv_in_project` already matches
/// on the exact parent, so it needs no new filter — what changes is that there's
/// no nested worktree list, and the todos/header come from its own session.
fn build_group_detail(key: &str, reg: &ConversationRegistry) -> ProjectDetailState {
    let frozen_bucket = key == FROZEN_GROUP;
    let unassigned = key == "(unassigned)";
    // "project/branch" ⇒ this screen is one worktree.
    let wt_entry = key
        .split_once('/')
        .and_then(|(project, branch)| WorktreeState::load().get(project, branch).cloned());
    let mut convs: Vec<Conversation> = reg
        .conversations
        .values()
        .filter(|c| {
            if frozen_bucket {
                c.is_frozen()
            } else if unassigned {
                c.parent.is_none()
            } else {
                conv_in_project(c, key)
            }
        })
        .cloned()
        .collect();
    sort_project_convs(&mut convs);
    let path = common_prefix(&convs.iter().map(|c| c.cwd.as_str()).collect::<Vec<_>>());

    // Worktree summary rows (real projects only), with per-worktree counts. A
    // worktree detail lists none — it IS one.
    let worktrees = if frozen_bucket || unassigned || wt_entry.is_some() {
        Vec::new()
    } else {
        let wts = WorktreeState::load();
        let mut rows: Vec<WtRow> = wts
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
                    // Same recency rule as the Browse rows (`browse_worktrees`):
                    // newest conversation activity, else when it was created.
                    last_activity: convs
                        .iter()
                        .filter(of)
                        .filter_map(|c| c.last_activity.clone())
                        .max()
                        .or_else(|| (!e.created_at.is_empty()).then(|| e.created_at.clone())),
                }
            })
            .collect();
        // Live work first, then most-recently-worked-in — the list is capped to the
        // latest few, so alphabetical order would make the cap arbitrary.
        rows.sort_by(|a, b| {
            b.live
                .cmp(&a.live)
                .then_with(|| b.last_activity.cmp(&a.last_activity))
                .then_with(|| a.branch.to_lowercase().cmp(&b.branch.to_lowercase()))
        });
        rows
    };

    // Session-level todos surfaced for the project — the header badge counts these,
    // so without this the drill-in showed nothing. Gather every session the project
    // touches (its own session, each worktree session, and any session its
    // conversations run in), deduped, keep those with todos, and flatten to
    // `(session, text)` rows in session order (so the cursor can select each one).
    let todos: Vec<(String, String)> = if frozen_bucket || unassigned {
        Vec::new()
    } else {
        let all_todos = load_session_todos();
        let mut candidates: Vec<String> = Vec::new();
        if let Some(config) = ProjectRegistry::load().projects.get(key) {
            candidates.push(ProjectRegistry::session_name(key, config));
        }
        // A worktree's todos live on its own session, which is registered rather
        // than derived — and is the only session it has when nothing is running.
        if let Some(e) = &wt_entry {
            candidates.push(e.session_name.clone());
        }
        candidates.extend(worktrees.iter().map(|w| w.session.clone()));
        candidates.extend(
            convs
                .iter()
                .filter_map(|c| c.placement.as_ref().map(|p| p.session_name.clone())),
        );
        let mut seen = HashSet::new();
        candidates
            .into_iter()
            .filter(|s| seen.insert(s.clone()))
            .flat_map(|s| {
                all_todos
                    .get(&s)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(move |t| (s.clone(), t))
            })
            .collect()
    };

    ProjectDetailState {
        key: key.to_string(),
        worktrees,
        convs,
        path,
        todos,
        sel: 0,
        wt_input: None,
        archive_input: None,
        show_all: false,
        bucket: frozen_bucket || unassigned,
        wt_info: wt_entry.map(|e| WtInfo {
            project: e.project_key,
            branch: e.branch,
            path: e.path,
            session: e.session_name,
        }),
    }
}

/// Resolve the selected row to a detail sub-screen: a conversation opens its
/// project; a Browse header opens its group directly (project / frozen /
/// unassigned); an Active header (a tmux session) resolves via its conversations.
fn detail_of_row(
    row: Option<&Row>,
    view: &View,
    groups: &[Group],
    convs: &[Conversation],
    reg: &ConversationRegistry,
    projects: &ProjectRegistry,
) -> Option<ProjectDetailState> {
    match row? {
        // A plain tmux window has no conversation/project to detail, and a `… more`
        // row is a control, not a thing.
        Row::Window { .. } | Row::More { .. } => None,
        // A worktree details as its project — that screen already lists every
        // worktree with its live/frozen counts.
        Row::Wt { gi, wi } => Some(build_group_detail(&groups[*gi].worktrees[*wi].project, reg)),
        Row::Conv { ci, .. } => {
            let pkey = convs[*ci]
                .parent
                .as_deref()
                .and_then(|p| project_key_of(p, projects))?;
            Some(build_group_detail(&pkey, reg))
        }
        Row::Header(gi) => match view {
            View::Browse => Some(build_group_detail(&groups[*gi].key, reg)),
            View::Active => {
                let pkey = groups[*gi].convs.iter().find_map(|&ci| {
                    convs[ci]
                        .parent
                        .as_deref()
                        .and_then(|p| project_key_of(p, projects))
                })?;
                Some(build_group_detail(&pkey, reg))
            }
        },
    }
}

// ── Conversation detail (async-enriched) ────────────────────────────────────

/// The heavy, per-conversation data — process tree, ports, commits, transcript
/// tail — gathered OFF the UI thread so the detail screen paints instantly.
struct ConvResources {
    running: bool,
    cpu: f32,
    mem_kb: u64,
    processes: Vec<ProcessInfo>,
    ports: Vec<ListeningPort>,
    /// Chrome tabs matched to the conversation's listening ports (tab, port).
    chrome: Vec<(ChromeTab, u16)>,
    commits: Vec<String>,
    preview: Vec<jsonl::ConversationMessage>,
}

/// The resource load is `Loading` until the worker thread swaps in `Ready`.
enum ResourceState {
    Loading,
    Ready(Box<ConvResources>),
}

/// The conversation detail sub-screen: instant static header (from the registry) +
/// resources that stream in behind it.
struct ConvDetailState {
    conv: Conversation,
    res: Arc<Mutex<ResourceState>>,
    scroll: usize,
    /// The conversation's tmux session's todos (session-level; instant to load).
    todos: Vec<String>,
    /// True while typing a new todo; `input` holds the text.
    adding: bool,
    /// True while editing the conversation's note; `input` holds the text.
    editing_note: bool,
    /// True after `d` is pressed, waiting for a digit to delete that todo.
    del_armed: bool,
    input: String,
}

impl ConvDetailState {
    /// The tmux session this conversation runs in (todos are keyed by it).
    fn session(&self) -> Option<&str> {
        self.conv
            .placement
            .as_ref()
            .map(|p| p.session_name.as_str())
    }
    /// Reload the session's todos after an edit.
    fn reload_todos(&mut self) {
        self.todos = self.session().map(session_todos).unwrap_or_default();
    }
}

/// The live conversation running in the caller's current tmux window (for `--detail`).
/// Prefers an exact session+window match; falls back to any live conversation in the
/// current session (mirroring classic's session-level `--detail`) when the window index
/// can't be read or doesn't line up — e.g. a popup reporting its own window.
fn current_window_conv(reg: &ConversationRegistry) -> Option<Conversation> {
    let session = get_current_tmux_session()?;
    let in_session: Vec<&Conversation> = reg
        .conversations
        .values()
        .filter(|c| c.lifecycle.is_actionable_here())
        .filter(|c| {
            c.placement
                .as_ref()
                .is_some_and(|p| p.session_name == session)
        })
        .collect();
    if let Some(w) = get_current_tmux_window() {
        if let Some(c) = in_session
            .iter()
            .find(|c| c.placement.as_ref().is_some_and(|p| p.window_index == w))
        {
            return Some((*c).clone());
        }
    }
    in_session.first().map(|c| (*c).clone())
}

/// The project key for the caller's current tmux window (for `--project-detail`).
///
/// Two ways in, because the window you press `prefix+a` from may not be running
/// Claude at all: the conversation in this window names its parent, and failing
/// that the pane's cwd resolves the same way the registry itself groups
/// conversations (`resolve_parent` — longest path prefix, worktree beats project).
/// A worktree resolves to its PROJECT: that screen lists the worktree, its
/// siblings, and every conversation under any of them, and the worktree's own
/// detail is one `Enter` away.
fn current_window_project(
    reg: &ConversationRegistry,
    projects: &ProjectRegistry,
) -> Option<String> {
    if let Some(key) = current_window_conv(reg)
        .and_then(|c| c.parent)
        .and_then(|p| project_key_of(&p, projects))
    {
        return Some(key);
    }
    let cwd = crate::common::tmux::get_current_tmux_pane_path()?;
    let parent = crate::common::registry::resolve_parent(&cwd, &WorktreeState::load(), projects)?;
    project_key_of(&parent, projects)
}

/// Open a conversation's detail: keep the instant static data and spawn a detached
/// worker for the slow bits. The UI polls `res` each redraw and fills in when ready.
fn spawn_conv_detail(c: &Conversation) -> ConvDetailState {
    let conv = c.clone();
    let res = Arc::new(Mutex::new(ResourceState::Loading));
    let sink = res.clone();
    let target = conv.clone();
    std::thread::spawn(move || {
        let gathered = gather_conv_resources(&target);
        if let Ok(mut guard) = sink.lock() {
            *guard = ResourceState::Ready(Box::new(gathered));
        }
    });
    let todos = conv
        .placement
        .as_ref()
        .map(|p| session_todos(&p.session_name))
        .unwrap_or_default();
    ConvDetailState {
        conv,
        res,
        scroll: 0,
        todos,
        adding: false,
        editing_note: false,
        del_armed: false,
        input: String::new(),
    }
}

/// Gather the slow bits. Live conversations get a CPU/mem/process/port sample;
/// commits and the transcript tail come from disk for live AND closed ones.
fn gather_conv_resources(conv: &Conversation) -> ConvResources {
    let (running, cpu, mem_kb, processes, ports) = if conv.lifecycle.is_actionable_here() {
        gather_live_resources(conv)
    } else {
        (false, 0.0, 0, Vec::new(), Vec::new())
    };
    // Chrome tabs matched to the ports — the slow JXA call runs here (off-thread),
    // only when there are ports to match.
    let chrome = if ports.is_empty() {
        Vec::new()
    } else {
        match_tabs_to_ports(&get_chrome_tabs(), &ports)
    };
    let commits = recent_commits(&conv.cwd);
    let mut preview = jsonl::get_conversation_messages_for(&conv.cwd, Some(conv.id.as_str()));
    // Keep only the tail — the last handful of turns.
    if preview.len() > 6 {
        preview.drain(0..preview.len() - 6);
    }
    ConvResources {
        running,
        cpu,
        mem_kb,
        processes,
        ports,
        chrome,
        commits,
        preview,
    }
}

/// Live CPU/mem/process/port sample for the conversation's window. Two sysinfo
/// refreshes across a short interval give an accurate CPU delta.
fn gather_live_resources(
    conv: &Conversation,
) -> (bool, f32, u64, Vec<ProcessInfo>, Vec<ListeningPort>) {
    let mut sys = System::new_all();
    sys.refresh_all();
    std::thread::sleep(Duration::from_millis(300));
    sys.refresh_all();

    let instances = instances::detect_all_instances();
    let want_pane = conv.placement.as_ref().and_then(|p| p.pane_id.clone());
    let Some(inst) = instances.iter().find(|i| {
        i.session_id.as_deref() == Some(conv.id.as_str())
            || want_pane.as_deref() == Some(i.pane_id.as_str())
    }) else {
        return (false, 0.0, 0, Vec::new(), Vec::new());
    };

    let mut cpu = 0.0f32;
    let mut mem_kb = 0u64;
    let mut processes = Vec::new();
    for &pid in &inst.pids {
        if let Some(info) = get_process_info(&sys, pid) {
            cpu += info.cpu_percent;
            mem_kb += info.memory_kb;
            processes.push(info);
        }
    }
    processes.sort_by(|a, b| {
        b.cpu_percent
            .partial_cmp(&a.cpu_percent)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let ports = get_listening_ports_for_pids(&inst.pids, &sys);
    (true, cpu, mem_kb, processes, ports)
}

/// The last few commits in a repo (`git -C <cwd> log --oneline -10`).
fn recent_commits(cwd: &str) -> Vec<String> {
    match Command::new("git")
        .args(["-C", cwd, "log", "--oneline", "-10"])
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(|l| l.to_string())
            .collect(),
        _ => Vec::new(),
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

/// Go to a session: `switch-client` when we're inside tmux (the popup / `prefix+s`
/// case), or `exec tmux attach` when we're not (the `hive start` cold-start picker,
/// launched from a bare terminal). The attach path replaces this process, so it
/// never returns — callers must treat it as terminal.
fn attach_or_switch(session: &str) {
    if std::env::var_os("TMUX").is_some() {
        switch_to_session(session);
    } else {
        use std::os::unix::process::CommandExt;
        let tmux = crate::common::tmux::resolve_tmux_path();
        let _ = Command::new(tmux)
            .args(["attach-session", "-t", &exact(session)])
            .exec();
    }
}

fn run_conversations_tui(opts: &ConvOptions) -> Result<()> {
    // One-shot, best-effort: if `[web].autostart` is set and no server is up yet,
    // launch one (detached tmux session) so the phone dashboard is live whenever
    // hive is open. Runs here — before the loop — so it fires exactly once per TUI
    // launch, never on a refresh tick.
    crate::serve::web::ensure_web_autostart();
    let mut terminal =
        ratatui::try_init().context("hive conversations: needs a terminal (run interactively)")?;
    let action = conversations_loop(&mut terminal, opts);
    ratatui::restore();

    match action? {
        Action::Quit => {}
        Action::Switch(c) => {
            if let Some(p) = &c.placement {
                unskip_session(&p.session_name);
                if !p.window_index.is_empty() && std::env::var_os("TMUX").is_some() {
                    // Inside tmux: switch, then focus the exact window.
                    switch_to_session(&p.session_name);
                    select_window(&p.session_name, &p.window_index);
                } else {
                    // Outside tmux (`hive start`): attach (exec, never returns).
                    attach_or_switch(&p.session_name);
                }
            }
        }
        Action::Reopen(c) => {
            // Resuming into a session you're actively opening should un-skip it.
            if let Some(s) = crate::common::conversations::target_session(&c) {
                unskip_session(&s);
            }
            println!("{}", reopen(&c)?)
        }
        Action::NewInProject(key) => println!("{}", new_conversation(&key)?),
        Action::NewTask(session, prompt) => {
            unskip_session(&session);
            println!("{}", new_task_in_session(&session, &prompt)?)
        }
        Action::Spread(n) => crate::cli::session::run_spread(n)?,
        Action::Collapse => crate::cli::session::run_collapse()?,
        Action::WtNew(project, branch) => {
            // A worktree you just created is where you meant to go — land in it, the
            // same as every other "start work here" action in this view.
            let session = crate::cli::worktree::run_wt_new(
                &project, &branch, None, false, "worktree", None, false, false,
            )?;
            attach_or_switch(&session);
        }
        Action::WtDelete(project, branch) => {
            crate::cli::worktree::run_wt_delete(&project, &branch, false, false)?
        }
        Action::ConnectWorktree(project, branch) => {
            println!("{}", connect_worktree(&project, &branch)?)
        }
        Action::NewInWorktree(project, branch) => {
            println!("{}", new_conversation_in_worktree(&project, &branch)?)
        }
        Action::SwitchSession(name) => {
            unskip_session(&name);
            attach_or_switch(&name);
        }
        Action::SwitchWindow(session, window) => {
            unskip_session(&session);
            if std::env::var_os("TMUX").is_some() {
                switch_to_session(&session);
                select_window(&session, &window);
            } else {
                // `attach` shows the session's current window, so set it first — otherwise
                // a cold start (`hive start` after a reboot) lands on whichever window was
                // created last rather than the one chosen.
                select_window(&session, &window);
                attach_or_switch(&session);
            }
        }
    }
    Ok(())
}

fn conversations_loop(
    terminal: &mut ratatui::DefaultTerminal,
    opts: &ConvOptions,
) -> Result<Action> {
    // One-shot worktree name migration — shared with the classic TUI so it runs
    // whichever view is the default (this is the default view now).
    crate::common::worktree::migrate_session_names_once();
    // Kept alive across gathers so per-conversation cpu_percent is a real delta.
    let mut sys = System::new_all();
    sys.refresh_all();
    let mut reg = gather_conversations_stats(&mut sys);
    // The TUI is one of hive's two continuous observers of the live set (the web data thread
    // is the other). Syncing here keeps the recovery frame current for anyone who hasn't
    // turned on the web autostart — opening the popup is enough to top it up.
    crate::common::conversations::sync_recovery_frame(&reg);
    // Nothing live + a frame on disk is the post-reboot state, and the one moment the
    // recovery screen is what you opened hive FOR — so offer it without being asked. It only
    // ever offers: nothing reopens until you press Enter. During normal use it stays out of
    // the way behind `R`.
    let mut recover: Option<RecoverState> = if reg
        .conversations
        .values()
        .any(|c| c.lifecycle == Lifecycle::Live)
    {
        None
    } else {
        build_recover(&reg)
    };
    let mut flags = Flags::load();
    let projects = ProjectRegistry::load();
    // Default to the Active view (live conversations by session); `/` browses all.
    // `--filter`/`--picker` start in Browse + search instead (mirroring classic).
    let mut view = View::Active;
    // Reveal archived projects in Browse (Ctrl+R). A Cell so the rebuild closure
    // can read it by shared ref while other handlers still flip it.
    let reveal_archived = std::cell::Cell::new(false);

    // Switch the current view, rebuilding groups + collapse state. Loads projects
    // FRESH each Browse rebuild so archive toggles take effect immediately.
    let rebuild = |view: &View,
                   reg: &ConversationRegistry,
                   skipped: &HashSet<String>,
                   query: &str|
     -> (Vec<Group>, Vec<Conversation>, HashSet<String>) {
        match view {
            View::Active => {
                let filtered = filter_registry(reg, query, &HashSet::new());
                let session_windows = get_all_windows();
                let (g, c) = build_active(&filtered, skipped, &session_windows);
                (g, c, HashSet::new())
            }
            View::Browse => {
                // No search → a flat project list (every group collapsed to just
                // its header, empty projects included). Searching → keep only
                // matching conversations plus the projects the query NAMES, and
                // expand so hits show under their project.
                let live_projects = ProjectRegistry::load();
                let matched = projects_matching(&live_projects, query);
                let filtered = filter_registry(reg, query, &matched);
                // Worktrees come from their own registry, not from conversations, so
                // they show (and are searchable) even when nothing has run in them.
                let live_sessions: HashSet<String> =
                    get_current_tmux_session_names().into_iter().collect();
                let wt_rows =
                    browse_worktrees(&WorktreeState::load(), reg, &live_sessions, query, &matched);
                let (g, c) = build_browse(
                    &filtered,
                    &live_projects,
                    query.is_empty(),
                    reveal_archived.get(),
                    &matched,
                    &wt_rows,
                );
                let collapsed = if query.is_empty() {
                    g.iter().map(|g| g.key.clone()).collect()
                } else {
                    HashSet::new()
                };
                (g, c, collapsed)
            }
        }
    };

    let mut query = String::new();
    let mut searching = false;
    // `--filter <q>` / `--picker` open straight into Browse + search, exactly as
    // pressing `/` (then typing) would — so the classic entry points keep working.
    if opts.filter.is_some() || opts.picker {
        view = View::Browse;
        searching = true;
        if let Some(f) = &opts.filter {
            query = f.clone();
        }
    }
    // Freeze-note input: Some(target) while typing the note for a pending freeze.
    let mut freezing: Option<FreezeTarget> = None;
    let mut freeze_note = String::new();
    // Hint-jump (`f`): each visible conversation gets a 2-char label; typing one
    // activates it. `hint_labels` maps label → conv index; `hint_buffer` is the
    // partial input.
    let mut hinting = false;
    let mut hint_labels: Vec<(String, usize)> = Vec::new();
    let mut hint_buffer = String::new();
    // Live auto-refresh: re-gather on idle ticks so status/CPU stay current without
    // a keypress (the classic TUI refreshes on a 1s background timer).
    let mut last_refresh = Instant::now();
    // Transient footer message (e.g. `R` with nothing to recover). Expires on its own so it
    // never becomes permanent chrome.
    let mut notice: Option<(String, Instant)> = None;
    // Spread prompt (`L` with ≤1 iTerm pane): typing a digit spreads N sessions.
    let mut spreading = false;
    // New-project wizard (`N`): Some while stepping through key → emoji → path.
    let mut wizard: Option<NewProject> = None;
    let (mut groups, mut convs, mut collapsed) = rebuild(&view, &reg, &flags.skipped, &query);
    // Sections the user opened past the `BROWSE_PAGE` cap, keyed by (group, section)
    // rather than by index so it survives a rebuild reordering the groups. Cleared
    // whenever the query changes — a new result set is a new list.
    let mut expanded: HashSet<(String, Section)> = HashSet::new();
    let mut sel: usize = 0;
    // Parent detail screens, so drilling project → worktree pops back to the
    // project rather than all the way out to the list.
    let mut detail_stack: Vec<ProjectDetailState> = Vec::new();
    // Project detail sub-screen: Some(state) while drilled into a project.
    let mut detail: Option<ProjectDetailState> = None;
    // Conversation detail: sits ON TOP of the project detail (so backing out of a
    // conversation returns to the project it was opened from, if any).
    let mut conv_detail: Option<ConvDetailState> = None;
    // `--project-detail` (`prefix+a`): open the current window's PROJECT detail on
    // startup. Independent of `--detail` — the conversation detail draws on top of
    // this one, so passing both lands on the conversation with its project
    // underneath, and Esc pops to the project rather than out to the list.
    if opts.project_detail {
        if let Some(key) = current_window_project(&reg, &projects) {
            detail = Some(build_group_detail(&key, &reg));
        }
    }
    // `--detail` (classic `prefix+d`): open the current tmux window's conversation
    // detail on startup. Resolves via the current session + window (the popup runs
    // against the client's session, same as classic's auto-detail).
    if opts.detail {
        if let Some(c) = current_window_conv(&reg) {
            conv_detail = Some(spawn_conv_detail(&c));
        }
    }
    // Some(conv) while awaiting y/n to close (kill the window of) a live conversation.
    let mut confirm: Option<Conversation> = None;
    // Some((project, branch)) while awaiting y/n to DELETE a worktree.
    let mut wt_confirm: Option<(String, String)> = None;

    // Enter/number activation: switch to a live conversation, else reopen it.
    let activate = |c: &Conversation| -> Action {
        // Opening an archived conversation puts it back in play, so it stops being
        // archived — otherwise it would run while hidden from Browse. Mirrors
        // un-skipping a session when you switch to it.
        if c.archived {
            set_conversation_archived(c.id.as_str(), false, None);
        }
        if c.lifecycle.is_actionable_here() {
            Action::Switch(Box::new(c.clone()))
        } else {
            Action::Reopen(Box::new(c.clone()))
        }
    };

    let mut showing_help = false;

    loop {
        // ── Close confirmation ──────────────────────────────────────────────
        // Closing kills a running Claude process, so it's gated behind y/n. On
        // confirm we pop any detail screens and refresh the (now-changed) list.
        if confirm.is_some() {
            {
                let c = confirm.as_ref().unwrap();
                terminal.draw(|frame| draw_confirm_close(frame, c))?;
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
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    let _ = close_conversation(confirm.as_ref().unwrap());
                    confirm = None;
                    conv_detail = None;
                    detail = None;
                    reg = gather_conversations_stats(&mut sys);
                    flags = Flags::load();
                    (groups, convs, collapsed) = rebuild(&view, &reg, &flags.skipped, &query);
                    sel = 0;
                }
                _ => confirm = None,
            }
            continue;
        }

        // ── Worktree-delete confirmation ────────────────────────────────────
        // Deleting a worktree removes its dir, branch, and session — gated by y/n
        // and returned as an Action so the CLI teardown runs after the TUI exits.
        if let Some((project, branch)) = wt_confirm.clone() {
            terminal.draw(|frame| draw_confirm_wt_delete(frame, &project, &branch))?;
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
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    return Ok(Action::WtDelete(project, branch));
                }
                _ => wt_confirm = None,
            }
            continue;
        }

        // ── Conversation detail sub-screen ──────────────────────────────────
        // Instant static header; resources stream in from a worker thread. The
        // 250ms redraw loop paints them the moment they land. Sits above the
        // project detail: Esc pops back to wherever it was opened from.
        if conv_detail.is_some() {
            {
                let cd = conv_detail.as_ref().unwrap();
                terminal.draw(|frame| draw_conv_detail(frame, cd, &flags))?;
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
            // Add-todo input mode takes every key while active.
            if conv_detail.as_ref().unwrap().adding {
                let cd = conv_detail.as_mut().unwrap();
                match key.code {
                    KeyCode::Esc => {
                        cd.adding = false;
                        cd.input.clear();
                    }
                    KeyCode::Enter => {
                        let text = cd.input.trim().to_string();
                        let session = cd.session().map(|s| s.to_string());
                        cd.adding = false;
                        cd.input.clear();
                        if let (false, Some(s)) = (text.is_empty(), session) {
                            add_session_todo(&s, &text);
                            cd.reload_todos();
                            flags = Flags::load();
                        }
                    }
                    KeyCode::Backspace => {
                        cd.input.pop();
                    }
                    KeyCode::Char(c) => cd.input.push(c),
                    _ => {}
                }
                continue;
            }

            // Note-editing input mode: Enter saves the note (persisted overlay).
            if conv_detail.as_ref().unwrap().editing_note {
                let cd = conv_detail.as_mut().unwrap();
                match key.code {
                    KeyCode::Esc => {
                        cd.editing_note = false;
                        cd.input.clear();
                    }
                    KeyCode::Enter => {
                        let note = cd.input.trim().to_string();
                        let id = cd.conv.id.as_str().to_string();
                        cd.editing_note = false;
                        cd.input.clear();
                        set_conversation_note(&id, &note);
                        cd.conv.note = note.clone();
                        if let Some(c) = reg.conversations.get_mut(&id) {
                            c.note = note;
                        }
                        (groups, convs, collapsed) = rebuild(&view, &reg, &flags.skipped, &query);
                    }
                    KeyCode::Backspace => {
                        cd.input.pop();
                    }
                    KeyCode::Char(c) => cd.input.push(c),
                    _ => {}
                }
                continue;
            }

            // Delete-todo armed (after `d`): the next digit deletes that todo; any
            // other key cancels. Either way the key is consumed.
            if conv_detail.as_ref().unwrap().del_armed {
                let cd = conv_detail.as_mut().unwrap();
                cd.del_armed = false;
                if let KeyCode::Char(d @ '1'..='9') = key.code {
                    if let Some(s) = cd.session().map(|s| s.to_string()) {
                        delete_session_todo(&s, d as usize - '1' as usize);
                        cd.reload_todos();
                        flags = Flags::load();
                    }
                }
                continue;
            }

            match key.code {
                KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') | KeyCode::Char('q') => {
                    conv_detail = None
                }
                KeyCode::Enter => {
                    let c = conv_detail.as_ref().unwrap().conv.clone();
                    return Ok(activate(&c));
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    conv_detail.as_mut().unwrap().scroll += 1;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    let cd = conv_detail.as_mut().unwrap();
                    cd.scroll = cd.scroll.saturating_sub(1);
                }
                // Jump up to the conversation's project detail.
                KeyCode::Char('p') => {
                    let c = conv_detail.as_ref().unwrap().conv.clone();
                    if let Some(pkey) = c
                        .parent
                        .as_deref()
                        .and_then(|p| project_key_of(p, &projects))
                    {
                        detail = Some(build_group_detail(&pkey, &reg));
                        conv_detail = None;
                    }
                }
                // Del: discard a frozen conversation, or close a live one (confirm).
                KeyCode::Delete => {
                    let c = conv_detail.as_ref().unwrap().conv.clone();
                    if c.is_frozen() {
                        let _ = discard_frozen(c.id.as_str());
                        conv_detail = None;
                        reg = gather_conversations_stats(&mut sys);
                        flags = Flags::load();
                        (groups, convs, collapsed) = rebuild(&view, &reg, &flags.skipped, &query);
                        sel = 0;
                    } else if c.lifecycle.is_actionable_here() {
                        confirm = Some(c);
                    }
                }
                // `P` pins/unpins the conversation (persisted overlay); `e` edits
                // its note. Both surface a closed conversation regardless of age.
                KeyCode::Char('P') => {
                    let cd = conv_detail.as_mut().unwrap();
                    let id = cd.conv.id.as_str().to_string();
                    let pinned = !cd.conv.pinned;
                    set_conversation_pin(&id, pinned);
                    cd.conv.pinned = pinned;
                    if let Some(c) = reg.conversations.get_mut(&id) {
                        c.pinned = pinned;
                    }
                    (groups, convs, collapsed) = rebuild(&view, &reg, &flags.skipped, &query);
                }
                KeyCode::Char('e') => {
                    let cd = conv_detail.as_mut().unwrap();
                    cd.editing_note = true;
                    cd.input = cd.conv.note.clone();
                }
                // `M` — ring for THIS conversation even while muted (see the list).
                KeyCode::Char('M') => {
                    let cd = conv_detail.as_mut().unwrap();
                    let id = cd.conv.id.as_str().to_string();
                    let on = !cd.conv.notify_override;
                    set_conversation_notify_override(&id, on);
                    cd.conv.notify_override = on;
                    if let Some(c) = reg.conversations.get_mut(&id) {
                        c.notify_override = on;
                    }
                    (groups, convs, collapsed) = rebuild(&view, &reg, &flags.skipped, &query);
                }
                // Session-level flags on the conversation's session (live only), and
                // `z` freezes its window — matching the classic detail view.
                KeyCode::Char('v')
                | KeyCode::Char('m')
                | KeyCode::Char('s')
                | KeyCode::Char('!') => {
                    if let Some(session) =
                        conv_detail.as_ref().unwrap().session().map(str::to_string)
                    {
                        let flag = match key.code {
                            KeyCode::Char('v') => Flag::Favorite,
                            KeyCode::Char('m') => Flag::Mute,
                            KeyCode::Char('!') => Flag::AutoApprove,
                            _ => Flag::Skip,
                        };
                        toggle_flag(&session, flag);
                        flags = Flags::load();
                    }
                }
                KeyCode::Char('z') | KeyCode::Char('Z') => {
                    let c = conv_detail.as_ref().unwrap().conv.clone();
                    if c.lifecycle.is_actionable_here() {
                        if let Some(t) = freeze_target_of(&c) {
                            freezing = Some(t);
                            freeze_note.clear();
                            conv_detail = None; // back to the list, which shows the note prompt
                        }
                    }
                }
                // Todos (session-level): `a` add, `1-9` mark the Nth done, `d`+digit
                // deletes the Nth outright.
                KeyCode::Char('a') => {
                    let cd = conv_detail.as_mut().unwrap();
                    if cd.session().is_some() {
                        cd.adding = true;
                        cd.input.clear();
                    }
                }
                KeyCode::Char('d') => {
                    let cd = conv_detail.as_mut().unwrap();
                    cd.del_armed = cd.session().is_some() && !cd.todos.is_empty();
                }
                KeyCode::Char(d @ '1'..='9') => {
                    let cd = conv_detail.as_mut().unwrap();
                    if let Some(s) = cd.session().map(|s| s.to_string()) {
                        done_session_todo(&s, d as usize - '1' as usize);
                        cd.reload_todos();
                        flags = Flags::load();
                    }
                }
                // `o` focuses the browser tabs matching this conversation's ports.
                KeyCode::Char('o') => {
                    let matched = match &*conv_detail.as_ref().unwrap().res.lock().unwrap() {
                        ResourceState::Ready(r) => r.chrome.clone(),
                        ResourceState::Loading => Vec::new(),
                    };
                    if !matched.is_empty() {
                        let _ = focus_all_matched_tabs(&matched);
                    }
                }
                _ => {}
            }
            continue;
        }

        // ── Recovery screen ─────────────────────────────────────────────────
        // Owns the frame + input while open. Multi-select: Space/digits toggle a row,
        // Enter reopens the ticked set through `reopen_conversation`.
        if recover.is_some() {
            {
                let r = recover.as_ref().unwrap();
                terminal.draw(|frame| draw_recover(frame, r, &projects))?;
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
            let r = recover.as_mut().unwrap();
            let toggle = |r: &mut RecoverState, i: usize| {
                if let Some(c) = r.rows.get(i) {
                    let id = c.id.as_str().to_string();
                    if !r.checked.remove(&id) {
                        r.checked.insert(id);
                    }
                }
            };
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => recover = None,
                KeyCode::Down | KeyCode::Char('j') => {
                    if r.sel + 1 < r.rows.len() {
                        r.sel += 1;
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => r.sel = r.sel.saturating_sub(1),
                KeyCode::Char(' ') => {
                    let i = r.sel;
                    toggle(r, i);
                }
                KeyCode::Char('a') => {
                    r.checked = r.rows.iter().map(|c| c.id.as_str().to_string()).collect();
                }
                KeyCode::Char('n') => r.checked.clear(),
                KeyCode::Char(d @ '1'..='9') => {
                    let i = d as usize - '1' as usize;
                    if i < r.rows.len() {
                        r.sel = i;
                        toggle(r, i);
                    }
                }
                KeyCode::Enter => {
                    // Rows are already in (session, window index) order, so replaying them
                    // rebuilds the layout you left rather than an arbitrary permutation.
                    let mut errors = Vec::new();
                    let mut restored: HashSet<String> = HashSet::new();
                    let picked: Vec<Conversation> = r
                        .rows
                        .iter()
                        .filter(|c| r.checked.contains(c.id.as_str()))
                        .cloned()
                        .collect();
                    let landing_id = landing_target(&picked).map(|c| c.id.as_str().to_string());
                    let mut landing: Option<(String, String)> = None;
                    for c in &picked {
                        // No fallback session: recovery must place a conversation via its own
                        // parent, never "wherever the cursor happens to be".
                        match reopen_conversation(c, None) {
                            Ok(session) => {
                                // Capture the window NOW: `new-window` just made it the session's
                                // current one, but a later restore into the same session takes
                                // that over, so asking after the loop would land on the wrong row.
                                if landing_id.as_deref() == Some(c.id.as_str()) {
                                    landing = crate::common::tmux::display_message_for_pane(
                                        &crate::common::tmux::exact_active_pane(&session),
                                        "#{window_index}",
                                    )
                                    .map(|idx| (session.clone(), idx));
                                }
                                restored.insert(c.id.as_str().to_string());
                            }
                            Err(e) => {
                                let title = c
                                    .title
                                    .clone()
                                    .unwrap_or_else(|| c.id.as_str().chars().take(8).collect());
                                errors.push(format!("{title}: {e}"));
                            }
                        }
                    }
                    // Drop what we launched instead of re-deriving the list from a fresh
                    // gather: Claude takes seconds to come up, so an immediate re-gather
                    // still reports it Closed and the row would linger as if nothing had
                    // happened. A failed row stays put, carrying its error.
                    r.rows.retain(|c| !restored.contains(c.id.as_str()));
                    r.checked.retain(|id| !restored.contains(id));
                    r.sel = r.sel.min(r.rows.len().saturating_sub(1));
                    // A clean restore ends where every other "start work here" action does:
                    // in the work. Staying in the TUI left you with detached sessions and no
                    // client on any of them — you had to quit hive and reattach by hand.
                    // Failures keep you here instead, since the footer is the only place
                    // they're reported.
                    if errors.is_empty() {
                        if let Some((session, window)) = landing {
                            return Ok(Action::SwitchWindow(session, window));
                        }
                    }
                    r.errors = errors;
                    let done = r.rows.is_empty();
                    // Refresh the list underneath so backing out lands on current state.
                    reg = gather_conversations_stats(&mut sys);
                    flags = Flags::load();
                    (groups, convs, collapsed) = rebuild(&view, &reg, &flags.skipped, &query);
                    // The list changed shape; other handlers reset the cursor the same way.
                    sel = 0;
                    if done {
                        recover = None;
                    }
                }
                _ => {}
            }
            continue;
        }

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
            // Branch-name input for a new worktree (`w`). Enter creates it; Esc cancels.
            if detail.as_ref().unwrap().wt_input.is_some() {
                let d = detail.as_mut().unwrap();
                match key.code {
                    KeyCode::Esc => d.wt_input = None,
                    KeyCode::Enter => {
                        let branch = d.wt_input.take().unwrap_or_default().trim().to_string();
                        if !branch.is_empty() {
                            return Ok(Action::WtNew(d.key.clone(), branch));
                        }
                    }
                    KeyCode::Backspace => {
                        if let Some(s) = d.wt_input.as_mut() {
                            s.pop();
                        }
                    }
                    KeyCode::Char(c) => {
                        if let Some(s) = d.wt_input.as_mut() {
                            s.push(c);
                        }
                    }
                    _ => {}
                }
                continue;
            }
            // Archive-reason input (`A`). Enter archives with the typed reason —
            // empty is allowed (archived with no reason); Esc cancels outright.
            if detail.as_ref().unwrap().archive_input.is_some() {
                let d = detail.as_mut().unwrap();
                match key.code {
                    KeyCode::Esc => d.archive_input = None,
                    KeyCode::Enter => {
                        let (id, reason) = d.archive_input.take().unwrap();
                        set_conversation_archived(&id, true, Some(&reason));
                        reg = gather_conversations_stats(&mut sys);
                        flags = Flags::load();
                        let key = detail.as_ref().unwrap().key.clone();
                        let sel = detail.as_ref().unwrap().sel;
                        let show_all = detail.as_ref().unwrap().show_all;
                        detail = Some(build_group_detail(&key, &reg));
                        // The row just moved to the archived tail; keep the cursor
                        // in range rather than pointing past the end. `show_all`
                        // rides along so a rebuild doesn't silently re-cap the list
                        // the user just opened.
                        let d = detail.as_mut().unwrap();
                        d.show_all = show_all;
                        d.sel = sel.min(d.num_items().saturating_sub(1));
                        (groups, convs, collapsed) = rebuild(&view, &reg, &flags.skipped, &query);
                    }
                    KeyCode::Backspace => {
                        if let Some((_, s)) = d.archive_input.as_mut() {
                            s.pop();
                        }
                    }
                    KeyCode::Char(c) => {
                        if let Some((_, s)) = d.archive_input.as_mut() {
                            s.push(c);
                        }
                    }
                    _ => {}
                }
                continue;
            }
            match key.code {
                // Esc pops one level (worktree → its project → the list); q/Q leaves
                // the detail screens outright.
                KeyCode::Esc => detail = detail_stack.pop(),
                KeyCode::Char('q') | KeyCode::Char('Q') => {
                    detail_stack.clear();
                    detail = None;
                }
                // Tab: full lists ⇄ the latest `BROWSE_PAGE` of each. Folding back
                // can leave the cursor past the end, so clamp it.
                KeyCode::Tab => {
                    let d = detail.as_mut().unwrap();
                    d.show_all = !d.show_all;
                    d.sel = d.sel.min(d.num_items().saturating_sub(1));
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let d = detail.as_mut().unwrap();
                    if d.sel + 1 < d.num_items() {
                        d.sel += 1;
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    let d = detail.as_mut().unwrap();
                    d.sel = d.sel.saturating_sub(1);
                }
                // Enter: on a todo → start a `task: <todo>` conversation in its
                // session; on a conversation → switch/resume it.
                KeyCode::Enter => {
                    let d = detail.as_ref().unwrap();
                    // A worktree drills into its own detail — its conversations, its
                    // todos, its path — with the project kept underneath for Esc.
                    if let Some(w) = d.selected_worktree() {
                        let wt_key = WorktreeState::make_key(&d.key, &w.branch);
                        let child = build_group_detail(&wt_key, &reg);
                        detail_stack.push(detail.take().unwrap());
                        detail = Some(child);
                    } else if let Some((session, text)) = d.selected_todo() {
                        return Ok(Action::NewTask(session.clone(), format!("task: {text}")));
                    } else if let Some(c) = d.selected_conv() {
                        return Ok(activate(c));
                    }
                }
                // → drills in: a worktree → its detail, a conversation → its own
                // (todos have none).
                KeyCode::Right | KeyCode::Char('l') => {
                    let d = detail.as_ref().unwrap();
                    if let Some(w) = d.selected_worktree() {
                        let wt_key = WorktreeState::make_key(&d.key, &w.branch);
                        let child = build_group_detail(&wt_key, &reg);
                        detail_stack.push(detail.take().unwrap());
                        detail = Some(child);
                    } else if let Some(c) = d.selected_conv() {
                        conv_detail = Some(spawn_conv_detail(c));
                    }
                }
                // Del: on a worktree → delete it; on a conversation → discard a
                // frozen one, or close a live one (both confirmed).
                KeyCode::Delete => {
                    let d = detail.as_ref().unwrap();
                    if let Some(w) = d.selected_worktree() {
                        wt_confirm = Some((d.key.clone(), w.branch.clone()));
                    } else if let Some(c) = d.selected_conv().cloned() {
                        if c.is_frozen() {
                            let _ = discard_frozen(c.id.as_str());
                            reg = gather_conversations_stats(&mut sys);
                            flags = Flags::load();
                            let key = detail.as_ref().unwrap().key.clone();
                            let show_all = detail.as_ref().unwrap().show_all;
                            detail = Some(build_group_detail(&key, &reg));
                            detail.as_mut().unwrap().show_all = show_all;
                            (groups, convs, collapsed) =
                                rebuild(&view, &reg, &flags.skipped, &query);
                        } else if c.lifecycle.is_actionable_here() {
                            confirm = Some(c);
                        }
                    }
                }
                // Digits address the NUMBERED rows, so they stop at the cap — the
                // number you press is the number you can see.
                KeyCode::Char(dch @ '1'..='9') => {
                    let d = detail.as_ref().unwrap();
                    let n = dch as usize - '1' as usize;
                    if n < d.visible_convs() {
                        if let Some(c) = d.convs.get(n) {
                            return Ok(activate(c));
                        }
                    }
                }
                // `n` starts a fresh conversation (real projects only — the frozen
                // and unassigned buckets have none); `r` resumes the last one.
                KeyCode::Char('n') => {
                    let d = detail.as_ref().unwrap();
                    if let Some(w) = &d.wt_info {
                        return Ok(Action::NewInWorktree(w.project.clone(), w.branch.clone()));
                    }
                    if projects.projects.contains_key(&d.key) {
                        return Ok(Action::NewInProject(d.key.clone()));
                    }
                }
                KeyCode::Char('r') => {
                    if let Some(c) = detail.as_ref().unwrap().most_recent().cloned() {
                        return Ok(activate(&c));
                    }
                }
                // `m` toggles the project's remembered mute (real projects only).
                KeyCode::Char('m') => {
                    let key = detail.as_ref().unwrap().key.clone();
                    if projects.projects.contains_key(&key) {
                        toggle_muted_project(&key);
                        flags = Flags::load();
                    }
                }
                // `a` archives / unarchives the project (real projects only). An
                // archived project drops off the unfiltered Browse list but stays
                // findable by name in search — this is where you unarchive it.
                KeyCode::Char('a') => {
                    let key = detail.as_ref().unwrap().key.clone();
                    if projects.projects.contains_key(&key) {
                        toggle_archived_project(&key);
                        flags = Flags::load();
                        (groups, convs, collapsed) = rebuild(&view, &reg, &flags.skipped, &query);
                    }
                }
                // `A` archives the selected conversation (prompting for a reason)
                // or unarchives it if it already is. Archiving hides it from Browse
                // but keeps it here, under "Archived", labelled with that reason —
                // so a conversation set aside can always be explained and recovered.
                KeyCode::Char('A') => {
                    let d = detail.as_mut().unwrap();
                    if let Some(c) = d.selected_conv().cloned() {
                        if c.archived {
                            set_conversation_archived(c.id.as_str(), false, None);
                            reg = gather_conversations_stats(&mut sys);
                            flags = Flags::load();
                            let key = detail.as_ref().unwrap().key.clone();
                            let sel = detail.as_ref().unwrap().sel;
                            let show_all = detail.as_ref().unwrap().show_all;
                            detail = Some(build_group_detail(&key, &reg));
                            let d = detail.as_mut().unwrap();
                            d.show_all = show_all;
                            d.sel = sel.min(d.num_items().saturating_sub(1));
                            (groups, convs, collapsed) =
                                rebuild(&view, &reg, &flags.skipped, &query);
                        } else {
                            d.archive_input = Some((c.id.as_str().to_string(), String::new()));
                        }
                    }
                }
                // `M` overrides mute for the selected conversation — it rings even
                // while the session / project / everything is muted.
                KeyCode::Char('M') => {
                    let d = detail.as_ref().unwrap();
                    if let Some(c) = d.selected_conv() {
                        let id = c.id.as_str().to_string();
                        let on = !c.notify_override;
                        set_conversation_notify_override(&id, on);
                        if let Some(cc) = reg.conversations.get_mut(&id) {
                            cc.notify_override = on;
                        }
                        let (key, sel, show_all) = (d.key.clone(), d.sel, d.show_all);
                        detail = Some(build_group_detail(&key, &reg));
                        let d = detail.as_mut().unwrap();
                        d.show_all = show_all;
                        d.sel = sel.min(d.num_items().saturating_sub(1));
                        (groups, convs, collapsed) = rebuild(&view, &reg, &flags.skipped, &query);
                    }
                }
                // `w` creates a worktree (prompts for a branch); `x` deletes the
                // selected conversation's worktree (with a y/n confirm).
                KeyCode::Char('w') => {
                    let d = detail.as_mut().unwrap();
                    if projects.projects.contains_key(&d.key) {
                        d.wt_input = Some(String::new());
                    }
                }
                KeyCode::Char('x') => {
                    let d = detail.as_ref().unwrap();
                    if let Some(w) = d.selected_worktree() {
                        // The row under the cursor is the obvious target.
                        wt_confirm = Some((d.key.clone(), w.branch.clone()));
                    } else if let Some(w) = &d.wt_info {
                        // On a worktree's own screen, `x` deletes that worktree.
                        wt_confirm = Some((w.project.clone(), w.branch.clone()));
                    } else if let Some(branch) = d
                        // Else fall back to the selected conversation's "project/branch".
                        .selected_conv()
                        .and_then(|c| c.parent.as_deref())
                        .and_then(|p| p.split_once('/'))
                        .filter(|(proj, _)| *proj == d.key)
                        .map(|(_, br)| br.to_string())
                    {
                        wt_confirm = Some((d.key.clone(), branch));
                    }
                }
                _ => {}
            }
            continue;
        }

        // Only Browse pages its sections; Active lists every live window (see
        // `visible_rows`).
        let page = matches!(view, View::Browse).then_some(BROWSE_PAGE);
        let rows = visible_rows(&groups, &collapsed, &expanded, page);
        if sel >= rows.len() {
            sel = rows.len().saturating_sub(1);
        }
        let search = searching.then(|| query.clone());
        let freeze = freezing.as_ref().map(|_| freeze_note.clone());
        // Hint labels (conv index → label) for the current visible rows.
        let hint_map: HashMap<usize, String> = if hinting {
            hint_labels.iter().map(|(l, ci)| (*ci, l.clone())).collect()
        } else {
            HashMap::new()
        };
        let hint_buf = hinting.then(|| hint_buffer.clone());
        // New-project wizard prompt for the footer (current step's field + value).
        let wiz_prompt = wizard.as_ref().map(|w| {
            let (label, val) = match w.step {
                0 => ("key", &w.key),
                1 => ("emoji (optional)", &w.emoji),
                _ => ("path", &w.path),
            };
            format!(" new project — {label}: {val}")
        });
        let wiz_error = wizard.as_ref().and_then(|w| w.error.clone());
        terminal.draw(|frame| {
            draw(
                frame,
                &view,
                &groups,
                &convs,
                &rows,
                sel,
                showing_help,
                &flags,
                &expanded,
                search.as_deref(),
                freeze.as_deref(),
                &hint_map,
                hint_buf.as_deref(),
                spreading,
                wiz_prompt.as_deref(),
                wiz_error.as_deref(),
                // Expire the notice rather than clearing it on the next keypress: a
                // message that outlives its moment turns into permanent chrome.
                notice
                    .as_ref()
                    .filter(|(_, at)| at.elapsed() < Duration::from_secs(4))
                    .map(|(m, _)| m.as_str()),
            )
        })?;

        if !event::poll(Duration::from_millis(250))? {
            // Idle tick: refresh the registry every ~2s so status/CPU/mem stay live.
            // Skipped while a modal/input is active (would disrupt the view), and the
            // selected conversation is re-located by id so the cursor doesn't jump.
            let quiet =
                !searching && !hinting && freezing.is_none() && !spreading && wizard.is_none();
            if quiet && last_refresh.elapsed() >= Duration::from_secs(2) {
                let keep = match rows.get(sel) {
                    Some(Row::Conv { ci, .. }) => Some(convs[*ci].id.clone()),
                    _ => None,
                };
                reg = gather_conversations_stats(&mut sys);
                crate::common::conversations::sync_recovery_frame(&reg);
                flags = Flags::load();
                (groups, convs, collapsed) = rebuild(&view, &reg, &flags.skipped, &query);
                if let Some(id) = keep {
                    let new_rows = visible_rows(&groups, &collapsed, &expanded, page);
                    if let Some(i) = new_rows
                        .iter()
                        .position(|r| matches!(r, Row::Conv { ci, .. } if convs[*ci].id == id))
                    {
                        sel = i;
                    }
                }
                last_refresh = Instant::now();
            }
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

        // Hint-jump: type a label to activate its conversation. A char that extends
        // no label's prefix is ignored; Esc cancels.
        if hinting {
            match key.code {
                KeyCode::Esc => {
                    hinting = false;
                    hint_buffer.clear();
                    hint_labels.clear();
                }
                KeyCode::Char(c) => {
                    let mut cand = hint_buffer.clone();
                    cand.push(c.to_ascii_lowercase());
                    if hint_labels.iter().any(|(l, _)| l.starts_with(&cand)) {
                        hint_buffer = cand.clone();
                        if let Some((_, ci)) = hint_labels.iter().find(|(l, _)| *l == cand) {
                            return Ok(activate(&convs[*ci]));
                        }
                    }
                }
                _ => {}
            }
            continue;
        }

        // Spread prompt: a digit spreads that many sessions into iTerm2 panes.
        if spreading {
            match key.code {
                KeyCode::Char(d @ '1'..='9') => {
                    return Ok(Action::Spread(d as usize - '0' as usize));
                }
                _ => spreading = false,
            }
            continue;
        }

        // New-project wizard: step through key → emoji → path, then register it.
        if wizard.is_some() {
            match key.code {
                KeyCode::Esc => wizard = None,
                // Enter advances a step — but only when the step is actually satisfied.
                // A blank key or path holds the wizard where it is with a reason, and a
                // failed save keeps everything you typed instead of dropping it.
                KeyCode::Enter => {
                    let w = wizard.as_mut().unwrap();
                    w.error = None;
                    match w.step {
                        0 => {
                            let exists =
                                ProjectRegistry::load().projects.contains_key(w.key.trim());
                            match wizard_key_error(&w.key, exists) {
                                Some(e) => w.error = Some(e),
                                None => w.step = 1,
                            }
                        }
                        1 => w.step = 2, // emoji is optional — always advances
                        _ if wizard_path_error(&w.path).is_some() => {
                            w.error = wizard_path_error(&w.path);
                        }
                        _ => {
                            let done = wizard.take().unwrap();
                            match create_project(
                                done.key.trim(),
                                done.emoji.trim(),
                                done.path.trim(),
                            ) {
                                Ok(()) => {
                                    (groups, convs, collapsed) =
                                        rebuild(&view, &reg, &flags.skipped, &query);
                                }
                                Err(e) => {
                                    // Put it back, filled in, so the input isn't lost.
                                    wizard = Some(NewProject {
                                        error: Some(format!("save failed: {e}")),
                                        ..done
                                    });
                                }
                            }
                        }
                    }
                }
                KeyCode::Backspace => {
                    let w = wizard.as_mut().unwrap();
                    w.error = None;
                    match w.step {
                        0 => w.key.pop(),
                        1 => w.emoji.pop(),
                        _ => w.path.pop(),
                    };
                }
                KeyCode::Char(c) => {
                    let w = wizard.as_mut().unwrap();
                    w.error = None;
                    match w.step {
                        0 => w.key.push(c),
                        1 => w.emoji.push(c),
                        _ => w.path.push(c),
                    }
                }
                _ => {}
            }
            continue;
        }

        // Search input mode: type to filter; Esc exits, Enter opens the selection.
        if searching {
            match key.code {
                // Esc cancels the search and returns to the Active list.
                KeyCode::Esc => {
                    searching = false;
                    query.clear();
                    view = View::Active;
                    (groups, convs, collapsed) = rebuild(&view, &reg, &flags.skipped, &query);
                    expanded.clear();
                    sel = 0;
                }
                // Enter opens: a conversation switches/resumes; a worktree opens its
                // session (creating it if it isn't running); a project opens detail.
                KeyCode::Enter => match rows.get(sel) {
                    Some(Row::Conv { ci, .. }) => return Ok(activate(&convs[*ci])),
                    Some(Row::Wt { gi, wi }) => {
                        let w = &groups[*gi].worktrees[*wi];
                        return Ok(Action::ConnectWorktree(w.project.clone(), w.branch.clone()));
                    }
                    // `… N more` / `… show fewer`: reveal (or re-fold) the rest of
                    // that section. The cursor stays put, which lands it on the first
                    // newly-revealed row.
                    Some(Row::More { gi, section }) => {
                        let k = (groups[*gi].key.clone(), *section);
                        if !expanded.remove(&k) {
                            expanded.insert(k);
                        }
                    }
                    Some(Row::Header(_)) => {
                        if let Some(d) =
                            detail_of_row(rows.get(sel), &view, &groups, &convs, &reg, &projects)
                        {
                            detail = Some(d);
                        }
                    }
                    // Window rows are Active-only (search runs in Browse), so this is
                    // unreachable in practice — no-op keeps the match exhaustive.
                    Some(Row::Window { .. }) | None => {}
                },
                // Tab expands/collapses the project under the cursor. The unsearched
                // flat list starts fully collapsed, so this is how you get at a
                // project's worktrees and conversations without typing a query.
                KeyCode::Tab => {
                    if let Some(Row::Header(gi)) = rows.get(sel) {
                        let key = groups[*gi].key.clone();
                        if !collapsed.remove(&key) {
                            collapsed.insert(key);
                        }
                    }
                }
                // → drills in: a conversation → its detail; a project → its detail;
                // a `… N more` → expands it, same as Enter.
                KeyCode::Right => {
                    if let Some(Row::Conv { ci, .. }) = rows.get(sel) {
                        conv_detail = Some(spawn_conv_detail(&convs[*ci]));
                    } else if let Some(Row::More { gi, section }) = rows.get(sel) {
                        let k = (groups[*gi].key.clone(), *section);
                        if !expanded.remove(&k) {
                            expanded.insert(k);
                        }
                    } else if let Some(d) =
                        detail_of_row(rows.get(sel), &view, &groups, &convs, &reg, &projects)
                    {
                        detail = Some(d);
                    }
                }
                KeyCode::Down => {
                    if sel + 1 < rows.len() {
                        sel += 1;
                    }
                }
                KeyCode::Up => sel = sel.saturating_sub(1),
                // Del: on a project header → archive/unarchive it; on a conversation
                // → discard (frozen) or close (live, confirm). Browse is ALWAYS in
                // search mode (`/` sets both, only Esc clears it — and that returns
                // to Active), so this branch, not the main key match, is where every
                // Browse action has to live.
                KeyCode::Delete => match rows.get(sel) {
                    Some(Row::Header(gi)) => {
                        if let Some(pkey) = project_key_of(&groups[*gi].key, &projects) {
                            toggle_archived_project(&pkey);
                            flags = Flags::load();
                            (groups, convs, collapsed) =
                                rebuild(&view, &reg, &flags.skipped, &query);
                        }
                    }
                    Some(Row::Conv { ci, .. }) => {
                        let c = convs[*ci].clone();
                        if c.is_frozen() {
                            let _ = discard_frozen(c.id.as_str());
                            reg = gather_conversations_stats(&mut sys);
                            flags = Flags::load();
                            (groups, convs, collapsed) =
                                rebuild(&view, &reg, &flags.skipped, &query);
                        } else if c.lifecycle.is_actionable_here() {
                            confirm = Some(c);
                        }
                    }
                    // On a worktree → delete it (dir + branch + session), behind the
                    // same y/n confirm the project detail's `x` uses.
                    Some(Row::Wt { gi, wi }) => {
                        let w = &groups[*gi].worktrees[*wi];
                        wt_confirm = Some((w.project.clone(), w.branch.clone()));
                    }
                    Some(Row::Window { .. } | Row::More { .. }) | None => {}
                },
                KeyCode::Backspace => {
                    query.pop();
                    (groups, convs, collapsed) = rebuild(&view, &reg, &flags.skipped, &query);
                    expanded.clear();
                    sel = 0;
                }
                // Ctrl+<key> is a shortcut, never search text: Ctrl+R reveals/hides
                // archived projects, anything else is ignored (it used to type its
                // bare letter into the query, which is why Ctrl+R never worked).
                KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if c == 'r' {
                        reveal_archived.set(!reveal_archived.get());
                        (groups, convs, collapsed) = rebuild(&view, &reg, &flags.skipped, &query);
                        expanded.clear();
                        sel = 0;
                    }
                }
                KeyCode::Char(c) => {
                    query.push(c);
                    (groups, convs, collapsed) = rebuild(&view, &reg, &flags.skipped, &query);
                    expanded.clear();
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
                    reg = gather_conversations_stats(&mut sys);
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
            // `L` spreads sessions into iTerm2 panes, or collapses if already spread.
            KeyCode::Char('L') => {
                if crate::common::iterm::get_iterm_pane_count() > 1 {
                    return Ok(Action::Collapse);
                }
                spreading = true;
            }
            // `N` starts the new-project wizard.
            KeyCode::Char('N') => {
                wizard = Some(NewProject {
                    step: 0,
                    key: String::new(),
                    emoji: String::new(),
                    path: String::new(),
                    error: None,
                });
            }
            // Number keys 1-9 jump to the Nth visible switch target — a conversation
            // or a plain window, in display order (like classic hive).
            KeyCode::Char(d @ '1'..='9') => {
                let n = d as usize - '1' as usize;
                match rows
                    .iter()
                    .filter(|r| matches!(r, Row::Conv { .. } | Row::Window { .. }))
                    .nth(n)
                {
                    Some(Row::Conv { ci, .. }) => return Ok(activate(&convs[*ci])),
                    Some(Row::Window { gi, wi }) => {
                        let g = &groups[*gi];
                        return Ok(Action::SwitchWindow(
                            g.key.clone(),
                            g.windows[*wi].index.clone(),
                        ));
                    }
                    _ => {}
                }
            }
            // `g` labels every visible conversation for hint-jump (beyond the 1-9 cap).
            // `f` (Vimium-style) labels every visible conversation for hint-jump.
            KeyCode::Char('f') => {
                let cis: Vec<usize> = rows
                    .iter()
                    .filter_map(|r| match r {
                        Row::Conv { ci, .. } => Some(*ci),
                        _ => None,
                    })
                    .collect();
                if !cis.is_empty() {
                    hint_labels = gen_hint_labels(cis.len()).into_iter().zip(cis).collect();
                    hint_buffer.clear();
                    hinting = true;
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
            // `/` jumps straight into Browse's search box — start typing at once.
            KeyCode::Char('/') => {
                view = View::Browse;
                searching = true;
                query.clear();
                (groups, convs, collapsed) = rebuild(&view, &reg, &flags.skipped, &query);
                sel = 0;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if sel + 1 < rows.len() {
                    sel += 1;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => sel = sel.saturating_sub(1),
            // Drill in: a conversation → its own detail; a header → the project.
            KeyCode::Right | KeyCode::Char('l') => {
                if let Some(Row::Conv { ci, .. }) = rows.get(sel) {
                    conv_detail = Some(spawn_conv_detail(&convs[*ci]));
                } else if let Some(d) =
                    detail_of_row(rows.get(sel), &view, &groups, &convs, &reg, &projects)
                {
                    detail = Some(d);
                }
            }
            // ← / h backs out of Browse to Active (Active has no fold to collapse).
            KeyCode::Left | KeyCode::Char('h') if matches!(view, View::Browse) => {
                view = View::Active;
                (groups, convs, collapsed) = rebuild(&view, &reg, &flags.skipped, &query);
                sel = 0;
            }
            KeyCode::Enter => match rows.get(sel) {
                // Enter on a conversation switches/resumes it; on a plain window it
                // switches to that window; on a header it opens the project detail —
                // except a bare Active session (no live conv), where there's nothing
                // to detail, so Enter attaches to the session.
                Some(Row::Conv { ci, .. }) => return Ok(activate(&convs[*ci])),
                Some(Row::Window { gi, wi }) => {
                    let g = &groups[*gi];
                    return Ok(Action::SwitchWindow(
                        g.key.clone(),
                        g.windows[*wi].index.clone(),
                    ));
                }
                // Worktree and `… more` rows are Browse-only (which is always in
                // search mode), so these arms are unreachable in practice — kept in
                // step with the search branch's Enter so the two never drift.
                Some(Row::Wt { gi, wi }) => {
                    let w = &groups[*gi].worktrees[*wi];
                    return Ok(Action::ConnectWorktree(w.project.clone(), w.branch.clone()));
                }
                Some(Row::More { gi, section }) => {
                    let k = (groups[*gi].key.clone(), *section);
                    if !expanded.remove(&k) {
                        expanded.insert(k);
                    }
                }
                Some(Row::Header(gi)) => {
                    if matches!(view, View::Active) && groups[*gi].convs.is_empty() {
                        return Ok(Action::SwitchSession(groups[*gi].key.clone()));
                    }
                    if let Some(d) =
                        detail_of_row(rows.get(sel), &view, &groups, &convs, &reg, &projects)
                    {
                        detail = Some(d);
                    }
                }
                None => {}
            },
            KeyCode::Char('r') => {
                reg = gather_conversations_stats(&mut sys);
                flags = Flags::load();
                (groups, convs, collapsed) = rebuild(&view, &reg, &flags.skipped, &query);
                sel = 0;
            }
            // `m` on a project header (Browse) toggles that project's remembered
            // mute — matched before the per-conversation `m` below.
            KeyCode::Char('m')
                if matches!(rows.get(sel), Some(Row::Header(_)))
                    && matches!(view, View::Browse) =>
            {
                if let Some(Row::Header(gi)) = rows.get(sel) {
                    if let Some(pkey) = project_key_of(&groups[*gi].key, &projects) {
                        toggle_muted_project(&pkey);
                        flags = Flags::load();
                    }
                }
            }
            // Session-level flag toggles. Target session = the selected LIVE
            // conversation's session, OR (Active) the selected session header —
            // so a bare skipped session can be un-skipped here too.
            // `v` favorites (★) — `f` is hint-jump, mirroring Vimium.
            KeyCode::Char('v') | KeyCode::Char('m') | KeyCode::Char('s') | KeyCode::Char('!') => {
                let session = match rows.get(sel) {
                    Some(Row::Conv { ci, .. }) => convs[*ci]
                        .placement
                        .as_ref()
                        .map(|p| p.session_name.clone()),
                    // A window or session header (Active) resolves to its session —
                    // these flags are session-level, so skipping a plain window
                    // skips its whole session.
                    Some(Row::Header(gi) | Row::Window { gi, .. })
                        if matches!(view, View::Active) =>
                    {
                        Some(groups[*gi].key.clone())
                    }
                    _ => None,
                };
                if let Some(session) = session {
                    let flag = match key.code {
                        KeyCode::Char('v') => Flag::Favorite,
                        KeyCode::Char('m') => Flag::Mute,
                        KeyCode::Char('!') => Flag::AutoApprove,
                        _ => Flag::Skip,
                    };
                    toggle_flag(&session, flag);
                    flags = Flags::load();
                    // Skip changes grouping/section, so rebuild.
                    (groups, convs, collapsed) = rebuild(&view, &reg, &flags.skipped, &query);
                }
            }
            // `G` toggles GLOBAL mute (all notifications), distinct from the
            // per-session `m` above. Persisted as the shared `muted-global` flag the
            // hook notifier checks, so it affects the web too.
            KeyCode::Char('G') => set_global_mute(!is_globally_muted()),
            // `R` reopens the last frame — the windows that were open when hive last saw
            // the machine. Nothing to show is not an error; it means nothing is missing.
            KeyCode::Char('R') => {
                recover = build_recover(&reg);
                // Silence here reads as a broken key, and this is the COMMON case: during
                // normal use the frame equals the live set, so there is nothing to restore.
                if recover.is_none() {
                    notice = Some((
                        "nothing to recover — every window hive last saw is still open".to_string(),
                        Instant::now(),
                    ));
                }
            }
            // Shift-M overrides mute for the selected CONVERSATION: it keeps ringing
            // even under global / project / session mute. Pairs with the lowercase
            // `m` (mute this session) — same key, opposite direction. Per-conversation,
            // so it lives in the overlay next to pin/note rather than in any mute file.
            KeyCode::Char('M') => {
                if let Some(c) = selected_conv(&rows, &convs) {
                    let id = c.id.as_str().to_string();
                    let on = !c.notify_override;
                    set_conversation_notify_override(&id, on);
                    if let Some(cc) = reg.conversations.get_mut(&id) {
                        cc.notify_override = on;
                    }
                    (groups, convs, collapsed) = rebuild(&view, &reg, &flags.skipped, &query);
                }
            }
            // `P` pins/unpins the selected conversation (persisted overlay). A pinned
            // conversation sorts to the top of its group and survives age-bounding.
            KeyCode::Char('P') => {
                if let Some(c) = selected_conv(&rows, &convs) {
                    let id = c.id.as_str().to_string();
                    let pinned = !c.pinned;
                    set_conversation_pin(&id, pinned);
                    if let Some(cc) = reg.conversations.get_mut(&id) {
                        cc.pinned = pinned;
                    }
                    (groups, convs, collapsed) = rebuild(&view, &reg, &flags.skipped, &query);
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
            // Del: discard a frozen conversation, or close a live one (confirm).
            // (Archiving a project lives in the search branch above — Browse never
            // reaches this match — and on `a` in the project detail.)
            KeyCode::Delete => {
                if let Some(c) = selected_conv(&rows, &convs) {
                    if c.is_frozen() {
                        let _ = discard_frozen(c.id.as_str());
                        reg = gather_conversations_stats(&mut sys);
                        flags = Flags::load();
                        (groups, convs, collapsed) = rebuild(&view, &reg, &flags.skipped, &query);
                    } else if c.lifecycle.is_actionable_here() {
                        confirm = Some(c);
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
    sel: usize,
    showing_help: bool,
    flags: &Flags,
    // Sections opened past the `BROWSE_PAGE` cap — a `More` row reads it to know
    // whether it says "N more" or "show fewer".
    expanded: &HashSet<(String, Section)>,
    search: Option<&str>,
    freeze: Option<&str>,
    hints: &HashMap<usize, String>,
    hint_buf: Option<&str>,
    spread_prompt: bool,
    wiz_prompt: Option<&str>,
    wiz_error: Option<&str>,
    // Transient one-line message, shown in place of the key hints.
    notice: Option<&str>,
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
            " → detail · Enter switch · f jump · z freeze · R recover · P pin · Del close · v★ m ! s · M ring · G mute-all · / search · ? · q",
        ),
        View::Browse => (
            "projects",
            format!(
                "{} projects · {live} live · {} closed",
                groups.len(),
                total - live
            ),
            " Enter/→ open · Del archive · ^R archived · type to search · Esc back",
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
    if is_globally_muted() {
        header_spans.push(Span::raw("   "));
        header_spans.push(Span::styled("🔇 muted", Style::default().fg(Color::Yellow)));
    }
    let hint = match view {
        View::Active => "  ( / search )",
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
    // The most-recently-active conversation of each group is shown in bold — "the
    // last conversation" (what `r` in its detail would resume).
    let mut bold_convs: HashSet<usize> = HashSet::new();
    for g in groups {
        if let Some(ci) = g
            .convs
            .iter()
            .copied()
            .max_by(|&a, &b| convs[a].last_activity.cmp(&convs[b].last_activity))
        {
            bold_convs.insert(ci);
        }
    }
    let mut display: Vec<Line> = Vec::new();
    let mut sel_display = 0usize;
    // Numbers 1-9 address every switchable row in order — conversations AND windows.
    let mut switch_seen = 0usize;
    let mut shown_skip = false;
    let mut shown_other = false;
    for (ri, row) in rows.iter().enumerate() {
        if let Row::Header(gi) = row {
            let is_skip_group = flags.skipped.contains(&groups[*gi].key);
            // A bare live session (no claude, not skipped) — classic's "other" bucket.
            let is_other_group =
                matches!(view, View::Active) && !is_skip_group && groups[*gi].convs.is_empty();
            if is_skip_group && !shown_skip {
                shown_skip = true;
                if !display.is_empty() {
                    display.push(Line::raw(""));
                }
                display.push(divider("skipped"));
            } else if is_other_group && !shown_other {
                shown_other = true;
                if !display.is_empty() {
                    display.push(Line::raw(""));
                }
                display.push(divider("other"));
            } else if !display.is_empty() && matches!(view, View::Active) {
                // Blank line between groups only in Active; Browse stays dense so
                // the many project headers are scannable at a glance.
                display.push(Line::raw(""));
            }
        }
        let num = if matches!(row, Row::Conv { .. } | Row::Window { .. }) {
            switch_seen += 1;
            (switch_seen <= 9).then_some(switch_seen)
        } else {
            None
        };
        if ri == sel {
            sel_display = display.len();
        }
        let selected = ri == sel;
        // Browse headers are projects → tint green when they have live work.
        let green_head = matches!(view, View::Browse);
        display.push(match row {
            Row::Header(gi) => {
                let g = &groups[*gi];
                let skipped = flags.skipped.contains(&g.key);
                // Browse group keys ARE project keys, so these read the prefs directly.
                let is_browse = matches!(view, View::Browse);
                let muted = is_browse && flags.muted_projects.contains(&g.key);
                let archived = is_browse && flags.archived_projects.contains(&g.key);
                // Summed todo count over the group's DISTINCT sessions (todos are
                // session-level): one session in Active, possibly several in Browse.
                let mut seen = HashSet::new();
                let todos = g
                    .convs
                    .iter()
                    .filter_map(|&ci| convs[ci].placement.as_ref())
                    .filter(|p| seen.insert(p.session_name.clone()))
                    .map(|p| flags.todo_counts.get(&p.session_name).copied().unwrap_or(0))
                    .sum();
                header_line(
                    g, convs, selected, green_head, skipped, muted, archived, todos,
                )
            }
            Row::Conv { ci, gi } => conv_line(
                &convs[*ci],
                &groups[*gi].path,
                selected,
                num,
                flags,
                bold_convs.contains(ci),
                hints.get(ci).map(|s| s.as_str()),
                None,
            ),
            Row::Window { gi, wi } => window_line(&groups[*gi].windows[*wi], selected, num),
            Row::Wt { gi, wi } => wt_line(&groups[*gi].worktrees[*wi], selected),
            Row::More { gi, section } => {
                let g = &groups[*gi];
                let (total, what) = match section {
                    Section::Worktrees => (g.worktrees.len(), "worktrees"),
                    Section::Convs => (g.convs.len(), "conversations"),
                };
                more_line(
                    total.saturating_sub(BROWSE_PAGE),
                    what,
                    expanded.contains(&(g.key.clone(), *section)),
                    selected,
                )
            }
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

    // Footer: wizard/spread prompt, hint-jump, freeze-note, search query, else hints.
    let footer_line = if let Some(w) = wiz_prompt {
        let mut spans = vec![
            Span::styled(w.to_string(), Style::default().fg(Color::Green)),
            Span::styled("█", Style::default().add_modifier(Modifier::SLOW_BLINK)),
        ];
        // The reason the last Enter didn't take, in red next to the field it belongs to.
        if let Some(e) = wiz_error {
            spans.push(Span::styled(
                format!("  ⚠ {e}"),
                Style::default().fg(Color::Red),
            ));
        }
        spans.push(Span::styled(
            "   Enter next/create · Esc cancel",
            Style::default().fg(Color::DarkGray),
        ));
        Line::from(spans)
    } else if spread_prompt {
        Line::from(Span::styled(
            " spread how many sessions into panes? press 1-9 · any other key cancels",
            Style::default().fg(Color::Yellow),
        ))
    } else if let Some(buf) = hint_buf {
        Line::from(vec![
            Span::styled(" jump: ", Style::default().fg(Color::Yellow)),
            Span::styled(
                buf.to_string(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("█", Style::default().add_modifier(Modifier::SLOW_BLINK)),
            Span::styled(
                "   type a label · Esc cancel",
                Style::default().fg(Color::DarkGray),
            ),
        ])
    } else if let Some(note) = freeze {
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
                "   Enter open · Tab expand · → detail · Del close · Esc cancel",
                Style::default().fg(Color::DarkGray),
            ),
        ])
    } else if let Some(msg) = notice {
        Line::from(Span::styled(
            format!(" {msg}"),
            Style::default().fg(Color::Yellow),
        ))
    } else {
        Line::from(Span::styled(footer, Style::default().fg(Color::DarkGray)))
    };
    frame.render_widget(Paragraph::new(footer_line), chunks[2]);
}

/// Render the project detail sub-screen: config, worktrees, and the project's
/// conversations (the only navigable rows). `n` starts a new one, `r` resumes the
/// last, Enter switches/resumes the selected one.
/// The recovery screen: pick which of the last frame's windows to reopen.
///
/// Multi-select rather than all-or-nothing because restoring is not free — every row is a
/// Claude process and its context — and the set you want after a crash mid-week is rarely
/// the set you want after a clean reboot.
fn draw_recover(frame: &mut ratatui::Frame, state: &RecoverState, projects: &ProjectRegistry) {
    let area = frame.area();
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(area);

    let picked = state.checked.len();
    let mut title = vec![
        Span::styled(
            " 🕘 Last session",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("   "),
        Span::styled(
            format!("{} windows · {picked} selected", state.rows.len()),
            Style::default().add_modifier(Modifier::DIM),
        ),
    ];
    if let Some(seen) = &state.last_seen {
        title.push(Span::raw("   "));
        // "last seen", not "as of shutdown": the frame stops updating when hive dies, so
        // this is the last moment hive OBSERVED the set — which on a machine that idled
        // before going down is meaningfully earlier than the crash itself.
        title.push(Span::styled(
            format!("last seen {}", relative_time(seen)),
            Style::default().fg(Color::Blue),
        ));
    }
    title.push(Span::styled(
        "   ( Esc back )",
        Style::default().add_modifier(Modifier::DIM),
    ));
    frame.render_widget(Paragraph::new(Line::from(title)), chunks[0]);

    let mut display: Vec<Line> = Vec::new();
    display.push(Line::raw(""));
    let mut item_pos: Vec<usize> = Vec::new();

    for (i, c) in state.rows.iter().enumerate() {
        let on = state.checked.contains(c.id.as_str());
        let selected = i == state.sel;
        item_pos.push(display.len());

        // Digits only reach the first nine rows; the rest are cursor + Space.
        let num = if i < 9 {
            format!("{} ", i + 1)
        } else {
            "  ".to_string()
        };
        let label = project_label(c, projects).unwrap_or_default();
        let title_text = c
            .title
            .clone()
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| "(untitled)".to_string());

        let base = if selected {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        let body = if on {
            base
        } else {
            base.add_modifier(Modifier::DIM)
        };
        let mut spans = vec![
            Span::styled(
                format!("  {}", if on { "[x]" } else { "[ ]" }),
                if on {
                    base.fg(Color::Green)
                } else {
                    base.add_modifier(Modifier::DIM)
                },
            ),
            Span::styled(format!(" {num}"), base.add_modifier(Modifier::DIM)),
            // Padded by display CELLS — an emoji is one char but two columns, so `{:<n}`
            // would stagger every row whose project has an icon.
            Span::styled(format!("{} ", fit_cells(&label, 22)), body),
            Span::styled(format!("{} ", fit_cells(&title_text, 34)), body),
            Span::styled(abbrev_home(&c.cwd), body.add_modifier(Modifier::DIM)),
        ];
        if c.auth_config_dir.is_some() {
            spans.push(Span::styled("  [work]", body.fg(Color::Magenta)));
        }
        display.push(Line::from(spans));
    }

    let h = chunks[1].height as usize;
    let sel_display = item_pos.get(state.sel).copied().unwrap_or(0);
    let offset = if sel_display >= h {
        sel_display + 1 - h
    } else {
        0
    };
    let lines: Vec<Line> = display.into_iter().skip(offset).take(h).collect();
    frame.render_widget(Paragraph::new(lines), chunks[1]);

    // Failures replace the hint: a restore that silently did nothing is worse than one that
    // says so.
    let footer = if state.errors.is_empty() {
        Line::from(Span::styled(
            format!(" Space toggle · a all · n none · Enter recover {picked} · Esc back"),
            Style::default().add_modifier(Modifier::DIM),
        ))
    } else {
        Line::from(Span::styled(
            format!(" {}", state.errors.join(" · ")),
            Style::default().fg(Color::Red),
        ))
    };
    frame.render_widget(Paragraph::new(footer), chunks[2]);
}

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

    // A worktree detail borrows its PROJECT's config (emoji, profile, startup) —
    // its own key isn't in the registry — but shows its own branch as the name.
    let config_key = match &state.wt_info {
        Some(w) => w.project.as_str(),
        None => state.key.as_str(),
    };
    let config = projects.projects.get(config_key);
    let emoji = config.map(|c| c.emoji.clone()).unwrap_or_default();
    let project_name = config
        .and_then(|c| c.display_name.clone())
        .unwrap_or_else(|| config_key.to_string());
    let name = match &state.wt_info {
        Some(w) => format!("{project_name} / {}", w.branch),
        None => project_name,
    };
    let live = state
        .convs
        .iter()
        .filter(|c| c.lifecycle.is_actionable_here())
        .count();
    let frozen = state.convs.iter().filter(|c| c.is_frozen()).count();
    let archived = state.convs.iter().filter(|c| c.archived).count();
    let closed = state.convs.len() - live - archived;

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
        // On the 💤 screen every row is frozen, so "0 live · 6 closed" is both
        // wrong (parked isn't closed) and redundant with the 💤 badge that used
        // to follow it. One honest count instead.
        Span::styled(
            if state.key == FROZEN_GROUP {
                format!("{} parked", state.convs.len())
            } else {
                format!("{live} live · {closed} closed")
            },
            Style::default().add_modifier(Modifier::DIM),
        ),
    ];
    // Read from `flags` (reloaded on every toggle), not the `projects` registry the
    // loop loaded once at startup — otherwise `a` wouldn't update the tag.
    if flags.archived_projects.contains(&state.key) {
        title_spans.push(Span::raw("   "));
        title_spans.push(Span::styled(
            "[archived]",
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if frozen > 0 && state.key != FROZEN_GROUP {
        title_spans.push(Span::raw("   "));
        title_spans.push(Span::styled(
            format!("💤 {frozen} frozen"),
            Style::default().fg(Color::Blue),
        ));
    }
    if archived > 0 {
        title_spans.push(Span::raw("   "));
        title_spans.push(Span::styled(
            format!("{archived} archived"),
            Style::default().fg(Color::DarkGray),
        ));
    }
    if let Some(profile) = config.and_then(|c| c.auth_profile.clone()) {
        title_spans.push(Span::raw("   "));
        title_spans.push(Span::styled(
            format!("[{profile}]"),
            Style::default().fg(Color::Magenta),
        ));
    }
    if flags.muted_projects.contains(&state.key) {
        title_spans.push(Span::raw("   "));
        title_spans.push(Span::styled("🔇 muted", Style::default().fg(Color::Yellow)));
    }
    title_spans.push(Span::styled(
        "   ( Esc back )",
        Style::default().add_modifier(Modifier::DIM),
    ));
    frame.render_widget(Paragraph::new(Line::from(title_spans)), chunks[0]);

    // Body: config block, todos, the navigable conversation list, then worktrees.
    let mut display: Vec<Line> = Vec::new();
    // Display line index of each selectable item, in cursor order: todos, then
    // conversations, then worktrees — the order they're drawn. `state.sel` indexes
    // into this, so pushes here must stay in step with `num_items`/`selected_*`.
    let mut item_pos: Vec<usize> = Vec::new();
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
    if let Some(w) = &state.wt_info {
        display.push(field("path", abbrev_home(&w.path)));
        display.push(field("session", w.session.clone()));
    }
    if let Some(c) = config {
        if state.wt_info.is_none() {
            display.push(field(
                "path",
                abbrev_home(expand_tilde(&c.project_root).to_string_lossy().as_ref()),
            ));
        }
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

    // Todos (session-level) — shown here because the header badge counts them.
    // Selectable: Enter on a todo starts a `task: <todo>` conversation in its session.
    if !state.todos.is_empty() {
        display.push(Line::raw(""));
        display.push(Line::from(Span::styled(
            format!("  Todos ({})", state.todos.len()),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        let multi = state
            .todos
            .iter()
            .map(|(s, _)| s)
            .collect::<HashSet<_>>()
            .len()
            > 1;
        let mut prev: Option<&str> = None;
        for (i, (session, text)) in state.todos.iter().enumerate() {
            // Label the session only when the project spans more than one.
            if multi && prev != Some(session.as_str()) {
                display.push(Line::from(Span::styled(
                    format!("    {session}"),
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM),
                )));
                prev = Some(session.as_str());
            }
            // Todos lead the cursor order, so their index IS `sel`.
            item_pos.push(display.len());
            let selected = state.sel == i;
            let base = if selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            display.push(Line::from(vec![
                Span::styled(
                    "    ☐ ",
                    if selected {
                        base
                    } else {
                        Style::default().fg(Color::Yellow)
                    },
                ),
                Span::styled(text.clone(), base),
            ]));
        }
    }

    // The 💤 screen gets its own heading and the three-line card layout below.
    // Every other screen keeps the uniform one-row `conv_line`.
    let cards = state.is_freeze_screen();
    display.push(Line::raw(""));
    display.push(Line::from(Span::styled(
        if cards {
            format!("  Parked ({})", state.convs.len())
        } else {
            "  Conversations".to_string()
        },
        Style::default().add_modifier(Modifier::BOLD),
    )));
    if state.convs.is_empty() {
        display.push(Line::from(Span::styled(
            "    (none — press n to start one)",
            Style::default().add_modifier(Modifier::DIM),
        )));
    } else {
        // Bold "the last conversation" — the most-recent one (what `r` resumes).
        let last_id = state.most_recent().map(|c| c.id.clone());
        // Sub-headers mark where the list stops being live work. Frozen ones are
        // pending by choice and sort just under the live rows, so they need a header
        // saying so — and, once one exists, the closed run needs one too, marking
        // where "parked, come back to it" ends and "over with" begins. Archived sort
        // last: set aside, not part of the working set. This is the only screen that
        // shows archived at all (Browse hides them).
        let arch_from = state.archived_from();
        let frozen_from = state.frozen_from();
        let closed_from = state.closed_from();
        let (n_frozen, n_closed) = state.section_counts();
        // The blank separator is skipped at `i == 0` — a section starting the list
        // is already separated from the "Conversations" header above it.
        let sub_header = |i: usize, text: String, color: Color| -> Vec<Line<'static>> {
            let header = Line::from(Span::styled(
                text,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ));
            if i == 0 {
                vec![header]
            } else {
                vec![Line::raw(""), header]
            }
        };
        for (i, c) in state.convs.iter().enumerate().take(state.visible_convs()) {
            if frozen_from == Some(i) {
                display.extend(sub_header(
                    i,
                    format!("    💤 Frozen ({n_frozen})"),
                    Color::Blue,
                ));
            }
            if closed_from == Some(i) {
                display.extend(sub_header(
                    i,
                    format!("    Closed ({n_closed})"),
                    Color::Gray,
                ));
            }
            if arch_from == Some(i) {
                display.extend(sub_header(
                    i,
                    format!("    Archived ({})", state.convs.len() - i),
                    Color::DarkGray,
                ));
            }
            item_pos.push(display.len());
            let num = (i < 9).then_some(i + 1);
            let bold = last_id.as_ref() == Some(&c.id);
            // Conversations sit after the todos in the selection space.
            let selected = state.sel == state.todos.len() + i;
            // On a bucket the rows span projects, so each says where it lives.
            let project = state.bucket.then(|| project_label(c, projects)).flatten();
            if cards {
                // Blank separator *between* cards only — one above the first would
                // just double the gap under the section header.
                if i > 0 {
                    display.push(Line::raw(""));
                }
                display.extend(frozen_card(
                    c,
                    projects,
                    selected,
                    num,
                    chunks[1].width as usize,
                ));
                // Anchor the item on the card's LAST line so scrolling down to it
                // brings the whole card on screen, not just its headline (the same
                // trick `archive_reason_line` needs below).
                if let Some(reason) = archive_reason_line(c, selected) {
                    display.push(reason);
                }
                *item_pos.last_mut().unwrap() = display.len() - 1;
                continue;
            }
            display.push(conv_line(
                c,
                &state.path,
                selected,
                num,
                flags,
                bold,
                None,
                project.as_deref(),
            ));
            // The reason lives on its own indented line: it's free text, and the
            // row above is already at the width limit. Re-anchor the item to that
            // line so scrolling to the last archived row keeps its reason on screen.
            if let Some(reason) = archive_reason_line(c, selected) {
                display.push(reason);
                *item_pos.last_mut().unwrap() = display.len() - 1;
            }
        }
        // No `… N more` row on the 💤 screen — `visible_convs` already drew the
        // whole list there.
        if let Some(more) = (!cards)
            .then(|| {
                detail_more_line(
                    state.convs.len(),
                    state.visible_convs(),
                    "conversations",
                    state.show_all,
                )
            })
            .flatten()
        {
            display.push(more);
        }
    }

    // Worktrees last: they're places to go, not work in flight, so they sit below
    // the conversations rather than pushing them off the screen. Skipped entirely
    // when there are none — an empty "Worktrees (0) / none" block is pure noise on
    // a project that doesn't use them. A worktree detail IS a worktree, and a bucket
    // (frozen / unassigned) has no project to own any.
    if state.wt_info.is_none() && !state.bucket && !state.worktrees.is_empty() {
        display.push(Line::raw(""));
        display.push(Line::from(Span::styled(
            format!("  Worktrees ({})", state.worktrees.len()),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for (i, w) in state
            .worktrees
            .iter()
            .take(state.visible_worktrees())
            .enumerate()
        {
            let mut tail = format!("{} live", w.live);
            if w.frozen > 0 {
                tail.push_str(&format!(" · {} frozen", w.frozen));
            }
            // Worktrees close the cursor order, after the todos and conversations.
            item_pos.push(display.len());
            let selected = state.sel == state.todos.len() + state.visible_convs() + i;
            let sel_style = Style::default().add_modifier(Modifier::REVERSED);
            let branch_style = if selected {
                sel_style
            } else {
                Style::default().fg(if w.live > 0 {
                    Color::Green
                } else {
                    Color::Gray
                })
            };
            let dim = if selected {
                sel_style
            } else {
                Style::default().add_modifier(Modifier::DIM)
            };
            // Ellipsized to their columns: ticket branches (and the session names
            // built from them) routinely run past 40 chars, and a plain `{:<18}` let
            // them run into the next column instead of widening it.
            display.push(Line::from(vec![
                Span::styled(
                    format!("    {:<44}", ellipsize(&w.branch, 42)),
                    branch_style,
                ),
                Span::styled(format!("{:<34}", ellipsize(&w.session, 32)), dim),
                Span::styled(tail, dim),
            ]));
        }
        if let Some(more) = detail_more_line(
            state.worktrees.len(),
            state.visible_worktrees(),
            "worktrees",
            state.show_all,
        ) {
            display.push(more);
        }
    }

    // Scroll so the selected row (todo, conversation or worktree) stays visible.
    let h = chunks[1].height as usize;
    let sel_display = item_pos.get(state.sel).copied().unwrap_or(0);
    let offset = if sel_display >= h {
        sel_display + 1 - h
    } else {
        0
    };
    let lines: Vec<Line> = display.into_iter().skip(offset).take(h).collect();
    frame.render_widget(Paragraph::new(lines), chunks[1]);

    let footer_line = if let Some(branch) = &state.wt_input {
        Line::from(vec![
            Span::styled(" new worktree branch: ", Style::default().fg(Color::Green)),
            Span::styled(branch.clone(), Style::default().fg(Color::Green)),
            Span::styled("█", Style::default().add_modifier(Modifier::SLOW_BLINK)),
            Span::styled(
                "   Enter create · Esc cancel",
                Style::default().fg(Color::DarkGray),
            ),
        ])
    } else if let Some((_, reason)) = &state.archive_input {
        Line::from(vec![
            Span::styled(" archive reason: ", Style::default().fg(Color::Yellow)),
            Span::styled(reason.clone(), Style::default().fg(Color::Yellow)),
            Span::styled("█", Style::default().add_modifier(Modifier::SLOW_BLINK)),
            Span::styled(
                "   Enter archive · Esc cancel",
                Style::default().fg(Color::DarkGray),
            ),
        ])
    } else if state.bucket {
        // A bucket owns nothing: no project to mute/archive, no worktrees to make,
        // no "new conversation" target. Only the per-row actions apply. The 💤
        // screen also drops `Tab show-all` — it has nothing left to reveal.
        Line::from(Span::styled(
            if state.is_freeze_screen() {
                " Enter thaw / switch · → detail · Del discard · Esc back"
            } else {
                " Enter thaw / switch · → detail · Tab show-all · Del discard · Esc back"
            },
            Style::default().fg(Color::DarkGray),
        ))
    } else if let Some(w) = &state.wt_info {
        Line::from(Span::styled(
            format!(
                " Enter switch / start-todo · → detail · Tab show-all · Del close · n new in {} · r resume · x delete-wt · M ring · A archive-conv · Esc back",
                w.branch
            ),
            Style::default().fg(Color::DarkGray),
        ))
    } else {
        Line::from(Span::styled(
            " Enter open wt / switch conv / start-todo · → detail · Tab show-all · Del close-conv / delete-wt · n new · r resume · w worktree · x delete-wt · m mute · M ring · a archive-project · A archive-conv · Esc back",
            Style::default().fg(Color::DarkGray),
        ))
    };
    frame.render_widget(Paragraph::new(footer_line), chunks[2]);
}

/// The `… N more` footer for a capped project-detail section. `None` when nothing
/// is hidden and the list is already showing everything. Unlike the Browse list's
/// `More` row this isn't selectable — one cursor runs across every section here,
/// so there's no per-section row to hang it on — so it names the key that reveals
/// the rest.
fn detail_more_line(
    total: usize,
    shown: usize,
    what: &str,
    show_all: bool,
) -> Option<Line<'static>> {
    // Nothing to page: no line in either state (an "all 3 — Tab for the latest 5"
    // hint on a 3-item list is noise).
    if total <= BROWSE_PAGE {
        return None;
    }
    let text = if show_all {
        format!("      … all {total} {what} — Tab for the latest {BROWSE_PAGE}")
    } else {
        format!("      … {} more {what} — Tab to show all", total - shown)
    };
    Some(Line::from(Span::styled(
        text,
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM | Modifier::ITALIC),
    )))
}

/// Full-screen y/n prompt before deleting a worktree (removes dir + branch + session).
fn draw_confirm_wt_delete(frame: &mut ratatui::Frame, project: &str, branch: &str) {
    let lines = vec![
        Line::raw(""),
        Line::raw(""),
        Line::from(Span::styled(
            "  Delete this worktree?",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            format!("    {project} / {branch}"),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            "  Removes the worktree directory, its branch, and its tmux session.",
            Style::default().add_modifier(Modifier::DIM),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled(
                "  [y] delete   ",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::styled("[n] cancel", Style::default().fg(Color::DarkGray)),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines), frame.area());
}

fn mem_str(kb: u64) -> String {
    if kb >= 1024 * 1024 {
        format!("{:.1}G", kb as f64 / 1024.0 / 1024.0)
    } else if kb >= 1024 {
        format!("{:.0}M", kb as f64 / 1024.0)
    } else {
        format!("{kb}K")
    }
}

fn cpu_color(cpu: f32) -> Color {
    if cpu < 20.0 {
        Color::Green
    } else if cpu < 100.0 {
        Color::Yellow
    } else {
        Color::Red
    }
}

fn mem_color(kb: u64) -> Color {
    if kb < 512_000 {
        Color::Green
    } else if kb < 2_048_000 {
        Color::Yellow
    } else {
        Color::Red
    }
}

/// Fit `s` into exactly `cells` terminal columns: truncated with `…` when wider,
/// space-padded when narrower. Uses DISPLAY width, unlike the `{:<n}` padding used
/// for plain-ASCII columns — an emoji is two cells but one char, so char padding
/// pushes everything after it one column right on exactly the rows that have one.
fn fit_cells(s: &str, cells: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    if cells == 0 {
        return String::new();
    }
    let total = s.width();
    if total <= cells {
        return format!("{s}{}", " ".repeat(cells - total));
    }
    let mut out = String::new();
    let mut w = 0;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if w + cw > cells - 1 {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    w += 1;
    out.push_str(&" ".repeat(cells.saturating_sub(w)));
    out
}

fn ellipsize(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        format!(
            "{}…",
            s.chars().take(n.saturating_sub(1)).collect::<String>()
        )
    } else {
        s.to_string()
    }
}

/// Render the conversation detail: an instant static header, then the async
/// resources (or a loading placeholder while the worker thread runs).
fn draw_conv_detail(frame: &mut ratatui::Frame, cd: &ConvDetailState, flags: &Flags) {
    let area = frame.area();
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(area);
    let c = &cd.conv;

    // Title bar (instant). A leading 📌 marks a pinned conversation.
    let title = c.title.clone().unwrap_or_else(|| short_id(c.id.as_str()));
    let pin = if c.pinned { "📌 " } else { "" };
    let mut title_spans = vec![Span::styled(
        format!(" {pin}{title}"),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )];
    if let Some(env) = env_label(c) {
        title_spans.push(Span::styled(
            format!("   [{env}]"),
            Style::default().fg(Color::Magenta),
        ));
    }
    title_spans.push(Span::styled(
        "   ( Esc back )",
        Style::default().add_modifier(Modifier::DIM),
    ));
    frame.render_widget(Paragraph::new(Line::from(title_spans)), chunks[0]);

    // Static header fields (instant, from the registry).
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::raw(""));
    let field = |label: &str, spans: Vec<Span<'static>>| -> Line<'static> {
        let mut v = vec![Span::styled(
            format!("  {label:<9}"),
            Style::default().add_modifier(Modifier::DIM),
        )];
        v.extend(spans);
        Line::from(v)
    };
    lines.push(field("id", vec![Span::raw(short_id(c.id.as_str()))]));
    let (slabel, scolor) = status_span(c);
    let status_text = if slabel.is_empty() {
        "closed".to_string()
    } else {
        slabel.to_string()
    };
    let ago = c
        .last_activity
        .as_deref()
        .map(|t| format!("   ({})", relative_time(t)))
        .unwrap_or_default();
    lines.push(field(
        "status",
        vec![
            Span::styled(status_text, Style::default().fg(scolor)),
            Span::styled(ago, Style::default().add_modifier(Modifier::DIM)),
        ],
    ));
    if let Some(parent) = &c.parent {
        lines.push(field("project", vec![Span::raw(parent.clone())]));
    }
    let where_text = match &c.placement {
        Some(p) => {
            let wname = if p.window_name.is_empty() {
                String::new()
            } else {
                format!(" ({})", p.window_name)
            };
            format!("tmux {} · win {}{}", p.session_name, p.window_index, wname)
        }
        None => "closed — not running here".to_string(),
    };
    lines.push(field("where", vec![Span::raw(where_text)]));
    lines.push(field("cwd", vec![Span::raw(abbrev_home(&c.cwd))]));
    if !c.note.trim().is_empty() {
        lines.push(field(
            "note",
            vec![Span::styled(
                c.note.clone(),
                Style::default().fg(Color::Yellow),
            )],
        ));
    }
    if let Some(f) = &c.frozen {
        if !f.note.trim().is_empty() {
            lines.push(field(
                "frozen",
                vec![Span::styled(
                    f.note.clone(),
                    Style::default().fg(Color::Blue),
                )],
            ));
        }
    }
    // Session flags (favorite / mute / auto-approve / skip), if any are set, plus
    // the conversation's own mute override — which has no session to hang off, so
    // it shows even for a closed conversation.
    {
        let mut fs: Vec<Span> = Vec::new();
        let tag = |fs: &mut Vec<Span<'static>>, on: bool, text: &'static str, col: Color| {
            if on {
                fs.push(Span::styled(text, Style::default().fg(col)));
                fs.push(Span::raw(" "));
            }
        };
        if let Some(session) = cd.session() {
            tag(
                &mut fs,
                flags.favorite.contains(session),
                "★fav",
                Color::Yellow,
            );
            tag(
                &mut fs,
                flags.auto_approve.contains(session),
                "[auto]",
                Color::Green,
            );
            tag(
                &mut fs,
                flags.muted.contains(session),
                "[muted]",
                Color::DarkGray,
            );
            tag(
                &mut fs,
                flags.skipped.contains(session),
                "[skip]",
                Color::DarkGray,
            );
        }
        tag(
            &mut fs,
            c.notify_override,
            "🔔 notify-override",
            Color::Yellow,
        );
        if !fs.is_empty() {
            lines.push(field("flags", fs));
        }
    }

    // Todos — session-level, surfaced here (only for a conversation with a session).
    if cd.session().is_some() {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "  Todos",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        if cd.todos.is_empty() {
            lines.push(Line::from(Span::styled(
                "    (none — press a to add)",
                Style::default().add_modifier(Modifier::DIM),
            )));
        } else {
            for (i, t) in cd.todos.iter().enumerate() {
                let num = if i < 9 {
                    format!("{}", i + 1)
                } else {
                    "·".to_string()
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("    {num}. "), Style::default().fg(Color::Yellow)),
                    Span::raw(t.clone()),
                ]));
            }
        }
    }
    lines.push(Line::raw(""));

    // Async resources (or a placeholder while the worker runs).
    match &*cd.res.lock().unwrap() {
        ResourceState::Loading => lines.push(Line::from(Span::styled(
            "  ⋯ loading resources…",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ))),
        ResourceState::Ready(r) => render_conv_resources(&mut lines, r),
    }

    // Scroll for overflow.
    let h = chunks[1].height as usize;
    let max_off = lines.len().saturating_sub(h);
    let off = cd.scroll.min(max_off);
    let view: Vec<Line> = lines.into_iter().skip(off).take(h).collect();
    frame.render_widget(Paragraph::new(view), chunks[1]);

    let footer_line = if cd.adding || cd.editing_note {
        let (label, verb) = if cd.adding {
            (" + todo: ", "add")
        } else {
            (" note: ", "save")
        };
        Line::from(vec![
            Span::styled(label, Style::default().fg(Color::Yellow)),
            Span::styled(cd.input.clone(), Style::default().fg(Color::Yellow)),
            Span::styled("█", Style::default().add_modifier(Modifier::SLOW_BLINK)),
            Span::styled(
                format!("   Enter {verb} · Esc cancel"),
                Style::default().fg(Color::DarkGray),
            ),
        ])
    } else if cd.del_armed {
        Line::from(Span::styled(
            " delete which todo? press 1-9 · any other key cancels",
            Style::default().fg(Color::Red),
        ))
    } else {
        Line::from(Span::styled(
            " Enter switch · v★ m ! s · z freeze · P pin · M ring · e note · a/1-9/d todo · o chrome · Del close · Esc back",
            Style::default().fg(Color::DarkGray),
        ))
    };
    frame.render_widget(Paragraph::new(footer_line), chunks[2]);
}

/// Append the gathered resource sections (CPU/mem, processes, ports, commits,
/// transcript tail) to the detail body.
fn render_conv_resources(lines: &mut Vec<Line<'static>>, r: &ConvResources) {
    let section = |label: String| -> Line<'static> {
        Line::from(Span::styled(
            format!("  {label}"),
            Style::default().add_modifier(Modifier::BOLD),
        ))
    };

    if r.running {
        lines.push(Line::from(vec![
            Span::styled("  CPU ", Style::default().add_modifier(Modifier::DIM)),
            Span::styled(
                format!("{:.1}%", r.cpu),
                Style::default().fg(cpu_color(r.cpu)),
            ),
            Span::raw("    "),
            Span::styled("MEM ", Style::default().add_modifier(Modifier::DIM)),
            Span::styled(mem_str(r.mem_kb), Style::default().fg(mem_color(r.mem_kb))),
        ]));
    } else {
        lines.push(Line::from(Span::styled(
            "  not running here — Enter to resume",
            Style::default().add_modifier(Modifier::DIM),
        )));
    }

    if !r.processes.is_empty() {
        lines.push(Line::raw(""));
        lines.push(section("Processes".to_string()));
        for p in &r.processes {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("    PID {:>6}  ", p.pid),
                    Style::default().add_modifier(Modifier::DIM),
                ),
                Span::styled(
                    format!("{:>5.1}%  ", p.cpu_percent),
                    Style::default().fg(cpu_color(p.cpu_percent)),
                ),
                Span::styled(
                    format!("{:>6}  ", mem_str(p.memory_kb)),
                    Style::default().add_modifier(Modifier::DIM),
                ),
                Span::styled(
                    ellipsize(&p.command, 56),
                    Style::default().add_modifier(Modifier::DIM),
                ),
            ]));
        }
    }

    if !r.ports.is_empty() {
        lines.push(Line::raw(""));
        let label = if r.chrome.is_empty() {
            "Ports".to_string()
        } else {
            "Ports  (o focuses tabs)".to_string()
        };
        lines.push(section(label));
        for p in &r.ports {
            let mut spans = vec![Span::styled(
                format!("    :{}  {}", p.port, p.process_name),
                Style::default().fg(Color::Green),
            )];
            // A matching Chrome tab title, if any.
            if let Some((tab, _)) = r.chrome.iter().find(|(_, port)| *port == p.port) {
                spans.push(Span::styled(
                    format!("  → {}", ellipsize(&tab.title, 50)),
                    Style::default().fg(Color::Cyan),
                ));
            }
            lines.push(Line::from(spans));
        }
    }

    if !r.commits.is_empty() {
        lines.push(Line::raw(""));
        lines.push(section(format!("Commits ({})", r.commits.len())));
        for commit in &r.commits {
            let (hash, msg) = commit.split_once(' ').unwrap_or(("", commit.as_str()));
            lines.push(Line::from(vec![
                Span::styled(format!("    {hash} "), Style::default().fg(Color::Yellow)),
                Span::styled(
                    msg.to_string(),
                    Style::default().add_modifier(Modifier::DIM),
                ),
            ]));
        }
    }

    if !r.preview.is_empty() {
        lines.push(Line::raw(""));
        lines.push(section("Recent".to_string()));
        for m in &r.preview {
            let (glyph, col) = if m.role == "user" {
                ("›", Color::Cyan)
            } else {
                ("‹", Color::White)
            };
            let text = m.text.replace('\n', " ");
            lines.push(Line::from(vec![
                Span::styled(format!("    {glyph} "), Style::default().fg(col)),
                Span::styled(
                    ellipsize(&text, 78),
                    Style::default().add_modifier(Modifier::DIM),
                ),
            ]));
        }
    }
}

/// Full-screen y/n prompt shown before closing (killing) a live conversation.
fn draw_confirm_close(frame: &mut ratatui::Frame, c: &Conversation) {
    let title = c.title.clone().unwrap_or_else(|| short_id(c.id.as_str()));
    let where_text = match &c.placement {
        Some(p) => format!("tmux {} · win {}", p.session_name, p.window_index),
        None => "—".to_string(),
    };
    let lines = vec![
        Line::raw(""),
        Line::raw(""),
        Line::from(Span::styled(
            "  Close this conversation?",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            format!("    {title}"),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!("    {where_text}"),
            Style::default().add_modifier(Modifier::DIM),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            "  Kills its tmux window (the Claude process). History stays on disk — resumable.",
            Style::default().add_modifier(Modifier::DIM),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled(
                "  [y] close   ",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::styled("[n] cancel", Style::default().fg(Color::DarkGray)),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines), frame.area());
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
        key(
            "f",
            "Hint-jump: label every conversation, type one to switch (Vimium-style)",
        ),
        key("↑/↓ j/k", "Move selection"),
        key(
            "Enter",
            "Conversation: switch/resume · worktree: open its session · project: detail",
        ),
        key(
            "/",
            "Search projects + worktrees + conversations (type immediately to filter)",
        ),
        key(
            "Tab",
            "Browse: expand/collapse a project · project detail: full lists / latest 5",
        ),
        key(
            "Enter on … more",
            "Browse lists the latest 5 per section — reveal the rest / fold back",
        ),
        key(
            "z",
            "Freeze the selected live conversation (prompts for a note)",
        ),
        key("v / m", "Favorite ★ / mute the conversation's session"),
        key(
            "m",
            "In a project detail: mute the whole project (remembered)",
        ),
        key(
            "M",
            "Mute override 🔔 — this conversation notifies even while muted",
        ),
        key("G", "Toggle global mute (silence all notifications)"),
        key(
            "P",
            "Pin/unpin a conversation (sorts to top, survives bounding)",
        ),
        key("e", "Edit a conversation's note (in its detail)"),
        key(
            "! / s",
            "Toggle auto-approve / skip the conversation's session",
        ),
        key("L", "Spread sessions into iTerm2 panes / collapse back"),
        key("N", "New-project wizard (key → emoji → path)"),
        key("w / x", "In a project detail: create / delete a worktree"),
        key(
            "Enter on a worktree",
            "In a project detail: open that worktree's own detail (Esc pops back)",
        ),
        key("a", "In a project detail: archive / unarchive the project"),
        key(
            "A",
            "In a project detail: archive a conversation (asks why) / unarchive it",
        ),
        key(
            "→ / l",
            "Detail: a conversation's own, or a project header's",
        ),
        key(
            "Del",
            "Close a live conv (kill window) · discard a frozen · archive a project · delete a worktree",
        ),
        key(
            "Ctrl+R",
            "Browse: reveal / hide archived projects (they also match by name in search)",
        ),
        key(
            "← / h",
            "Browse → Active · in a project detail, back to the list",
        ),
        key(
            "Esc",
            "Browse → Active · Active → quit · cancel search/freeze",
        ),
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
                "   ○ closed (resumable)   💤 frozen   ❯ window (no claude)   ",
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled("skipped/archived", Style::default().fg(Color::DarkGray)),
        ]),
    ]
}

/// A group header row: the project/session key, an icon, and counts. Tinted green
/// when it has live work (`green_when_live`, i.e. Browse project rows); toned down
/// to a dim gray when its session is `skipped`, so it reads apart from active ones.
#[allow(clippy::too_many_arguments)]
fn header_line(
    g: &Group,
    convs: &[Conversation],
    selected: bool,
    green_when_live: bool,
    skipped: bool,
    muted: bool,
    archived: bool,
    todos: usize,
) -> Line<'static> {
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
    // Trailing 🔇 marks a project muted; [archived] marks a revealed archived one.
    let mute_mark = if muted { "  🔇" } else { "" };
    let arch_mark = if archived { "  [archived]" } else { "" };
    // A bare session/project (no conversations) reads cleaner as just its name.
    let text = if n == 0 {
        format!("{icon}{}{mute_mark}{arch_mark}", g.key)
    } else {
        format!("{icon}{}  ({n}, {live} live){mute_mark}{arch_mark}", g.key)
    };
    let mut style = if archived {
        // Archived (only shown when revealed): plain gray — set apart from an active
        // header (which is bold cyan/green) without DIM-on-DarkGray, which many
        // themes render nearly black and so illegible on a dark background.
        Style::default().fg(Color::Gray)
    } else if skipped {
        // Toned down from an active header, but still legible: DIM over the base
        // `Blue` came out near-black on a dark background, so dim the LIGHT blue
        // instead — same recessive intent, two steps lighter.
        Style::default()
            .fg(Color::LightBlue)
            .add_modifier(Modifier::DIM)
    } else {
        let color = if green_when_live && live > 0 {
            Color::Green
        } else {
            Color::Cyan
        };
        Style::default().fg(color).add_modifier(Modifier::BOLD)
    };
    if selected {
        style = style.add_modifier(Modifier::REVERSED);
    }
    let mut spans = vec![Span::styled(text, style)];
    // Worktree count — the flat Browse list keeps groups collapsed, so without this
    // a project's worktrees would be invisible until you expanded (Tab) or searched.
    if !g.worktrees.is_empty() {
        let running = g.worktrees.iter().filter(|w| w.session_live).count();
        let col = if selected {
            style
        } else {
            Style::default().add_modifier(Modifier::DIM)
        };
        let mut badge = format!("  {} wt", g.worktrees.len());
        if running > 0 {
            badge.push_str(&format!(", {running} up"));
        }
        spans.push(Span::styled(badge, col));
    }
    // Todo badge on the header — todos are session-level, so every conversation in
    // the group shares them; showing the summed count once here avoids repeating it.
    if todos > 0 {
        let col = if selected {
            style
        } else {
            Style::default().fg(Color::Yellow)
        };
        spans.push(Span::styled(format!("  ☑{todos}"), col));
    }
    Line::from(spans)
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

/// The indented "archived <when> — <reason>" line rendered under an archived
/// conversation in the project detail. `None` for anything not archived, so the
/// working set is unaffected. An archived conversation with no reason still gets
/// the line (with the timestamp) — the absence of a reason is itself the answer.
fn archive_reason_line(c: &Conversation, selected: bool) -> Option<Line<'static>> {
    if !c.archived {
        return None;
    }
    let when = c
        .archived_at
        .as_deref()
        .map(|t| format!(" {}", relative_time(t)))
        .unwrap_or_default();
    let reason = c.archive_reason.as_deref().unwrap_or("").trim();
    let text = if reason.is_empty() {
        format!("        archived{when} — no reason given")
    } else {
        format!("        archived{when} — {reason}")
    };
    let style = if selected {
        Style::default().add_modifier(Modifier::DIM)
    } else {
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC)
    };
    Some(Line::from(Span::styled(text, style)))
}

/// Left gutter of a frozen card: `  N  ` / `     `, the same 5 cells `conv_line`
/// spends on its favourite star + quick-jump number, so both screens' markers
/// land in the same column.
const CARD_GUTTER: usize = 5;
/// Where a card's text starts: the gutter plus the `💤 ` marker (2 cells + space).
const CARD_INDENT: usize = CARD_GUTTER + 3;

/// The note a frozen card leads with: the freeze note first (what you typed at
/// `Z` time — the reason this is parked), then the generic overlay note, and
/// failing both an explicit placeholder. The line is never dropped: a missing
/// reason is information, and keeping all three lines keeps every card the same
/// height, so the eye can scan down a column instead of re-finding it per row.
fn card_note(c: &Conversation) -> (String, bool) {
    let frozen = c.frozen.as_ref().map(|f| f.note.trim()).unwrap_or("");
    if !frozen.is_empty() {
        return (frozen.to_string(), true);
    }
    let overlay = c.note.trim();
    if !overlay.is_empty() {
        return (overlay.to_string(), true);
    }
    ("(no note)".to_string(), false)
}

/// A frozen conversation as a **three-line card**, for the `💤 frozen` screen
/// only. Every other list renders conversations through `conv_line`, which packs
/// the same fields onto one row — and on this screen that ordering is upside
/// down: the note is the last column, so at popup width (`display-popup -w 80%`,
/// ~100 cells) the terminal truncates away the one field that says *why* the
/// window is parked, which is the entire reason freeze exists.
///
/// So this list gets its own shape, reading top-down in the order you ask the
/// questions: **where it lives**, **why you parked it**, **what it was**.
///
/// ```text
///   1  💤 📊 Avateen / sos-avatar                       [work]  frozen 45m ago
///         SOS: Waiting for approval
///         Branch from experiment crisis colombia · 4a44b59a
/// ```
///
/// `width` is the body's cell width: the profile + age tail is right-aligned
/// against it and the project label is fitted to whatever is left, so the tail
/// stays readable instead of being the first thing cut.
fn frozen_card(
    c: &Conversation,
    projects: &ProjectRegistry,
    selected: bool,
    num: Option<usize>,
    width: usize,
) -> Vec<Line<'static>> {
    use unicode_width::UnicodeWidthStr;

    let head_base = if selected {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default()
    };
    let col = |c: Color| {
        if selected {
            head_base
        } else {
            Style::default().fg(c)
        }
    };

    // ── line 1: where it lives ────────────────────────────────────────────
    let num_prefix = match num {
        Some(n) => format!("{n} "),
        None => "  ".to_string(),
    };
    let profile = env_label(c).map(|e| format!("[{e}]  ")).unwrap_or_default();
    // Age is measured from the freeze, not the last message: on a parked window
    // "I set this down 35 days ago" is the number that decides whether to thaw it
    // or discard it. `last_activity` is the fallback for a synthetic entry.
    let when = c
        .frozen
        .as_ref()
        .map(|f| f.frozen_at.as_str())
        .or(c.last_activity.as_deref())
        .map(|t| format!("frozen {}", relative_time(t)))
        .unwrap_or_else(|| "frozen".to_string());
    let tail_w = profile.width() + when.width() + 1;
    let label_w = width
        .saturating_sub(CARD_INDENT + tail_w)
        .max(MIN_CARD_LABEL);
    let label = project_label(c, projects).unwrap_or_else(|| abbrev_home(&c.cwd));

    let mut head = vec![
        Span::styled(format!("  {num_prefix} "), head_base),
        Span::styled("💤 ", head_base),
        Span::styled(fit_cells(&label, label_w), col(Color::Cyan)),
    ];
    if !profile.is_empty() {
        head.push(Span::styled(profile, col(Color::Magenta)));
    }
    head.push(Span::styled(
        when,
        if selected {
            head_base
        } else {
            Style::default().add_modifier(Modifier::DIM)
        },
    ));

    // ── lines 2 & 3: why, then what ───────────────────────────────────────
    // The selected card is marked by the reversed headline plus a bar in the
    // gutter of its continuation lines — a three-row block of inverse video for
    // one selection reads as a wall, and the bar is what ties the rows together
    // as one card either way.
    let bar = || -> Span<'static> {
        if selected {
            Span::styled("  ▌     ", Style::default().fg(Color::Blue))
        } else {
            Span::raw(" ".repeat(CARD_INDENT))
        }
    };
    let body_w = width.saturating_sub(CARD_INDENT + 1);

    let (note, has_note) = card_note(c);
    let note_style = match (selected, has_note) {
        // The note is the headline of the card's meaning — brightest row of the
        // three when it exists, quiet and italic when it's the placeholder.
        (true, _) => Style::default().add_modifier(Modifier::BOLD),
        (false, true) => Style::default(),
        (false, false) => Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
    };
    let note_line = Line::from(vec![
        bar(),
        Span::styled(ellipsize(&note, body_w), note_style),
    ]);

    let title = c.title.as_deref().unwrap_or("").trim();
    let title = if title.is_empty() {
        "(untitled)"
    } else {
        title
    };
    // The id closes the card: it's what `claude --resume <id>` takes, so it's
    // worth keeping — just not in the first column, where it was.
    let id = short_id(c.id.as_str());
    let conv = ellipsize(title, body_w.saturating_sub(id.width() + 3));
    let conv_style = if selected {
        Style::default()
    } else {
        Style::default().add_modifier(Modifier::DIM)
    };
    let conv_line = Line::from(vec![
        bar(),
        Span::styled(conv, conv_style),
        Span::styled(
            format!(" · {id}"),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ),
    ]);

    vec![Line::from(head), note_line, conv_line]
}

/// Floor for the project column on a frozen card, so a narrow popup shrinks the
/// label instead of collapsing it to nothing.
const MIN_CARD_LABEL: usize = 12;

#[allow(clippy::too_many_arguments)]
fn conv_line(
    c: &Conversation,
    group_path: &str,
    selected: bool,
    num: Option<usize>,
    flags: &Flags,
    bold: bool,
    hint: Option<&str>,
    // Where this conversation lives, for a list that spans projects (the frozen
    // bucket). Replaces the subpath column — in a mixed list a shared-prefix
    // subpath is noise, and the project is the context you actually need.
    project: Option<&str>,
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
    // The subpath is dropped when a project column leads: in a cross-project list
    // the cwd's shared-prefix remainder just restates it.
    let sub_part = match project {
        Some(_) => String::new(),
        None => {
            let sub = rel_below(&c.cwd, group_path);
            if sub.is_empty() {
                String::new()
            } else {
                format!("  {sub}")
            }
        }
    };

    let skip = is_skipped(c, &flags.skipped);
    // At-a-glance color: live non-archived → blue (busy) / green (free); skipped
    // and archived → gray; closed → gray; selected → reversed highlight. The
    // most-recent conversation of a group is bold ("the last conversation").
    let mut base = if selected {
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
    if bold && !selected {
        base = base.add_modifier(Modifier::BOLD);
    }
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
    // Leading slot: the hint-jump label (highlighted) during hint mode, else the
    // 1-9 quick-jump number — both occupy the same 4-char width so rows don't shift.
    let lead = match hint {
        Some(label) => Span::styled(
            format!(" {label} "),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        None => Span::styled(format!(" {num_prefix} "), base),
    };
    // Line 1 fragment: marker · [project] · id · title (classic name column). The
    // project column only exists in a cross-project list, and leads — it's the
    // first thing you want to know about a row you can't place.
    let mut spans = vec![fav_span, lead, Span::styled(format!("{marker} "), base)];
    if let Some(p) = project {
        spans.push(Span::styled(
            fit_cells(p, 24),
            if selected {
                base
            } else {
                Style::default().fg(Color::Cyan)
            },
        ));
    }
    spans.push(Span::styled(
        format!("{:8}  {:<36}", short_id(c.id.as_str()), title),
        base,
    ));

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
    // Compact live CPU/mem (only when the window's process tree reports usage).
    if c.lifecycle.is_actionable_here() && (c.cpu >= 0.05 || c.mem_kb > 0) {
        let col = |co| {
            if selected {
                base
            } else {
                Style::default().fg(co)
            }
        };
        spans.push(Span::styled(
            format!("  {:.0}%", c.cpu),
            col(cpu_color(c.cpu)),
        ));
        spans.push(Span::styled(
            format!(" {}", mem_str(c.mem_kb)),
            col(mem_color(c.mem_kb)),
        ));
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
    spans.extend(tag(c.archived, "  [archived]", Color::DarkGray));
    // The mute override — shown next to [muted] because that's the pair that
    // explains itself: silenced session, but this conversation still rings.
    if c.notify_override {
        let col = if selected {
            base
        } else {
            Style::default().fg(Color::Yellow)
        };
        spans.push(Span::styled("  🔔", col));
    }
    // Overlay: a pin marker and the free-text note (persisted per-conversation).
    if c.pinned {
        let col = if selected {
            base
        } else {
            Style::default().fg(Color::Yellow)
        };
        spans.push(Span::styled("  📌", col));
    }
    let ov_note = c.note.trim();
    if !ov_note.is_empty() {
        spans.push(Span::styled(
            format!("  📝 {}", ov_note.chars().take(40).collect::<String>()),
            dim(base),
        ));
    }
    Line::from(spans)
}

/// A non-Claude tmux window row: a dimmed `❯` line (shell prompt glyph) aligned
/// under the session's conversations, with a trailing `window` tag so it reads
/// clearly as a plain tmux window, not a Claude conversation.
fn window_line(w: &WinRow, selected: bool, num: Option<usize>) -> Line<'static> {
    let num_prefix = match num {
        Some(n) => format!("{n} "),
        None => "  ".to_string(),
    };
    // Dim gray throughout — deliberately quieter than a colored conversation row.
    let base = if selected {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM)
    };
    let name = if w.name.is_empty() {
        "(window)".to_string()
    } else {
        w.name.chars().take(36).collect::<String>()
    };
    Line::from(vec![
        // Blank favorite slot + 1-9 lead, matching conv_line's leading columns so
        // window rows line up under the conversations.
        Span::styled(" ", base),
        Span::styled(format!(" {num_prefix} "), base),
        Span::styled(format!("❯ {:8}  {:<36}", w.index, name), base),
        Span::styled("  window", base),
    ])
}

/// The `… N more worktrees` / `… show fewer` row that ends a capped section.
/// Deliberately quiet (dim, italic) — it's a control, not an item.
fn more_line(hidden: usize, what: &str, expanded: bool, selected: bool) -> Line<'static> {
    let base = if selected {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM | Modifier::ITALIC)
    };
    let text = if expanded {
        format!("      … show fewer {what}")
    } else {
        format!("      … {hidden} more {what}")
    };
    Line::from(Span::styled(text, base))
}

/// A registered worktree row (Browse): the branch, a `worktree` tag, and how much
/// is running there. Uses the conversation rows' ●/○ vocabulary for "its session is
/// up / not", and greens a running one so the eye lands on live work first.
fn wt_line(w: &BrowseWt, selected: bool) -> Line<'static> {
    let base = if selected {
        Style::default().add_modifier(Modifier::REVERSED)
    } else if w.session_live {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::Gray).add_modifier(Modifier::DIM)
    };
    let dim = if selected {
        base
    } else {
        Style::default().add_modifier(Modifier::DIM)
    };
    let marker = if w.session_live { '●' } else { '○' };
    let branch = ellipsize(&w.branch, 46);
    let mut spans = vec![
        // Blank favorite + 1-9 slots so worktrees align with the conversation rows.
        Span::styled("  ", base),
        Span::styled(format!("  {marker} {branch:<46}"), base),
        Span::styled("  worktree", dim),
    ];
    if w.live > 0 {
        let col = if selected {
            base
        } else {
            Style::default().fg(Color::Green)
        };
        spans.push(Span::styled(format!("  {} live", w.live), col));
    }
    Line::from(spans)
}

/// The tmux session that owns this conversation's project/worktree, if resolvable
/// from its logical `parent` key.
/// Reopen a closed conversation via the shared [`crate::common::conversations`]
/// core (resume in its project/worktree session under its auth profile, creating
/// the session if needed), then switch this tmux client to it. Falls back to the
/// current session when the conversation has no resolvable parent.
fn reopen(c: &Conversation) -> Result<String> {
    let frozen = c.is_frozen();
    let session = crate::common::conversations::reopen_conversation(c, get_current_tmux_session())?;
    attach_or_switch(&session);
    let verb = if frozen { "Thawed" } else { "Reopened" };
    Ok(format!("{verb} {} in {session}", short_id(c.id.as_str())))
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
    // Starting a conversation here means the project is active again.
    activate_project(key);
    let session = ProjectRegistry::session_name(key, config);
    let root = expand_tilde(&config.project_root)
        .to_string_lossy()
        .into_owned();
    open_new_conversation(&session, &root, &config.tmux_env())
}

/// Start a fresh conversation in a WORKTREE: a new `claude` window in its session
/// (created at the worktree path under the project's auth env if it isn't running).
/// Same shape as [`new_conversation`], keyed off the worktree registry instead of
/// the project one — a worktree's session name is registered, not derived.
fn new_conversation_in_worktree(project: &str, branch: &str) -> Result<String> {
    let wts = WorktreeState::load();
    let entry = wts
        .get(project, branch)
        .ok_or_else(|| anyhow!("unknown worktree '{project}/{branch}'"))?;
    let env = ProjectRegistry::load()
        .projects
        .get(project)
        .map(|c| c.tmux_env())
        .unwrap_or_default();
    activate_project(project);
    open_new_conversation(&entry.session_name, &entry.path, &env)
}

/// The shared half: open a `claude` window in `session` at `cwd`, creating the
/// session if needed, then switch to it.
fn open_new_conversation(session: &str, cwd: &str, env: &[(String, String)]) -> Result<String> {
    // Exact target throughout: `-t "📊 Avateen"` prefix-matches "📊 Avateen Hub", which
    // put the new window (and the switch) in the wrong project's session.
    let target = exact(session);
    let alive = Command::new("tmux")
        .args(["has-session", "-t", &target])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if alive {
        let mut cmd = Command::new("tmux");
        cmd.args(["new-window", "-t", &target, "-c", cwd]);
        for (k, v) in env {
            cmd.arg("-e").arg(format!("{k}={v}"));
        }
        if !cmd.output().map(|o| o.status.success()).unwrap_or(false) {
            return Err(anyhow!("failed to open a new window in '{session}'"));
        }
        // Session target for new-window, ACTIVE-PANE target for send-keys: `=name`
        // is not a pane and send-keys would fail, leaving a bare shell.
        let _ = Command::new("tmux")
            .args([
                "send-keys",
                "-t",
                &exact_active_pane(session),
                "claude",
                "Enter",
            ])
            .output();
    } else if !ensure_tmux_session(session, cwd, Some("claude"), env) {
        return Err(anyhow!("failed to create session '{session}'"));
    }

    attach_or_switch(session);
    Ok(format!("New conversation in {session}"))
}

/// Open a registered worktree: switch to its tmux session, creating it at the
/// worktree path (with the project's startup command and auth env, exactly as
/// `hive wt new` would) when it isn't running. The worktree directory, branch, and
/// hooks are untouched — this only (re)creates the session you jump into, which is
/// what makes a worktree with no live conversation a usable Browse row.
fn connect_worktree(project: &str, branch: &str) -> Result<String> {
    let wts = WorktreeState::load();
    let entry = wts
        .get(project, branch)
        .ok_or_else(|| anyhow!("unknown worktree '{project}/{branch}'"))?;
    let session = entry.session_name.clone();

    let alive = Command::new("tmux")
        .args(["has-session", "-t", &exact(&session)])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !alive {
        let projects = ProjectRegistry::load();
        let config = projects.projects.get(project);
        let startup = config.and_then(|c| c.startup_command.as_deref());
        let env = config.map(|c| c.tmux_env()).unwrap_or_default();
        if !ensure_tmux_session(&session, &entry.path, startup, &env) {
            return Err(anyhow!("failed to create session '{session}'"));
        }
    }
    // Opening a worktree is an explicit choice to work there — put it back in the
    // cycle, same as switching to a conversation does, and put its project back on
    // the Browse list (the session's startup command is usually `claude`).
    unskip_session(&session);
    activate_project(project);
    attach_or_switch(&session);
    Ok(format!(
        "{} {session}",
        if alive { "Switched to" } else { "Started" }
    ))
}

/// POSIX single-quote a string so it survives being typed onto a shell command line
/// (todos can contain apostrophes, colons, `$`, …): wrap in `'…'` and rewrite each
/// embedded `'` as `'\''`.
fn sh_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Resolve a tmux session name back to a `(cwd, env)` for creating a window in it —
/// via the project registry (project session) or the worktree registry (worktree
/// session, inheriting the parent project's env). None if it matches neither.
fn resolve_session_target(session: &str) -> Option<(String, Vec<(String, String)>)> {
    let projects = ProjectRegistry::load();
    for (key, config) in &projects.projects {
        if ProjectRegistry::session_name(key, config) == session {
            let cwd = expand_tilde(&config.project_root)
                .to_string_lossy()
                .into_owned();
            return Some((cwd, config.tmux_env()));
        }
    }
    let wts = WorktreeState::load();
    if let Some(e) = wts.worktrees.values().find(|e| e.session_name == session) {
        let env = projects
            .projects
            .get(&e.project_key)
            .map(|c| c.tmux_env())
            .unwrap_or_default();
        return Some((e.path.clone(), env));
    }
    None
}

/// Start a fresh conversation in `session` with an initial `prompt` (`claude "…"`).
/// Opens a new window if the session is alive, else recreates it from the project /
/// worktree registry. Then switches to it.
fn new_task_in_session(session: &str, prompt: &str) -> Result<String> {
    let startup = format!("claude {}", sh_quote(prompt));
    let target = resolve_session_target(session);
    let tmux_target = exact(session);
    let alive = Command::new("tmux")
        .args(["has-session", "-t", &tmux_target])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if alive {
        let mut cmd = Command::new("tmux");
        cmd.args(["new-window", "-t", &tmux_target]);
        if let Some((cwd, env)) = &target {
            cmd.args(["-c", cwd]);
            for (k, v) in env {
                cmd.arg("-e").arg(format!("{k}={v}"));
            }
        }
        if !cmd.output().map(|o| o.status.success()).unwrap_or(false) {
            return Err(anyhow!("failed to open a new window in '{session}'"));
        }
        let _ = Command::new("tmux")
            .args([
                "send-keys",
                "-t",
                &exact_active_pane(session),
                &startup,
                "Enter",
            ])
            .output();
    } else {
        let (cwd, env) = target.ok_or_else(|| {
            anyhow!("session '{session}' isn't alive and matches no known project/worktree")
        })?;
        if !ensure_tmux_session(session, &cwd, Some(&startup), &env) {
            return Err(anyhow!("failed to create session '{session}'"));
        }
    }

    attach_or_switch(session);
    Ok(format!("Started task in {session} — {startup}"))
}

/// Close a live conversation: kill its tmux window (freeing the Claude process).
/// The window's the unit — sibling conversations in the session are untouched;
/// if it was the last window, tmux drops the session. History stays on disk, so
/// the conversation becomes Closed and is resumable.
fn close_conversation(conv: &Conversation) -> Result<String> {
    let p = conv
        .placement
        .as_ref()
        .ok_or_else(|| anyhow!("not running here"))?;
    let target = if p.window_index.is_empty() {
        exact(&p.session_name)
    } else {
        exact_window(&p.session_name, &p.window_index)
    };
    let ok = Command::new("tmux")
        .args(["kill-window", "-t", &target])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if ok {
        Ok(format!("Closed {}", short_id(conv.id.as_str())))
    } else {
        Err(anyhow!("failed to close window '{target}'"))
    }
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
    // Archived conversations are hidden here for the same reason they're hidden
    // in Browse — this is a default listing. They stay visible in project detail.
    let listed = || {
        reg.conversations
            .values()
            .filter(|c| !c.archived || c.lifecycle.is_actionable_here())
    };
    let total = listed().count();
    let live = listed()
        .filter(|s| s.lifecycle.is_actionable_here())
        .count();
    let closed = total - live;
    let frozen = listed().filter(|c| c.is_frozen()).count();

    // Frozen conversations are enumerated first under a pinned "💤 frozen" header;
    // the rest are grouped by parent (BTreeMap gives stable ordering).
    let mut groups: BTreeMap<String, Vec<&Conversation>> = BTreeMap::new();
    let mut frozen_group: Vec<&Conversation> = Vec::new();
    for s in listed() {
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

    // The `N` wizard used to discard everything in silence when a required field was
    // blank — an empty Enter looked exactly like a successful create. Each step now
    // reports why it won't advance.
    #[test]
    fn wizard_key_step_requires_a_key() {
        assert_eq!(
            wizard_key_error("", false).as_deref(),
            Some("key is required")
        );
        assert_eq!(
            wizard_key_error("   ", false).as_deref(),
            Some("key is required")
        );
        assert!(wizard_key_error("avateen", false).is_none());
    }

    // Re-adding a registered key would overwrite its auth profile / ports / worktrees
    // dir with defaults, so the wizard refuses rather than clobbering.
    #[test]
    fn wizard_key_step_rejects_duplicates() {
        assert_eq!(
            wizard_key_error("avateen", true).as_deref(),
            Some("project 'avateen' already exists")
        );
        // The reported key is trimmed — it's what would actually be written.
        assert_eq!(
            wizard_key_error("  avateen  ", true).as_deref(),
            Some("project 'avateen' already exists")
        );
    }

    #[test]
    fn wizard_path_step_requires_a_path() {
        assert_eq!(wizard_path_error("").as_deref(), Some("path is required"));
        assert_eq!(
            wizard_path_error("  \t ").as_deref(),
            Some("path is required")
        );
        assert!(wizard_path_error("~/Projects/avateen").is_none());
    }

    /// Build a `session_windows` map (session → its `(index, name)` windows) for
    /// `build_active`, from a compact literal.
    fn win_map(pairs: &[(&str, &[(&str, &str)])]) -> HashMap<String, Vec<(String, String)>> {
        pairs
            .iter()
            .map(|(s, ws)| {
                let ws = ws
                    .iter()
                    .map(|(i, n)| (i.to_string(), n.to_string()))
                    .collect();
                (s.to_string(), ws)
            })
            .collect()
    }

    #[test]
    fn test_hint_labels_unique_stable_2char() {
        let labels = gen_hint_labels(40);
        assert_eq!(labels.len(), 40);
        assert!(labels.iter().all(|l| l.chars().count() == 2));
        let set: std::collections::HashSet<&String> = labels.iter().collect();
        assert_eq!(set.len(), labels.len(), "labels must be unique");
        assert_eq!(&labels[0..3], &["aa", "as", "ad"]); // stable order
    }

    #[test]
    fn test_sh_quote() {
        assert_eq!(sh_quote("task: fix scrolling"), "'task: fix scrolling'");
        // An apostrophe is broken out and backslash-escaped so the shell rejoins it.
        assert_eq!(sh_quote("it's a $test"), "'it'\\''s a $test'");
        assert_eq!(sh_quote(""), "''");
    }

    #[test]
    fn test_build_active_surfaces_bare_live_sessions() {
        // Every live tmux session shows as a bare (0-conv) group for parity with
        // classic's session-first list — skipped ones and plain shells alike.
        let reg = ConversationRegistry::default();
        let one = |s: &str| -> HashSet<String> { [s.to_string()].into_iter().collect() };

        let (groups, convs) = build_active(
            &reg,
            &one("🌳 Bare"),
            &win_map(&[("🌳 Bare", &[("0", "bash")])]),
        );
        let bare = groups.iter().find(|g| g.key == "🌳 Bare");
        assert!(bare.is_some(), "bare skipped shows");
        assert!(convs.is_empty(), "bare session has no conversations");
        assert_eq!(bare.unwrap().windows.len(), 1, "its window shows as a row");

        // A non-skipped bare live session (e.g. `00-main`, a plain shell) now shows
        // too — it belongs to the trailing "other" bucket, not hidden.
        let (groups, _) = build_active(
            &reg,
            &HashSet::new(),
            &win_map(&[("00-main", &[("0", "zsh")])]),
        );
        assert!(
            groups.iter().any(|g| g.key == "00-main"),
            "non-skipped bare live session shows in the other bucket"
        );
    }

    #[test]
    fn test_build_active_orders_normal_other_skipped() {
        // Section order matches classic: claude groups, then bare "other", then skipped.
        use crate::common::registry::TmuxPlacement;
        let mut reg = ConversationRegistry::default();
        let mut live = mk(
            "live",
            Lifecycle::Live,
            Some("hive"),
            Some("2026-07-01T00:00:00Z"),
        );
        live.placement = Some(TmuxPlacement {
            session_name: "🐝 claude".to_string(),
            window_index: "0".to_string(),
            window_name: String::new(),
            pane_id: None,
        });
        reg.conversations.insert("live".into(), live);
        // 🐝 claude has one window (index 0) that its conversation occupies; 00-other
        // is a plain two-window session; 🌳 skip is skipped.
        let session_windows = win_map(&[
            ("🐝 claude", &[("0", "claude")]),
            ("00-other", &[("0", "bash"), ("1", "server")]),
            ("🌳 skip", &[("0", "bash")]),
        ]);
        let skipped: HashSet<String> = ["🌳 skip"].into_iter().map(String::from).collect();
        let (groups, _) = build_active(&reg, &skipped, &session_windows);
        let keys: Vec<&str> = groups.iter().map(|g| g.key.as_str()).collect();
        assert_eq!(keys, vec!["🐝 claude", "00-other", "🌳 skip"]);
        // The claude window is covered by its conversation → no window row for it.
        let claude = groups.iter().find(|g| g.key == "🐝 claude").unwrap();
        assert!(
            claude.windows.is_empty(),
            "the claude window is covered by its conversation"
        );
        // The plain session surfaces both its windows as rows.
        let other = groups.iter().find(|g| g.key == "00-other").unwrap();
        assert_eq!(other.windows.len(), 2, "both plain windows show");
    }

    #[test]
    fn test_build_browse_archived_hidden_on_list_shown_on_search() {
        // Matches the classic picker: archived projects hide on the full list but
        // reappear during a search (or when revealed).
        let mut reg = ConversationRegistry::default();
        reg.conversations.insert(
            "a".into(),
            mk(
                "a",
                Lifecycle::Closed,
                Some("arch"),
                Some("2026-07-01T00:00:00Z"),
            ),
        );
        let mut projects = ProjectRegistry::default();
        projects.projects.insert(
            "arch".into(),
            ProjectConfig {
                archived: true,
                ..Default::default()
            },
        );
        let has = |g: &[Group], k: &str| g.iter().any(|x| x.key == k);
        let none = HashSet::new();
        let no_wts = HashMap::new();

        // Full list (include_empty=true), not revealed → hidden.
        let (g, _) = build_browse(&reg, &projects, true, false, &none, &no_wts);
        assert!(!has(&g, "arch"), "archived hidden on the full list");
        // Search (include_empty=false), not revealed → findable.
        let (g, _) = build_browse(&reg, &projects, false, false, &none, &no_wts);
        assert!(has(&g, "arch"), "archived findable during search");
        // Revealed → shown regardless.
        let (g, _) = build_browse(&reg, &projects, true, true, &none, &no_wts);
        assert!(has(&g, "arch"), "archived shown when revealed");
    }

    #[test]
    fn test_build_browse_shows_archived_project_with_a_live_conversation() {
        // You can't hide something that's running — the project-level twin of
        // `test_build_browse_shows_archived_conversation_that_is_live`. Starting a
        // conversation in an archived project (e.g. plain `claude` in a tmux window,
        // which bypasses every unarchive-on-start hook) must not make it invisible.
        let mut projects = ProjectRegistry::default();
        projects.projects.insert(
            "arch".into(),
            ProjectConfig {
                archived: true,
                ..Default::default()
            },
        );
        let none = HashSet::new();
        let mut wts: HashMap<String, Vec<BrowseWt>> = HashMap::new();
        wts.insert(
            "arch".into(),
            vec![BrowseWt {
                project: "arch".into(),
                branch: "b".into(),
                session_live: false,
                live: 0,
                last_activity: None,
            }],
        );
        let has = |g: &[Group], k: &str| g.iter().any(|x| x.key == k);

        // Closed conversation only → still hidden on the full list.
        let mut reg = ConversationRegistry::default();
        reg.conversations.insert(
            "a".into(),
            mk("a", Lifecycle::Closed, Some("arch"), Some("2026-07-01")),
        );
        let (g, _) = build_browse(&reg, &projects, true, false, &none, &wts);
        assert!(!has(&g, "arch"), "no live work → archived stays hidden");

        // One live conversation → the project (and its worktrees) come back.
        reg.conversations.insert(
            "b".into(),
            mk("b", Lifecycle::Live, Some("arch"), Some("2026-07-02")),
        );
        let (g, _) = build_browse(&reg, &projects, true, false, &none, &wts);
        let group = g
            .iter()
            .find(|x| x.key == "arch")
            .expect("archived project with a live conversation shows on the full list");
        assert_eq!(group.worktrees.len(), 1, "its worktrees come with it");
    }

    #[test]
    fn test_build_browse_surfaces_name_matched_empty_projects() {
        // The regression that made archiving a one-way door: a project whose
        // conversations aged out of the registry (or never existed) was dropped from
        // every search, and archiving hides it from the full list — so it could not
        // be reached at all. A project the query NAMES must surface regardless, and
        // rank above incidental conversation hits.
        let mut reg = ConversationRegistry::default();
        reg.conversations.insert(
            "other".into(),
            mk(
                "other",
                Lifecycle::Closed,
                Some("busy"),
                Some("2026-07-01T00:00:00Z"),
            ),
        );
        let mut projects = ProjectRegistry::default();
        projects.projects.insert(
            "arch".into(),
            ProjectConfig {
                archived: true,
                ..Default::default()
            },
        );
        projects
            .projects
            .insert("busy".into(), ProjectConfig::default());
        let matched: HashSet<String> = ["arch"].into_iter().map(String::from).collect();

        // Searching (include_empty=false) with "arch" named: it shows despite having
        // zero conversations and being archived, ahead of the conversation hit.
        let (g, _) = build_browse(&reg, &projects, false, false, &matched, &HashMap::new());
        let keys: Vec<&str> = g.iter().map(|x| x.key.as_str()).collect();
        assert_eq!(keys, vec!["arch", "busy"], "named project first");
    }

    /// A `WorktreeState` from `(project, branch, session, path)` tuples.
    fn wt_state(entries: &[(&str, &str, &str, &str)]) -> WorktreeState {
        use crate::common::worktree::WorktreeEntry;
        let mut state = WorktreeState::default();
        for (project, branch, session, path) in entries {
            state.add(WorktreeEntry {
                project_key: project.to_string(),
                branch: branch.to_string(),
                session_name: session.to_string(),
                path: path.to_string(),
                worktree_type: "worktree".to_string(),
                metadata: serde_json::Value::Null,
                created_at: String::new(),
            });
        }
        state
    }

    /// Build a group with `n_wt` worktrees and `n_conv` conversations, plus the
    /// `convs` backing store `visible_rows` indexes into.
    fn paged_group(key: &str, n_wt: usize, n_conv: usize) -> (Vec<Group>, Vec<Conversation>) {
        let convs: Vec<Conversation> = (0..n_conv)
            .map(|i| mk(&format!("c{i}"), Lifecycle::Closed, Some(key), None))
            .collect();
        let worktrees = (0..n_wt)
            .map(|i| BrowseWt {
                project: key.to_string(),
                branch: format!("br{i}"),
                session_live: false,
                live: 0,
                last_activity: None,
            })
            .collect();
        let group = Group {
            key: key.to_string(),
            convs: (0..n_conv).collect(),
            path: String::new(),
            emoji: String::new(),
            windows: Vec::new(),
            worktrees,
        };
        (vec![group], convs)
    }

    #[test]
    fn test_visible_rows_pages_sections_behind_a_more_row() {
        let (groups, _) = paged_group("cs", 26, 29);
        let no_collapse = HashSet::new();
        let mut expanded = HashSet::new();
        let count = |rows: &[Row], f: fn(&Row) -> bool| rows.iter().filter(|r| f(r)).count();
        let wts = |r: &Row| matches!(r, Row::Wt { .. });
        let cvs = |r: &Row| matches!(r, Row::Conv { .. });
        let more = |r: &Row| matches!(r, Row::More { .. });

        // Browse pages both sections: 5 + 5 rows, one More row each.
        let rows = visible_rows(&groups, &no_collapse, &expanded, Some(BROWSE_PAGE));
        assert_eq!(count(&rows, wts), 5);
        assert_eq!(count(&rows, cvs), 5);
        assert_eq!(count(&rows, more), 2);
        // The More row ends its own section, so the cursor lands on the first
        // revealed item when it expands in place.
        assert!(matches!(
            rows[6],
            Row::More {
                section: Section::Worktrees,
                ..
            }
        ));

        // Expanding one section leaves the other capped, and keeps its More row so
        // it can be folded back.
        expanded.insert(("cs".to_string(), Section::Worktrees));
        let rows = visible_rows(&groups, &no_collapse, &expanded, Some(BROWSE_PAGE));
        assert_eq!(count(&rows, wts), 26);
        assert_eq!(count(&rows, cvs), 5);
        assert_eq!(count(&rows, more), 2, "both More rows stay");

        // Active (page: None) never hides a row — a live window you can't see is a
        // window you can't get back to.
        let rows = visible_rows(&groups, &no_collapse, &HashSet::new(), None);
        assert_eq!(count(&rows, wts), 26);
        assert_eq!(count(&rows, cvs), 29);
        assert_eq!(count(&rows, more), 0);

        // A section at or under the cap gets no More row at all.
        let (small, _) = paged_group("tiny", 2, 5);
        let rows = visible_rows(&small, &no_collapse, &HashSet::new(), Some(BROWSE_PAGE));
        assert_eq!(count(&rows, more), 0);

        // A collapsed group shows only its header.
        let collapsed: HashSet<String> = ["cs".to_string()].into_iter().collect();
        let rows = visible_rows(&groups, &collapsed, &expanded, Some(BROWSE_PAGE));
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn test_browse_worktrees_orders_by_recency() {
        // "Latest 5" has to mean latest: running first, then most-recent activity,
        // with creation time standing in for a worktree nothing has run in yet.
        let mut wts = wt_state(&[
            ("cs", "stale", "s-stale", "/w/stale"),
            ("cs", "recent", "s-recent", "/w/recent"),
            ("cs", "fresh-empty", "s-fresh", "/w/fresh"),
            ("cs", "running", "s-running", "/w/running"),
        ]);
        // A brand-new worktree with no conversations, created after the others ran.
        wts.worktrees.get_mut("cs/fresh-empty").unwrap().created_at =
            "2026-07-20T00:00:00Z".to_string();
        let mut reg = ConversationRegistry::default();
        for (id, branch, when) in [
            ("a", "stale", "2026-01-01T00:00:00Z"),
            ("b", "recent", "2026-07-25T00:00:00Z"),
            ("c", "running", "2026-02-01T00:00:00Z"),
        ] {
            reg.conversations.insert(
                id.into(),
                mk(
                    id,
                    Lifecycle::Closed,
                    Some(&format!("cs/{branch}")),
                    Some(when),
                ),
            );
        }
        let live: HashSet<String> = ["s-running"].into_iter().map(String::from).collect();
        let rows = browse_worktrees(&wts, &reg, &live, "", &HashSet::new());
        let order: Vec<&str> = rows["cs"].iter().map(|w| w.branch.as_str()).collect();
        assert_eq!(order, vec!["running", "recent", "fresh-empty", "stale"]);
    }

    #[test]
    fn test_browse_worktrees_filters_by_branch_session_and_path() {
        let wts = wt_state(&[
            ("cs", "CSD-2723-dashboard", "🌳 [cs] CSD-2723", "/w/cs/2723"),
            ("cs", "upgrade-versions", "🌳 [cs] upgrade", "/w/cs/upgrade"),
            ("dio", "tactics-assets", "🏔️ [dio] tactics", "/w/dio/assets"),
        ]);
        let reg = ConversationRegistry::default();
        let live: HashSet<String> = ["🌳 [cs] CSD-2723"].into_iter().map(String::from).collect();
        let none = HashSet::new();
        let branches = |m: &HashMap<String, Vec<BrowseWt>>, k: &str| -> Vec<String> {
            m.get(k)
                .map(|v| v.iter().map(|w| w.branch.clone()).collect())
                .unwrap_or_default()
        };

        // No query → every registered worktree, running sessions first.
        let all = browse_worktrees(&wts, &reg, &live, "", &none);
        assert_eq!(
            branches(&all, "cs"),
            vec!["CSD-2723-dashboard", "upgrade-versions"],
            "the running worktree sorts first"
        );
        assert!(all["cs"][0].session_live && !all["cs"][1].session_live);

        // Branch text — the case that returned nothing before: a worktree with no
        // conversations at all was unreachable from Browse.
        let hit = browse_worktrees(&wts, &reg, &live, "tactics-assets", &none);
        assert_eq!(branches(&hit, "dio"), vec!["tactics-assets"]);
        assert!(!hit.contains_key("cs"), "non-matching projects drop out");

        // Session name and path match too.
        assert_eq!(
            branches(&browse_worktrees(&wts, &reg, &live, "🏔️", &none), "dio"),
            vec!["tactics-assets"]
        );
        assert_eq!(
            branches(
                &browse_worktrees(&wts, &reg, &live, "/w/cs/upgrade", &none),
                "cs"
            ),
            vec!["upgrade-versions"]
        );

        // Naming the PROJECT brings all of its worktrees, matching how conversations
        // follow their project.
        let named: HashSet<String> = ["cs"].into_iter().map(String::from).collect();
        assert_eq!(
            browse_worktrees(&wts, &reg, &live, "zzz", &named)["cs"].len(),
            2
        );
    }

    #[test]
    fn test_browse_worktrees_counts_live_conversations() {
        let wts = wt_state(&[("cs", "CSD-1", "🌳 [cs] CSD-1", "/w/cs/1")]);
        let mut reg = ConversationRegistry::default();
        reg.conversations
            .insert("a".into(), mk("a", Lifecycle::Live, Some("cs/CSD-1"), None));
        reg.conversations.insert(
            "b".into(),
            mk("b", Lifecycle::Closed, Some("cs/CSD-1"), None),
        );
        // A conversation of the project root, not this worktree — must not count.
        reg.conversations
            .insert("c".into(), mk("c", Lifecycle::Live, Some("cs"), None));
        let rows = browse_worktrees(&wts, &reg, &HashSet::new(), "", &HashSet::new());
        assert_eq!(rows["cs"][0].live, 1);
    }

    #[test]
    fn test_build_browse_surfaces_projects_for_their_worktrees() {
        // The worktree analog of the name-match fix: a project whose only match is a
        // branch has to surface — with zero conversations of its own — or that
        // worktree can't be reached from `/` at all.
        let reg = ConversationRegistry::default();
        let mut projects = ProjectRegistry::default();
        projects
            .projects
            .insert("dio".into(), ProjectConfig::default());
        projects.projects.insert(
            "arch".into(),
            ProjectConfig {
                archived: true,
                ..Default::default()
            },
        );
        let wt = |project: &str, branch: &str| BrowseWt {
            project: project.to_string(),
            branch: branch.to_string(),
            session_live: false,
            live: 0,
            last_activity: None,
        };
        let matched_wts: HashMap<String, Vec<BrowseWt>> =
            [("dio".to_string(), vec![wt("dio", "tactics-assets")])]
                .into_iter()
                .collect();
        let none = HashSet::new();

        let (g, _) = build_browse(&reg, &projects, false, false, &none, &matched_wts);
        let dio = g.iter().find(|x| x.key == "dio").expect("project surfaces");
        assert_eq!(dio.worktrees.len(), 1, "its worktree rides along as a row");

        // On the full list an archived project stays hidden even though it has
        // worktrees — the archived filter still wins there.
        let arch_wts: HashMap<String, Vec<BrowseWt>> =
            [("arch".to_string(), vec![wt("arch", "old-branch")])]
                .into_iter()
                .collect();
        let (g, _) = build_browse(&reg, &projects, true, false, &none, &arch_wts);
        assert!(!g.iter().any(|x| x.key == "arch"), "archived stays hidden");
        // …but a search finds it through its branch.
        let (g, _) = build_browse(&reg, &projects, false, false, &none, &arch_wts);
        assert!(
            g.iter().any(|x| x.key == "arch"),
            "findable via its worktree"
        );
    }

    #[test]
    fn test_projects_matching() {
        let mut projects = ProjectRegistry::default();
        projects
            .projects
            .insert("spotify".into(), ProjectConfig::default());
        projects.projects.insert(
            "cs".into(),
            ProjectConfig {
                display_name: Some("Clear Session".into()),
                ..Default::default()
            },
        );
        let m = |q: &str| {
            let mut v: Vec<String> = projects_matching(&projects, q).into_iter().collect();
            v.sort();
            v
        };
        assert_eq!(m("spot"), vec!["spotify"], "matches the key");
        assert_eq!(m("SPOT"), vec!["spotify"], "case-insensitive");
        assert_eq!(m("clear"), vec!["cs"], "matches the display name");
        assert!(m("").is_empty(), "empty query matches nothing");
        assert!(m("zzz").is_empty());
    }

    fn open_window(sid: &str, session: &str, idx: &str) -> crate::common::activity::OpenWindow {
        crate::common::activity::OpenWindow {
            session_name: session.to_string(),
            window_name: String::new(),
            window_index: idx.to_string(),
            cwd: "/home/u/hive".to_string(),
            claude_session_id: sid.to_string(),
            claude_config_dir: None,
            first_seen: "2026-09-05T21:00:00Z".to_string(),
            last_seen: "2026-09-05T21:30:00Z".to_string(),
        }
    }

    fn registry_of(convs: Vec<Conversation>) -> ConversationRegistry {
        ConversationRegistry {
            conversations: convs
                .into_iter()
                .map(|c| (c.id.as_str().to_string(), c))
                .collect(),
        }
    }

    /// What makes the screen idempotent: a window that is already running is not a restore
    /// candidate, so opening it twice has nothing to do the second time. Reconciling against
    /// the REGISTRY is the point — a process's `--resume` argv holds the id it was launched
    /// with, which goes stale as soon as that window starts a different conversation.
    #[test]
    fn recover_rows_skips_what_is_already_live() {
        let reg = registry_of(vec![
            mk("live", Lifecycle::Live, Some("hive"), None),
            mk("closed", Lifecycle::Closed, Some("hive"), None),
        ]);
        let windows = HashMap::from([
            ("live".to_string(), open_window("live", "🐝 hive", "1")),
            ("closed".to_string(), open_window("closed", "🐝 hive", "2")),
        ]);

        let rows = recover_rows(&windows, &reg);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id.as_str(), "closed");
    }

    /// Replay order rebuilds the layout you left, so window index must sort NUMERICALLY —
    /// as strings, "10" lands between "1" and "2" and the windows come back shuffled.
    #[test]
    fn recover_rows_orders_by_session_then_numeric_window_index() {
        let ids = ["a", "b", "c", "d"];
        let reg = registry_of(
            ids.iter()
                .map(|i| mk(i, Lifecycle::Closed, Some("hive"), None))
                .collect(),
        );
        let windows = HashMap::from([
            ("a".to_string(), open_window("a", "📁 thesis", "2")),
            ("b".to_string(), open_window("b", "🐝 hive", "10")),
            ("c".to_string(), open_window("c", "🐝 hive", "9")),
            ("d".to_string(), open_window("d", "📁 thesis", "1")),
        ]);

        let rows = recover_rows(&windows, &reg);
        let order: Vec<&str> = rows.iter().map(|c| c.id.as_str()).collect();
        // 🐝 hive before 📁 thesis (byte order), and 9 before 10 within the session.
        assert_eq!(order, ["c", "b", "d", "a"]);
    }

    /// Recovery lands you in what you were last doing, not in whatever row sorts first.
    #[test]
    fn landing_target_is_the_most_recently_active_conversation() {
        let picked = vec![
            mk(
                "hive",
                Lifecycle::Closed,
                Some("hive"),
                Some("2026-09-05T10:00:00Z"),
            ),
            mk(
                "thesis",
                Lifecycle::Closed,
                Some("thesis"),
                Some("2026-09-05T21:29:00Z"),
            ),
            mk(
                "eve",
                Lifecycle::Closed,
                Some("eve"),
                Some("2026-09-05T18:00:00Z"),
            ),
        ];
        assert_eq!(landing_target(&picked).unwrap().id.as_str(), "thesis");
    }

    /// No activity anywhere (or a tie) keeps replay order: the first row wins, so the
    /// choice is deterministic rather than hash-order.
    #[test]
    fn landing_target_ties_keep_replay_order() {
        let picked = vec![
            mk("first", Lifecycle::Closed, Some("hive"), None),
            mk("second", Lifecycle::Closed, Some("hive"), None),
        ];
        assert_eq!(landing_target(&picked).unwrap().id.as_str(), "first");
        assert!(landing_target(&[]).is_none());
    }

    /// A frame entry the registry has bounded out (too old, no parent, unpinned) has nothing
    /// to resume it *with*, so offering it would be a button that cannot work.
    #[test]
    fn recover_rows_drops_entries_the_registry_no_longer_knows() {
        let reg = registry_of(vec![mk("known", Lifecycle::Closed, Some("hive"), None)]);
        let windows = HashMap::from([
            ("known".to_string(), open_window("known", "🐝 hive", "1")),
            ("gone".to_string(), open_window("gone", "🐝 hive", "2")),
        ]);

        let rows = recover_rows(&windows, &reg);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id.as_str(), "known");
    }

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
            notify_override: false,
            archive_reason: None,
            archived_at: None,
            title: None,
            auth_config_dir: None,
            cpu: 0.0,
            mem_kb: 0,
            ports: Vec::new(),
            pids: Vec::new(),
        }
    }

    /// `mk` + the archive overlay, for the archived-conversation tests.
    fn mk_archived(
        id: &str,
        lc: Lifecycle,
        parent: Option<&str>,
        last: Option<&str>,
        reason: Option<&str>,
    ) -> Conversation {
        let mut c = mk(id, lc, parent, last);
        c.archived = true;
        c.archive_reason = reason.map(|s| s.to_string());
        c.archived_at = Some("2026-07-20T00:00:00Z".to_string());
        c
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

    // ---- Archived conversations: hidden from the listings, shown in project detail ----

    #[test]
    fn test_build_browse_hides_archived_conversations() {
        // Archiving a conversation takes it off Browse — that's the whole point.
        // It is NOT deleted: `build_group_detail` still lists it (see the sort
        // test below), which is why the registry keeps it.
        let mut reg = ConversationRegistry::default();
        for c in [
            mk("keep", Lifecycle::Closed, Some("hive"), Some("2026-07-02")),
            mk_archived(
                "gone",
                Lifecycle::Closed,
                Some("hive"),
                Some("2026-07-03"),
                Some("superseded"),
            ),
        ] {
            reg.conversations.insert(c.id.as_str().to_string(), c);
        }
        let (_, convs) = build_browse(
            &reg,
            &ProjectRegistry::default(),
            false,
            false,
            &HashSet::new(),
            &HashMap::new(),
        );
        let ids: Vec<&str> = convs.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["keep"], "archived conversation hidden in Browse");
    }

    #[test]
    fn test_build_browse_shows_archived_conversation_that_is_live() {
        // You can't hide something that's running: a live conversation shows even
        // if the archive flag is still set on it.
        let mut reg = ConversationRegistry::default();
        let c = mk_archived(
            "running",
            Lifecycle::Live,
            Some("hive"),
            Some("2026-07-03"),
            Some("was set aside"),
        );
        reg.conversations.insert(c.id.as_str().to_string(), c);
        let (_, convs) = build_browse(
            &reg,
            &ProjectRegistry::default(),
            false,
            false,
            &HashSet::new(),
            &HashMap::new(),
        );
        assert_eq!(convs.len(), 1, "a live archived conversation still shows");
    }

    #[test]
    fn test_sort_project_convs_puts_archived_last() {
        // The project detail sorts archived to the tail so they can be sectioned
        // off under an "Archived" header — even when an archived one is MORE
        // recent than the working-set rows above it.
        let mut convs = vec![
            mk_archived(
                "arch-recent",
                Lifecycle::Closed,
                Some("hive"),
                Some("2026-07-09"),
                Some("wrong approach"),
            ),
            mk(
                "closed",
                Lifecycle::Closed,
                Some("hive"),
                Some("2026-07-01"),
            ),
            mk("live", Lifecycle::Live, Some("hive"), Some("2026-07-02")),
        ];
        sort_project_convs(&mut convs);
        let ids: Vec<&str> = convs.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["live", "closed", "arch-recent"]);
    }

    #[test]
    fn test_archived_from_and_most_recent_skip_archived() {
        // `archived_from` marks where the Archived section starts, and `r`
        // (resume last) must never land on an archived conversation — otherwise
        // archiving the newest one would make `r` reopen the thing you set aside.
        let mut convs = vec![
            mk("live", Lifecycle::Live, Some("hive"), Some("2026-07-02")),
            mk_archived(
                "arch",
                Lifecycle::Closed,
                Some("hive"),
                Some("2026-07-09"),
                None,
            ),
        ];
        sort_project_convs(&mut convs);
        let state = ProjectDetailState {
            key: "hive".to_string(),
            worktrees: Vec::new(),
            convs,
            path: String::new(),
            todos: Vec::new(),
            sel: 0,
            wt_input: None,
            archive_input: None,
            show_all: false,
            wt_info: None,
            bucket: false,
        };
        assert_eq!(state.archived_from(), Some(1));
        assert_eq!(
            state.most_recent().map(|c| c.id.as_str().to_string()),
            Some("live".to_string()),
            "resume-last skips the (newer) archived conversation"
        );
    }

    /// `mk` + the freeze overlay, for the frozen-section tests.
    fn mk_frozen(id: &str, parent: Option<&str>, last: Option<&str>) -> Conversation {
        let mut c = mk(id, Lifecycle::Closed, parent, last);
        c.frozen = Some(crate::common::registry::FrozenInfo {
            note: "postponed".to_string(),
            pinned: true,
            frozen_at: "2026-07-20T00:00:00Z".to_string(),
        });
        c
    }

    /// Flatten a rendered `Line` back to its plain text, for asserting layout.
    fn text_of(l: &Line<'_>) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// Display width of a rendered line, in terminal cells (emoji count as two).
    fn cells(s: &str) -> usize {
        unicode_width::UnicodeWidthStr::width(s)
    }

    #[test]
    fn test_card_note_prefers_the_freeze_reason() {
        // The freeze note is what you typed at `Z` time — the reason this window
        // is parked — so it wins over the generic overlay note. With neither, the
        // line still renders: a missing reason is itself the answer, and dropping
        // it would make cards different heights.
        let mut c = mk_frozen("a", Some("hive"), None);
        c.note = "overlay".to_string();
        assert_eq!(card_note(&c), ("postponed".to_string(), true));

        c.frozen.as_mut().unwrap().note = "   ".to_string();
        assert_eq!(
            card_note(&c),
            ("overlay".to_string(), true),
            "a blank freeze note falls through to the overlay note"
        );

        c.note = String::new();
        assert_eq!(card_note(&c), ("(no note)".to_string(), false));
    }

    #[test]
    fn test_frozen_card_is_three_lines_project_note_conversation() {
        let mut reg = ProjectRegistry::default();
        reg.projects.insert(
            "hive".to_string(),
            ProjectConfig {
                emoji: "🐝".to_string(),
                ..Default::default()
            },
        );
        let mut c = mk_frozen("4a44b59aXXXX", Some("hive"), None);
        c.title = Some("Branch from experiment crisis colombia".to_string());

        let card = frozen_card(&c, &reg, false, Some(1), 100);
        assert_eq!(
            card.len(),
            3,
            "always three lines, so cards scan as a column"
        );

        let lines: Vec<String> = card.iter().map(text_of).collect();
        // Line 1 = where it lives, line 2 = why it's parked, line 3 = what it was.
        assert!(
            lines[0].contains("🐝 hive"),
            "line 1 names the project: {:?}",
            lines[0]
        );
        assert!(
            lines[0].contains("frozen "),
            "line 1 carries the freeze age"
        );
        assert!(!lines[0].contains("postponed"), "the note is NOT on line 1");
        assert_eq!(lines[1].trim(), "postponed");
        assert!(lines[2].contains("Branch from experiment crisis colombia"));
        assert!(
            lines[2].contains("4a44b59a"),
            "the resume id closes the card"
        );

        // The profile + age tail is right-aligned against the body width, so it's
        // the project label that gives, never the age.
        assert!(
            cells(&lines[0]) <= 100,
            "line 1 fits the given width: {} cells",
            cells(&lines[0])
        );
    }

    #[test]
    fn test_frozen_card_falls_back_to_cwd_and_untitled() {
        // No resolvable parent (an unregistered dir) and no transcript title: the
        // card still says *something* locating in every slot rather than blanking.
        let c = mk_frozen("b", None, None);
        let lines: Vec<String> = frozen_card(&c, &ProjectRegistry::default(), false, None, 100)
            .iter()
            .map(text_of)
            .collect();
        assert!(
            lines[0].contains("/home/u/hive"),
            "falls back to the cwd: {:?}",
            lines[0]
        );
        assert!(lines[2].contains("(untitled)"));
    }

    #[test]
    fn test_frozen_card_keeps_the_age_readable_when_narrow() {
        // The bug this layout exists to fix: at popup width the one-line row cut
        // the note off. Here the project label shrinks to its floor and the tail
        // survives instead.
        let mut c = mk_frozen("c", Some("some-very-long-project-key-indeed"), None);
        c.title = Some("t".to_string());
        let lines: Vec<String> = frozen_card(&c, &ProjectRegistry::default(), false, Some(1), 44)
            .iter()
            .map(text_of)
            .collect();
        assert!(
            lines[0].ends_with("ago"),
            "the age survives: {:?}",
            lines[0]
        );
        assert!(lines[0].contains('…'), "the project label is what gives");
        assert_eq!(
            lines[1].trim(),
            "postponed",
            "the note is never truncated away"
        );
    }

    #[test]
    fn test_frozen_convs_sort_above_closed_and_section_off() {
        // Frozen == pending work you parked on purpose. Sorted below the closed pile
        // it fell off the end of the capped list, so the detail had to be paged open
        // to find the one conversation you froze *to remember it*. It now sits right
        // under the live rows, with its own header — and the closed run gets one too,
        // so it's clear where "parked" ends.
        let mut convs = vec![
            mk(
                "closed-recent",
                Lifecycle::Closed,
                Some("hive"),
                Some("2026-07-25"),
            ),
            mk_frozen("frozen", Some("hive"), Some("2026-07-01")),
            mk_archived(
                "arch",
                Lifecycle::Closed,
                Some("hive"),
                Some("2026-07-26"),
                None,
            ),
            mk("live", Lifecycle::Live, Some("hive"), Some("2026-07-02")),
        ];
        sort_project_convs(&mut convs);
        let ids: Vec<&str> = convs.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["live", "frozen", "closed-recent", "arch"],
            "frozen outranks a MORE recent plain closed conversation"
        );

        let mut state = ProjectDetailState {
            key: "hive".to_string(),
            worktrees: Vec::new(),
            convs,
            path: String::new(),
            todos: Vec::new(),
            sel: 0,
            wt_input: None,
            archive_input: None,
            show_all: false,
            wt_info: None,
            bucket: false,
        };
        assert_eq!(state.frozen_from(), Some(1));
        assert_eq!(state.closed_from(), Some(2));
        assert_eq!(state.archived_from(), Some(3));
        assert_eq!(
            state.section_counts(),
            (1, 1),
            "one frozen, one plain closed"
        );

        // With nothing frozen the closed rows just continue the list — no header,
        // exactly as before.
        let all = std::mem::take(&mut state.convs);
        state.convs = all.iter().filter(|c| !c.is_frozen()).cloned().collect();
        assert_eq!(state.frozen_from(), None);
        assert_eq!(state.closed_from(), None);
        assert_eq!(state.section_counts(), (0, 1));

        // An all-frozen list (the 💤 bucket) gets no header either: it would section
        // off everything, restating the screen title and every row's 💤 marker.
        state.convs = all.iter().filter(|c| c.is_frozen()).cloned().collect();
        assert_eq!(state.frozen_from(), None);
        assert_eq!(state.section_counts(), (1, 0));
    }

    #[test]
    fn test_freeze_screen_is_never_capped() {
        // Paging protects a project detail's config/todos/worktrees from a long
        // conversation list. The 💤 screen has none of those competing for the
        // space and exists solely to show the parked set, so it draws all of it —
        // a `… N more` row there would hide the entry you froze *to remember it*.
        let mut state = ProjectDetailState {
            key: FROZEN_GROUP.to_string(),
            worktrees: Vec::new(),
            convs: (0..29)
                .map(|i| mk_frozen(&format!("c{i}"), Some("cs"), None))
                .collect(),
            path: String::new(),
            todos: Vec::new(),
            sel: 0,
            wt_input: None,
            archive_input: None,
            show_all: false,
            wt_info: None,
            bucket: true,
        };
        assert!(state.is_freeze_screen());
        assert_eq!(
            state.visible_convs(),
            29,
            "uncapped even with show_all off — Tab has nothing to reveal"
        );
        // The cursor reaches the last card, so every parked window is selectable.
        assert_eq!(state.num_items(), 29);
        state.sel = 28;
        assert_eq!(
            state.selected_conv().map(|c| c.id.as_str().to_string()),
            Some("c28".to_string())
        );

        // A normal project detail with the same list still pages.
        state.key = "cs".to_string();
        assert!(!state.is_freeze_screen());
        assert_eq!(state.visible_convs(), BROWSE_PAGE);
    }

    #[test]
    fn test_project_detail_caps_lists_until_show_all() {
        // The detail lists the latest `BROWSE_PAGE` of each section; `Tab` reveals
        // the rest. `num_items` has to follow the cap, or the cursor walks off into
        // conversations that aren't drawn.
        let mut state = ProjectDetailState {
            key: "cs".to_string(),
            worktrees: (0..26)
                .map(|i| WtRow {
                    branch: format!("br{i}"),
                    session: format!("s{i}"),
                    live: 0,
                    frozen: 0,
                    last_activity: None,
                })
                .collect(),
            convs: (0..29)
                .map(|i| mk(&format!("c{i}"), Lifecycle::Closed, Some("cs"), None))
                .collect(),
            path: String::new(),
            todos: vec![("s".to_string(), "todo".to_string())],
            sel: 0,
            wt_input: None,
            archive_input: None,
            show_all: false,
            wt_info: None,
            bucket: false,
        };
        assert_eq!(state.visible_worktrees(), BROWSE_PAGE);
        assert_eq!(state.visible_convs(), BROWSE_PAGE);
        assert_eq!(
            state.num_items(),
            1 + BROWSE_PAGE + BROWSE_PAGE,
            "1 todo + capped convs + capped worktrees"
        );

        // The cursor runs todos → conversations → worktrees, in drawn order.
        state.sel = 0;
        assert_eq!(state.selected_todo().map(|(_, t)| t.as_str()), Some("todo"));
        assert!(state.selected_conv().is_none() && state.selected_worktree().is_none());
        state.sel = 1; // first conversation
        assert!(state.selected_todo().is_none());
        assert_eq!(
            state.selected_conv().map(|c| c.id.as_str().to_string()),
            Some("c0".to_string())
        );
        // The conversation run stops at the cap, not at conversation 29 — past it
        // the cursor is on a worktree, not a conversation that isn't drawn.
        state.sel = BROWSE_PAGE;
        assert_eq!(
            state.selected_conv().map(|c| c.id.as_str().to_string()),
            Some("c4".to_string())
        );
        state.sel = BROWSE_PAGE + 1; // first worktree
        assert!(state.selected_conv().is_none());
        assert_eq!(
            state.selected_worktree().map(|w| w.branch.as_str()),
            Some("br0")
        );
        state.sel = state.num_items() - 1;
        assert_eq!(
            state.selected_worktree().map(|w| w.branch.as_str()),
            Some("br4")
        );

        state.show_all = true;
        assert_eq!(state.visible_worktrees(), 26);
        assert_eq!(state.visible_convs(), 29);
        assert_eq!(state.num_items(), 1 + 29 + 26);
        // Offsets follow the now-uncapped conversation list.
        state.sel = 1 + 29;
        assert_eq!(
            state.selected_worktree().map(|w| w.branch.as_str()),
            Some("br0")
        );

        // Sections at or under the cap are unaffected, and get no "more" line.
        state.show_all = false;
        state.convs.truncate(3);
        assert_eq!(state.visible_convs(), 3);
        assert_eq!(state.num_items(), 1 + 3 + BROWSE_PAGE);
        assert!(detail_more_line(3, 3, "conversations", false).is_none());
        assert!(detail_more_line(3, 3, "conversations", true).is_none());
        assert!(detail_more_line(29, BROWSE_PAGE, "conversations", false).is_some());
    }

    #[test]
    fn test_fit_cells_pads_and_truncates_by_display_width() {
        use unicode_width::UnicodeWidthStr;
        // Emoji are two cells but one char, so `{:<n}` would over-pad these rows.
        for label in ["🚀 Eve-online", "📁 ProMobile ProSys", "👁️ iris", "plain"] {
            assert_eq!(
                fit_cells(label, 24).width(),
                24,
                "{label:?} must occupy exactly 24 columns"
            );
        }
        // Too wide ⇒ truncated with an ellipsis, still exactly `cells` columns.
        let long = fit_cells("🌳 Clear Session / CSD-2723-clinic-admin-dashboard", 24);
        assert_eq!(long.width(), 24);
        assert!(long.contains('…'));
        assert_eq!(fit_cells("x", 0), "");
    }

    #[test]
    fn test_project_label_names_project_and_worktree() {
        // The frozen bucket lists conversations from everywhere, so each row has to
        // say where it lives — the cwd's shared prefix ("00-Personal/…") doesn't.
        let mut projects = ProjectRegistry::default();
        projects.projects.insert(
            "cs".into(),
            ProjectConfig {
                emoji: "🌳".into(),
                display_name: Some("Clear Session".into()),
                ..Default::default()
            },
        );
        let label = |parent: Option<&str>| {
            project_label(&mk("x", Lifecycle::Closed, parent, None), &projects)
        };
        assert_eq!(label(Some("cs")), Some("🌳 Clear Session".to_string()));
        assert_eq!(
            label(Some("cs/CSD-2723")),
            Some("🌳 Clear Session / CSD-2723".to_string()),
            "a worktree conversation names its branch too"
        );
        // An unregistered project falls back to its key; no parent ⇒ no label.
        assert_eq!(label(Some("ghost")), Some("ghost".to_string()));
        assert_eq!(label(None), None);
    }

    #[test]
    fn test_archive_reason_line() {
        // Only archived rows get the line; the reason is rendered when present,
        // and its absence is stated rather than left blank.
        assert!(
            archive_reason_line(&mk("a", Lifecycle::Closed, None, None), false).is_none(),
            "non-archived rows get no reason line"
        );
        let text = |c: &Conversation| {
            archive_reason_line(c, false)
                .unwrap()
                .spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        };
        let with = mk_archived("b", Lifecycle::Closed, None, None, Some("superseded by X"));
        assert!(text(&with).contains("superseded by X"));
        let without = mk_archived("c", Lifecycle::Closed, None, None, None);
        assert!(text(&without).contains("no reason given"));
    }
}
