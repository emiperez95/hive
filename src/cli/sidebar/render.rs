//! Drawing. Every draw also records what is clickable where (`App::hits`), so
//! mouse handling never re-derives the layout.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use super::model::{self, Item, Sect, View};
use super::{App, Ctl, DlgHit, Hit, KeyFocus, Pane, TodoBtn, TodoLine};
use crate::cli::conversations::fit_cells;
use crate::ipc::messages::SessionStatus;

/// The list never shrinks below this many rows: it is the reason the panel
/// exists, and a pane dragged over it would hide the thing you switch with.
const LIST_MIN: u16 = 6;

const DIM: Style = Style::new().fg(Color::DarkGray);
const HEAD_BG: Color = Color::Rgb(38, 38, 50);
const SEL_BG: Color = Color::Rgb(45, 45, 58);

/// Lines plus, per line, the clickable spans on it.
#[derive(Default)]
struct Out {
    lines: Vec<Line<'static>>,
    hits: Vec<Vec<(u16, u16, Hit)>>,
}

impl Out {
    fn push(&mut self, line: Line<'static>, hits: Vec<(u16, u16, Hit)>) {
        self.lines.push(line);
        self.hits.push(hits);
    }
}

/// Split the rows below the header between the list and the panes: each pane
/// gets its header plus its body height (0 when collapsed), shrinking the
/// tallest bodies first until the list keeps [`LIST_MIN`] rows.
pub(crate) fn layout(height: u16, panes: &[(bool, u16)]) -> (u16, Vec<u16>) {
    let avail = height.saturating_sub(1);
    let heads = panes.len() as u16;
    let mut bodies: Vec<u16> = panes
        .iter()
        .map(|&(collapsed, want)| if collapsed { 0 } else { want.max(1) })
        .collect();
    while heads + bodies.iter().sum::<u16>() + LIST_MIN > avail {
        match bodies.iter_mut().filter(|b| **b > 1).max_by_key(|b| **b) {
            Some(b) => *b -= 1,
            None => break,
        }
    }
    (
        avail.saturating_sub(heads + bodies.iter().sum::<u16>()),
        bodies,
    )
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
    ("NEEDS YOU", Color::Yellow),
    ("READY FOR REVIEW", Color::Cyan),
    ("IDLE", Color::Green),
    ("WORKING", Color::Red),
];

fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

impl App {
    pub(crate) fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        let (w, h) = (area.width as usize, area.height);
        self.hits = vec![Vec::new(); h as usize];

        let has_conv = self.focused_conv().is_some();
        let mut panes = vec![(Pane::Details, self.prefs.details_collapsed || !has_conv)];
        if has_conv {
            panes.push((Pane::Notes, self.prefs.notes_collapsed));
        }
        if self.todos_session.is_some() {
            panes.push((Pane::Todos, self.prefs.todos_collapsed));
        }
        let spec: Vec<(bool, u16)> = panes
            .iter()
            .map(|&(p, c)| (c, self.prefs.height(p)))
            .collect();
        let (list_h, bodies) = layout(h, &spec);
        self.list_rows = list_h as usize;
        self.pane_max = h.saturating_sub(1 + panes.len() as u16 + LIST_MIN).max(1);

        self.draw_header(f, Rect::new(area.x, area.y, area.width, 1.min(h)));
        self.draw_list(f, Rect::new(area.x, area.y + 1, area.width, list_h));

        let mut y = 1 + list_h;
        for (i, &(pane, _)) in panes.iter().enumerate() {
            if y >= h {
                break;
            }
            self.draw_pane_head(f, pane, Rect::new(area.x, area.y + y, area.width, 1));
            let body = Rect::new(area.x, area.y + y + 1, area.width, bodies[i].min(h - y - 1));
            if body.height > 0 {
                match pane {
                    Pane::Details => self.draw_details(f, body),
                    Pane::Notes => self.draw_notes(f, body),
                    Pane::Todos => self.draw_todos(f, body),
                }
            }
            y += 1 + bodies[i];
        }

