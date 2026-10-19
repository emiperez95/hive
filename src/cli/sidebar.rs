//! `hive sidebar` — the attention list as a terminal UI, for a Ghostty split.
//!
//! The terminal-native counterpart of the web sidebar (`/?sidebar=1`), for
//! terminals with no Toolbelt to host a webview. It sits in its own Ghostty
//! split beside the one running `tmux attach`, so it is outside tmux and stays
//! put while the client moves between sessions — the same permanence the
//! Toolbelt had.
//!
//! It draws, and decides nothing: rows, tiers, ranks, held-for ages and pings
//! all come from `/api/active`, computed by the web data thread's one gather.
//! That keeps the order identical to `Ctrl+g` and to the web sidebar, and keeps
//! N sidebars (one per tab) as cheap as N HTTP polls. Focus is pushed, via the
//! same `/api/ambient-wait` long-poll the web page holds.

use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::cli::conversations::fit_cells;
use crate::common::config::HiveConfig;
use crate::common::projects::ProjectRegistry;
use crate::common::terminal::{self, SIDEBAR_TITLE};
use crate::common::tmux;
use crate::ipc::messages::SessionStatus;
use crate::serve::web_types::SessionView;

/// Width the sidebar settles at when it opens as a split, in columns. Matches
/// the web sidebar's ~280px at a typical cell width.
const SIDEBAR_COLS: u16 = 40;

/// One conversation row, already in attention order.
#[derive(Debug, Clone, PartialEq)]
struct Row {
    session: String,
    window_index: String,
    title: String,
    place: String,
    tier: u8,
    status: Option<SessionStatus>,
    age: String,
    unseen: bool,
}

/// Where the tmux client is, from `/api/ambient`.
#[derive(Debug, Clone, Default)]
struct Focus {
    session: Option<String>,
    window_index: Option<String>,
    global_mute: bool,
}

#[derive(Default)]
struct Shared {
    sessions: Option<Vec<SessionView>>,
    focus: Focus,
    /// Set by the fetch threads, cleared by the draw — redraw only on change.
    dirty: bool,
    error: Option<String>,
}

/// Open the sidebar as a split of the current Ghostty tab (if it has none).
/// Returns false when there is no Ghostty to split.
pub fn open_split() -> bool {
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "hive".to_string());
    // A new split starts with Ghostty's launchd PATH, which has no tmux.
    let path = std::env::var("PATH").unwrap_or_default();
    let command = format!(
        "env PATH={} {} sidebar --fit",
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
    let shared = Arc::new(Mutex::new(Shared::default()));
    spawn_fetchers(port, Arc::clone(&shared));

    let mut term = ratatui::try_init().context("hive sidebar: needs a terminal")?;
    crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::SetTitle(SIDEBAR_TITLE),
        crossterm::event::EnableMouseCapture
    )?;
    if fit {
        fit_width();
    }
    let result = sidebar_loop(&mut term, &shared);
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
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

fn spawn_fetchers(port: u16, shared: Arc<Mutex<Shared>>) {
    // The rows: same cadence as the web page's poll.
    let s = Arc::clone(&shared);
    std::thread::spawn(move || loop {
        let fetched = http_get(port, "/api/active", Duration::from_secs(3))
            .ok_or_else(|| format!("no hive web on :{port}"))
            .and_then(|b| serde_json::from_str::<Vec<SessionView>>(&b).map_err(|e| e.to_string()));
        {
            let mut g = s.lock().unwrap();
            match fetched {
                Ok(v) => {
                    g.sessions = Some(v);
                    g.error = None;
                }
                Err(e) => g.error = Some(e),
            }
            g.dirty = true;
        }
        std::thread::sleep(Duration::from_millis(1500));
    });

    // Focus: held open until the client moves (≤25s), answered at once the
    // first time. A failure backs off rather than spinning.
    std::thread::spawn(move || {
        let mut gen: Option<u64> = None;
        loop {
            let path = match gen {
                Some(g) => format!("/api/ambient-wait?gen={g}"),
                None => "/api/ambient".to_string(),
            };
            let Some(body) = http_get(port, &path, Duration::from_secs(30)) else {
                std::thread::sleep(Duration::from_secs(2));
                continue;
            };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) else {
                std::thread::sleep(Duration::from_secs(2));
                continue;
            };
            gen = v["gen"].as_u64();
            let mut g = shared.lock().unwrap();
            g.focus = Focus {
                session: v["session"].as_str().map(str::to_string),
                window_index: v["window_index"].as_str().map(str::to_string),
                global_mute: v["global_mute"].as_bool().unwrap_or(false),
            };
            g.dirty = true;
        }
    });
}

