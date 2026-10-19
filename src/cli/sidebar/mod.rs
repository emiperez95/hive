//! `hive sidebar` — the sidebar as a terminal UI, for a Ghostty split.
//!
//! The terminal-native counterpart of the web sidebar (`/?sidebar=1`), for
//! terminals with no Toolbelt to host a webview. It sits in its own Ghostty
//! split beside the one running `tmux attach`, so it is outside tmux and stays
//! put while the client moves between sessions — the permanence the Toolbelt had.
//!
//! It draws and decides nothing the web sidebar doesn't: rows, tiers, ranks,
//! held-for ages, pings, spend, todos and dispatch all come from the dashboard's
//! JSON API ([`api`]), computed by the web data thread's one gather. That keeps
//! the attention order identical to `Ctrl+g`, and N sidebars (one per tab) as
//! cheap as N HTTP polls.
//!
//! Layout, top to bottom: header (counts + view / archived / mute toggles), the
//! list (Live · Attention · Projects, with Skipped and Frozen folded below), then
//! three panes — Details (spend + git), Notes, Todos — each collapsible and
//! resizable by dragging its header.

mod api;
mod editor;
mod model;
mod render;

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use serde::{Deserialize, Serialize};

use crate::common::config::HiveConfig;
use crate::common::terminal::{self, SIDEBAR_TITLE};
use crate::common::tmux;
use crate::serve::web_types::SessionView;
use api::{Api, Focus, FrozenItem, Msg, Stats, Todos};
use editor::Editor;
use model::{Conv, Item, ListState, Projects, Sect, View};

/// Width the sidebar settles at when it opens as a split, in columns. Matches
/// the web sidebar's ~280px at a typical cell width.
const SIDEBAR_COLS: u16 = 40;

/// A click this soon after the pointer was over a spot counts as a click there
/// — see [`App::on_focus_gained`].
const CLICK_THROUGH: Duration = Duration::from_millis(1500);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Pane {
    Details,
    Notes,
    Todos,
}

/// Where keystrokes go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyFocus {
    List,
    Notes,
    Todos,
}

/// Remembered across launches (`~/.hive/cache/sidebar.json`) — the web page
/// keeps the same things in localStorage, for the same reason: a view that
/// resets on every restart reads as the feature not working.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct Prefs {
    view: View,
    skipped_open: bool,
    frozen_open: bool,
    show_archived: bool,
    details_collapsed: bool,
    notes_collapsed: bool,
    todos_collapsed: bool,
    done_open: bool,
    /// Body height per pane, in rows; set by dragging a pane header.
    details_h: u16,
    notes_h: u16,
    todos_h: u16,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            view: View::Live,
            skipped_open: false,
            frozen_open: false,
            show_archived: false,
            details_collapsed: false,
            notes_collapsed: false,
            todos_collapsed: false,
            done_open: false,
            details_h: 9,
            notes_h: 5,
            todos_h: 7,
        }
    }
}

impl Prefs {
    fn path() -> Option<std::path::PathBuf> {
        crate::common::persistence::cache_dir().map(|d| d.join("sidebar.json"))
    }

