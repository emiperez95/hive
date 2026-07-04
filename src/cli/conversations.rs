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

use crate::common::instances;
use crate::common::jsonl;
use crate::common::projects::ProjectRegistry;
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
        if let Some(sid) = inst.session_id {
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
}

/// A visible row: a group header, or a conversation under an expanded group.
enum Row {
    Header(usize),                 // index into `groups`
    Conv { ci: usize, gi: usize }, // conversation index + its group index
}

/// Flatten the registry into grouped, sorted display order.
fn build_groups(reg: &ConversationRegistry) -> (Vec<Group>, Vec<Conversation>) {
    use std::collections::BTreeMap;
    let mut grouped: BTreeMap<String, Vec<&Conversation>> = BTreeMap::new();
    for c in reg.conversations.values() {
        let key = c
            .parent
            .clone()
            .unwrap_or_else(|| "(unassigned)".to_string());
        grouped.entry(key).or_default().push(c);
    }
    let mut groups = Vec::new();
    let mut convs = Vec::new();
    for (key, mut group) in grouped {
        group.sort_by(|a, b| {
            b.lifecycle
                .is_actionable_here()
                .cmp(&a.lifecycle.is_actionable_here())
                .then_with(|| b.last_activity.cmp(&a.last_activity))
                .then_with(|| a.id.as_str().cmp(b.id.as_str()))
        });
        let path = common_prefix(&group.iter().map(|c| c.cwd.as_str()).collect::<Vec<_>>());
        let mut idxs = Vec::new();
        for c in group {
            idxs.push(convs.len());
            convs.push(c.clone());
        }
        groups.push(Group {
            key,
            convs: idxs,
            path,
        });
    }
    (groups, convs)
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
fn build_active(reg: &ConversationRegistry) -> (Vec<Group>, Vec<Conversation>) {
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
    let mut groups = Vec::new();
    let mut convs = Vec::new();
    for (key, mut group) in grouped {
        group.sort_by(|a, b| {
            b.last_activity
                .cmp(&a.last_activity)
                .then_with(|| a.id.as_str().cmp(b.id.as_str()))
        });
        let path = common_prefix(&group.iter().map(|c| c.cwd.as_str()).collect::<Vec<_>>());
        let mut idxs = Vec::new();
        for c in group {
            idxs.push(convs.len());
            convs.push(c.clone());
        }
        groups.push(Group {
            key,
            convs: idxs,
            path,
        });
    }
    (groups, convs)
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
    }
    Ok(())
}

fn conversations_loop(terminal: &mut ratatui::DefaultTerminal) -> Result<Action> {
    let mut reg = gather_conversations();
    // Default to the Active view (live conversations by session); `/` browses all.
    let mut view = View::Active;
    let (mut groups, mut convs) = build_active(&reg);
    let mut collapsed: HashSet<String> = HashSet::new(); // Active: everything expanded
    let mut sel: usize = 0;

    // Switch the current view, rebuilding groups + collapse state.
    let rebuild = |view: &View,
                   reg: &ConversationRegistry|
     -> (Vec<Group>, Vec<Conversation>, HashSet<String>) {
        match view {
            View::Active => {
                let (g, c) = build_active(reg);
                (g, c, HashSet::new())
            }
            View::Browse => {
                let (g, c) = build_groups(reg);
                let collapsed = g.iter().map(|g| g.key.clone()).collect();
                (g, c, collapsed)
            }
        }
    };

    loop {
        let rows = visible_rows(&groups, &collapsed);
        if sel >= rows.len() {
            sel = rows.len().saturating_sub(1);
        }
        terminal.draw(|frame| draw(frame, &view, &groups, &convs, &rows, &collapsed, sel))?;

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
            KeyCode::Char('q') | KeyCode::Char('Q') => return Ok(Action::Quit),
            // Esc backs out of Browse to Active; from Active it quits.
            KeyCode::Esc => match view {
                View::Browse => {
                    view = View::Active;
                    (groups, convs, collapsed) = rebuild(&view, &reg);
                    sel = 0;
                }
                View::Active => return Ok(Action::Quit),
            },
            // `/` opens the full browse-everything list.
            KeyCode::Char('/') => {
                if matches!(view, View::Active) {
                    view = View::Browse;
                    (groups, convs, collapsed) = rebuild(&view, &reg);
                    sel = 0;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if sel + 1 < rows.len() {
                    sel += 1;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => sel = sel.saturating_sub(1),
            // Expand the selected group.
            KeyCode::Right | KeyCode::Char('l') => {
                if let Some(Row::Header(gi)) = rows.get(sel) {
                    collapsed.remove(&groups[*gi].key);
                }
            }
            // Collapse: on a header, fold it; on a conversation, fold its group and
            // move the cursor up to that header.
            KeyCode::Left | KeyCode::Char('h') => match rows.get(sel) {
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
            },
            KeyCode::Enter => match rows.get(sel) {
                Some(Row::Conv { ci, .. }) => {
                    let c = &convs[*ci];
                    return Ok(if c.lifecycle.is_actionable_here() {
                        Action::Switch(Box::new(c.clone()))
                    } else {
                        Action::Reopen(Box::new(c.clone()))
                    });
                }
                // Enter on a header toggles it.
                Some(Row::Header(gi)) => {
                    let key = groups[*gi].key.clone();
                    if !collapsed.remove(&key) {
                        collapsed.insert(key);
                    }
                }
                None => {}
            },
            KeyCode::Char('r') => {
                reg = gather_conversations();
                (groups, convs, collapsed) = rebuild(&view, &reg);
                sel = 0;
            }
            _ => {}
        }
    }
}

fn draw(
    frame: &mut ratatui::Frame,
    view: &View,
    groups: &[Group],
    convs: &[Conversation],
    rows: &[Row],
    collapsed: &HashSet<String>,
    sel: usize,
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
    let (header, footer): (String, &str) = match view {
        View::Active => (
            format!(
                " hive · active — {live} running in {} sessions   ( / browse all )",
                groups.len()
            ),
            " ↑/↓ move · Enter switch · / browse all · r refresh · q quit",
        ),
        View::Browse => (
            format!(
                " hive conversations — {total} · {live} live · {} closed · {} groups   ( Esc back )",
                total - live,
                groups.len()
            ),
            " ←/→ collapse · ↑/↓ move · Enter open/resume · Esc active · r refresh · q quit",
        ),
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            header,
            Style::default().add_modifier(Modifier::BOLD),
        ))),
        chunks[0],
    );

    // Scroll so the selected row stays visible.
    let h = chunks[1].height as usize;
    let offset = if sel >= h { sel + 1 - h } else { 0 };
    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .skip(offset)
        .take(h)
        .map(|(ri, row)| {
            let selected = ri == sel;
            match row {
                Row::Header(gi) => header_line(&groups[*gi], convs, collapsed, selected),
                Row::Conv { ci, gi } => conv_line(&convs[*ci], &groups[*gi].path, selected),
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), chunks[1]);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            footer,
            Style::default().fg(Color::DarkGray),
        ))),
        chunks[2],
    );
}

