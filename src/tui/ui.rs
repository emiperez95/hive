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
        if app.input_mode == InputMode::AddTodo {
            let ctx = app
                .detail_session_name()
                .unwrap_or_else(|| "session".to_string());
            render_input_modal(
                frame,
                app,
                chunks[1],
                "Add Todo",
                &ctx,
                None,
                "add",
                Color::Cyan,
            );
        } else if app.input_mode == InputMode::WorktreeBranch {
            let ctx = app
                .detail_session_name()
                .unwrap_or_else(|| "session".to_string());
            render_input_modal(
                frame,
                app,
                chunks[1],
                "New Worktree",
                &ctx,
                None,
                "create",
                Color::Green,
            );
        } else if app.input_mode == InputMode::WorktreeBase {
            render_base_picker_modal(frame, app, chunks[1]);
        } else if app.input_mode == InputMode::WorktreeConfirmDelete {
            render_delete_confirm_modal(frame, app, chunks[1]);
        }
    } else {
        render_session_list(frame, app, chunks[1]);
        if app.input_mode == InputMode::SpreadPrompt {
            render_spread_prompt(frame, chunks[1]);
        } else if app.input_mode == InputMode::NewProjectKey {
            render_input_modal(
                frame,
                app,
                chunks[1],
                "New Project",
                "key",
                None,
                "next",
                Color::Yellow,
            );
        } else if app.input_mode == InputMode::NewProjectEmoji {
            let key = app.np_key.clone().unwrap_or_default();
            render_input_modal(
                frame,
                app,
                chunks[1],
                "New Project",
                &key,
                Some("emoji (default 📁)"),
                "create",
                Color::Yellow,
            );
        }
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
    } else if app.input_mode == InputMode::NewProjectKey {
        Line::from(vec![
            Span::styled("Step 1/2 ", Style::default().fg(Color::Yellow)),
            Span::styled("key ", Style::default().add_modifier(Modifier::BOLD)),
            Span::styled("[Enter]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" next "),
            Span::styled("[Esc]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" cancel"),
        ])
    } else if app.input_mode == InputMode::NewProjectEmoji {
        Line::from(vec![
            Span::styled("Step 2/2 ", Style::default().fg(Color::Yellow)),
            Span::styled("emoji ", Style::default().add_modifier(Modifier::BOLD)),
            Span::styled("[Enter]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" create "),
            Span::styled("[Esc]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" cancel"),
        ])
    } else if app.showing_detail.is_some() {
        Line::from(vec![
            Span::styled("[A]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("dd todo "),
            Span::styled("[D]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("elete "),
            Span::styled("[Enter]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("switch "),
            Span::styled("[F]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("av "),
            Span::styled("[!]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("auto "),
            Span::styled("[M]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("ute "),
            Span::styled("[S]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("kip "),
            Span::styled("[W]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("t "),
            Span::styled("[X]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("del "),
            Span::styled("[?]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("help "),
            Span::styled("[Esc]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("back "),
            Span::styled("[Q]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("uit"),
        ])
    } else {
        Line::from(vec![
            Span::styled("[↑↓]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("select "),
            Span::styled("[Enter]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("detail "),
            Span::styled("[1-9]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("switch "),
            Span::styled("[/]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("search "),
            Span::styled("[N]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("ew "),
            Span::styled("[R]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("efresh "),
            Span::styled("[M]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("ute "),
            Span::styled("[?]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("help "),
            Span::styled("[Q]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("uit"),
        ])
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
        Line::raw("    N           New project"),
        Line::raw("    L           Spread/collapse iTerm2 panes"),
        Line::raw("    M           Toggle global mute"),
        Line::raw("    R           Force refresh"),
        Line::raw(""),
        Line::from(Span::styled(
            "  Detail View",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::raw("    ↑/↓ j/k    Navigate todos/ports"),
        Line::raw("    Enter       Switch to session / open port in Chrome"),
        Line::raw("    O           Open all ports in Chrome"),
        Line::raw("    A           Add todo"),
        Line::raw("    D           Delete selected todo"),
        Line::raw("    F           Toggle favorite"),
        Line::raw("    !           Toggle auto-approve"),
        Line::raw("    M           Toggle notifications mute"),
        Line::raw("    S           Toggle skip from cycling"),
        Line::raw("    W           New worktree from session"),
        Line::raw("    X           Delete worktree"),
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

        let needed = lines_for_session(session_info, app.is_auto_approved(&session_info.name));
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

        let is_fav = app.is_favorite(&session_info.name);

        if is_claude {
            let is_auto = app.is_auto_approved(&session_info.name);

            let header_style = if is_selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else if is_skipped {
                Style::default().add_modifier(Modifier::DIM)
            } else {
                Style::default()
            };

            let sep = if is_fav {
                Span::styled("★", Style::default().fg(Color::Yellow))
            } else {
                Span::styled(".", header_style)
            };

            let name_style =
                if session_info.is_current_session || session_info.attached_other_client {
                    header_style.add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                } else {
                    header_style.add_modifier(Modifier::BOLD)
                };

            let current_marker = if session_info.is_current_session {
                Span::styled("›", header_style.add_modifier(Modifier::BOLD))
            } else {
                Span::styled(" ", header_style)
            };

            let todo_count = app.todo_count(&session_info.name);

            if is_auto {
                // Auto-approved layout:
                // Line 1: name only (+ fav star, todo count)
                // Line 2: [cpu/mem] [auto] [muted]
                // Line 3: simplified status
                let mut header_spans = vec![
                    prefix_span,
                    sep,
                    current_marker,
                    Span::styled(session_info.name.clone(), name_style),
                ];

                if todo_count > 0 {
                    header_spans.push(Span::styled(
                        format!(" [{}]", todo_count),
                        Style::default().fg(Color::Magenta),
                    ));
                }

                lines.push(Line::from(header_spans));

                // Line 2: status + resource stats + tags (merged, no selection highlight)
                let dim = Style::default().add_modifier(Modifier::DIM);

                let mut line2_spans = Vec::new();

                if let Some(ref status) = session_info.claude_status {
                    let (label, color) = match status {
                        ClaudeStatus::Waiting => ("idle", Color::Cyan),
                        ClaudeStatus::PlanReview => ("plan", Color::Magenta),
                        ClaudeStatus::QuestionAsked => ("ask?", Color::Magenta),
                        _ => ("work", Color::DarkGray),
                    };

                    line2_spans.push(Span::styled(
                        format!("   → {}", label),
                        Style::default().fg(color),
                    ));
                }

                line2_spans.push(Span::styled(" [", dim));
                line2_spans.push(Span::styled(cpu_text, Style::default().fg(cpu_color)));
                line2_spans.push(Span::styled("/", dim));
                line2_spans.push(Span::styled(mem_text, Style::default().fg(mem_color)));
                line2_spans.push(Span::styled("]", dim));

                if app.is_muted(&session_info.name) {
                    line2_spans.push(Span::styled(
                        " [muted]",
                        Style::default().fg(Color::DarkGray),
                    ));
                }

                lines.push(Line::from(line2_spans));
                lines.push(Line::raw(""));
            } else {
                // Standard layout: name + stats on line 1, detailed status on line 2-3
                let mut header_spans = vec![
                    prefix_span,
                    sep,
                    current_marker,
                    Span::styled(session_info.name.clone(), name_style),
                    Span::styled(" [", header_style),
                    Span::styled(cpu_text, header_style.fg(cpu_color)),
                    Span::styled("/", header_style),
                    Span::styled(mem_text, header_style.fg(mem_color)),
                    Span::styled("]", header_style),
                ];

                if todo_count > 0 {
                    header_spans.push(Span::styled(
                        format!(" [{}]", todo_count),
                        Style::default().fg(Color::Magenta),
                    ));
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
            }
        } else {
            let header_style = if is_selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default().add_modifier(Modifier::DIM)
            };

            let sep = if is_fav {
                Span::styled("★", Style::default().fg(Color::Yellow))
            } else {
                Span::styled(".", header_style)
            };

            let name_style =
                if session_info.is_current_session || session_info.attached_other_client {
                    header_style.add_modifier(Modifier::UNDERLINED)
                } else {
                    header_style
                };

            let current_marker = if session_info.is_current_session {
                Span::styled("›", header_style.add_modifier(Modifier::BOLD))
            } else {
                Span::styled(" ", header_style)
            };

            let mut header_spans = vec![
                prefix_span,
                sep,
                current_marker,
                Span::styled(session_info.name.clone(), name_style),
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
                    Style::default().fg(Color::Magenta),
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
                SearchResult::Active(name) => {
                    let Some(info) = app.session_infos.iter().find(|s| &s.name == name) else {
                        idx += 1;
                        continue;
                    };
                    let star = if app.is_favorite(&info.name) {
                        Span::styled("★ ", Style::default().fg(Color::Yellow))
                    } else {
                        Span::styled(". ", style)
                    };
                    let status_text = match &info.claude_status {
                        Some(ClaudeStatus::NeedsPermission(_, _)) => " [permission]",
                        Some(ClaudeStatus::EditApproval(_)) => " [edit]",
                        Some(ClaudeStatus::Waiting) => " [waiting]",
                        Some(ClaudeStatus::PlanReview) => " [plan]",
                        Some(ClaudeStatus::QuestionAsked) => " [question]",
                        Some(ClaudeStatus::Unknown) => " [working]",
                        None => "",
                    };
                    let mut spans = vec![
                        prefix,
                        star,
                        Span::styled(info.name.clone(), style.add_modifier(Modifier::BOLD)),
                        Span::styled(status_text, Style::default().fg(Color::Cyan)),
                    ];
                    let todo_count = app.todo_count(&info.name);
                    if todo_count > 0 {
                        spans.push(Span::styled(
                            format!(" [{}]", todo_count),
                            Style::default().fg(Color::Magenta),
                        ));
                    }
                    lines.push(Line::from(spans));
                    lines_remaining -= 1;
                }
                SearchResult::Project(name) => {
                    let star = if app.is_favorite(name) {
                        Span::styled("★ ", Style::default().fg(Color::Yellow))
                    } else {
                        Span::styled(". ", style)
                    };
                    lines.push(Line::from(vec![
                        prefix,
                        star,
                        Span::styled(name.clone(), style),
                        Span::styled(" [project]", Style::default().fg(Color::Cyan)),
                    ]));
                    lines_remaining -= 1;
                }
                SearchResult::Worktree(name) => {
                    let star = if app.is_favorite(name) {
                        Span::styled("★ ", Style::default().fg(Color::Yellow))
                    } else {
                        Span::styled(". ", style)
                    };
                    lines.push(Line::from(vec![
                        prefix,
                        star,
                        Span::styled(name.clone(), style),
                        Span::styled(" [worktree]", Style::default().fg(Color::Green)),
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

    let Some(session_info) = app.detail_session_info() else {
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
    if app.is_favorite(&session_info.name) {
        flag_spans.push(Span::styled("[★ fav] ", Style::default().fg(Color::Yellow)));
    }
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

    let mut item_lines: Vec<usize> = Vec::new();

    let todos = app.detail_todos();
    if !todos.is_empty() {
        lines.push(Line::from(Span::styled(
            "Todos:",
            Style::default().add_modifier(Modifier::BOLD),
        )));

        for (i, todo) in todos.iter().enumerate() {
            item_lines.push(lines.len());
            let letter = (b'a' + i as u8) as char;
            let is_selected = app.detail_selected == Some(i);

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
            item_lines.push(lines.len());
            let sel_idx = port_selection_offset + i;
            let is_selected = app.detail_selected == Some(sel_idx);

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

    // Branch commits section (worktree sessions only)
    if !app.detail_commits.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("Commits ({}):", app.detail_commits.len()),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for commit in &app.detail_commits {
            let (hash, msg) = commit.split_once(' ').unwrap_or(("", commit));
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(hash.to_string(), Style::default().fg(Color::Yellow)),
                Span::raw(" "),
                Span::styled(
                    msg.to_string(),
                    Style::default().add_modifier(Modifier::DIM),
                ),
            ]));
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

    // Apply scroll offset with auto-scroll for selected items
    let available_height = area.height as usize;
    app.detail_item_lines = item_lines;
    app.detail_total_lines = lines.len();
    app.ensure_detail_visible(available_height);

    let visible_lines: Vec<Line> = lines
        .into_iter()
        .skip(app.detail_scroll_offset)
        .take(available_height)
        .collect();

    frame.render_widget(Paragraph::new(visible_lines), area);
}

/// Render a multiline input modal overlay
#[allow(clippy::too_many_arguments)]
fn render_input_modal(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    title: &str,
    context: &str,
    prompt: Option<&str>,
    submit_label: &str,
    border_color: Color,
) {
    let session_name = context;

    let input_lines: Vec<&str> = app.input_buffer.split('\n').collect();
    let num_input_lines = input_lines.len().max(1);
    let prompt_lines = if prompt.is_some() { 1 } else { 0 };

    let modal_width = (area.width.saturating_sub(4)).clamp(40, 60);
    let modal_height =
        (num_input_lines as u16 + 4 + prompt_lines).min(area.height.saturating_sub(2));

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

    if let Some(p) = prompt {
        lines.push(Line::from(Span::styled(
            p,
            Style::default().add_modifier(Modifier::DIM),
        )));
    }

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

/// Render a spread prompt overlay in the center of the area
fn render_spread_prompt(frame: &mut Frame, area: Rect) {
    let text = " Spread: press 1-9 (Esc to cancel) ";
    let width = (text.len() as u16 + 2).min(area.width);
    let height = 3u16;

    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let modal_area = Rect::new(x, y, width, height);

    frame.render_widget(Clear, modal_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let inner = block.inner(modal_area);
    frame.render_widget(block, modal_area);

    let line = Line::from(Span::styled(
        text.trim(),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ));
    frame.render_widget(Paragraph::new(line), inner);
}

/// Render the base branch picker modal
fn render_base_picker_modal(frame: &mut Frame, app: &App, area: Rect) {
    let item_count = app.wt_base_choices.len();
    // 2 border + items + 1 blank + 1 footer
    let modal_height = (item_count as u16 + 4).min(area.height.saturating_sub(2));
    let modal_width = 40u16.min(area.width.saturating_sub(4));

    let x = area.x + (area.width.saturating_sub(modal_width)) / 2;
    let y = area.y + (area.height.saturating_sub(modal_height)) / 2;
    let modal_area = Rect::new(x, y, modal_width, modal_height);

    frame.render_widget(Clear, modal_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Green))
        .title(" Base Branch ");

    let inner = block.inner(modal_area);
    frame.render_widget(block, modal_area);

    let mut lines: Vec<Line> = Vec::new();
    for (i, choice) in app.wt_base_choices.iter().enumerate() {
        let is_selected = i == app.wt_base_selected;
        if is_selected {
            lines.push(Line::from(vec![
                Span::styled("> ", Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(
                    choice.clone(),
                    Style::default().add_modifier(Modifier::REVERSED),
                ),
            ]));
        } else {
            lines.push(Line::from(vec![Span::raw("  "), Span::raw(choice.clone())]));
        }
    }

    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled("[Enter]", Style::default().add_modifier(Modifier::DIM)),
        Span::styled(" select  ", Style::default().add_modifier(Modifier::DIM)),
        Span::styled("[↑↓]", Style::default().add_modifier(Modifier::DIM)),
        Span::styled(" navigate  ", Style::default().add_modifier(Modifier::DIM)),
        Span::styled("[Esc]", Style::default().add_modifier(Modifier::DIM)),
        Span::styled(" cancel", Style::default().add_modifier(Modifier::DIM)),
    ]));

    frame.render_widget(Paragraph::new(lines), inner);
}

/// Render the worktree delete confirmation modal
fn render_delete_confirm_modal(frame: &mut Frame, app: &App, area: Rect) {
    let project = app.wt_delete_project.as_deref().unwrap_or("?");
    let branch = app.wt_delete_branch.as_deref().unwrap_or("?");

    let label = format!("{}/{}", project, branch);

    // Look up session name and path from worktree state
    let wt_state = crate::common::worktree::WorktreeState::load();
    let entry = wt_state.get(project, branch);
    let session_line = entry
        .map(|e| format!("Session: {}", e.session_name))
        .unwrap_or_default();
    let path_line = entry
        .map(|e| format!("Path: {}", e.path))
        .unwrap_or_default();

    let content_lines =
        3 + usize::from(!session_line.is_empty()) + usize::from(!path_line.is_empty());
    let modal_height = (content_lines as u16 + 4).min(area.height.saturating_sub(2));
    let modal_width = 50u16.min(area.width.saturating_sub(4));

    let x = area.x + (area.width.saturating_sub(modal_width)) / 2;
    let y = area.y + (area.height.saturating_sub(modal_height)) / 2;
    let modal_area = Rect::new(x, y, modal_width, modal_height);

    frame.render_widget(Clear, modal_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Red))
        .title(" Delete Worktree ");

    let inner = block.inner(modal_area);
    frame.render_widget(block, modal_area);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![
        Span::raw("Delete "),
        Span::styled(
            label,
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        Span::raw("?"),
    ]));

    if !session_line.is_empty() {
        lines.push(Line::from(Span::styled(
            session_line,
            Style::default().add_modifier(Modifier::DIM),
        )));
    }
    if !path_line.is_empty() {
        lines.push(Line::from(Span::styled(
            path_line,
            Style::default().add_modifier(Modifier::DIM),
        )));
    }

    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled("[Enter]", Style::default().add_modifier(Modifier::DIM)),
        Span::styled(" delete  ", Style::default().add_modifier(Modifier::DIM)),
        Span::styled("[Esc]", Style::default().add_modifier(Modifier::DIM)),
        Span::styled(" cancel", Style::default().add_modifier(Modifier::DIM)),
    ]));

    frame.render_widget(Paragraph::new(lines), inner);
}