    fn load() -> Self {
        Self::path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn save(&self) {
        if let (Some(p), Ok(s)) = (Self::path(), serde_json::to_string_pretty(self)) {
            let tmp = p.with_extension("json.tmp");
            if std::fs::write(&tmp, s).is_ok() {
                let _ = std::fs::rename(tmp, p);
            }
        }
    }

    fn collapsed(&self, p: Pane) -> bool {
        match p {
            Pane::Details => self.details_collapsed,
            Pane::Notes => self.notes_collapsed,
            Pane::Todos => self.todos_collapsed,
        }
    }

    fn toggle(&mut self, p: Pane) {
        match p {
            Pane::Details => self.details_collapsed ^= true,
            Pane::Notes => self.notes_collapsed ^= true,
            Pane::Todos => self.todos_collapsed ^= true,
        }
    }

    fn height(&self, p: Pane) -> u16 {
        match p {
            Pane::Details => self.details_h,
            Pane::Notes => self.notes_h,
            Pane::Todos => self.todos_h,
        }
    }

    fn set_height(&mut self, p: Pane, h: u16) {
        match p {
            Pane::Details => self.details_h = h,
            Pane::Notes => self.notes_h = h,
            Pane::Todos => self.todos_h = h,
        }
    }
}

/// A clickable thing on screen, recorded per row by the last draw.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Hit {
    Ctl(Ctl),
    Item(usize),
    Mute(String),
    PaneHead(Pane),
    /// Visual (row, col) inside the notes text.
    Notes(usize, usize),
    TodoAdd,
    Todo(usize, TodoBtn),
    DoneToggle,
    Done(usize),
    Dlg(DlgHit),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Ctl {
    Archived,
    Attention,
    Projects,
    Mute,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum TodoBtn {
    Check,
    Text,
    Delete,
    Go,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum DlgHit {
    Close,
    Send,
    New,
    Worktree,
    Branch,
    Create,
    Go,
    Inside,
}

/// The "start todo" dialog (`serve/dispatch.rs` has the rules; this mirrors
/// them so a disabled option can say why).
struct Dialog {
    session: String,
    text: String,
    /// The conversation "send" would type into: (pane id, title, blocked).
    conv: Option<(String, String, bool)>,
    /// `None` while checking; `Some(None)` = not a registered project.
    project: Option<Option<String>>,
    wt_open: bool,
    branch: Editor,
    suggesting: bool,
    source: Option<String>,
    job: Option<u64>,
    job_polled: Instant,
    msg: Option<(String, bool)>,
    /// A finished worktree's session, offered as "Go".
    landed: Option<String>,
}

struct Drag {
    pane: Pane,
    y0: u16,
    h0: u16,
    moved: bool,
}

pub(crate) struct App {
    api: Api,
    prefs: Prefs,
    sessions: Vec<SessionView>,
    /// Rows that arrived while the pointer hovered the attention list.
    pending: Option<Vec<SessionView>>,
    convs: Vec<Conv>,
    frozen: Vec<FrozenItem>,
    projects: Projects,
    focus: Focus,
    error: Option<String>,
    flash: Option<(String, Instant)>,
    /// Ids clicked since the last poll: clears a ping instantly instead of when
    /// the server next sees the client land.
    seen: HashSet<String>,
    expanded: HashSet<String>,

    sel_key: Option<String>,
    follow_sel: bool,
    list_scroll: usize,
    /// Last pointer position and when — click-through and hover.
    hover: Option<(u16, u16, Instant)>,
    key_focus: KeyFocus,

    stats: Option<Stats>,
    stats_id: Option<String>,
    stats_at: Option<Instant>,
    stats_inflight: bool,
    details_scroll: usize,

    notes: Editor,
    notes_id: Option<String>,
    notes_loading: bool,
    notes_dirty: bool,
    notes_edited: Instant,
    notes_state: String,
    notes_scroll: usize,

    todos: Option<Todos>,
    todos_session: Option<String>,
    todos_at: Option<Instant>,
    todo_input: Editor,
    /// 0 = add line, then items, then the Done toggle, then done items.
    todo_sel: usize,
    todos_scroll: usize,

    dialog: Option<Dialog>,
    drag: Option<Drag>,

    frozen_at: Option<Instant>,
    projects_at: Option<Instant>,

    /// Filled by the draw: clickable spans per screen row.
    hits: Vec<Vec<(u16, u16, Hit)>>,
    /// Filled by the draw: list area rows, notes text width, max pane height.
    list_rows: usize,
    notes_width: usize,
    pane_max: u16,
    quit: bool,
}

/// Open the sidebar as a split of the current Ghostty tab (if it has none).
/// Returns false when there is no Ghostty to split.
pub fn open_split() -> bool {
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "hive".to_string());
    // A new split starts with Ghostty's launchd PATH, which has no tmux.
    let path = std::env::var("PATH").unwrap_or_default();
    let debug = if crate::common::debug::is_debug_enabled() {
        " --debug"
    } else {
        ""
    };
    let command = format!(
        "env PATH={} {}{debug} sidebar --fit",
        tmux::sh_quote(&path),
        tmux::sh_quote(&exe)
    );
    match terminal::open_sidebar_split(&command) {
        terminal::SidebarSplit::Opened | terminal::SidebarSplit::AlreadyOpen => true,
        terminal::SidebarSplit::Unavailable => false,
    }
}

/// `hive sidebar [--split] [--fit]`.
pub fn run_sidebar(split: bool, fit: bool) -> Result<()> {
    if split {
        if !open_split() {
            anyhow::bail!(
                "hive sidebar --split: needs Ghostty (detected: {:?})",
                terminal::detect()
            );
        }
        return Ok(());
    }

    crate::serve::web::ensure_web_autostart();
    let port = HiveConfig::load().web.port;
    let (tx, rx) = mpsc::channel();
    let api = Api::new(port, tx);
    api.start_loops();

    let mut term = ratatui::try_init().context("hive sidebar: needs a terminal")?;
    crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::SetTitle(SIDEBAR_TITLE),
        crossterm::event::EnableMouseCapture,
        crossterm::event::EnableFocusChange,
        crossterm::event::EnableBracketedPaste
    )?;
    if fit {
        fit_width();
    }
    let mut app = App::new(api);
    let result = app.run(&mut term, &rx);
    app.flush_notes();
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::DisableBracketedPaste,
        crossterm::event::DisableFocusChange,
        crossterm::event::DisableMouseCapture
    );
    ratatui::restore();
    result
}

/// Settle a freshly opened split at [`SIDEBAR_COLS`].
///
/// Ghostty resizes in screen POINTS while the pty reports PIXELS, and the ratio
/// is the display's scale factor — 2 on a retina panel, 1 on many external
/// monitors. Rather than ask which screen the window is on, resize once on a
/// retina guess, measure what that actually moved, and correct with the
/// observed columns-per-point. Measured: the naive pixel count overshot a
/// 49-column split to 31.
fn fit_width() {
    // Give Ghostty a beat to apply the title, which is how the split is found.
    std::thread::sleep(Duration::from_millis(150));
    let cols = || {
        crossterm::terminal::window_size()
            .ok()
            .map(|ws| (i32::from(ws.columns), i32::from(ws.width)))
    };
    let Some((c0, px)) = cols() else { return };
    let target = i32::from(SIDEBAR_COLS);
    if c0 <= target || px == 0 {
        return;
    }
    let guess = (c0 - target) * (px / c0) / 2;
    crate::common::debug::debug_log(&format!(
        "sidebar fit: {c0} cols, {px}px wide, first resize {guess}pt"
    ));
    if guess == 0 || !terminal::resize_sidebar_split(guess) {
        return;
    }
    std::thread::sleep(Duration::from_millis(150));
    let Some((c1, _)) = cols() else { return };
    let moved = c0 - c1;
    crate::common::debug::debug_log(&format!("sidebar fit: now {c1} cols (moved {moved})"));
    if moved > 0 && c1 != target {
        terminal::resize_sidebar_split((c1 - target) * guess / moved);
    }
}

impl App {
    fn new(api: Api) -> Self {
        Self {
            api,
            prefs: Prefs::load(),
            sessions: Vec::new(),
            pending: None,
            convs: Vec::new(),
            frozen: Vec::new(),
            projects: Projects::default(),
            focus: Focus::default(),
            error: None,
            flash: None,
            seen: HashSet::new(),
            expanded: HashSet::new(),
            sel_key: None,
            follow_sel: false,
            list_scroll: 0,
            hover: None,
            key_focus: KeyFocus::List,
            stats: None,
            stats_id: None,
            stats_at: None,
            stats_inflight: false,
            details_scroll: 0,
            notes: Editor::default(),
            notes_id: None,
            notes_loading: false,
            notes_dirty: false,
            notes_edited: Instant::now(),
            notes_state: String::new(),
            notes_scroll: 0,
            todos: None,
            todos_session: None,
            todos_at: None,
            todo_input: Editor::default(),
            todo_sel: 0,
            todos_scroll: 0,
            dialog: None,
            drag: None,
            frozen_at: None,
            projects_at: None,
            hits: Vec::new(),
            list_rows: 0,
            notes_width: 30,
            pane_max: 30,
            quit: false,
        }
    }