        if self.dialog.is_some() {
            self.draw_dialog(f, area, w);
        }
    }

    /// Copy region hits into the screen-row map.
    fn place_hits(&mut self, top: u16, rows: Vec<Vec<(u16, u16, Hit)>>) {
        for (i, row) in rows.into_iter().enumerate() {
            if let Some(slot) = self.hits.get_mut(top as usize + i) {
                *slot = row;
            }
        }
    }

    fn draw_header(&mut self, f: &mut Frame, r: Rect) {
        if r.height == 0 {
            return;
        }
        let w = r.width;
        let live: Vec<_> = self
            .convs
            .iter()
            .filter(|c| !c.skipped && c.window_index.is_some())
            .collect();
        let need = live.iter().filter(|c| c.blocks()).count();
        let title = if self.prefs.view == View::Projects {
            "projects".to_string()
        } else if need > 0 {
            format!("{} live · {need} need you", live.len())
        } else {
            format!("{} live", live.len())
        };

        // Right to left: mute, projects, attention, archived. Archived is the
        // leftmost so it's the only one that moves when it appears — the view
        // toggles are the way back and must stay put (same rule as the web).
        let on = |b: bool| {
            if b {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                DIM
            }
        };
        let view = self.prefs.view;
        let mut ctls: Vec<(String, Style, Ctl)> = vec![
            (
                if self.focus.global_mute {
                    "🔕"
                } else {
                    "🔔"
                }
                .into(),
                if self.focus.global_mute {
                    Style::default().fg(Color::Red)
                } else {
                    Style::default()
                },
                Ctl::Mute,
            ),
            (" P ".into(), on(view == View::Projects), Ctl::Projects),
            (" ! ".into(), on(view == View::Attention), Ctl::Attention),
        ];
        if view == View::Projects {
            ctls.push((" A ".into(), on(self.prefs.show_archived), Ctl::Archived));
        }
        let mut x = w;
        let mut placed = Vec::new();
        let mut hits = Vec::new();
        for (label, style, ctl) in ctls {
            let cw = width(&label) as u16;
            x = x.saturating_sub(cw + 1);
            hits.push((x, x + cw, Hit::Ctl(ctl)));
            placed.push((x, label, style));
        }
        placed.reverse();
        let left = format!(" 🐝 {title}");
        let left = fit_cells(&left, (x as usize).saturating_sub(1));
        let mut spans = vec![Span::styled(
            left,
            Style::default().add_modifier(Modifier::BOLD),
        )];
        let mut cur = x.saturating_sub(1) as usize;
        cur = cur.max(spans[0].width());
        for (px, label, style) in placed {
            if (px as usize) > cur {
                spans.push(Span::raw(" ".repeat(px as usize - cur)));
            }
            cur = px as usize + width(&label);
            spans.push(Span::styled(label, style));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), r);
        self.place_hits(r.y, vec![hits]);
    }

    fn draw_list(&mut self, f: &mut Frame, r: Rect) {
        if r.height == 0 {
            return;
        }
        let w = r.width as usize;
        let items = self.items();
        let sel = self.sel_index(&items);
        let hover_row = self.hover.map(|(_, y, _)| y);
        let mut out = Out::default();

        if let Some(e) = self
            .error
            .clone()
            .or_else(|| self.flash.as_ref().map(|f| f.0.clone()))
        {
            out.push(
                Line::styled(format!(" {e}"), Style::default().fg(Color::Red)),
                vec![],
            );
        }
        let mut sel_lines = (0, 0);
        for (i, item) in items.iter().enumerate() {
            let first = out.lines.len();
            let selected =
                Some(i) == sel && self.key_focus == KeyFocus::List && self.dialog.is_none();
            let row_y = |n: usize| r.y as usize + n.saturating_sub(self.list_scroll);
            let hovered = hover_row.is_some_and(|y| y as usize == row_y(first));
            self.render_item(&mut out, i, item, selected, hovered, w);
            if Some(i) == sel {
                sel_lines = (first, out.lines.len());
            }
        }

        // Keep a keyboard-moved selection on screen; leave the pointer alone.
        let rows = r.height as usize;
        if self.follow_sel {
            let (a, b) = sel_lines;
            if a < self.list_scroll {
                self.list_scroll = a.saturating_sub(1);
            } else if b > self.list_scroll + rows {
                self.list_scroll = b - rows;
            }
            self.follow_sel = false;
        }
        self.list_scroll = self.list_scroll.min(out.lines.len().saturating_sub(rows));
        let scroll = self.list_scroll;
        let hits: Vec<_> = out.hits.into_iter().skip(scroll).take(rows).collect();
        f.render_widget(Paragraph::new(out.lines).scroll((scroll as u16, 0)), r);
        self.place_hits(r.y, hits);
    }

    fn render_item(
        &self,
        out: &mut Out,
        i: usize,
        item: &Item,
        selected: bool,
        hovered: bool,
        w: usize,
    ) {
        let base = if selected {
            Style::default().bg(SEL_BG)
        } else {
            Style::default()
        };
        let whole = |w: usize| vec![(0u16, w as u16, Hit::Item(i))];
        let bell = |key: &str| -> (Span<'static>, Option<(u16, u16, Hit)>) {
            let muted = self.focus.muted_projects.iter().any(|k| k == key);
            let span = if muted {
                Span::styled("🔕", Style::default().fg(Color::Red))
            } else if hovered {
                Span::raw("🔔")
            } else {
                Span::raw("  ")
            };
            (
                span,
                Some((
                    w.saturating_sub(3) as u16,
                    w as u16,
                    Hit::Mute(key.to_string()),
                )),
            )
        };
        match item {
            Item::Note(s) => out.push(Line::styled(format!(" {s}"), DIM), vec![]),
            Item::Blank => out.push(Line::raw(""), vec![]),
            Item::Tier { tier, n } => {
                let (label, color) = TIERS
                    .get(*tier as usize)
                    .copied()
                    .unwrap_or(("OTHER", Color::Gray));
                out.push(
                    Line::from(vec![
                        Span::styled(
                            format!(" {label}"),
                            Style::default().fg(color).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(format!(" {n}"), DIM),
                    ]),
                    vec![],
                );
            }
            Item::Group { emoji, label, key } => {
                let (b, hit) = bell(key);
                let text = format!(" {} {}", if emoji.is_empty() { "💻" } else { emoji }, label);
                out.push(
                    Line::from(vec![
                        Span::styled(
                            fit_cells(&text, w.saturating_sub(3)),
                            Style::default().add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(" "),
                        b,
                    ]),
                    hit.into_iter().collect(),
                );
            }
            Item::Worktree(b) => {
                out.push(
                    Line::styled(
                        fit_cells(&format!("   · {b}"), w),
                        Style::default().fg(Color::Cyan),
                    ),
                    vec![],
                );
            }
            Item::Section { sect, n, open } => {
                let label = match sect {
                    Sect::Skipped => "Skipped",
                    Sect::Frozen => "Frozen",
                };
                let caret = if *open { "▾" } else { "▸" };
                out.push(
                    Line::styled(
                        fit_cells(&format!(" {caret} {label} ({n})"), w),
                        base.fg(Color::Gray),
                    ),
                    whole(w),
                );
            }
            Item::ProjectHead {
                key,
                emoji,
                label,
                archived,
                count,
                open,
            } => {
                let (b, hit) = bell(key);
                let caret = if *open { "▾" } else { "▸" };
                let tail = match (archived, count) {
                    (true, 0) => " arch".to_string(),
                    (true, n) => format!(" arch {n}"),
                    (false, 0) => String::new(),
                    (false, n) => format!(" {n}"),
                };
                let head = format!(
                    " {caret} {} {label}",
                    if emoji.is_empty() { "💻" } else { emoji }
                );
                let head_w = w.saturating_sub(3 + width(&tail));
                let style = if *archived {
                    base.fg(Color::DarkGray)
                } else {
                    base
                };
                let mut hits = whole(w.saturating_sub(3));
                hits.extend(hit);
                out.push(
                    Line::from(vec![
                        Span::styled(fit_cells(&head, head_w), style.add_modifier(Modifier::BOLD)),
                        Span::styled(tail, base.fg(Color::DarkGray)),
                        Span::styled(" ", base),
                        b,
                    ]),
                    hits,
                );
            }
            Item::Conv { idx, flat, indent } => {
                let Some(c) = self.convs.get(*idx) else {
                    return;
                };
                let current = self.is_current(c);
                let marker = if current {
                    Span::styled("▌", base.fg(Color::Blue))
                } else {
                    Span::styled(" ", base)
                };
                let pad = " ".repeat(*indent as usize);
                let ping = if c.unseen && !current { "● " } else { "" };
                let word = model::state_word(&c.status);
                let tail_w = width(ping) + 3 + 1 + 5;
                let title_w = w.saturating_sub(1 + pad.len() + 2 + 1 + tail_w);
                let title_style = if current {
                    base.add_modifier(Modifier::BOLD)
                } else {
                    base
                };
                out.push(
                    Line::from(vec![
                        marker.clone(),
                        Span::styled(pad.clone(), base),
                        Span::styled("●", base.fg(state_color(&c.status))),
                        Span::styled(" ", base),
                        Span::styled(fit_cells(&c.title, title_w), title_style),
                        Span::styled(" ", base),
                        Span::styled(ping.to_string(), base.fg(Color::Magenta)),
                        Span::styled(format!("{:>3} ", c.age), base.fg(Color::DarkGray)),
                        Span::styled(format!("{word:<5}"), base.fg(state_color(&c.status))),
                    ]),
                    whole(w),
                );
                // The attention view is ungrouped, so each row says where it lives.
                if *flat {
                    out.push(
                        Line::from(vec![
                            marker,
                            Span::styled("  ", base),
                            Span::styled(
                                fit_cells(&c.place.line(), w.saturating_sub(3)),
                                base.fg(Color::DarkGray),
                            ),
                        ]),
                        whole(w),
                    );
                }
            }
            Item::Frozen { idx, indent } => {
                let Some(fz) = self.frozen.get(*idx) else {
                    return;
                };
                // The note is why you parked it — that's the headline.
                let title = [&fz.note, &fz.window_label, &fz.session_name]
                    .into_iter()
                    .find(|s| !s.is_empty())
                    .cloned()
                    .unwrap_or_default();
                let age = fz.relative.trim_end_matches(" ago").to_string();
                let pad = " ".repeat(*indent as usize + 1);
                let title_w = w.saturating_sub(pad.len() + 3 + 1 + width(&age) + 1);
                out.push(
                    Line::from(vec![
                        Span::styled(pad, base),
                        Span::styled("💤 ", base),
                        Span::styled(fit_cells(&title, title_w), base),
                        Span::styled(format!(" {age} "), base.fg(Color::DarkGray)),
                    ]),
                    whole(w),
                );
            }
        }
    }

    fn draw_pane_head(&mut self, f: &mut Frame, pane: Pane, r: Rect) {
        let w = r.width as usize;
        let has_conv = self.focused_conv().is_some();
        let collapsed = self.prefs.collapsed(pane) || (pane == Pane::Details && !has_conv);
        let focused = matches!(
            (pane, self.key_focus),
            (Pane::Notes, KeyFocus::Notes) | (Pane::Todos, KeyFocus::Todos)
        );
        let (name, extra) = match pane {
            Pane::Details => (
                "Details".to_string(),
                if has_conv {
                    String::new()
                } else {
                    "no conversation here".into()
                },
            ),
            Pane::Notes => ("Notes".to_string(), self.notes_state.clone()),
            Pane::Todos => {
                let place = self
                    .todos_session
                    .as_deref()
                    .map(|s| {
                        let p = model::place_of(s, &self.projects);
                        match p.branch {
                            Some(b) => format!("{} · {b}", p.label),
                            None => p.label,
                        }
                    })
                    .unwrap_or_default();
                let n = self.todos.as_ref().map_or(0, |t| t.todos.len());
                (
                    format!("Todos · {place}"),
                    if n > 0 { n.to_string() } else { String::new() },
                )
            }
        };
        let caret = if collapsed { "▸" } else { "▾" };
        let bg = Style::default().bg(HEAD_BG);
        let name_style = if focused {
            bg.fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            bg.add_modifier(Modifier::BOLD)
        };
        let extra_w = width(&extra);
        let name_w = w.saturating_sub(3 + extra_w + 1);
        let line = Line::from(vec![
            Span::styled(format!(" {caret} "), bg.fg(Color::Gray)),
            Span::styled(fit_cells(&name, name_w), name_style),
            Span::styled(format!("{extra} "), bg.fg(Color::DarkGray)),
        ]);
        f.render_widget(Paragraph::new(line).style(bg), r);
        self.place_hits(r.y, vec![vec![(0, r.width, Hit::PaneHead(pane))]]);
    }

    fn draw_details(&mut self, f: &mut Frame, r: Rect) {
        let w = r.width as usize;
        let mut lines: Vec<Line<'static>> = Vec::new();
        let Some(conv) = self.focused_conv().cloned() else {
            return;
        };
        lines.push(Line::styled(
            fit_cells(&format!(" {}", conv.title), w),
            Style::default().add_modifier(Modifier::BOLD),
        ));
        lines.push(Line::styled(
            fit_cells(&format!(" {}", conv.place.line()), w),
            DIM,
        ));
        match &self.stats {
            None => lines.push(Line::styled(" Reading transcript…", DIM)),
            Some(s) if s.error.is_some() => lines.push(Line::styled(
                format!(" {}", s.error.clone().unwrap_or_default()),
                DIM,
            )),
            Some(s) => {
                // Model | tokens | cost columns. Every row carries a cost cell,
                // blank when unpriced, so the token column scans straight down.
                let row = |name: String, tok: u64, cost: Option<f64>, style: Style| {
                    let cost = cost.map(model::fmt_cost).unwrap_or_default();
                    let name_w = w.saturating_sub(1 + 7 + 8);
                    Line::styled(
                        format!(
                            " {}{:>7}{:>8}",
                            fit_cells(&name, name_w),
                            model::fmt_tokens(tok),
                            cost
                        ),
                        style,
                    )
                };
                for (m, t) in &s.usage.main {
                    lines.push(row(
                        model::short_model(m).to_string(),
                        t.sum(),
                        s.cost_main.get(m).copied(),
                        Style::default(),
                    ));
                }
                for (m, t) in &s.usage.sidechain {
                    lines.push(row(
                        format!("↳ {}", model::short_model(m)),
                        t.sum(),
                        s.cost_sidechain.get(m).copied(),
                        Style::default().fg(Color::Gray),
                    ));
                }
                if s.usage.main.is_empty() && s.usage.sidechain.is_empty() {
                    lines.push(Line::styled(" No usage recorded yet.", DIM));
                } else {
                    lines.push(row(
                        "total".into(),
                        s.totals.sum(),
                        s.cost_usd,
                        Style::default().add_modifier(Modifier::BOLD),
                    ));
                }
                // An unpriced model is named, never silently treated as free.
                if !s.unpriced_models.is_empty() {
                    let names: Vec<&str> = s
                        .unpriced_models
                        .iter()
                        .map(|m| model::short_model(m))
                        .collect();
                    let text = format!(
                        "No rate for {} — add [pricing] to ~/.hive/config.toml for cost.",
                        names.join(", ")
                    );
                    for chunk in wrap_words(&text, w.saturating_sub(2)) {
                        lines.push(Line::styled(format!(" {chunk}"), DIM));
                    }
                }
                if let Some(g) = &s.worktree {
                    lines.push(Line::raw(""));
                    let branch = if g.detached {
                        "detached HEAD"
                    } else {
                        &g.branch
                    };
                    lines.push(Line::from(vec![
                        Span::styled(" ⑂ ", DIM),
                        Span::styled(
                            branch.to_string(),
                            Style::default().add_modifier(Modifier::BOLD),
                        ),
                    ]));
                    // Only non-zero counts render, so a clean landed tree is a
                    // branch and a `merged` chip, not a row of zeros.
                    let work = Style::default().fg(Color::Yellow);
                    let mut chips: Vec<Span<'static>> = vec![Span::raw(" ")];
                    let mut chip = |text: String, style: Style| {
                        chips.push(Span::styled(text, style));
                        chips.push(Span::raw(" "));
                    };
                    if g.dirty > 0 {
                        chip(format!("{} dirty", g.dirty), work);
                    }
                    if g.untracked > 0 {
                        chip(format!("{} new", g.untracked), work);
                    }
                    if g.ahead > 0 {
                        chip(format!("{} ahead", g.ahead), work);
                    }
                    if g.behind > 0 {
                        chip(format!("{} behind", g.behind), DIM);
                    }
                    if g.dirty == 0 && g.untracked == 0 && g.ahead == 0 {
                        if g.merged {
                            chip("merged".into(), Style::default().fg(Color::Green));
                        } else {
                            chip("clean".into(), DIM);
                        }
                    }
                    lines.push(Line::from(chips));
                    let age = g
                        .last_commit_unix
                        .map(|t| model::dur((chrono::Utc::now().timestamp() - t).max(0) as u64));
                    let base: Vec<String> = [
                        g.base.as_ref().map(|b| format!("base {b}")),
                        age.map(|a| format!("last commit {a}")),
                    ]
                    .into_iter()
                    .flatten()
                    .collect();
                    if !base.is_empty() {
                        lines.push(Line::styled(
                            fit_cells(&format!(" {}", base.join(" · ")), w),
                            DIM,
                        ));
                    }
                }
            }
        }
        let rows = r.height as usize;
        self.details_scroll = self.details_scroll.min(lines.len().saturating_sub(rows));
        f.render_widget(
            Paragraph::new(lines).scroll((self.details_scroll as u16, 0)),
            r,
        );
    }

    fn draw_notes(&mut self, f: &mut Frame, r: Rect) {
        let tw = (r.width as usize).saturating_sub(2).max(1);
        self.notes_width = tw;
        let rows = r.height as usize;
        let focused = self.key_focus == KeyFocus::Notes && self.dialog.is_none();
        if self.notes_loading {
            f.render_widget(Paragraph::new(Line::styled(" loading…", DIM)), r);
            return;
        }
        let wrapped = self.notes.wrap(tw);
        let (crow, ccol) = self.notes.cursor_pos(tw);
        if focused {
            if crow < self.notes_scroll {
                self.notes_scroll = crow;
            } else if crow >= self.notes_scroll + rows {
                self.notes_scroll = crow + 1 - rows;
            }
        }
        self.notes_scroll = self.notes_scroll.min(wrapped.len().saturating_sub(rows));
        let lines: Vec<Line<'static>> = if self.notes.is_empty() && !focused {
            vec![Line::styled(" Click to add notes…", DIM)]
        } else {
            wrapped
                .iter()
                .skip(self.notes_scroll)
                .take(rows)
                .map(|&l| Line::raw(format!(" {}", self.notes.line_text(l))))
                .collect()
        };
        f.render_widget(Paragraph::new(lines), r);
        let hits = (0..rows)
            .map(|i| vec![(0, r.width, Hit::Notes(self.notes_scroll + i, 0))])
            .collect();
        self.place_hits(r.y, hits);
        if focused && crow >= self.notes_scroll {
            f.set_cursor_position((
                r.x + 1 + ccol as u16,
                r.y + (crow - self.notes_scroll) as u16,
            ));
        }
    }

    fn draw_todos(&mut self, f: &mut Frame, r: Rect) {
        let w = r.width as usize;
        let focused = self.key_focus == KeyFocus::Todos && self.dialog.is_none();
        let mut out = Out::default();
        // Which visual line each logical (selectable) line is on.
        let mut logical_at: Vec<usize> = Vec::new();
        let mut cursor: Option<(u16, usize)> = None;
        let todos = self.todos.clone().unwrap_or_default();

        for i in 0..self.todo_lines() {
            let selected = focused && i == self.todo_sel;
            let base = if selected {
                Style::default().bg(SEL_BG)
            } else {
                Style::default()
            };
            logical_at.push(out.lines.len());
            match self.todo_line(i) {
                TodoLine::Add => {
                    let text = self.todo_input.text();
                    let (shown, style) = if text.is_empty() && !selected {
                        ("Add a todo…".to_string(), base.fg(Color::DarkGray))
                    } else {
                        (text, base)
                    };
                    if selected {
                        cursor = Some((out.lines.len() as u16, 3 + width(&self.todo_input.text())));
                    }
                    out.push(
                        Line::from(vec![
                            Span::styled(" + ", base.fg(Color::Cyan)),
                            Span::styled(fit_cells(&shown, w.saturating_sub(3)), style),
                        ]),
                        vec![(0, w as u16, Hit::TodoAdd)],
                    );
                    if todos.todos.is_empty() && self.todos.is_some() {
                        out.push(Line::styled("   Nothing to do", DIM), vec![]);
                    }
                }
                TodoLine::Item(n) => {
                    let text = todos.todos.get(n).cloned().unwrap_or_default();
                    let tw = w.saturating_sub(3 + 5);
                    out.push(
                        Line::from(vec![
                            Span::styled(" ○ ", base.fg(Color::Gray)),
                            Span::styled(fit_cells(&text, tw), base),
                            Span::styled(" × ", base.fg(Color::DarkGray)),
                            Span::styled("▶ ", base.fg(Color::Cyan)),
                        ]),
                        vec![
                            (0, 3, Hit::Todo(n, TodoBtn::Check)),
                            (
                                w.saturating_sub(5) as u16,
                                w.saturating_sub(2) as u16,
                                Hit::Todo(n, TodoBtn::Delete),
                            ),
                            (
                                w.saturating_sub(2) as u16,
                                w as u16,
                                Hit::Todo(n, TodoBtn::Go),
                            ),
                            (3, w.saturating_sub(5) as u16, Hit::Todo(n, TodoBtn::Text)),
                        ],
                    );
                }
                TodoLine::DoneToggle => {
                    let caret = if self.prefs.done_open { "▾" } else { "▸" };
                    out.push(
                        Line::styled(
                            format!(" {caret} Done ({})", todos.done.len()),
                            base.fg(Color::Gray),
                        ),
                        vec![(0, w as u16, Hit::DoneToggle)],
                    );
                }
                TodoLine::Done(n) => {
                    let text = todos.done.get(n).cloned().unwrap_or_default();
                    out.push(
                        Line::from(vec![
                            Span::styled(" ● ", base.fg(Color::Green)),
                            Span::styled(
                                fit_cells(&text, w.saturating_sub(3)),
                                base.fg(Color::DarkGray).add_modifier(Modifier::CROSSED_OUT),
                            ),
                        ]),
                        vec![(0, w as u16, Hit::Done(n))],
                    );
                }
            }
        }

        let rows = r.height as usize;
        if focused {
            let at = logical_at.get(self.todo_sel).copied().unwrap_or(0);
            if at < self.todos_scroll {
                self.todos_scroll = at;
            } else if at >= self.todos_scroll + rows {
                self.todos_scroll = at + 1 - rows;
            }
        }
        self.todos_scroll = self.todos_scroll.min(out.lines.len().saturating_sub(rows));
        let scroll = self.todos_scroll;
        let hits: Vec<_> = out.hits.into_iter().skip(scroll).take(rows).collect();
        f.render_widget(Paragraph::new(out.lines).scroll((scroll as u16, 0)), r);
        self.place_hits(r.y, hits);
        if let Some((line, col)) = cursor {
            let line = line as usize;
            if line >= scroll && line < scroll + rows {
                f.set_cursor_position((
                    r.x + col.min(w.saturating_sub(1)) as u16,
                    r.y + (line - scroll) as u16,
                ));
            }
        }
    }

    fn draw_dialog(&mut self, f: &mut Frame, area: Rect, w: usize) {
        let Some(d) = self.dialog.as_ref() else {
            return;
        };
        let inner_w = w.saturating_sub(4).max(10);
        let mut out = Out::default();
        let running = d.job.is_some();
        for chunk in wrap_words(&d.text, inner_w).into_iter().take(3) {
            out.push(
                Line::styled(chunk, Style::default().add_modifier(Modifier::BOLD)),
                vec![],
            );
        }
        out.push(Line::raw(""), vec![]);

        let place = model::place_of(&d.session, &self.projects);
        let where_ = match place.branch {
            Some(b) => format!("{} · {b}", place.label),
            None => place.label,
        };
        let send_why = match &d.conv {
            None => "No conversation in this window".to_string(),
            Some((_, _, true)) => "Waiting on a decision — answer it first".to_string(),
            Some((_, title, false)) => title.clone(),
        };
        let wt_why = match &d.project {
            None => "Checking…".to_string(),
            Some(None) => "This session isn't a registered project".to_string(),
            Some(Some(p)) => format!("Project {p}"),
        };
        let opts = [
            (
                "1",
                "Send to this conversation",
                send_why,
                !matches!(d.conv, Some((_, _, false))),
                DlgHit::Send,
            ),
            ("2", "New conversation here", where_, false, DlgHit::New),
            (
                "3",
                "New worktree",
                wt_why,
                !matches!(d.project, Some(Some(_))),
                DlgHit::Worktree,
            ),
        ];
        for (n, title, sub, disabled, hit) in opts {
            let style = if disabled || running {
                DIM
            } else {
                Style::default()
            };
            let mark = if hit == DlgHit::Worktree && d.wt_open {
                "▾"
            } else {
                n
            };
            out.push(
                Line::from(vec![
                    Span::styled(format!("{mark} "), Style::default().fg(Color::Cyan)),
                    Span::styled(
                        fit_cells(title, inner_w.saturating_sub(2)),
                        style.add_modifier(Modifier::BOLD),
                    ),
                ]),
                vec![(0, inner_w as u16, Hit::Dlg(hit))],
            );
            out.push(
                Line::styled(
                    format!("  {}", fit_cells(&sub, inner_w.saturating_sub(2))),
                    DIM,
                ),
                vec![(0, inner_w as u16, Hit::Dlg(hit))],
            );
        }

        let mut cursor = None;
        if d.wt_open {
            out.push(Line::raw(""), vec![]);
            let label = "branch: ";
            let text = d.branch.text();
            if !running && !d.suggesting {
                cursor = Some((out.lines.len(), label.len() + width(&text)));
            }
            out.push(
                Line::from(vec![
                    Span::styled(label, DIM),
                    Span::styled(
                        fit_cells(&text, inner_w.saturating_sub(label.len())),
                        Style::default().fg(Color::Yellow),
                    ),
                ]),
                vec![(0, inner_w as u16, Hit::Dlg(DlgHit::Branch))],
            );
            let create_style = if running || d.suggesting {
                DIM
            } else {
                Style::default().add_modifier(Modifier::REVERSED)
            };
            out.push(
                Line::from(vec![
                    Span::styled(" Create ", create_style),
                    Span::styled("  Enter", DIM),
                ]),
                vec![(0, 8, Hit::Dlg(DlgHit::Create))],
            );
            let src = if d.suggesting {
                "Suggesting a name…"
            } else {
                match d.source.as_deref() {
                    Some("ticket") => "From the ticket key in the todo",
                    Some("model") => "Suggested by Haiku from your existing branches — edit freely",
                    Some(_) => "From the todo's words (the model didn't answer)",
                    None => "",
                }
            };
            for chunk in wrap_words(src, inner_w) {
                out.push(Line::styled(chunk, DIM), vec![]);
            }
        }
        if let Some((msg, err)) = &d.msg {
            out.push(Line::raw(""), vec![]);
            let style = if *err {
                Style::default().fg(Color::Red)
            } else {
                Style::default()
            };
            for chunk in wrap_words(msg, inner_w) {
                out.push(Line::styled(chunk, style), vec![]);
            }
        }
        if d.landed.is_some() {
            out.push(
                Line::from(vec![
                    Span::styled(" Go ", Style::default().add_modifier(Modifier::REVERSED)),
                    Span::styled("  Enter", DIM),
                ]),
                vec![(0, 4, Hit::Dlg(DlgHit::Go))],
            );
        }
        out.push(Line::styled("Esc close · 1 2 3 pick", DIM), vec![]);

        let bh = (out.lines.len() as u16 + 2).min(area.height);
        let bw = (inner_w as u16 + 2).min(area.width);
        let bx = area.x + (area.width - bw) / 2;
        let by = area.y + (area.height - bh) / 2;
        let rect = Rect::new(bx, by, bw, bh);
        f.render_widget(Clear, rect);
        let block = Block::bordered()
            .title(" Start todo ")
            .border_style(Style::default().fg(Color::Cyan));
        let inner = block.inner(rect);
        f.render_widget(block, rect);
        f.render_widget(Paragraph::new(out.lines), inner);

        // Inside the box nothing falls through to what's drawn underneath.
        for y in rect.y..rect.y + rect.height {
            if let Some(row) = self.hits.get_mut(y as usize) {
                row.clear();
            }
        }
        for (i, row) in out.hits.into_iter().enumerate() {
            let y = inner.y as usize + i;
            if y >= (inner.y + inner.height) as usize {
                break;
            }
            if let Some(slot) = self.hits.get_mut(y) {
                slot.extend(
                    row.into_iter()
                        .map(|(a, b, h)| (inner.x + a, inner.x + b, h)),
                );
            }
        }
        // The close mark sits on the top border, right end.
        if let Some(row) = self.hits.get_mut(rect.y as usize) {
            row.push((
                rect.x + rect.width.saturating_sub(4),
                rect.x + rect.width,
                Hit::Dlg(DlgHit::Close),
            ));
        }
        for y in rect.y..rect.y + rect.height {
            if let Some(row) = self.hits.get_mut(y as usize) {
                row.push((rect.x, rect.x + rect.width, Hit::Dlg(DlgHit::Inside)));
            }
        }
        let close = Rect::new(rect.x + rect.width.saturating_sub(4), rect.y, 3, 1);
        f.render_widget(
            Paragraph::new(Span::styled(" × ", Style::default().fg(Color::Cyan))),
            close,
        );
        if let Some((line, col)) = cursor {
            if line < inner.height as usize {
                f.set_cursor_position((inner.x + col as u16, inner.y + line as u16));
            }
        }
    }
}

