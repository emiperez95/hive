//! Drawing. Every draw also records what is clickable where (`App::hits`), so
//! mouse handling never re-derives the layout.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use super::model::{self, Item, Sect, View};
use super::theme::Theme;
use super::{App, Ctl, DlgHit, Hit, KeyFocus, Pane, TodoBtn, TodoLine};
use crate::cli::conversations::fit_cells;
use crate::ipc::messages::SessionStatus;

/// The list never shrinks below this many rows: it is the reason the panel
/// exists, and a pane dragged over it would hide the thing you switch with.
const LIST_MIN: u16 = 6;

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

impl Theme {
    /// The web's `statusInfo` colours: blocked orange, idle green, a workflow
    /// blue, anything else busy red.
    fn state(&self, st: &Option<SessionStatus>) -> Color {
        match st {
            Some(s) if s.blocks_human() => self.attention,
            Some(SessionStatus::Waiting) => self.idle,
            Some(SessionStatus::RunningWorkflow { .. }) => self.primary,
            Some(_) => self.busy,
            None => self.faint,
        }
    }

    fn dim(&self) -> Style {
        Style::new().fg(self.faint)
    }

    /// Group, tier, section and project headings: small, bold, uppercase —
    /// labels, not rows, so they never compete with the cards under them.
    fn label(&self) -> Style {
        Style::new().fg(self.muted).add_modifier(Modifier::BOLD)
    }
}