    fn run(&mut self, term: &mut ratatui::DefaultTerminal, rx: &mpsc::Receiver<Msg>) -> Result<()> {
        let mut dirty = true;
        while !self.quit {
            while let Ok(m) = rx.try_recv() {
                self.on_msg(m);
                dirty = true;
            }
            dirty |= self.tick();
            if dirty {
                term.draw(|f| self.draw(f))?;
                dirty = false;
            }
            if event::poll(Duration::from_millis(100))? {
                dirty = self.on_event(event::read()?);
            }
        }
        Ok(())
    }

    // ── data ────────────────────────────────────────────────────────────────

    /// The conversation the tmux client is looking at.
    fn focused_conv(&self) -> Option<&Conv> {
        self.convs
            .iter()
            .find(|c| self.is_current(c) && c.session_id.is_some())
    }

    fn is_current(&self, c: &Conv) -> bool {
        self.focus.session.as_deref() == Some(c.session.as_str())
            && c.window_index.is_some()
            && self.focus.window_index == c.window_index
    }

    fn rebuild(&mut self) {
        let mut convs = model::conv_rows(&self.sessions, &self.projects, chrono::Utc::now());
        for c in &mut convs {
            if c.session_id
                .as_ref()
                .is_some_and(|id| self.seen.contains(id))
            {
                c.unseen = false;
            }
        }
        self.convs = convs;
    }

    fn hovering_recently(&self) -> bool {
        self.hover
            .is_some_and(|(_, y, at)| at.elapsed() < Duration::from_secs(2) && self.in_list(y))
    }

    fn in_list(&self, y: u16) -> bool {
        y >= 1 && (y as usize) < 1 + self.list_rows
    }

    fn on_msg(&mut self, m: Msg) {
        match m {
            Msg::Active(Ok(v)) => {
                self.error = None;
                // Don't move a click target out from under the pointer: in the
                // attention view a row can change tier and jump between deciding
                // to click and clicking.
                if self.prefs.view == View::Attention && self.hovering_recently() {
                    self.pending = Some(v);
                } else {
                    self.apply_sessions(v);
                }
            }
            Msg::Active(Err(e)) => self.error = Some(e),
            Msg::Ambient(f) => self.focus = f,
            Msg::Frozen(v) => self.frozen = v,
            Msg::Projects(v) => {
                self.projects = Projects::new(v);
                self.rebuild();
            }
            Msg::Stats(id, s) => {
                self.stats_inflight = false;
                if Some(&id) == self.stats_id.as_ref() {
                    self.stats = Some(*s);
                    self.stats_at = Some(Instant::now());
                }
            }
            Msg::Notes(id, text) => {
                if Some(&id) == self.notes_id.as_ref() {
                    self.notes.set(&text);
                    self.notes_loading = false;
                    self.notes_dirty = false;
                }
            }
            Msg::NotesSaved(id, ok) => {
                if Some(&id) == self.notes_id.as_ref() {
                    self.notes_state = if ok { "saved" } else { "not saved" }.into();
                }
            }
            Msg::Todos(session, t) => {
                if Some(&session) == self.todos_session.as_ref() {
                    self.todos = Some(t);
                    self.todos_at = Some(Instant::now());
                    self.todo_sel = self.todo_sel.min(self.todo_lines().saturating_sub(1));
                }
            }
            Msg::GlobalMute(b) => self.focus.global_mute = b,
            Msg::ProjectMute(key, muted) => {
                self.focus.muted_projects.retain(|k| *k != key);
                if muted {
                    self.focus.muted_projects.push(key);
                }
            }
            Msg::Refresh(err) => {
                if let Some(e) = err {
                    self.flash = Some((e, Instant::now()));
                }
                self.api.fetch_frozen();
            }
            Msg::Target(session, project) => {
                if let Some(d) = self.dialog.as_mut().filter(|d| d.session == session) {
                    d.project = Some(project);
                }
            }
            Msg::Branch(r) => {
                if let Some(d) = self.dialog.as_mut() {
                    d.suggesting = false;
                    match r {
                        // Don't clobber a name typed while the model was thinking.
                        Ok((branch, source)) if d.branch.is_empty() => {
                            d.branch.set(&branch);
                            d.source = Some(source);
                        }
                        Ok(_) => {}
                        Err(e) => d.msg = Some((e, true)),
                    }
                }
            }
            Msg::Dispatched(r) => match r {
                Ok(None) => {
                    self.dialog = None;
                    terminal::focus_work_split();
                }
                Ok(Some(job)) => {
                    if let Some(d) = self.dialog.as_mut() {
                        d.job = Some(job);
                        d.job_polled = Instant::now();
                        d.msg = Some((
                            "Creating worktree… project hooks can take a while. Esc hides this."
                                .into(),
                            false,
                        ));
                    }
                }
                Err(e) => {
                    if let Some(d) = self.dialog.as_mut() {
                        d.msg = Some((e, true));
                    }
                }
            },
            Msg::Job(j) => {
                if let Some(d) = self.dialog.as_mut() {
                    match j.state.as_str() {
                        "running" => {}
                        "ok" => {
                            d.job = None;
                            d.landed = j.session;
                            d.msg = Some((j.message, false));
                        }
                        _ => {
                            d.job = None;
                            d.msg = Some((j.message, true));
                        }
                    }
                }
            }
        }
    }

    fn apply_sessions(&mut self, v: Vec<SessionView>) {
        self.sessions = v;
        self.seen.clear();
        self.rebuild();
    }

