//! `hive conversations` — browse every known Claude conversation (live AND
//! closed), grouped by its resolved parent (project / worktree), and jump to or
//! resume one. A conversation-focused counterpart to the classic session TUI;
//! classic `hive` is left untouched, so you can run either and go back and forth.
//!
//! Built as a read-only shadow over the re-rooted [`ConversationRegistry`]
//! (existing `state.json` + a disk scan + the `conversations.json` overlay +
//! live tmux placements). Only the switch/resume actions touch tmux.

use anyhow::{anyhow, Context, Result};
use std::collections::HashMap;
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

/// A display row: a group header, or a conversation (index into the `convs` vec).
enum Item {
    Header(String),
    Conv(usize),
}

/// Flatten the registry into grouped, sorted display order: owned conversations
/// plus an interleaved header/row item list.
fn build_items(reg: &ConversationRegistry) -> (Vec<Item>, Vec<Conversation>) {
    use std::collections::BTreeMap;
    let mut groups: BTreeMap<String, Vec<&Conversation>> = BTreeMap::new();
    for c in reg.conversations.values() {
        let key = c
            .parent
            .clone()
            .unwrap_or_else(|| "(unassigned)".to_string());
        groups.entry(key).or_default().push(c);
    }
    let mut items = Vec::new();
    let mut convs = Vec::new();
    for (parent, mut group) in groups {
        group.sort_by(|a, b| {
            b.lifecycle
                .is_actionable_here()
                .cmp(&a.lifecycle.is_actionable_here())
                .then_with(|| b.last_activity.cmp(&a.last_activity))
                .then_with(|| a.id.as_str().cmp(b.id.as_str()))
        });
        items.push(Item::Header(parent));
        for c in group {
            items.push(Item::Conv(convs.len()));
            convs.push(c.clone());
        }
    }
    (items, convs)
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
    let reg = gather_conversations();
    let (mut items, mut convs) = build_items(&reg);
    let mut sel: usize = 0;

    loop {
        terminal.draw(|frame| draw(frame, &items, &convs, sel))?;

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
            KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => return Ok(Action::Quit),
            KeyCode::Down | KeyCode::Char('j') => {
                if sel + 1 < convs.len() {
                    sel += 1;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => sel = sel.saturating_sub(1),
            KeyCode::Char('r') => {
                let reg = gather_conversations();
                let (i, c) = build_items(&reg);
                items = i;
                convs = c;
                sel = sel.min(convs.len().saturating_sub(1));
            }
            KeyCode::Enter => {
                if let Some(c) = convs.get(sel) {
                    return Ok(if c.lifecycle.is_actionable_here() {
                        Action::Switch(Box::new(c.clone()))
                    } else {
                        Action::Reopen(Box::new(c.clone()))
                    });
                }
            }
            _ => {}
        }
    }
}

fn draw(frame: &mut ratatui::Frame, items: &[Item], convs: &[Conversation], sel: usize) {
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
    let groups = items
        .iter()
        .filter(|it| matches!(it, Item::Header(_)))
        .count();
    let header = format!(
        " hive conversations — {total} known · {live} live · {} closed · {groups} groups",
        total - live
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            header,
            Style::default().add_modifier(Modifier::BOLD),
        ))),
        chunks[0],
    );

    // Scroll so the selected conversation stays visible.
    let h = chunks[1].height as usize;
    let sel_flat = items
        .iter()
        .position(|it| matches!(it, Item::Conv(i) if *i == sel))
        .unwrap_or(0);
    let offset = if sel_flat >= h { sel_flat + 1 - h } else { 0 };

    let lines: Vec<Line> = items
        .iter()
        .skip(offset)
        .take(h)
        .map(|it| match it {
            Item::Header(parent) => Line::from(Span::styled(
                parent.clone(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Item::Conv(i) => conv_line(&convs[*i], *i == sel),
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), chunks[1]);

    let footer = " ↑/↓ or j/k move · Enter switch/resume · r refresh · q quit";
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            footer,
            Style::default().fg(Color::DarkGray),
        ))),
        chunks[2],
    );
}

fn conv_line(c: &Conversation, selected: bool) -> Line<'static> {
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
        .map(|t| t.chars().take(26).collect::<String>())
        .unwrap_or_default();
    let last = c
        .last_activity
        .as_deref()
        .map(|t| t.chars().take(10).collect::<String>())
        .unwrap_or_default();
    let text = format!(
        "{} {} {:8}  {:<26}  {:<12}  {}  {}",
        if selected { "▶" } else { " " },
        marker,
        short_id(c.id.as_str()),
        title,
        status_label(c),
        shorten_tail(&c.cwd, 44),
        last,
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

/// Keep the tail of a path when it's longer than `max` chars (UTF-8 safe).
fn shorten_tail(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    let tail: String = chars[chars.len() - (max - 1)..].iter().collect();
    format!("…{tail}")
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
            out.push_str(&format!(
                "  {} {:8}  {:<28}  {:<13}  {}  {}\n",
                marker,
                short_id(s.id.as_str()),
                title,
                status_label(s),
                s.cwd,
                last,
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