/// A group header row: a disclosure triangle, the parent key, and counts.
fn header_line(
    g: &Group,
    convs: &[Conversation],
    collapsed: &HashSet<String>,
    selected: bool,
) -> Line<'static> {
    let tri = if collapsed.contains(&g.key) {
        "▸"
    } else {
        "▾"
    };
    let n = g.convs.len();
    let live = g
        .convs
        .iter()
        .filter(|&&ci| convs[ci].lifecycle.is_actionable_here())
        .count();
    let text = format!("{tri} {}  ({n}, {live} live)", g.key);
    let mut style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    if selected {
        style = style.add_modifier(Modifier::REVERSED);
    }
    Line::from(Span::styled(text, style))
}

fn conv_line(c: &Conversation, group_path: &str, selected: bool) -> Line<'static> {
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
    let last = c
        .last_activity
        .as_deref()
        .map(|t| t.chars().take(10).collect::<String>())
        .unwrap_or_default();
    // Only the subpath that distinguishes this conversation from its group's
    // shared path (empty for the common case — the whole group shares one dir).
    let sub = rel_below(&c.cwd, group_path);
    let text = format!(
        "    {} {:8}  {:<36}  {:<12}  {}  {}",
        marker,
        short_id(c.id.as_str()),
        title,
        status_label(c),
        last,
        sub,
    );
    let style = if selected {
        Style::default().add_modifier(Modifier::REVERSED)
    } else if c.lifecycle.is_actionable_here() {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::Gray)
    };
    Line::from(Span::styled(text, style))
}

/// Reopen a closed conversation: add a window to the current tmux session at the
/// conversation's cwd and `claude --resume <id>` in it. (v1: reopens in the
/// current session; profile-aware placement into the project's own session is a
/// follow-up.)
fn reopen(c: &Conversation) -> Result<String> {
    let session = get_current_tmux_session()
        .ok_or_else(|| anyhow!("not inside a tmux session — cannot reopen here"))?;
    let startup = format!("claude --resume {}", c.id);

    let mut cmd = Command::new("tmux");
    cmd.args(["new-window", "-t", &session, "-c", &c.cwd]);
    if let Some(title) = &c.title {
        if !title.is_empty() {
            cmd.args(["-n", title]);
        }
    }
    let ok = cmd.output().map(|o| o.status.success()).unwrap_or(false);
    if !ok {
        return Err(anyhow!("failed to open a new window in '{session}'"));
    }
    let _ = Command::new("tmux")
        .args(["send-keys", "-t", &session, &startup, "Enter"])
        .output();
    Ok(format!("Reopened {} — {startup}", short_id(c.id.as_str())))
}

fn short_id(id: &str) -> String {
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

/// Render the registry grouped by parent key (deterministic ordering) — pure, so
/// it is unit-testable without tmux/disk.
pub fn render_conversations(reg: &ConversationRegistry) -> String {
    use std::collections::BTreeMap;

    let total = reg.conversations.len();
    let live = reg
        .conversations
        .values()
        .filter(|s| s.lifecycle.is_actionable_here())
        .count();
    let closed = total - live;

    // Group by parent; None → "(unassigned)". BTreeMap gives stable ordering.
    let mut groups: BTreeMap<String, Vec<&Conversation>> = BTreeMap::new();
    for s in reg.conversations.values() {
        let key = s
            .parent
            .clone()
            .unwrap_or_else(|| "(unassigned)".to_string());
        groups.entry(key).or_default().push(s);
    }

    let mut out = String::new();
    out.push_str(&format!(
        "hive conversations — {total} known ({live} live · {closed} closed) across {} groups\n",
        groups.len()
    ));

    for (parent, mut sessions) in groups {
        // Live first, then most-recently-active, then id for stability.
        sessions.sort_by(|a, b| {
            b.lifecycle
                .is_actionable_here()
                .cmp(&a.lifecycle.is_actionable_here())
                .then_with(|| b.last_activity.cmp(&a.last_activity))
                .then_with(|| a.id.as_str().cmp(b.id.as_str()))
        });
        out.push('\n');
        out.push_str(&format!("{parent}\n"));
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
            out.push_str(&format!(
                "  {} {:8}  {:<28}  {:<13}  {}  {}\n",
                marker,
                short_id(s.id.as_str()),
                title,
                status_label(s),
                last,
                sub,
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
