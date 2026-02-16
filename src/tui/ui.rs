//! TUI rendering functions.

use crate::common::types::{
    format_duration_ago, format_memory, lines_for_session, truncate_command, ClaudeStatus,
};
use crate::tui::app::{App, InputMode, SearchResult};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

/// Build the ratatui UI
pub fn ui(frame: &mut Frame, app: &mut App) {
    app.clear_old_error();
    let area = frame.area();

    // Determine if we need an error line
    let error_height = if app.error_message.is_some() { 1 } else { 0 };

    let chunks = Layout::vertical([
        Constraint::Length(1),            // header
        Constraint::Min(0),               // session list
        Constraint::Length(error_height), // error message (if any)
        Constraint::Length(1),            // footer
    ])
    .split(area);

    // --- Header ---
    let now = chrono::Local::now();
    let title = if app.input_mode == InputMode::Search {
        "hive [SEARCH]".to_string()
    } else if app.showing_detail.is_some() {
        if let Some(name) = app.detail_session_name() {
            format!("hive [{}]", name)
        } else {
            "hive [DETAIL]".to_string()
        }
    } else if let Some(ref name) = app.showing_parked_detail {
        format!("hive [{}]", name)
    } else if app.showing_parked {
        "hive [PARKED]".to_string()
    } else {
        "hive".to_string()
    };

    let mut header_spans = vec![Span::styled(
        title,
        Style::default().add_modifier(Modifier::BOLD),
    )];
    if app.global_mute {
        header_spans.push(Span::raw("  "));
        header_spans.push(Span::styled(
            "[MUTED]",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }
    header_spans.push(Span::raw("  "));
    header_spans.push(Span::styled(
        now.format("%H:%M:%S").to_string(),
        Style::default().add_modifier(Modifier::DIM),
    ));
    header_spans.push(Span::raw("  "));
    header_spans.push(Span::styled(
        format!("{}s refresh", app.interval),
        Style::default().add_modifier(Modifier::DIM),
    ));
    let header = Line::from(header_spans);
    frame.render_widget(Paragraph::new(header), chunks[0]);

    // --- Main content ---
    if app.showing_help {
        render_help_screen(frame, chunks[1]);
    } else if app.input_mode == InputMode::Search {
        render_search_view(frame, app, chunks[1]);
    } else if app.showing_detail.is_some() {
        render_detail_view(frame, app, chunks[1]);
        if app.input_mode == InputMode::ParkNote {
            render_input_modal(frame, app, chunks[1], "Park", "park", Color::Yellow);
        } else if app.input_mode == InputMode::AddTodo {
            render_input_modal(frame, app, chunks[1], "Add Todo", "add", Color::Cyan);
        }
    } else if app.showing_parked_detail.is_some() {
        render_parked_detail_view(frame, app, chunks[1]);
    } else if app.showing_parked {
        render_parked_view(frame, app, chunks[1]);
    } else {
        render_session_list(frame, app, chunks[1]);
    }

    // --- Error message ---
    if let Some((ref msg, _)) = app.error_message {
        let error_line = Line::from(Span::styled(
            msg.clone(),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
        frame.render_widget(Paragraph::new(error_line), chunks[2]);
    }

    // --- Footer ---
    let footer = if app.showing_help {
        Line::from(vec![
            Span::styled("[?/Esc]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("close help"),
        ])
    } else if app.input_mode == InputMode::Search {
        Line::from(vec![
            Span::styled("/", Style::default().fg(Color::Yellow)),
            Span::styled(&app.search_query, Style::default().fg(Color::Yellow)),
            Span::styled("█", Style::default().add_modifier(Modifier::SLOW_BLINK)),
            Span::raw("  "),
            Span::styled("[Enter]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("select "),
            Span::styled("[Esc]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("cancel"),
        ])
    } else if app.showing_detail.is_some() {
        Line::from(vec![
            Span::styled("[A]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("dd todo "),
            Span::styled("[D]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("elete "),
            Span::styled("[Enter]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("switch "),
            Span::styled("[P]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("ark "),
            Span::styled("[!]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("auto "),
            Span::styled("[M]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("ute "),
            Span::styled("[S]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("kip "),
            Span::styled("[?]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("help "),
            Span::styled("[Esc]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("back "),
            Span::styled("[Q]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("uit"),
        ])
    } else if app.showing_parked_detail.is_some() {
        Line::from(vec![
            Span::styled("[Enter]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("unpark "),
            Span::styled("[Esc]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("back "),
            Span::styled("[Q]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("uit"),
        ])
    } else if app.showing_parked {
        Line::from(vec![
            Span::styled("[a-z]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("select "),
            Span::styled("[Enter]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("unpark "),
            Span::styled("[U/Esc]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("back "),
            Span::styled("[Q]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("uit"),
        ])
    } else {
        let parked_count = app.parked_sessions.len();
        let mut spans = vec![
            Span::styled("[↑↓]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("select "),
            Span::styled("[Enter]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("detail "),
            Span::styled("[1-9]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("switch "),
        ];
        if parked_count > 0 {
            spans.push(Span::styled(
                "[U]",
                Style::default().add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(format!("parked({}) ", parked_count)));
        } else {
            spans.push(Span::styled(
                "[U]",
                Style::default().add_modifier(Modifier::DIM),
            ));
            spans.push(Span::styled(
                "parked ",
                Style::default().add_modifier(Modifier::DIM),
            ));
        }
        spans.push(Span::styled(
            "[R]",
            Style::default().add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw("efresh "));
        spans.push(Span::styled(
            "[M]",
            Style::default().add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw("ute "));
        spans.push(Span::styled(
            "[?]",
            Style::default().add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw("help "));
        spans.push(Span::styled(
            "[Q]",
            Style::default().add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw("uit"));
        Line::from(spans)
    };
    frame.render_widget(Paragraph::new(footer), chunks[3]);
}

/// Render the help screen overlay
fn render_help_screen(frame: &mut Frame, area: Rect) {
    let help_text = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "  List View",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::raw("    ↑/↓ j/k    Navigate sessions"),
        Line::raw("    Enter       Open detail view"),
        Line::raw("    1-9         Switch to session by number"),
        Line::raw("    y/z/x/w/v  Approve permission (once)"),
        Line::raw("    Y/Z/X/W/V  Approve permission (always)"),
        Line::raw("    /           Search sessions"),
        Line::raw("    U           View parked sessions"),
        Line::raw("    M           Toggle global mute"),
        Line::raw("    R           Force refresh"),
        Line::raw(""),
        Line::from(Span::styled(
            "  Detail View",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::raw("    ↑/↓ j/k    Navigate todos/ports"),
        Line::raw("    Enter       Switch to session / open port in Chrome"),
        Line::raw("    A           Add todo"),
        Line::raw("    D           Delete selected todo"),
        Line::raw("    P           Park session (requires sesh)"),
        Line::raw("    !           Toggle auto-approve"),
        Line::raw("    M           Toggle notifications mute"),
        Line::raw("    S           Toggle skip from cycling"),
        Line::raw(""),
        Line::from(Span::styled(
            "  Search",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::raw("    Type        Filter by name"),
        Line::raw("    ↑/↓         Navigate results"),
        Line::raw("    Enter       Open / connect"),
        Line::raw("    Esc         Cancel search"),
        Line::raw(""),
        Line::from(Span::styled(
            "  Global",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::raw("    ?           Toggle this help screen"),
        Line::raw("    Esc/Q       Quit"),
    ];

    frame.render_widget(Paragraph::new(help_text), area);
}

/// Render the normal session list view
pub fn render_session_list(frame: &mut Frame, app: &mut App, area: Rect) {
    let available_height = area.height as usize;

    app.ensure_visible(available_height);

    let non_claude_start = app
        .session_infos
        .iter()
        .position(|s| !app.is_skipped(&s.name) && s.claude_status.is_none());
    let skipped_start = app
        .session_infos
        .iter()
        .position(|s| app.is_skipped(&s.name));

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::raw(""));
    let mut lines_remaining = available_height.saturating_sub(1);
    let mut idx = app.scroll_offset;
    let mut shown_non_claude_divider = false;
    let mut shown_skipped_divider = false;

    while idx < app.session_infos.len() {
        let session_info = &app.session_infos[idx];
        let is_skipped = app.is_skipped(&session_info.name);
        let is_claude = session_info.claude_status.is_some();

        if !is_skipped && !is_claude && !shown_non_claude_divider && non_claude_start == Some(idx) {
            shown_non_claude_divider = true;
            if lines_remaining < 2 {
                break;
            }
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "── other ──",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
            )));
            lines_remaining -= 2;
        }

        if is_skipped && !shown_skipped_divider && skipped_start == Some(idx) {
            shown_skipped_divider = true;
            if lines_remaining < 2 {
                break;
            }
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "── skipped ──",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
            )));
            lines_remaining -= 2;
        }

        let needed = lines_for_session(session_info);
        if lines_remaining < needed {
            break;
        }

        let display_num = idx + 1;
        let is_selected = app.show_selection && idx == app.selected;

        let cpu_text = format!("{:.1}%", session_info.total_cpu);
        let cpu_color = if session_info.total_cpu < 20.0 {
            Color::Green
        } else if session_info.total_cpu < 100.0 {
            Color::Yellow
        } else {
            Color::Red
        };

        let mem_text = format_memory(session_info.total_mem_kb);
        let mem_color = if session_info.total_mem_kb < 512000 {
            Color::Green
        } else if session_info.total_mem_kb < 2048000 {
            Color::Yellow
        } else {
            Color::Red
        };

        let prefix_span = if is_selected {
            Span::styled(">", Style::default().add_modifier(Modifier::BOLD))
        } else if display_num <= 9 {
            Span::styled(
                format!("{}", display_num),
                Style::default().add_modifier(Modifier::BOLD),
            )
        } else {
            Span::raw(" ")
        };

        if is_claude {
            let header_style = if is_selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else if is_skipped {
                Style::default().add_modifier(Modifier::DIM)
            } else {
                Style::default()
            };

            let mut header_spans = vec![
                prefix_span,
                Span::styled(".", header_style),
                Span::styled(" ", header_style),
                Span::styled(
                    session_info.name.clone(),
                    header_style.add_modifier(Modifier::BOLD),
                ),
                Span::styled(" [", header_style),
                Span::styled(cpu_text, header_style.fg(cpu_color)),
                Span::styled("/", header_style),
                Span::styled(mem_text, header_style.fg(mem_color)),
                Span::styled("]", header_style),
            ];

            let todo_count = app.todo_count(&session_info.name);
            if todo_count > 0 {
                header_spans.push(Span::styled(
                    format!(" [{}]", todo_count),
                    Style::default().fg(Color::Cyan),
                ));
            }

            if app.is_auto_approved(&session_info.name) {
                header_spans.push(Span::styled(" [auto]", Style::default().fg(Color::Green)));
            }

            if app.is_muted(&session_info.name) {
                header_spans.push(Span::styled(
                    " [muted]",
                    Style::default().fg(Color::DarkGray),
                ));
            }

            lines.push(Line::from(header_spans));

            if let Some(ref status) = session_info.claude_status {
                let ago_text = session_info
                    .last_activity
                    .as_ref()
                    .map(|ts| format!(" ({})", format_duration_ago(ts)))
                    .unwrap_or_default();

                match status {
                    ClaudeStatus::NeedsPermission(cmd, desc) => {
                        let text = if let Some(key) = session_info.permission_key {
                            format!(
                                "   → [{}/{}] needs permission: {}",
                                key,
                                key.to_ascii_uppercase(),
                                cmd
                            )
                        } else {
                            format!("   → needs permission: {}", cmd)
                        };
                        lines.push(Line::from(vec![
                            Span::styled(text, Style::default().fg(Color::Yellow)),
                            Span::styled(
                                ago_text.clone(),
                                Style::default().add_modifier(Modifier::DIM),
                            ),
                        ]));
                        let desc_text = desc.as_deref().unwrap_or("");
                        lines.push(Line::from(Span::styled(
                            format!("     {}", desc_text),
                            Style::default().add_modifier(Modifier::DIM),
                        )));
                    }
                    ClaudeStatus::EditApproval(filename) => {
                        let text = if let Some(key) = session_info.permission_key {
                            format!(
                                "   → [{}/{}] edit: {}",
                                key,
                                key.to_ascii_uppercase(),
                                filename
                            )
                        } else {
                            format!("   → edit: {}", filename)
                        };
                        lines.push(Line::from(vec![
                            Span::styled(text, Style::default().fg(Color::Yellow)),
                            Span::styled(
                                ago_text.clone(),
                                Style::default().add_modifier(Modifier::DIM),
                            ),
                        ]));
                        lines.push(Line::raw(""));
                    }
                    ClaudeStatus::PlanReview => {
                        lines.push(Line::from(vec![
                            Span::styled(
                                format!("   → {}", status),
                                Style::default().fg(Color::Magenta),
                            ),
                            Span::styled(
                                ago_text.clone(),
                                Style::default().add_modifier(Modifier::DIM),
                            ),
                        ]));
                        lines.push(Line::raw(""));
                    }
                    ClaudeStatus::QuestionAsked => {
                        lines.push(Line::from(vec![
                            Span::styled(
                                format!("   → {}", status),
                                Style::default().fg(Color::Magenta),
                            ),
                            Span::styled(
                                ago_text.clone(),
                                Style::default().add_modifier(Modifier::DIM),
                            ),
                        ]));
                        lines.push(Line::raw(""));
                    }
                    ClaudeStatus::Waiting => {
                        lines.push(Line::from(vec![
                            Span::styled(
                                format!("   → {}", status),
                                Style::default().fg(Color::Cyan),
                            ),
                            Span::styled(
                                ago_text.clone(),
                                Style::default().add_modifier(Modifier::DIM),
                            ),
                        ]));
                        lines.push(Line::raw(""));
                    }
                    _ => {
                        lines.push(Line::from(vec![
                            Span::styled(
                                format!("   → {}", status),
                                Style::default().add_modifier(Modifier::DIM),
                            ),
                            Span::styled(
                                ago_text.clone(),
                                Style::default().add_modifier(Modifier::DIM),
                            ),
                        ]));
                        lines.push(Line::raw(""));
                    }
                }
            }
        } else {
            let header_style = if is_selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default().add_modifier(Modifier::DIM)
            };

            let mut header_spans = vec![
                prefix_span,
                Span::styled(".", header_style),
                Span::styled(" ", header_style),
                Span::styled(session_info.name.clone(), header_style),
                Span::styled(" [", header_style),
                Span::styled(cpu_text, header_style.fg(cpu_color)),
                Span::styled("/", header_style),
                Span::styled(mem_text, header_style.fg(mem_color)),
                Span::styled("]", header_style),
            ];

            let todo_count = app.todo_count(&session_info.name);
            if todo_count > 0 {
                header_spans.push(Span::styled(
                    format!(" [{}]", todo_count),
                    Style::default().fg(Color::Cyan),
                ));
            }

            if app.is_auto_approved(&session_info.name) {
                header_spans.push(Span::styled(" [auto]", Style::default().fg(Color::Green)));
            }

            if app.is_muted(&session_info.name) {
                header_spans.push(Span::styled(
                    " [muted]",
                    Style::default().fg(Color::DarkGray),
                ));
            }

            lines.push(Line::from(header_spans));
        }

        lines_remaining -= needed;
        idx += 1;
    }

    frame.render_widget(Paragraph::new(lines), area);
}

/// Render the parked sessions view
pub fn render_parked_view(frame: &mut Frame, app: &mut App, area: Rect) {
    let parked_list = app.parked_list();
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::raw(""));

    if parked_list.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No parked sessions",
            Style::default().add_modifier(Modifier::DIM),
        )));
    } else {
        for (i, (name, note)) in parked_list.iter().enumerate() {
            let letter = (b'a' + i as u8) as char;
            let is_selected = i == app.parked_selected;

            let prefix = if is_selected {
                Span::styled(">", Style::default().add_modifier(Modifier::BOLD))
            } else {
                Span::styled(
                    format!("{}", letter),
                    Style::default().add_modifier(Modifier::BOLD),
                )
            };

            let style = if is_selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };

            lines.push(Line::from(vec![
                prefix,
                Span::styled(". ", style),
                Span::styled(name.clone(), style),
            ]));

            if !note.is_empty() {
                let note_style = Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM);
                for (j, note_line) in note.split('\n').enumerate() {
                    let prefix = if j == 0 { "   → " } else { "     " };
                    lines.push(Line::from(Span::styled(
                        format!("{}{}", prefix, note_line),
                        note_style,
                    )));
                }
            }
        }
    }

    frame.render_widget(Paragraph::new(lines), area);
}

/// Render the search results view
pub fn render_search_view(frame: &mut Frame, app: &mut App, area: Rect) {
    let available_height = area.height as usize;

    app.ensure_search_visible(available_height.saturating_sub(1));

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::raw(""));

    if app.search_results.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No matches",
            Style::default().add_modifier(Modifier::DIM),
        )));
    } else {
        let mut lines_remaining = available_height.saturating_sub(1);
        let mut idx = app.search_scroll_offset;

        while idx < app.search_results.len() && lines_remaining > 0 {
            let result = &app.search_results[idx];
            let is_selected = idx == app.selected;
            let prefix = if is_selected {
                Span::styled(">", Style::default().add_modifier(Modifier::BOLD))
            } else {
                Span::raw(" ")
            };

            let style = if is_selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };

            match result {
                SearchResult::Active(session_idx) => {
                    let info = &app.session_infos[*session_idx];
                    let status_text = match &info.claude_status {
                        Some(ClaudeStatus::NeedsPermission(_, _)) => " [permission]",
                        Some(ClaudeStatus::EditApproval(_)) => " [edit]",
                        Some(ClaudeStatus::Waiting) => " [waiting]",
                        Some(ClaudeStatus::PlanReview) => " [plan]",
                        Some(ClaudeStatus::QuestionAsked) => " [question]",
                        Some(ClaudeStatus::Unknown) => " [working]",
                        None => "",
                    };
                    lines.push(Line::from(vec![
                        prefix,
                        Span::styled(". ", style),
                        Span::styled(info.name.clone(), style.add_modifier(Modifier::BOLD)),
                        Span::styled(status_text, Style::default().fg(Color::Cyan)),
                    ]));
                    lines_remaining -= 1;
                }
                SearchResult::Parked(name) => {
                    let has_note = app
                        .parked_sessions
                        .get(name)
                        .map(|n| !n.is_empty())
                        .unwrap_or(false);
                    let needed = if has_note { 2 } else { 1 };
                    if lines_remaining < needed {
                        break;
                    }

                    lines.push(Line::from(vec![
                        prefix,
                        Span::styled(". ", style),
                        Span::styled(name.clone(), style),
                        Span::styled(" [parked]", Style::default().fg(Color::DarkGray)),
                    ]));
                    lines_remaining -= 1;

                    if has_note {
                        if let Some(note) = app.parked_sessions.get(name) {
                            let note_style =
                                Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM);
                            for (j, note_line) in note.split('\n').enumerate() {
                                let prefix = if j == 0 { "   → " } else { "     " };
                                lines.push(Line::from(Span::styled(
                                    format!("{}{}", prefix, note_line),
                                    note_style,
                                )));
                                lines_remaining -= 1;
                            }
                        }
                    }
                }
                SearchResult::SeshProject(name) => {
                    lines.push(Line::from(vec![
                        prefix,
                        Span::styled(". ", style),
                        Span::styled(name.clone(), style),
                        Span::styled(" [sesh]", Style::default().fg(Color::Cyan)),
                    ]));
                    lines_remaining -= 1;
                }
            }
            idx += 1;
        }
    }

    frame.render_widget(Paragraph::new(lines), area);
}

/// Render the session detail view
pub fn render_detail_view(frame: &mut Frame, app: &mut App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::raw(""));

    let Some(idx) = app.showing_detail else {
        return;
    };
    let Some(session_info) = app.session_infos.get(idx) else {
        return;
    };

    let cpu_text = format!("{:.1}%", session_info.total_cpu);
    let cpu_color = if session_info.total_cpu < 20.0 {
        Color::Green
    } else if session_info.total_cpu < 100.0 {
        Color::Yellow
    } else {
        Color::Red
    };

    let mem_text = format_memory(session_info.total_mem_kb);
    let mem_color = if session_info.total_mem_kb < 512000 {
        Color::Green
    } else if session_info.total_mem_kb < 2048000 {
        Color::Yellow
    } else {
        Color::Red
    };

    lines.push(Line::from(vec![
        Span::styled("CPU: ", Style::default().add_modifier(Modifier::DIM)),
        Span::styled(cpu_text, Style::default().fg(cpu_color)),
        Span::raw("  "),
        Span::styled("MEM: ", Style::default().add_modifier(Modifier::DIM)),
        Span::styled(mem_text, Style::default().fg(mem_color)),
    ]));

    if let Some(ref status) = session_info.claude_status {
        let (status_text, status_color) = match status {
            ClaudeStatus::Waiting => ("waiting for input".to_string(), Color::Cyan),
            ClaudeStatus::NeedsPermission(cmd, _) => {
                (format!("needs permission: {}", cmd), Color::Yellow)
            }
            ClaudeStatus::EditApproval(filename) => {
                (format!("edit approval: {}", filename), Color::Yellow)
            }
            ClaudeStatus::PlanReview => ("plan ready for review".to_string(), Color::Magenta),
            ClaudeStatus::QuestionAsked => ("question asked".to_string(), Color::Magenta),
            ClaudeStatus::Unknown => ("working".to_string(), Color::White),
        };
        lines.push(Line::from(vec![
            Span::styled("Claude: ", Style::default().add_modifier(Modifier::DIM)),
            Span::styled(status_text, Style::default().fg(status_color)),
        ]));
    } else {
        lines.push(Line::from(Span::styled(
            "Claude: not running",
            Style::default().add_modifier(Modifier::DIM),
        )));
    }

    let mut flag_spans: Vec<Span> = Vec::new();
    if app.is_auto_approved(&session_info.name) {
        flag_spans.push(Span::styled(
            "[auto-approve] ",
            Style::default().fg(Color::Green),
        ));
    }
    if app.is_muted(&session_info.name) {
        flag_spans.push(Span::styled(
            "[muted] ",
            Style::default().fg(Color::DarkGray),
        ));
    }
    if app.is_skipped(&session_info.name) {
        flag_spans.push(Span::styled(
            "[skip-cycling] ",
            Style::default().fg(Color::DarkGray),
        ));
    }
    if !flag_spans.is_empty() {
        flag_spans.insert(
            0,
            Span::styled("Flags: ", Style::default().add_modifier(Modifier::DIM)),
        );
        lines.push(Line::from(flag_spans));
    }

    lines.push(Line::raw(""));

    let todos = app.detail_todos();
    if !todos.is_empty() {
        lines.push(Line::from(Span::styled(
            "Todos:",
            Style::default().add_modifier(Modifier::BOLD),
        )));

        for (i, todo) in todos.iter().enumerate() {
            let letter = (b'a' + i as u8) as char;
            let is_selected = i == app.detail_selected;

            let prefix = if is_selected {
                Span::styled(">", Style::default().add_modifier(Modifier::BOLD))
            } else {
                Span::styled(
                    format!("{}", letter),
                    Style::default().add_modifier(Modifier::BOLD),
                )
            };

            let style = if is_selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };

            let todo_lines: Vec<&str> = todo.split('\n').collect();
            lines.push(Line::from(vec![
                Span::raw("  "),
                prefix,
                Span::styled(". ", style),
                Span::styled(todo_lines[0].to_string(), style),
            ]));
            for cont_line in &todo_lines[1..] {
                lines.push(Line::from(vec![
                    Span::raw("     "),
                    Span::styled(cont_line.to_string(), style),
                ]));
            }
        }

        lines.push(Line::raw(""));
    }

    // Ports section
    let listening_ports = &session_info.listening_ports;
    let port_selection_offset = todos.len();
    if !listening_ports.is_empty() {
        lines.push(Line::from(Span::styled(
            "Ports:",
            Style::default().add_modifier(Modifier::BOLD),
        )));

        for (i, port_info) in listening_ports.iter().enumerate() {
            let sel_idx = port_selection_offset + i;
            let is_selected = sel_idx == app.detail_selected;

            let port_label = format!(":{:<5}  ({})", port_info.port, port_info.process_name);

            let tab_match = app
                .detail_chrome_tabs
                .iter()
                .find(|(_, p)| *p == port_info.port);

            let style = if is_selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default().fg(Color::Green)
            };

            let prefix = if is_selected {
                Span::styled(">", Style::default().add_modifier(Modifier::BOLD))
            } else {
                Span::raw(" ")
            };

            if let Some((tab, _)) = tab_match {
                let max_title = (area.width as usize).saturating_sub(port_label.len() + 8);
                let title = if tab.title.len() > max_title {
                    format!("{}...", &tab.title[..max_title.saturating_sub(3)])
                } else {
                    tab.title.clone()
                };
                lines.push(Line::from(vec![
                    Span::raw(" "),
                    prefix,
                    Span::styled(format!(" {}", port_label), style),
                    Span::styled(
                        format!(" → {}", title),
                        if is_selected {
                            style
                        } else {
                            Style::default().fg(Color::Cyan)
                        },
                    ),
                ]));
            } else {
                lines.push(Line::from(vec![
                    Span::raw(" "),
                    prefix,
                    Span::styled(format!(" {}", port_label), style),
                ]));
            }
        }

        lines.push(Line::raw(""));
    }

    // Processes section
    lines.push(Line::from(Span::styled(
        "Processes:",
        Style::default().add_modifier(Modifier::BOLD),
    )));

    let processes = &session_info.processes;
    let cwd_prefix = session_info.cwd.as_ref().map(|c| {
        if c.ends_with('/') {
            c.clone()
        } else {
            format!("{}/", c)
        }
    });
    if processes.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no active processes)",
            Style::default().add_modifier(Modifier::DIM),
        )));
    } else {
        let max_cmd_width = (area.width as usize).saturating_sub(40);
        for proc in processes {
            let pid_text = format!("PID {:>5}", proc.pid);
            let cpu_text = format!("{:>5.1}%", proc.cpu_percent);
            let proc_cpu_color = if proc.cpu_percent < 10.0 {
                Color::Green
            } else if proc.cpu_percent < 50.0 {
                Color::Yellow
            } else {
                Color::Red
            };

            let mem_text = format!("{:>5}", format_memory(proc.memory_kb));
            let proc_mem_color = if proc.memory_kb < 102400 {
                Color::Green
            } else if proc.memory_kb < 512000 {
                Color::Yellow
            } else {
                Color::Red
            };

            let proc_name = if proc.name.len() > 15 {
                format!("({}...)", &proc.name[..12])
            } else {
                format!("({})", proc.name)
            };

            let cmd_relative = match &cwd_prefix {
                Some(prefix) => proc.command.replace(prefix, "./"),
                None => proc.command.clone(),
            };
            let cmd_display = truncate_command(&cmd_relative, max_cmd_width.max(10));

            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(pid_text, Style::default().add_modifier(Modifier::DIM)),
                Span::raw("  "),
                Span::styled(cpu_text, Style::default().fg(proc_cpu_color)),
                Span::raw("  "),
                Span::styled(mem_text, Style::default().fg(proc_mem_color)),
                Span::raw("  "),
                Span::styled(proc_name, Style::default().add_modifier(Modifier::DIM)),
                Span::raw(" "),
                Span::styled(cmd_display, Style::default().add_modifier(Modifier::DIM)),
            ]));
        }
    }

    // Apply scroll offset
    let available_height = area.height as usize;
    let total_lines = lines.len();

    if total_lines > available_height {
        let max_scroll = total_lines.saturating_sub(available_height);
        if app.detail_scroll_offset > max_scroll {
            app.detail_scroll_offset = max_scroll;
        }
    } else {
        app.detail_scroll_offset = 0;
    }

    let visible_lines: Vec<Line> = lines
        .into_iter()
        .skip(app.detail_scroll_offset)
        .take(available_height)
        .collect();

    frame.render_widget(Paragraph::new(visible_lines), area);
}

/// Render a multiline input modal overlay
fn render_input_modal(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    title: &str,
    submit_label: &str,
    border_color: Color,
) {
    let session_name = app
        .detail_session_name()
        .unwrap_or_else(|| "session".to_string());

    let input_lines: Vec<&str> = app.input_buffer.split('\n').collect();
    let num_input_lines = input_lines.len().max(1);

    let modal_width = (area.width.saturating_sub(4)).clamp(40, 60);
    let modal_height = (num_input_lines as u16 + 4).min(area.height.saturating_sub(2));

    let x = area.x + (area.width.saturating_sub(modal_width)) / 2;
    let y = area.y + (area.height.saturating_sub(modal_height)) / 2;
    let modal_area = Rect::new(x, y, modal_width, modal_height);

    frame.render_widget(Clear, modal_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(format!(" {}: {} ", title, session_name));

    let inner = block.inner(modal_area);
    frame.render_widget(block, modal_area);

    let inner_width = inner.width as usize;
    let mut lines: Vec<Line> = Vec::new();

    for (i, line_text) in input_lines.iter().enumerate() {
        let is_last = i == input_lines.len() - 1;
        let max_visible = inner_width.saturating_sub(1);

        let visible = if line_text.len() > max_visible {
            &line_text[line_text.len() - max_visible..]
        } else {
            line_text
        };

        if is_last {
            lines.push(Line::from(vec![
                Span::raw(visible),
                Span::styled("█", Style::default().add_modifier(Modifier::SLOW_BLINK)),
            ]));
        } else {
            lines.push(Line::from(Span::raw(visible)));
        }
    }

    lines.push(Line::raw(""));

    lines.push(Line::from(vec![
        Span::styled("[Enter]", Style::default().add_modifier(Modifier::DIM)),
        Span::styled(
            format!(" {}  ", submit_label),
            Style::default().add_modifier(Modifier::DIM),
        ),
        Span::styled("[Alt+Enter]", Style::default().add_modifier(Modifier::DIM)),
        Span::styled(" newline  ", Style::default().add_modifier(Modifier::DIM)),
        Span::styled("[Esc]", Style::default().add_modifier(Modifier::DIM)),
        Span::styled(" cancel", Style::default().add_modifier(Modifier::DIM)),
    ]));

    frame.render_widget(Paragraph::new(lines), inner);
}

/// Render the parked session detail view
pub fn render_parked_detail_view(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::raw(""));

    let Some(ref name) = app.showing_parked_detail else {
        return;
    };
    let note = app.parked_sessions.get(name).cloned().unwrap_or_default();

    lines.push(Line::from(vec![
        Span::styled("Session: ", Style::default().add_modifier(Modifier::DIM)),
        Span::styled(name.clone(), Style::default().add_modifier(Modifier::BOLD)),
    ]));

    lines.push(Line::raw(""));

    lines.push(Line::from(Span::styled(
        "Note:",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    if note.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no note)",
            Style::default().add_modifier(Modifier::DIM),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            format!("  {}", note),
            Style::default().fg(Color::Cyan),
        )));
    }

    lines.push(Line::raw(""));

    lines.push(Line::from(vec![
        Span::styled("Status: ", Style::default().add_modifier(Modifier::DIM)),
        Span::styled("[parked]", Style::default().fg(Color::DarkGray)),
    ]));

    frame.render_widget(Paragraph::new(lines), area);
}