    /// Timers: refetches on focus changes and TTLs, debounced note saves.
    /// Returns whether anything visible changed.
    fn tick(&mut self) -> bool {
        let mut changed = false;
        let stale = |at: Option<Instant>, ttl: u64| at.is_none_or(|t| t.elapsed().as_secs() >= ttl);

        if self.pending.is_some() && !self.hovering_recently() {
            let v = self.pending.take().unwrap();
            self.apply_sessions(v);
            changed = true;
        }
        if stale(self.frozen_at, 5) {
            self.frozen_at = Some(Instant::now());
            self.api.fetch_frozen();
        }
        if stale(self.projects_at, 30) {
            self.projects_at = Some(Instant::now());
            self.api.fetch_projects();
        }
        if self
            .flash
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed().as_secs() >= 4)
        {
            self.flash = None;
            changed = true;
        }

        let conv_id = self.focused_conv().and_then(|c| c.session_id.clone());
        // Stats: on a focus change at once (clearing the old numbers, so one
        // conversation's title never sits over another's spend), else every 10s.
        if conv_id != self.stats_id {
            self.stats_id = conv_id.clone();
            self.stats = None;
            self.stats_at = None;
            self.details_scroll = 0;
            changed = true;
        }
        if let Some(id) = &self.stats_id {
            if !self.stats_inflight && !self.prefs.details_collapsed && stale(self.stats_at, 10) {
                self.stats_inflight = true;
                self.stats_at = Some(Instant::now());
                self.api.fetch_stats(id.clone());
            }
        }

        // Notes follow the conversation; the previous one's edits flush first.
        if conv_id != self.notes_id {
            self.flush_notes();
            self.notes_id = conv_id.clone();
            self.notes.set("");
            self.notes_state.clear();
            self.notes_scroll = 0;
            self.notes_loading = conv_id.is_some();
            if let Some(id) = conv_id {
                self.api.fetch_notes(id);
            } else if self.key_focus == KeyFocus::Notes {
                self.key_focus = KeyFocus::List;
            }
            changed = true;
        }
        if self.notes_dirty && self.notes_edited.elapsed() >= Duration::from_millis(700) {
            self.flush_notes();
            changed = true;
        }

        // Todos follow the SESSION — todos are session-level everywhere in hive.
        if self.focus.session != self.todos_session {
            self.todos_session = self.focus.session.clone();
            self.todos = None;
            self.todos_at = None;
            self.todo_sel = 0;
            self.todos_scroll = 0;
            changed = true;
        }
        if let Some(s) = &self.todos_session {
            if stale(self.todos_at, 5) {
                self.todos_at = Some(Instant::now());
                self.api.fetch_todos(s.clone());
            }
        }