/// Minimal HTTP/1.0 GET against the local dashboard. 1.0 so the response is
/// never chunked and the server closes the connection when it's done.
fn http_get(port: u16, path: &str, timeout: Duration) -> Option<String> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(300)).ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;
    let req = format!("GET {path} HTTP/1.0\r\nHost: localhost\r\n\r\n");
    stream.write_all(req.as_bytes()).ok()?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    let (head, body) = text.split_once("\r\n\r\n")?;
    head.starts_with("HTTP/1.")
        .then_some(())
        .filter(|_| head.split_whitespace().nth(1) == Some("200"))?;
    Some(body.to_string())
}

fn sidebar_loop(term: &mut ratatui::DefaultTerminal, shared: &Arc<Mutex<Shared>>) -> Result<()> {
    let reg = ProjectRegistry::load();
    let mut sel: usize = 0;
    let mut scroll: usize = 0;
    // Screen row → index into `rows`, rebuilt on every draw for mouse hits.
    let mut hit: Vec<Option<usize>> = Vec::new();
    let mut rows: Vec<Row> = Vec::new();
    let mut focus = Focus::default();
    let mut error: Option<String> = None;
    let mut force = true;

    loop {
        {
            let mut g = shared.lock().unwrap();
            if g.dirty || force {
                if let Some(s) = &g.sessions {
                    rows = attention_rows(s, &reg, chrono::Utc::now());
                }
                focus = g.focus.clone();
                error = g.error.clone();
                g.dirty = false;
                force = true;
            }
        }
        if force {
            sel = sel.min(rows.len().saturating_sub(1));
            term.draw(|f| {
                let area = f.area();
                let (lines, map) =
                    render(&rows, &focus, error.as_deref(), sel, area.width as usize);
                // Keep the selection's two lines on screen.
                let height = area.height as usize;
                if let Some(first) = map.iter().position(|m| *m == Some(sel)) {
                    if first < scroll + 1 {
                        scroll = first.saturating_sub(1);
                    } else if first + 2 > scroll + height {
                        scroll = first + 2 - height;
                    }
                }
                scroll = scroll.min(lines.len().saturating_sub(height));
                hit = map.into_iter().skip(scroll).collect();
                f.render_widget(Paragraph::new(lines).scroll((scroll as u16, 0)), area);
            })?;
            force = false;
        }

        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        force = true;
        match event::read()? {
            Event::Key(k) if k.kind == KeyEventKind::Press => match k.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Down | KeyCode::Char('j') => {
                    sel = (sel + 1).min(rows.len().saturating_sub(1))
                }
                KeyCode::Up | KeyCode::Char('k') => sel = sel.saturating_sub(1),
                KeyCode::Enter => {
                    if let Some(r) = rows.get(sel) {
                        switch_to(r, shared);
                    }
                }
                _ => {}
            },
            Event::Mouse(m) => match m.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(Some(i)) = hit.get(m.row as usize) {
                        sel = *i;
                        if let Some(r) = rows.get(*i) {
                            switch_to(r, shared);
                        }
                    }
                }
                MouseEventKind::ScrollDown => scroll += 2,
                MouseEventKind::ScrollUp => scroll = scroll.saturating_sub(2),
                _ => {}
            },
            _ => {}
        }
    }
}

/// Move the tmux client to a row's window, then give the keyboard back to it.
fn switch_to(r: &Row, shared: &Arc<Mutex<Shared>>) {
    let clients = tmux::list_clients();
    if let Some(c) = tmux::pick_client(&clients) {
        tmux::switch_client_to(&c.tty, &r.session, Some(&r.window_index));
    }
    // Optimistic: move the marker now rather than when the push arrives.
    {
        let mut g = shared.lock().unwrap();
        g.focus.session = Some(r.session.clone());
        g.focus.window_index = Some(r.window_index.clone());
        g.dirty = true;
    }
    terminal::focus_work_split();
}