/// Greedy word wrap by display width; a word longer than the width is cut.
pub(crate) fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let need = if cur.is_empty() {
            width_of(word)
        } else {
            width_of(&cur) + 1 + width_of(word)
        };
        if need > width && !cur.is_empty() {
            lines.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
        if width_of(&cur) > width {
            lines.push(fit_cells(&cur, width).trim_end().to_string());
            cur.clear();
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

fn width_of(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_gives_panes_their_height_when_it_fits() {
        // 40 rows: header 1, three heads, bodies 9+5+7, list gets the rest.
        let (list, bodies) = layout(40, &[(false, 9), (false, 5), (false, 7)]);
        assert_eq!(bodies, [9, 5, 7]);
        assert_eq!(list, 40 - 1 - 3 - 21);
    }

    #[test]
    fn collapsed_panes_take_only_their_header() {
        let (list, bodies) = layout(30, &[(true, 9), (false, 5), (true, 7)]);
        assert_eq!(bodies, [0, 5, 0]);
        assert_eq!(list, 30 - 1 - 3 - 5);
    }

    // The list keeps its minimum; the tallest pane gives way first.
    #[test]
    fn panes_shrink_tallest_first_to_keep_the_list() {
        let (list, bodies) = layout(20, &[(false, 9), (false, 5), (false, 7)]);
        assert_eq!(list, LIST_MIN);
        assert_eq!(bodies.iter().sum::<u16>(), 20 - 1 - 3 - LIST_MIN);
        assert!(bodies.iter().all(|&b| b >= 1));
        assert!(bodies[0] <= 5 && bodies[2] <= 5, "{bodies:?}");
    }

    #[test]
    fn tiny_terminal_never_underflows() {
        let (list, bodies) = layout(3, &[(false, 9), (false, 5), (false, 7)]);
        assert_eq!(list, 0);
        assert_eq!(bodies, [1, 1, 1]);
    }

    #[test]
    fn wrap_words_breaks_on_width() {
        assert_eq!(
            wrap_words("one two three four", 9),
            ["one two", "three", "four"]
        );
        assert_eq!(wrap_words("", 5), Vec::<String>::new());
        assert_eq!(
            wrap_words("abcdefghij", 4).len(),
            1,
            "long word is cut, not looped"
        );
    }
}