const TIERS: [&str; 4] = ["NEEDS YOU", "READY FOR REVIEW", "IDLE", "WORKING"];

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
            "PROJECTS".to_string()
        } else if need > 0 {
            format!("{} LIVE · {need} NEED YOU", live.len())
        } else {
            format!("{} LIVE", live.len())
        };

        // Right to left: mute, projects, attention, archived. Archived is the
        // leftmost so it's the only one that moves when it appears — the view
        // toggles are the way back and must stay put (same rule as the web).
        let t = self.theme;
        let on = |b: bool| {
            if b {
                Style::new()
                    .fg(t.primary)
                    .bg(t.current)
                    .add_modifier(Modifier::BOLD)
            } else {
                t.dim()
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
                    Style::new().fg(t.busy)
                } else {
                    Style::new()
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
            Style::new().fg(t.text).add_modifier(Modifier::BOLD),
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
                Line::styled(format!(" {e}"), Style::new().fg(self.theme.busy)),
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
        let t = self.theme;
        // Headings take the selection as a background; cards take it as their
        // hover tint (below).
        let base = if selected {
            Style::new().bg(t.card_hover)
        } else {
            Style::new()
        };
        let whole = |w: usize| vec![(0u16, w as u16, Hit::Item(i))];
        let bell = |key: &str| -> (Span<'static>, Option<(u16, u16, Hit)>) {
            let muted = self.focus.muted_projects.iter().any(|k| k == key);
            let span = if muted {
                Span::styled("🔕", Style::new().fg(t.busy))
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
        let emoji_or = |e: &str| if e.is_empty() { "💻" } else { e }.to_string();
        match item {
            Item::Note(s) => out.push(Line::styled(format!(" {s}"), t.dim()), vec![]),
            Item::Blank => out.push(Line::raw(""), vec![]),
            Item::Tier { tier, n } => {
                let label = TIERS.get(*tier as usize).copied().unwrap_or("OTHER");
                let color = match tier {
                    0 => t.busy,
                    1 => t.primary,
                    _ => t.muted,
                };
                out.push(
                    Line::from(vec![
                        Span::styled(format!(" {label}"), t.label().fg(color)),
                        Span::styled(format!(" {n}"), t.dim()),
                    ]),
                    vec![],
                );
            }
            Item::Group { emoji, label, key } => {
                let (b, hit) = bell(key);
                let text = format!(" {} {}", emoji_or(emoji), label.to_uppercase());
                out.push(
                    Line::from(vec![
                        Span::styled(fit_cells(&text, w.saturating_sub(3)), t.label()),
                        Span::raw(" "),
                        b,
                    ]),
                    hit.into_iter().collect(),
                );
            }
            Item::Worktree(b) => {
                out.push(
                    Line::styled(
                        fit_cells(&format!("  ⎇ {b}"), w),
                        Style::new().fg(t.faint).add_modifier(Modifier::BOLD),
                    ),
                    vec![],
                );
            }
            Item::Section { sect, n, open } => {
                let label = match sect {
                    Sect::Skipped => "SKIPPED",
                    Sect::Frozen => "FROZEN",
                };
                let caret = if *open { "▾" } else { "▸" };
                out.push(
                    Line::styled(
                        fit_cells(&format!(" {caret} {label} ({n})"), w),
                        base.patch(t.label()),
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
                    (true, 0) => " ARCH".to_string(),
                    (true, n) => format!(" ARCH {n}"),
                    (false, 0) => String::new(),
                    (false, n) => format!(" {n}"),
                };
                let head = format!(" {caret} {} {}", emoji_or(emoji), label.to_uppercase());
                let head_w = w.saturating_sub(3 + width(&tail));
                let style = base.patch(t.label()).fg(if *archived {
                    t.faint
                } else if *open {
                    t.text
                } else {
                    t.muted
                });
                let mut hits = whole(w.saturating_sub(3));
                hits.extend(hit);
                out.push(
                    Line::from(vec![
                        Span::styled(fit_cells(&head, head_w), style),
                        Span::styled(tail, base.fg(t.faint)),
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
                let unseen = c.unseen && !current;
                // The web's card: a tinted row with a coloured left edge for the
                // three states worth a glance — here, blocked, pinged.
                let bg = if current {
                    t.current
                } else if c.blocks() {
                    t.need
                } else if selected || hovered {
                    t.card_hover
                } else {
                    t.card
                };
                let card = Style::new().bg(bg);
                let edge = if current || unseen {
                    Some(t.primary)
                } else if c.blocks() {
                    Some(t.attention)
                } else {
                    None
                };
                let bar = match edge {
                    Some(col) => Span::styled("▌", card.fg(col)),
                    None => Span::styled(" ", card),
                };
                let gutter = " ".repeat((*indent as usize).max(1));
                let title_fg = if current {
                    t.on_current
                } else if c.skipped {
                    t.muted
                } else {
                    t.text
                };
                let title_style = if current {
                    card.fg(title_fg).add_modifier(Modifier::BOLD)
                } else {
                    card.fg(title_fg)
                };
                let ping = if unseen { "● " } else { "" };
                let word = model::state_word(&c.status).to_uppercase();
                // gutter · bar · dot+space · title · space · ping · age · space · word · space · right gutter
                let tail_w = width(ping) + 3 + 1 + 5 + 1;
                let title_w = w.saturating_sub(gutter.len() + 1 + 2 + 1 + tail_w + 1);
                out.push(
                    Line::from(vec![
                        Span::raw(gutter.clone()),
                        bar.clone(),
                        Span::styled("● ", card.fg(t.state(&c.status))),
                        Span::styled(fit_cells(&c.title, title_w), title_style),
                        Span::styled(" ", card),
                        Span::styled(ping.to_string(), card.fg(t.primary)),
                        Span::styled(format!("{:>3} ", c.age), card.fg(t.faint)),
                        Span::styled(
                            format!("{word:<5}"),
                            card.fg(t.state(&c.status)).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(" ", card),
                    ]),
                    whole(w),
                );
                // The attention view is ungrouped, so each row says where it lives.
                if *flat {
                    let place_fg = if current { t.on_current } else { t.muted };
                    out.push(
                        Line::from(vec![
                            Span::raw(gutter.clone()),
                            bar,
                            Span::styled("  ", card),
                            Span::styled(
                                fit_cells(&c.place.line(), w.saturating_sub(gutter.len() + 5)),
                                card.fg(place_fg),
                            ),
                            Span::styled(" ", card),
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
                let gutter = " ".repeat((*indent as usize).max(1));
                let card = Style::new().bg(if selected || hovered {
                    t.card_hover
                } else {
                    t.card
                });
                let title_w = w.saturating_sub(gutter.len() + 4 + 1 + width(&age) + 3);
                out.push(
                    Line::from(vec![
                        Span::raw(gutter),
                        Span::styled(" 💤 ", card),
                        Span::styled(fit_cells(&title, title_w), card.fg(t.text)),
                        Span::styled(format!(" {age} "), card.fg(t.faint)),
                        Span::styled(" ", card),
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
        let t = self.theme;
        let bg = Style::new().bg(t.band);
        let name_style = if focused {
            bg.fg(t.primary).add_modifier(Modifier::BOLD)
        } else {
            bg.fg(t.muted).add_modifier(Modifier::BOLD)
        };
        let extra_w = width(&extra);
        let name_w = w.saturating_sub(3 + extra_w + 1);
        let line = Line::from(vec![
            Span::styled(format!(" {caret} "), bg.fg(t.faint)),
            Span::styled(fit_cells(&name, name_w), name_style),
            Span::styled(format!("{extra} "), bg.fg(t.faint)),
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
            self.theme.dim(),
        ));
        match &self.stats {
            None => lines.push(Line::styled(" Reading transcript…", self.theme.dim())),
            Some(s) if s.error.is_some() => lines.push(Line::styled(
                format!(" {}", s.error.clone().unwrap_or_default()),
                self.theme.dim(),
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
                        Style::default().fg(self.theme.muted),
                    ));
                }
                if s.usage.main.is_empty() && s.usage.sidechain.is_empty() {
                    lines.push(Line::styled(" No usage recorded yet.", self.theme.dim()));
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
                        lines.push(Line::styled(format!(" {chunk}"), self.theme.dim()));
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
                        Span::styled(" ⑂ ", self.theme.dim()),
                        Span::styled(
                            branch.to_string(),
                            Style::default().add_modifier(Modifier::BOLD),
                        ),
                    ]));
                    // Only non-zero counts render, so a clean landed tree is a
                    // branch and a `merged` chip, not a row of zeros.
                    let work = Style::default().fg(self.theme.attention);
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
                        chip(format!("{} behind", g.behind), self.theme.dim());
                    }
                    if g.dirty == 0 && g.untracked == 0 && g.ahead == 0 {
                        if g.merged {
                            chip("merged".into(), Style::default().fg(self.theme.idle));
                        } else {
                            chip("clean".into(), self.theme.dim());
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
                            self.theme.dim(),
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
            f.render_widget(
                Paragraph::new(Line::styled(" loading…", self.theme.dim())),
                r,
            );
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
            vec![Line::styled(" Click to add notes…", self.theme.dim())]
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
                Style::default().bg(self.theme.card_hover)
            } else {
                Style::default()
            };
            logical_at.push(out.lines.len());
            match self.todo_line(i) {
                TodoLine::Add => {
                    let text = self.todo_input.text();
                    let (shown, style) = if text.is_empty() && !selected {
                        ("Add a todo…".to_string(), base.fg(self.theme.faint))
                    } else {
                        (text, base)
                    };
                    if selected {
                        cursor = Some((out.lines.len() as u16, 3 + width(&self.todo_input.text())));
                    }
                    out.push(
                        Line::from(vec![
                            Span::styled(" + ", base.fg(self.theme.primary)),
                            Span::styled(fit_cells(&shown, w.saturating_sub(3)), style),
                        ]),
                        vec![(0, w as u16, Hit::TodoAdd)],
                    );
                    if todos.todos.is_empty() && self.todos.is_some() {
                        out.push(Line::styled("   Nothing to do", self.theme.dim()), vec![]);
                    }
                }
                TodoLine::Item(n) => {
                    let text = todos.todos.get(n).cloned().unwrap_or_default();
                    let tw = w.saturating_sub(3 + 5);
                    out.push(
                        Line::from(vec![
                            Span::styled(" ○ ", base.fg(self.theme.muted)),
                            Span::styled(fit_cells(&text, tw), base),
                            Span::styled(" × ", base.fg(self.theme.faint)),
                            Span::styled("▶ ", base.fg(self.theme.primary)),
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
                            base.fg(self.theme.muted),
                        ),
                        vec![(0, w as u16, Hit::DoneToggle)],
                    );
                }
                TodoLine::Done(n) => {
                    let text = todos.done.get(n).cloned().unwrap_or_default();
                    out.push(
                        Line::from(vec![
                            Span::styled(" ● ", base.fg(self.theme.idle)),
                            Span::styled(
                                fit_cells(&text, w.saturating_sub(3)),
                                base.fg(self.theme.faint)
                                    .add_modifier(Modifier::CROSSED_OUT),
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
                self.theme.dim()
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
                    Span::styled(format!("{mark} "), Style::default().fg(self.theme.primary)),
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
                    self.theme.dim(),
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
                    Span::styled(label, self.theme.dim()),
                    Span::styled(
                        fit_cells(&text, inner_w.saturating_sub(label.len())),
                        Style::default().fg(self.theme.attention),
                    ),
                ]),
                vec![(0, inner_w as u16, Hit::Dlg(DlgHit::Branch))],
            );
            let create_style = if running || d.suggesting {
                self.theme.dim()
            } else {
                Style::default().add_modifier(Modifier::REVERSED)
            };
            out.push(
                Line::from(vec![
                    Span::styled(" Create ", create_style),
                    Span::styled("  Enter", self.theme.dim()),
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
                out.push(Line::styled(chunk, self.theme.dim()), vec![]);
            }
        }
        if let Some((msg, err)) = &d.msg {
            out.push(Line::raw(""), vec![]);
            let style = if *err {
                Style::default().fg(self.theme.busy)
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
                    Span::styled("  Enter", self.theme.dim()),
                ]),
                vec![(0, 4, Hit::Dlg(DlgHit::Go))],
            );
        }
        out.push(
            Line::styled("Esc close · 1 2 3 pick", self.theme.dim()),
            vec![],
        );

        let bh = (out.lines.len() as u16 + 2).min(area.height);
        let bw = (inner_w as u16 + 2).min(area.width);
        let bx = area.x + (area.width - bw) / 2;
        let by = area.y + (area.height - bh) / 2;
        let rect = Rect::new(bx, by, bw, bh);
        f.render_widget(Clear, rect);
        let block = Block::bordered()
            .title(" Start todo ")
            .border_style(Style::default().fg(self.theme.primary));
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
            Paragraph::new(Span::styled(" × ", Style::default().fg(self.theme.primary))),
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