/// The attention view's rows: every ranked window (skipped sessions carry no
/// rank and so never appear), in rank order.
fn attention_rows(
    sessions: &[SessionView],
    reg: &ProjectRegistry,
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<Row> {
    let mut ranked: Vec<(u32, Row)> = sessions
        .iter()
        .filter(|s| !s.skipped)
        .flat_map(|s| {
            s.windows.iter().filter_map(move |w| {
                let rank = w.attention_rank?;
                let named = !w.window_name.is_empty()
                    && !w
                        .window_name
                        .chars()
                        .all(|c| c.is_ascii_digit() || c == '.');
                let title = if named {
                    w.window_name.clone()
                } else {
                    format!("Window {}", w.window_index)
                };
                let held = (w.attention == 0 || w.attention == 3)
                    && w.status != Some(SessionStatus::Waiting);
                let age = if held {
                    w.state_secs.map(|s| dur(u64::from(s))).unwrap_or_default()
                } else {
                    w.last_activity
                        .as_deref()
                        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                        .map(|t| {
                            dur((now - t.with_timezone(&chrono::Utc)).num_seconds().max(0) as u64)
                        })
                        .unwrap_or_default()
                };
                Some((
                    rank,
                    Row {
                        session: s.name.clone(),
                        window_index: w.window_index.clone(),
                        title,
                        place: place_of(&s.name, reg),
                        tier: w.attention,
                        status: w.status.clone(),
                        age,
                        unseen: w.unseen,
                    },
                ))
            })
        })
        .collect();
    ranked.sort_by_key(|(rank, _)| *rank);
    ranked.into_iter().map(|(_, r)| r).collect()
}

/// "Where is this" for a session name: `emoji project · branch` for a worktree
/// session (`{emoji} [{key}] {branch}`), `emoji project` for a project's own.
fn place_of(session: &str, reg: &ProjectRegistry) -> String {
    if let (Some(open), Some(close)) = (session.find('['), session.find(']')) {
        if open < close {
            let key = &session[open + 1..close];
            let branch = session[close + 1..].trim();
            let (emoji, label) = match reg.projects.get(key) {
                Some(p) => (
                    p.emoji.clone(),
                    p.display_name.clone().unwrap_or_else(|| key.to_string()),
                ),
                None => (session[..open].trim().to_string(), key.to_string()),
            };
            let branch = if branch.is_empty() {
                "(worktree)"
            } else {
                branch
            };
            return format!("{emoji} {label} · {branch}").trim().to_string();
        }
    }
    match reg.find_by_session_name(session) {
        Some((key, p)) => format!("{} {}", p.emoji, p.display_name.as_deref().unwrap_or(key)),
        None => session.to_string(),
    }
}

fn dur(s: u64) -> String {
    match s {
        0..=59 => "now".into(),
        60..=3599 => format!("{}m", s / 60),
        3600..=86399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

/// The web sidebar's state words, so both surfaces read the same.
fn state_word(st: &Option<SessionStatus>) -> &'static str {
    match st {
        None => "—",
        Some(SessionStatus::Waiting) => "idle",
        Some(SessionStatus::Working) => "work",
        Some(SessionStatus::QuestionAsked) => "ask",
        Some(SessionStatus::PlanReview) => "plan",
        Some(SessionStatus::NeedsPermission { .. }) => "perm",
        Some(SessionStatus::EditApproval { .. }) => "edit",
        Some(SessionStatus::RunningWorkflow { .. }) => "flow",
        Some(SessionStatus::NeedsInput { .. }) => "needs",
        Some(SessionStatus::Unknown) => "work",
    }
}

fn state_color(st: &Option<SessionStatus>) -> Color {
    match st {
        Some(s) if s.blocks_human() => Color::Yellow,
        Some(SessionStatus::Waiting) => Color::Green,
        Some(SessionStatus::RunningWorkflow { .. }) => Color::Blue,
        Some(_) => Color::Red,
        None => Color::DarkGray,
    }
}

const TIERS: [(&str, Color); 4] = [
    ("BLOCKED", Color::Yellow),
    ("READY FOR REVIEW", Color::Cyan),
    ("IDLE", Color::Green),
    ("WORKING", Color::Red),
];

/// Lines to draw, plus which row each line belongs to (for mouse hits).
fn render(
    rows: &[Row],
    focus: &Focus,
    error: Option<&str>,
    sel: usize,
    width: usize,
) -> (Vec<Line<'static>>, Vec<Option<usize>>) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut map: Vec<Option<usize>> = Vec::new();
    let dim = Style::default().fg(Color::DarkGray);

    let mute = if focus.global_mute { " 🔕" } else { "" };
    lines.push(Line::from(vec![
        Span::styled(" 🐝 hive", Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(format!("  attention{mute}"), dim),
    ]));
    map.push(None);
    if let Some(e) = error {
        lines.push(Line::styled(
            format!(" {e}"),
            Style::default().fg(Color::Red),
        ));
        map.push(None);
    }

    let mut last_tier: Option<u8> = None;
    for (i, r) in rows.iter().enumerate() {
        if last_tier != Some(r.tier) {
            let (label, color) = TIERS
                .get(r.tier as usize)
                .copied()
                .unwrap_or(("OTHER", Color::Gray));
            let n = rows.iter().filter(|x| x.tier == r.tier).count();
            lines.push(Line::raw(""));
            map.push(None);
            lines.push(Line::from(vec![
                Span::styled(
                    format!(" {label}"),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!(" {n}"), dim),
            ]));
            map.push(None);
            last_tier = Some(r.tier);
        }

        let current = focus.session.as_deref() == Some(r.session.as_str())
            && focus.window_index.as_deref() == Some(r.window_index.as_str());
        let base = if i == sel {
            Style::default().bg(Color::Rgb(45, 45, 55))
        } else {
            Style::default()
        };
        let marker = if current {
            Span::styled("▌", base.fg(Color::Blue))
        } else {
            Span::styled(" ", base)
        };
        let ping = if r.unseen && !current { "● " } else { "" };
        let tail = format!("{ping}{:>3} {:<5}", r.age, state_word(&r.status));
        // marker(1) + dot(1) + space(1) + title + space(1) + tail
        let title_w =
            width.saturating_sub(4 + unicode_width::UnicodeWidthStr::width(tail.as_str()));
        let title_style = if current {
            base.add_modifier(Modifier::BOLD)
        } else {
            base
        };
        lines.push(Line::from(vec![
            marker.clone(),
            Span::styled("●", base.fg(state_color(&r.status))),
            Span::styled(" ", base),
            Span::styled(fit_cells(&r.title, title_w), title_style),
            Span::styled(" ", base),
            Span::styled(ping.to_string(), base.fg(Color::Magenta)),
            Span::styled(format!("{:>3} ", r.age), base.fg(Color::DarkGray)),
            Span::styled(
                format!("{:<5}", state_word(&r.status)),
                base.fg(state_color(&r.status)),
            ),
        ]));
        map.push(Some(i));
        lines.push(Line::from(vec![
            marker,
            Span::styled("  ", base),
            Span::styled(
                fit_cells(&r.place, width.saturating_sub(3)),
                base.fg(Color::DarkGray),
            ),
        ]));
        map.push(Some(i));
    }
    if rows.is_empty() && error.is_none() {
        lines.push(Line::raw(""));
        lines.push(Line::styled(" no live conversations", dim));
        map.extend([None, None]);
    }
    (lines, map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::web_types::WindowView;

    fn win(
        idx: &str,
        name: &str,
        tier: u8,
        rank: Option<u32>,
        status: SessionStatus,
    ) -> WindowView {
        serde_json::from_value(serde_json::json!({
            "pane_id": format!("%{idx}"), "window_index": idx, "window_name": name,
            "status": status, "cpu": 0.0, "mem_kb": 0, "ports": [], "cwd": null,
            "last_activity": "2026-10-19T10:00:00+00:00", "pane": null,
            "attention": tier, "attention_rank": rank, "state_secs": 120,
        }))
        .unwrap()
    }

    fn session(name: &str, skipped: bool, windows: Vec<WindowView>) -> SessionView {
        let mut v: SessionView = serde_json::from_value(serde_json::json!({
            "name": name, "status": null, "cpu": 0.0, "mem_kb": 0, "ports": [],
            "processes": [], "cwd": null, "last_activity": null, "attached": false,
            "pane": null, "windows": [], "skipped": skipped,
        }))
        .unwrap();
        v.windows = windows;
        v
    }

    fn now() -> chrono::DateTime<chrono::Utc> {
        "2026-10-19T10:30:00Z".parse().unwrap()
    }

    #[test]
    fn rows_follow_the_server_rank_across_sessions() {
        let s = vec![
            session(
                "🐝 hive",
                false,
                vec![win("1", "a", 2, Some(2), SessionStatus::Waiting)],
            ),
            session(
                "📁 other",
                false,
                vec![
                    win("1", "b", 0, Some(0), SessionStatus::QuestionAsked),
                    win("2", "c", 3, Some(3), SessionStatus::Working),
                ],
            ),
            session(
                "🌳 x",
                false,
                vec![win("1", "d", 1, Some(1), SessionStatus::Waiting)],
            ),
        ];
        let rows = attention_rows(&s, &ProjectRegistry::default(), now());
        let titles: Vec<_> = rows.iter().map(|r| r.title.as_str()).collect();
        assert_eq!(titles, ["b", "d", "a", "c"]);
    }

    // An unranked window is in a skipped session — not a routing target, same
    // rule as `cycle-free`.
    #[test]
    fn skipped_and_unranked_windows_are_left_out() {
        let s = vec![
            session(
                "a",
                true,
                vec![win("1", "skipped", 2, Some(0), SessionStatus::Waiting)],
            ),
            session(
                "b",
                false,
                vec![win("1", "unranked", 2, None, SessionStatus::Waiting)],
            ),
        ];
        assert!(attention_rows(&s, &ProjectRegistry::default(), now()).is_empty());
    }

    // Working and blocked rows show how long they've held the state (their
    // last_activity is always "now"); idle rows show how long since activity.
    #[test]
    fn age_is_held_time_for_working_and_blocked_else_last_activity() {
        let s = vec![session(
            "s",
            false,
            vec![
                win("1", "busy", 3, Some(0), SessionStatus::Working),
                win("2", "idle", 2, Some(1), SessionStatus::Waiting),
            ],
        )];
        let rows = attention_rows(&s, &ProjectRegistry::default(), now());
        assert_eq!(rows[0].age, "2m");
        assert_eq!(rows[1].age, "30m");
    }

    #[test]
    fn numeric_window_names_get_a_readable_title() {
        let s = vec![session(
            "s",
            false,
            vec![win("3", "2.1.45", 2, Some(0), SessionStatus::Waiting)],
        )];
        assert_eq!(
            attention_rows(&s, &ProjectRegistry::default(), now())[0].title,
            "Window 3"
        );
    }

    #[test]
    fn place_names_worktree_project_and_branch() {
        let reg = ProjectRegistry::default();
        assert_eq!(
            place_of("📊 [avateen] live-avatar", &reg),
            "📊 avateen · live-avatar"
        );
        assert_eq!(place_of("📊 [avateen]", &reg), "📊 avateen · (worktree)");
        assert_eq!(place_of("🐝 hive", &reg), "🐝 hive");
    }

    #[test]
    fn state_words_match_the_web_sidebar() {
        assert_eq!(state_word(&Some(SessionStatus::Waiting)), "idle");
        assert_eq!(
            state_word(&Some(SessionStatus::NeedsInput { reason: None })),
            "needs"
        );
        assert_eq!(state_word(&None), "—");
    }

    #[test]
    fn every_row_line_maps_back_to_its_row() {
        let rows = attention_rows(
            &[session(
                "s",
                false,
                vec![
                    win("1", "a", 0, Some(0), SessionStatus::QuestionAsked),
                    win("2", "b", 2, Some(1), SessionStatus::Waiting),
                ],
            )],
            &ProjectRegistry::default(),
            now(),
        );
        let (lines, map) = render(&rows, &Focus::default(), None, 0, 40);
        assert_eq!(lines.len(), map.len());
        assert_eq!(map.iter().filter(|m| **m == Some(0)).count(), 2);
        assert_eq!(map.iter().filter(|m| **m == Some(1)).count(), 2);
    }
}