        if let Some(d) = self.dialog.as_mut() {
            if let Some(job) = d.job {
                if d.job_polled.elapsed() >= Duration::from_secs(1) {
                    d.job_polled = Instant::now();
                    self.api.fetch_job(job);
                }
            }
        }
        changed
    }

    fn flush_notes(&mut self) {
        if let (true, Some(id)) = (self.notes_dirty, self.notes_id.clone()) {
            self.notes_dirty = false;
            self.notes_state = "saving…".into();
            self.api.save_notes(id, self.notes.text());
        }
    }

    // ── list ────────────────────────────────────────────────────────────────

    pub(crate) fn items(&self) -> Vec<Item> {
        let st = ListState {
            view: self.prefs.view,
            skipped_open: self.prefs.skipped_open,
            frozen_open: self.prefs.frozen_open,
            show_archived: self.prefs.show_archived,
            expanded: &self.expanded,
        };
        model::build_items(&st, &self.convs, &self.frozen, &self.projects)
    }

    /// Index of the selected item in `items`, falling back to the first
    /// selectable one.
    pub(crate) fn sel_index(&self, items: &[Item]) -> Option<usize> {
        let found = self.sel_key.as_ref().and_then(|k| {
            items
                .iter()
                .position(|i| i.key(&self.convs, &self.frozen).as_ref() == Some(k))
        });
        found.or_else(|| items.iter().position(Item::selectable))
    }

    fn select(&mut self, items: &[Item], i: usize) {
        self.sel_key = items
            .get(i)
            .and_then(|it| it.key(&self.convs, &self.frozen));
    }

    fn move_sel(&mut self, down: bool) {
        let items = self.items();
        let Some(cur) = self.sel_index(&items) else {
            return;
        };
        let next = if down {
            (cur + 1..items.len()).find(|&i| items[i].selectable())
        } else {
            (0..cur).rev().find(|&i| items[i].selectable())
        };
        if let Some(i) = next {
            self.select(&items, i);
            self.follow_sel = true;
        }
    }

    /// Enter / click on a list item.
    fn activate(&mut self, item: &Item) {
        match item {
            Item::Conv { idx, .. } => {
                if let Some(c) = self.convs.get(*idx).cloned() {
                    self.switch_to(&c.session, c.window_index.as_deref());
                    if let Some(id) = c.session_id {
                        self.seen.insert(id);
                        self.rebuild();
                    }
                }
            }
            Item::Frozen { idx, .. } => {
                if let Some(f) = self.frozen.get(*idx) {
                    self.api.thaw(f.key.clone());
                }
            }
            Item::Section { sect, .. } => {
                match sect {
                    Sect::Skipped => self.prefs.skipped_open ^= true,
                    Sect::Frozen => self.prefs.frozen_open ^= true,
                }
                self.prefs.save();
            }
            // Toggle: the guard's `remove` collapses an open project; only a
            // closed one falls through to be opened.
            Item::ProjectHead { key, .. } if !self.expanded.remove(key) => {
                self.expanded.insert(key.clone());
            }
            _ => {}
        }
    }

    /// Move the tmux client, then give the keyboard back to the work split.
    fn switch_to(&mut self, session: &str, window: Option<&str>) {
        let clients = tmux::list_clients();
        if let Some(c) = tmux::pick_client(&clients) {
            tmux::switch_client_to(&c.tty, session, window);
        }
        // Optimistic: move the marker now rather than when the push arrives.
        self.focus.session = Some(session.to_string());
        self.focus.window_index = window.map(str::to_string);
        terminal::focus_work_split();
    }

    fn set_view(&mut self, v: View) {
        self.prefs.view = if self.prefs.view == v { View::Live } else { v };
        self.prefs.save();
        self.list_scroll = 0;
        if let Some(p) = self.pending.take() {
            self.apply_sessions(p);
        }
    }

    fn selected_project(&self) -> Option<String> {
        let items = self.items();
        match items.get(self.sel_index(&items)?)? {
            Item::Conv { idx, .. } => self.convs.get(*idx).map(|c| c.place.key.clone()),
            Item::Frozen { idx, .. } => self
                .frozen
                .get(*idx)
                .map(|f| model::place_of(&f.session_name, &self.projects).key),
            Item::ProjectHead { key, .. } => Some(key.clone()),
            _ => None,
        }
    }

    // ── todos ───────────────────────────────────────────────────────────────

    /// Selectable lines in the todos pane.
    pub(crate) fn todo_lines(&self) -> usize {
        let Some(t) = &self.todos else { return 1 };
        let done = if t.done.is_empty() {
            0
        } else if self.prefs.done_open {
            1 + t.done.len()
        } else {
            1
        };
        1 + t.todos.len() + done
    }

    /// What todo line `i` is: the add line, active item n, the Done toggle, or
    /// done item n (index into the SERVER's done list — the display is reversed,
    /// most recently finished first).
    pub(crate) fn todo_line(&self, i: usize) -> TodoLine {
        let t = self.todos.clone().unwrap_or_default();
        if i == 0 {
            return TodoLine::Add;
        }
        let i = i - 1;
        if i < t.todos.len() {
            return TodoLine::Item(i);
        }
        let i = i - t.todos.len();
        if i == 0 {
            return TodoLine::DoneToggle;
        }
        TodoLine::Done(t.done.len().saturating_sub(i))
    }

    fn todo_act(&mut self, action: &'static str, extra: serde_json::Value) {
        if let Some(s) = self.todos_session.clone() {
            self.api.todo_action(s, action, extra);
        }
    }

    fn open_dialog(&mut self, index: usize) {
        let (Some(session), Some(text)) = (
            self.todos_session.clone(),
            self.todos
                .as_ref()
                .and_then(|t| t.todos.get(index).cloned()),
        ) else {
            return;
        };
        let conv = self
            .focused_conv()
            .filter(|c| c.session == session)
            .map(|c| {
                (
                    c.pane_id.clone().unwrap_or_default(),
                    c.title.clone(),
                    c.blocks(),
                )
            });
        self.api.fetch_target(session.clone());
        self.dialog = Some(Dialog {
            session,
            text,
            conv,
            project: None,
            wt_open: false,
            branch: Editor::default(),
            suggesting: false,
            source: None,
            job: None,
            job_polled: Instant::now(),
            msg: None,
            landed: None,
        });
    }

    fn dialog_pick(&mut self, hit: DlgHit) {
        let Some(d) = self.dialog.as_mut() else {
            return;
        };
        let running = d.job.is_some();
        match hit {
            DlgHit::Close => {
                if !running {
                    self.dialog = None;
                }
            }
            DlgHit::Send if !running => {
                if let Some((pane, _, false)) = &d.conv {
                    let body = serde_json::json!({
                        "session": d.session, "text": d.text, "mode": "send", "window": pane,
                    });
                    self.api.dispatch(body);
                }
            }
            DlgHit::New if !running => {
                let body =
                    serde_json::json!({ "session": d.session, "text": d.text, "mode": "new" });
                self.api.dispatch(body);
            }
            DlgHit::Worktree if !running => {
                if matches!(d.project, Some(Some(_))) {
                    d.wt_open = true;
                    if d.branch.is_empty() && !d.suggesting && d.source.is_none() {
                        d.suggesting = true;
                        self.api.suggest_branch(d.session.clone(), d.text.clone());
                    }
                }
            }
            DlgHit::Create if !running => {
                let branch = d.branch.text().trim().to_string();
                if branch.is_empty() {
                    d.msg = Some(("Name the branch first".into(), true));
                } else {
                    let body = serde_json::json!({
                        "session": d.session, "text": d.text, "mode": "worktree", "branch": branch,
                    });
                    self.api.dispatch(body);
                }
            }
            DlgHit::Go => {
                if let Some(s) = d.landed.clone() {
                    self.dialog = None;
                    self.switch_to(&s, None);
                }
            }
            _ => {}
        }
    }

    // ── input ───────────────────────────────────────────────────────────────

    /// Returns whether to redraw.
    fn on_event(&mut self, ev: Event) -> bool {
        match ev {
            Event::Key(k) if k.kind == KeyEventKind::Press => self.on_key(k),
            Event::Paste(s) => self.on_paste(&s),
            Event::Mouse(m) => match m.kind {
                MouseEventKind::Down(MouseButton::Left) => self.click(m.column, m.row),
                MouseEventKind::Drag(MouseButton::Left) => {
                    if let Some(d) = self.drag.as_mut() {
                        let h = i32::from(d.h0) + i32::from(d.y0) - i32::from(m.row);
                        let h = h.clamp(1, i32::from(self.pane_max.max(1))) as u16;
                        d.moved |= h != d.h0;
                        let pane = d.pane;
                        self.prefs.set_height(pane, h);
                    }
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    if let Some(d) = self.drag.take() {
                        if !d.moved {
                            self.prefs.toggle(d.pane);
                        }
                        self.prefs.save();
                    }
                }
                MouseEventKind::Moved => {
                    let before = self.hover.map(|(x, y, _)| (x, y));
                    self.hover = Some((m.column, m.row, Instant::now()));
                    // Selection follows the pointer over list rows, like hover.
                    if self.key_focus == KeyFocus::List && self.dialog.is_none() {
                        if let Some(Hit::Item(i)) = self.hit_at(m.column, m.row) {
                            let items = self.items();
                            if items.get(i).is_some_and(Item::selectable) {
                                self.select(&items, i);
                            }
                        }
                    }
                    return before.map(|(_, y)| y) != Some(m.row);
                }
                MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                    let down = m.kind == MouseEventKind::ScrollDown;
                    let step = |v: &mut usize| {
                        *v = if down { *v + 2 } else { v.saturating_sub(2) };
                    };
                    match self.pane_at(m.row) {
                        Some(Pane::Details) => step(&mut self.details_scroll),
                        Some(Pane::Notes) => step(&mut self.notes_scroll),
                        Some(Pane::Todos) => step(&mut self.todos_scroll),
                        None => step(&mut self.list_scroll),
                    }
                }
                _ => return false,
            },
            Event::FocusGained => self.on_focus_gained(),
            Event::Resize(..) => {}
            _ => return false,
        }
        true
    }

    /// Ghostty never passes the click that focuses an unfocused split on to the
    /// program (ghostty#6998, modelled on iTerm2), so clicking the sidebar from
    /// the work split took two clicks. Motion IS reported while unfocused
    /// (confirmed in the debug log), so the spot under the pointer is known:
    /// focus arriving right after the pointer was there is that click. The
    /// recency bound keeps a keyboard `goto_split` into the sidebar, with the
    /// pointer parked over it, from clicking anything.
    fn on_focus_gained(&mut self) {
        crate::common::debug::debug_log(&format!("sidebar: focus gained, hover {:?}", self.hover));
        if let Some((x, y, at)) = self.hover {
            if at.elapsed() < CLICK_THROUGH {
                self.click(x, y);
            }
        }
    }

    pub(crate) fn hit_at(&self, x: u16, y: u16) -> Option<Hit> {
        self.hits
            .get(y as usize)?
            .iter()
            .find(|(x0, x1, _)| x >= *x0 && x < *x1)
            .map(|(_, _, h)| h.clone())
    }

    fn pane_at(&self, y: u16) -> Option<Pane> {
        // Pane headers mark where each pane starts; rows below one belong to it
        // until the next header.
        let mut pane = None;
        for row in self.hits.iter().take(y as usize + 1) {
            if let Some((_, _, Hit::PaneHead(p))) =
                row.iter().find(|(_, _, h)| matches!(h, Hit::PaneHead(_)))
            {
                pane = Some(*p);
            }
        }
        pane
    }

    fn click(&mut self, x: u16, y: u16) {
        let hit = self.hit_at(x, y);
        if self.dialog.is_some() {
            match hit {
                Some(Hit::Dlg(DlgHit::Branch)) => {}
                Some(Hit::Dlg(h)) => self.dialog_pick(h),
                // Outside the box closes it, unless a job is running.
                _ => self.dialog_pick(DlgHit::Close),
            }
            return;
        }
        let Some(hit) = hit else { return };
        // Clicks in the header and list are navigation: hand the keyboard back
        // to the work split afterwards, or the next thing typed (a `q`) lands
        // in the sidebar. Notes and todos keep it — you clicked there to type.
        let mut refocus = true;
        match hit {
            Hit::Ctl(Ctl::Archived) => {
                self.prefs.show_archived ^= true;
                self.prefs.save();
            }
            Hit::Ctl(Ctl::Attention) => self.set_view(View::Attention),
            Hit::Ctl(Ctl::Projects) => self.set_view(View::Projects),
            Hit::Ctl(Ctl::Mute) => self.api.toggle_mute(),
            Hit::Mute(key) => self.api.toggle_project_mute(key),
            Hit::Item(i) => {
                let items = self.items();
                if let Some(item) = items.get(i).cloned() {
                    if item.selectable() {
                        self.select(&items, i);
                    }
                    // A switch refocuses by itself.
                    refocus = !matches!(item, Item::Conv { .. });
                    self.activate(&item);
                }
            }
            Hit::PaneHead(p) => {
                self.drag = Some(Drag {
                    pane: p,
                    y0: y,
                    h0: self.prefs.height(p),
                    moved: false,
                });
                refocus = false;
            }
            Hit::Notes(row, _) => {
                self.key_focus = KeyFocus::Notes;
                let w = self.notes_width;
                // Text starts one column in from the pane edge.
                self.notes.place(row, x.saturating_sub(1) as usize, w);
                refocus = false;
            }
            Hit::TodoAdd => {
                self.key_focus = KeyFocus::Todos;
                self.todo_sel = 0;
                refocus = false;
            }
            Hit::Todo(i, btn) => {
                self.key_focus = KeyFocus::Todos;
                self.todo_sel = 1 + i;
                refocus = false;
                match btn {
                    TodoBtn::Check => self.todo_act("done", serde_json::json!({ "index": i })),
                    TodoBtn::Delete => self.todo_act("delete", serde_json::json!({ "index": i })),
                    TodoBtn::Go => self.open_dialog(i),
                    TodoBtn::Text => {}
                }
            }
            Hit::DoneToggle => {
                self.prefs.done_open ^= true;
                self.prefs.save();
                refocus = false;
            }
            Hit::Done(i) => {
                self.todo_act("reopen", serde_json::json!({ "index": i }));
                refocus = false;
            }
            Hit::Dlg(_) => {}
        }
        if refocus {
            self.key_focus = KeyFocus::List;
            terminal::focus_work_split();
        }
    }

    fn on_paste(&mut self, s: &str) {
        if let Some(d) = self.dialog.as_mut() {
            if d.wt_open {
                d.branch.insert_str(&s.replace(['\n', '\r'], ""));
            }
            return;
        }
        match self.key_focus {
            KeyFocus::Notes if !self.notes_loading => {
                self.notes.insert_str(s);
                self.notes_touched();
            }
            KeyFocus::Todos if self.todo_sel == 0 => {
                self.todo_input.insert_str(&s.replace(['\n', '\r'], " "));
            }
            _ => {}
        }
    }

    fn notes_touched(&mut self) {
        self.notes_dirty = true;
        self.notes_edited = Instant::now();
        self.notes_state = "…".into();
    }

    fn next_pane(&mut self) {
        let has_notes = self.notes_id.is_some() && !self.prefs.notes_collapsed;
        let has_todos = self.todos_session.is_some() && !self.prefs.todos_collapsed;
        self.key_focus = match self.key_focus {
            KeyFocus::List if has_notes => KeyFocus::Notes,
            KeyFocus::List | KeyFocus::Notes if has_todos => KeyFocus::Todos,
            _ => KeyFocus::List,
        };
        if self.key_focus != KeyFocus::Notes {
            self.flush_notes();
        }
    }

    fn on_key(&mut self, k: KeyEvent) {
        if self.dialog.is_some() {
            self.dialog_key(k);
            return;
        }
        if k.code == KeyCode::Tab {
            self.next_pane();
            return;
        }
        match self.key_focus {
            KeyFocus::List => self.list_key(k),
            KeyFocus::Notes => self.notes_key(k),
            KeyFocus::Todos => self.todos_key(k),
        }
    }

    fn list_key(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Char('q') => self.quit = true,
            // Back to the terminal without picking anything.
            KeyCode::Esc => {
                terminal::focus_work_split();
            }
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(true),
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(false),
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Char(' ') => {
                let items = self.items();
                if let Some(item) = self.sel_index(&items).and_then(|i| items.get(i).cloned()) {
                    self.activate(&item);
                }
            }
            KeyCode::Char('a') => self.set_view(View::Attention),
            KeyCode::Char('p') => self.set_view(View::Projects),
            KeyCode::Char('A') => {
                self.prefs.show_archived ^= true;
                self.prefs.save();
            }
            KeyCode::Char('m') => self.api.toggle_mute(),
            KeyCode::Char('M') => {
                if let Some(key) = self.selected_project() {
                    self.api.toggle_project_mute(key);
                }
            }
            _ => {}
        }
    }

    fn notes_key(&mut self, k: KeyEvent) {
        if self.notes_loading {
            if k.code == KeyCode::Esc {
                self.key_focus = KeyFocus::List;
            }
            return;
        }
        let w = self.notes_width;
        match k.code {
            KeyCode::Esc => {
                self.flush_notes();
                self.key_focus = KeyFocus::List;
                return;
            }
            KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                self.notes.insert(c)
            }
            KeyCode::Enter => self.notes.insert('\n'),
            KeyCode::Backspace => self.notes.backspace(),
            KeyCode::Delete => self.notes.delete(),
            KeyCode::Left => return self.notes.left(),
            KeyCode::Right => return self.notes.right(),
            KeyCode::Up => return self.notes.up(w),
            KeyCode::Down => return self.notes.down(w),
            KeyCode::Home => return self.notes.home(),
            KeyCode::End => return self.notes.end(),
            _ => return,
        }
        self.notes_touched();
    }

    fn todos_key(&mut self, k: KeyEvent) {
        let lines = self.todo_lines();
        match k.code {
            KeyCode::Esc => {
                self.key_focus = KeyFocus::List;
                return;
            }
            KeyCode::Down => {
                self.todo_sel = (self.todo_sel + 1).min(lines - 1);
                return;
            }
            KeyCode::Up => {
                self.todo_sel = self.todo_sel.saturating_sub(1);
                return;
            }
            _ => {}
        }
        match self.todo_line(self.todo_sel) {
            TodoLine::Add => match k.code {
                KeyCode::Enter => {
                    let text = self.todo_input.text().trim().to_string();
                    if !text.is_empty() {
                        self.todo_input.set("");
                        self.todo_act("add", serde_json::json!({ "text": text }));
                    }
                }
                KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.todo_input.insert(c)
                }
                KeyCode::Backspace => self.todo_input.backspace(),
                KeyCode::Left => self.todo_input.left(),
                KeyCode::Right => self.todo_input.right(),
                _ => {}
            },
            TodoLine::Item(i) => match k.code {
                KeyCode::Char('x') | KeyCode::Char(' ') => {
                    self.todo_act("done", serde_json::json!({ "index": i }))
                }
                KeyCode::Char('d') | KeyCode::Delete | KeyCode::Backspace => {
                    self.todo_act("delete", serde_json::json!({ "index": i }))
                }
                KeyCode::Enter | KeyCode::Char('s') => self.open_dialog(i),
                _ => {}
            },
            TodoLine::DoneToggle => {
                if matches!(k.code, KeyCode::Enter | KeyCode::Char(' ')) {
                    self.prefs.done_open ^= true;
                    self.prefs.save();
                }
            }
            TodoLine::Done(i) => {
                if matches!(k.code, KeyCode::Enter | KeyCode::Char(' ')) {
                    self.todo_act("reopen", serde_json::json!({ "index": i }));
                }
            }
        }
    }

    fn dialog_key(&mut self, k: KeyEvent) {
        let Some(d) = self.dialog.as_mut() else {
            return;
        };
        if k.code == KeyCode::Esc {
            // A running job keeps going server-side; Esc only hides it.
            self.dialog = None;
            return;
        }
        if d.landed.is_some() && matches!(k.code, KeyCode::Enter | KeyCode::Char('g')) {
            return self.dialog_pick(DlgHit::Go);
        }
        if d.wt_open && d.job.is_none() {
            match k.code {
                KeyCode::Enter => return self.dialog_pick(DlgHit::Create),
                KeyCode::Char(c)
                    if !c.is_whitespace() && !k.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    d.branch.insert(c);
                    d.source = None;
                    return;
                }
                KeyCode::Backspace => return d.branch.backspace(),
                KeyCode::Left => return d.branch.left(),
                KeyCode::Right => return d.branch.right(),
                _ => {}
            }
        }
        match k.code {
            KeyCode::Char('1') => self.dialog_pick(DlgHit::Send),
            KeyCode::Char('2') => self.dialog_pick(DlgHit::New),
            KeyCode::Char('3') => self.dialog_pick(DlgHit::Worktree),
            _ => {}
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum TodoLine {
    Add,
    Item(usize),
    DoneToggle,
    Done(usize),
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn app() -> App {
        let (tx, _rx) = mpsc::channel();
        let mut a = App::new(Api::new(0, tx));
        a.prefs = Prefs::default();
        let w = serde_json::json!({
            "pane_id": "%1", "window_index": "1", "window_name": "fix the bug",
            "session_id": "abc", "status": "Waiting", "cpu": 0.0, "mem_kb": 0, "ports": [],
            "cwd": null, "last_activity": "2026-10-19T10:00:00+00:00", "pane": null,
            "attention": 2, "attention_rank": 0,
        });
        a.sessions = vec![serde_json::from_value(serde_json::json!({
            "name": "🐝 hive", "status": null, "cpu": 0.0, "mem_kb": 0, "ports": [],
            "processes": [], "cwd": null, "last_activity": null, "attached": true,
            "pane": null, "windows": [w], "skipped": false,
        }))
        .unwrap()];
        a.rebuild();
        a.focus = Focus {
            session: Some("🐝 hive".into()),
            window_index: Some("1".into()),
            ..Focus::default()
        };
        a.notes_id = Some("abc".into());
        a.stats_id = Some("abc".into());
        a.todos_session = Some("🐝 hive".into());
        a.todos = Some(Todos {
            todos: vec!["write the docs".into()],
            done: vec!["ship it".into()],
        });
        a
    }

    fn screen(a: &mut App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| a.draw(f)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn draws_list_and_all_three_panes() {
        let mut a = app();
        let s = screen(&mut a, 40, 40);
        assert!(s.contains("1 live"), "{s}");
        assert!(s.contains("fix the bug"));
        assert!(s.contains("Details"));
        assert!(s.contains("Notes"));
        assert!(s.contains("Todos"));
        assert!(s.contains("write the docs"));
    }

    // Every row the draw records as clickable must be a row that exists.
    #[test]
    fn hit_map_matches_the_screen() {
        let mut a = app();
        screen(&mut a, 40, 40);
        assert_eq!(a.hits.len(), 40);
        let todo_row = a
            .hits
            .iter()
            .position(|r| {
                r.iter()
                    .any(|(_, _, h)| matches!(h, Hit::Todo(0, TodoBtn::Go)))
            })
            .expect("the todo's start button is clickable");
        assert_eq!(
            a.hit_at(39, todo_row as u16),
            Some(Hit::Todo(0, TodoBtn::Go))
        );
        assert_eq!(
            a.hit_at(1, todo_row as u16),
            Some(Hit::Todo(0, TodoBtn::Check))
        );
    }

    #[test]
    fn dialog_draws_over_everything_and_owns_its_clicks() {
        let mut a = app();
        a.open_dialog(0);
        let d = a.dialog.as_mut().unwrap();
        d.project = Some(Some("hive".into()));
        d.wt_open = true;
        d.branch.set("docs");
        let s = screen(&mut a, 40, 40);
        assert!(s.contains("Start todo"), "{s}");
        assert!(s.contains("New worktree"));
        assert!(s.contains("Project hive"));
        assert!(s.contains("branch: docs"));
        // The focused conversation is idle, so "send" is offered with its title.
        assert!(s.contains("fix the bug"));
        let create = a
            .hits
            .iter()
            .any(|r| r.iter().any(|(_, _, h)| *h == Hit::Dlg(DlgHit::Create)));
        assert!(create);
    }

    #[test]
    fn todo_lines_map_done_items_newest_first() {
        let mut a = app();
        a.todos = Some(Todos {
            todos: vec!["a".into()],
            done: vec!["old".into(), "new".into()],
        });
        a.prefs.done_open = true;
        assert_eq!(a.todo_lines(), 1 + 1 + 1 + 2);
        assert_eq!(a.todo_line(0), TodoLine::Add);
        assert_eq!(a.todo_line(1), TodoLine::Item(0));
        assert_eq!(a.todo_line(2), TodoLine::DoneToggle);
        // Displayed newest first, but the index is the server's.
        assert_eq!(a.todo_line(3), TodoLine::Done(1));
        assert_eq!(a.todo_line(4), TodoLine::Done(0));
    }

    #[test]
    fn tiny_and_narrow_terminals_do_not_panic() {
        for (w, h) in [(4, 3), (12, 8), (40, 5), (200, 60)] {
            let mut a = app();
            a.open_dialog(0);
            screen(&mut a, w, h);
            a.dialog = None;
            screen(&mut a, w, h);
        }
    }

    #[test]
    fn keyboard_moves_between_selectable_rows_and_panes() {
        let mut a = app();
        a.prefs.frozen_open = true;
        a.frozen = vec![FrozenItem {
            key: "k".into(),
            session_name: "🐝 hive".into(),
            window_label: "w".into(),
            note: "parked".into(),
            relative: "2d ago".into(),
        }];
        screen(&mut a, 40, 40);
        let items = a.items();
        let first = a.sel_index(&items).unwrap();
        assert!(matches!(items[first], Item::Conv { .. }));
        a.move_sel(true);
        let items = a.items();
        assert!(matches!(
            items[a.sel_index(&items).unwrap()],
            Item::Section {
                sect: Sect::Frozen,
                ..
            }
        ));
        a.next_pane();
        assert_eq!(a.key_focus, KeyFocus::Notes);
        a.next_pane();
        assert_eq!(a.key_focus, KeyFocus::Todos);
        a.next_pane();
        assert_eq!(a.key_focus, KeyFocus::List);
    }
}
