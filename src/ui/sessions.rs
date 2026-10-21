use crate::app::App;
use crate::chat_view::{ChatLayout, ChatViewMetrics, CHROME_COLS};
use crate::locale::t;
use crate::model::{AgentSession, ChatRole, FileOp, MAX_CHAT_ITEMS, MAX_CHAT_MESSAGES};
use crate::theme::Theme;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Clear, Paragraph, Row, Table};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use super::{btop_block_active, fmt_mem_kb, fmt_tokens, grad_at, make_gradient, truncate_str};

pub(crate) fn draw_sessions_panel(f: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    draw_sessions_panel_active(f, app, area, theme, false);
}

pub(crate) fn draw_sessions_panel_active(
    f: &mut Frame,
    app: &App,
    area: Rect,
    theme: &Theme,
    active: bool,
) {
    // Render the outer block
    let block = btop_block_active("sessions", theme.cpu_box, theme, active);
    f.render_widget(block, area);

    let inner = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };

    let visible = app.visible_indices();
    let session_rows: u16 = visible
        .iter()
        .map(|&i| {
            let base = 1u16;
            if app.tree_view {
                base + app.sessions[i].subagents.len() as u16
            } else {
                base
            }
        })
        .sum();
    let detail_reserve: u16 = if app.show_timeline {
        (inner.height * 2 / 3).min(inner.height.saturating_sub(5))
    } else if inner.height <= 12 {
        6.min(inner.height.saturating_sub(3))
    } else {
        10.min(inner.height / 2)
    };
    let max_table = inner.height.saturating_sub(detail_reserve);
    let table_h = (1 + session_rows).min(max_table);

    let panel_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(table_h),
            Constraint::Length(1), // separator line
            Constraint::Min(0),
        ])
        .split(inner);

    // Draw separator line between session list and detail
    {
        let sep_area = panel_chunks[1];
        let sep_line = "─".repeat(sep_area.width as usize);
        f.render_widget(
            Paragraph::new(Span::styled(sep_line, Style::default().fg(theme.cpu_box))),
            sep_area,
        );
    }

    // ── Session list table ──
    let proc_grad = make_gradient(
        theme.proc_grad.start,
        theme.proc_grad.mid,
        theme.proc_grad.end,
    );
    let mut rows = Vec::new();

    // Keep the requested columns and order at every terminal width.
    let w = inner.width;
    let session_name_w: u16 = if w >= 120 {
        24
    } else if w >= 80 {
        18
    } else {
        10
    };
    let status_w: u16 = if w >= 80 { 9 } else { 3 };
    let context_w: u16 = if w >= 100 {
        7
    } else if w >= 80 {
        4
    } else {
        3
    };
    let context_label = if w >= 100 {
        t("col.context")
    } else {
        t("col.ctx")
    };
    let tokens_w: u16 = if w >= 100 { 7 } else { 6 };
    let scrollbar_w = u16::from(session_rows > table_h.saturating_sub(1) && w > 2);
    let model_w = w
        .saturating_sub(1 + 8 + status_w + session_name_w + context_w + tokens_w + 6 + scrollbar_w);
    let name_extra_w = model_w / 3;
    let session_name_w = session_name_w + name_extra_w;
    let model_w = model_w - name_extra_w;

    let visible = app.visible_indices();
    for &i in &visible {
        let session = &app.sessions[i];
        let selected = i == app.selected;
        let marker = if selected { "►" } else { " " };

        let (agent_base, agent_color) = match session.agent_cli {
            "claude" => ("Claude", Color::Rgb(217, 119, 87)), // #D97757 terracotta
            "codex" => ("Codex", Color::Rgb(122, 157, 255)),  // #7A9DFF periwinkle
            "opencode" => ("#OC", Color::Rgb(74, 222, 128)),  // #4ADE80 emerald
            other => {
                let fallback: String = other.chars().take(3).collect::<String>().to_uppercase();
                (
                    Box::leak(fallback.into_boxed_str()) as &str,
                    theme.inactive_fg,
                )
            }
        };
        // App/IDE-launched sessions keep their suffix, separated from the full name.
        let suffix = session.launch_surface.label_suffix();
        let agent_label = if suffix.is_empty() {
            agent_base.to_string()
        } else {
            format!("{agent_base} {suffix}")
        };

        let (status_icon_str, status_color) = match &session.status {
            crate::model::SessionStatus::Thinking => (t("sess.think"), theme.proc_misc),
            crate::model::SessionStatus::Executing => (t("sess.think"), theme.proc_misc),
            crate::model::SessionStatus::AwaitingInput => (t("sess.reply"), theme.hi_fg),
            crate::model::SessionStatus::Waiting | crate::model::SessionStatus::RateLimited => {
                (t("sess.wait"), grad_at(&proc_grad, 50.0))
            }
            crate::model::SessionStatus::Unknown => (t("sess.unknown"), theme.inactive_fg),
            crate::model::SessionStatus::Done => (t("sess.done"), theme.inactive_fg),
        };

        let is_1m = session.context_window >= 1_000_000 || session.model.contains("[1m]");
        let model_short = shorten_model(&session.model, is_1m);
        let ctx_color = grad_at(&proc_grad, session.context_percent);

        let is_done = matches!(session.status, crate::model::SessionStatus::Done);
        let row_style = if selected {
            Style::default()
                .bg(theme.selected_bg)
                .fg(theme.selected_fg)
                .add_modifier(Modifier::BOLD)
        } else if is_done {
            Style::default().fg(theme.inactive_fg)
        } else {
            Style::default()
        };

        let mut cells = vec![
            Cell::from(Span::styled(marker, Style::default().fg(theme.hi_fg))),
            Cell::from(Span::styled(agent_label, Style::default().fg(agent_color))),
        ];
        cells.push(Cell::from(Span::styled(
            truncate_str(&status_icon_str, status_w as usize),
            Style::default().fg(status_color),
        )));
        cells.push(Cell::from(Span::styled(
            truncate_str(
                if session.session_name.is_empty() {
                    "—"
                } else {
                    &session.session_name
                },
                session_name_w as usize,
            ),
            Style::default().fg(theme.session_id),
        )));
        cells.push(Cell::from(Span::styled(
            truncate_str(&model_short, model_w as usize),
            Style::default().fg(if model_short == "-" {
                theme.inactive_fg
            } else {
                theme.graph_text
            }),
        )));
        cells.push(Cell::from(Span::styled(
            format!("{:.0}%", session.context_percent),
            Style::default().fg(ctx_color),
        )));
        cells.push(Cell::from(Span::styled(
            fmt_tokens(session.total_tokens()),
            Style::default().fg(theme.main_fg),
        )));

        rows.push(Row::new(cells).style(row_style).height(1));

        // Tree view: show subagents as indented rows
        if app.tree_view && !session.subagents.is_empty() {
            for (sa_idx, sa) in session.subagents.iter().enumerate() {
                let is_last = sa_idx == session.subagents.len() - 1;
                // Tree connector fits the agent column (was truncated before).
                let prefix = if is_last { "└─" } else { "├─" };
                let is_working = sa.status.eq_ignore_ascii_case("working")
                    || sa.status.eq_ignore_ascii_case("in_progress");
                let icon = if is_working { "●" } else { "✓" };
                let sa_fg = if is_working {
                    theme.proc_misc
                } else {
                    theme.inactive_fg
                };

                let mut sa_cells: Vec<Cell> = vec![
                    Cell::from(""),
                    Cell::from(Span::styled(prefix, Style::default().fg(theme.div_line))),
                    Cell::from(Span::styled(icon, Style::default().fg(sa_fg))),
                ];
                sa_cells.push(Cell::from(Span::styled(
                    truncate_str(&sa.name, session_name_w as usize),
                    Style::default().fg(theme.graph_text),
                )));
                sa_cells.push(Cell::from(""));
                sa_cells.push(Cell::from(""));
                sa_cells.push(Cell::from(Span::styled(
                    fmt_tokens(sa.tokens),
                    Style::default().fg(theme.graph_text),
                )));
                rows.push(Row::new(sa_cells).height(1));
            }
        }
    }

    let header_style = Style::default()
        .fg(theme.main_fg)
        .add_modifier(Modifier::BOLD);
    let header_cells = vec![
        Cell::from(""),
        Cell::from(Span::styled(t("col.ai"), header_style)),
        Cell::from(Span::styled(t("col.status"), header_style)),
        Cell::from(Span::styled(t("col.session_name"), header_style)),
        Cell::from(Span::styled(t("col.model"), header_style)),
        Cell::from(Span::styled(context_label, header_style)),
        Cell::from(Span::styled(t("col.tokens"), header_style)),
    ];
    let header = Row::new(header_cells).height(1);

    let widths_vec: Vec<Constraint> = vec![
        Constraint::Length(1), // marker
        Constraint::Length(8), // full agent name and optional App/IDE suffix
        Constraint::Length(status_w),
        Constraint::Length(session_name_w),
        Constraint::Fill(1), // model (fills remaining)
        Constraint::Length(context_w),
        Constraint::Length(tokens_w),
    ];

    // Scroll: rows vary per session in tree view; use the built row list as the source of truth.
    let visible_sessions = app.visible_indices();
    let total_rows = rows.len();
    let needs_scroll = total_rows > panel_chunks[0].height.saturating_sub(1) as usize;

    // Split table area into [table | scrollbar(1)] when scrollable
    let table_area;
    let scrollbar_area: Option<Rect>;
    if needs_scroll && panel_chunks[0].width > 2 {
        let hsplit = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(panel_chunks[0]);
        table_area = hsplit[0];
        scrollbar_area = Some(hsplit[1]);
    } else {
        table_area = panel_chunks[0];
        scrollbar_area = None;
    }

    let visible_rows = table_area.height.saturating_sub(1) as usize; // -1 for header
                                                                     // Row offset for the selected session, accounting for subagent rows in tree view
                                                                     // and filter-hidden sessions above it.
    let selected_pos = visible_sessions
        .iter()
        .position(|&i| i == app.selected)
        .unwrap_or(0);
    let selected_row_start: usize = visible_sessions
        .iter()
        .take(selected_pos)
        .map(|&i| {
            let base = 1;
            if app.tree_view {
                base + app.sessions[i].subagents.len()
            } else {
                base
            }
        })
        .sum();
    let selected_session_rows = if app.tree_view {
        1 + app
            .sessions
            .get(app.selected)
            .map_or(0, |s| s.subagents.len())
    } else {
        1
    };
    let selected_row_end = selected_row_start + selected_session_rows;
    let scroll_offset = selected_row_end.saturating_sub(visible_rows);
    let visible = if scroll_offset < rows.len() {
        rows.into_iter().skip(scroll_offset).collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    f.render_widget(Clear, table_area);
    // Clear resets to the terminal default background; repaint the theme's.
    let table = Table::new(visible, widths_vec)
        .header(header)
        .style(Style::default().bg(theme.main_bg).fg(theme.main_fg));
    f.render_widget(table, table_area);

    // ── Scrollbar column (dedicated 1-char width, btop-style) ──
    if let Some(sb) = scrollbar_area {
        let bar_h = sb.height as usize;
        if bar_h > 0 {
            let thumb_size = ((visible_rows as f64 / total_rows as f64) * bar_h as f64)
                .ceil()
                .max(1.0) as usize;
            let thumb_size = thumb_size.min(bar_h);
            let thumb_pos = if total_rows > visible_rows {
                ((scroll_offset as f64 / (total_rows - visible_rows) as f64)
                    * (bar_h - thumb_size) as f64)
                    .round() as usize
            } else {
                0
            };

            let buf = f.buffer_mut();
            for i in 0..bar_h {
                let y = sb.y + i as u16;
                let (ch, color) = if i >= thumb_pos && i < thumb_pos + thumb_size {
                    ("┃", theme.main_fg)
                } else {
                    ("│", theme.div_line)
                };
                buf[(sb.x, y)].set_symbol(ch).set_fg(color);
            }

            // ↑/↓ arrows at edges when more content exists
            if scroll_offset > 0 {
                buf[(sb.x, sb.y)].set_symbol("↑").set_fg(theme.cpu_box);
            }
            if scroll_offset + visible_rows < total_rows {
                buf[(sb.x, sb.y + sb.height - 1)]
                    .set_symbol("↓")
                    .set_fg(theme.cpu_box);
            }
        }
    }

    // ── Detail section for selected session (full-width Paragraph, not Table) ──
    // Chat scrolling is a no-op unless the chat view below sets its metrics.
    app.chat_view.set(ChatViewMetrics::default());
    if let Some(session) = app.sessions.get(app.selected) {
        let detail_area = panel_chunks[2];
        if detail_area.height < 3 {
            return;
        }

        // Reserve bottom lines for MEM + version
        let footer_h = 3u16;
        let detail_body_h = detail_area.height.saturating_sub(footer_h);
        let detail_body = Rect {
            x: detail_area.x,
            y: detail_area.y,
            width: detail_area.width,
            height: detail_body_h,
        };
        let detail_footer = Rect {
            x: detail_area.x,
            y: detail_area.y + detail_body_h,
            width: detail_area.width,
            height: footer_h.min(detail_area.height),
        };

        let has_children = !session.children.is_empty();
        let has_subagents = !session.subagents.is_empty();
        let has_tool_calls = !session.tool_calls.is_empty();
        let has_chat = !app.chat_items(session).is_empty();
        let has_left_detail = has_children || has_subagents;
        let has_file_audit = app.show_file_audit && !session.file_accesses.is_empty();
        // Focus mode: file audit (F) takes priority over timeline (L) when both
        // are toggled on. Only one "full lower" mode is active at a time.
        let file_audit_focused = has_file_audit;
        let timeline_focused = !file_audit_focused && app.show_timeline && has_tool_calls;
        // Default detail is chat alone when available; otherwise fall back to
        // the compact timeline, split with children/subagents on a wide
        // enough terminal.
        let chat_full_width = !file_audit_focused && !app.show_timeline && has_chat;
        const TIMELINE_SPLIT_MIN_WIDTH: u16 = 120;
        let timeline_default = !file_audit_focused
            && !app.show_timeline
            && !chat_full_width
            && has_tool_calls
            && detail_body.width >= TIMELINE_SPLIT_MIN_WIDTH;
        let timeline_side_by_side = timeline_default && has_left_detail;
        let timeline_full_width = timeline_default && !has_left_detail;

        // Chat is the default lower view; a session without recent messages
        // (for example OpenCode, whose conversation is not collected) gets a
        // one-line notice under the SESSION title instead.
        let chat_empty_notice =
            !file_audit_focused && !app.show_timeline && !has_chat && detail_body.height >= 3;
        // Always show SESSION header at top, then children/subagents/timeline/file_audit below
        let session_header_h: u16 = if chat_empty_notice { 2 } else { 1 }; // SESSION title (+ notice)
        let has_lower = file_audit_focused
            || timeline_focused
            || chat_full_width
            || timeline_full_width
            || timeline_side_by_side
            || has_children
            || has_subagents;
        let (header_area, lower_area) = if has_lower {
            let parts = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(session_header_h), Constraint::Min(1)])
                .split(detail_body);
            (parts[0], Some(parts[1]))
        } else {
            (detail_body, None)
        };

        // SESSION header — always rendered
        {
            let mut lines = Vec::new();
            let sid_short = if session.session_id.len() >= 8 {
                &session.session_id[..8]
            } else {
                &session.session_id
            };
            let session_ref = if header_area.width <= 80 {
                format!("►{} · {}", sid_short, session.project_name)
            } else {
                format!("►{} · {}", session.session_id, session.cwd)
            };
            lines.push(Line::from(Span::styled(
                truncate_str(
                    &format!(" {} ({})", t("detail.session").as_str(), session_ref),
                    header_area.width as usize,
                ),
                Style::default()
                    .fg(theme.title)
                    .add_modifier(Modifier::BOLD),
            )));
            if chat_empty_notice {
                let reason = if session.agent_cli == "opencode" {
                    "conversation text is not collected for OpenCode sessions"
                } else {
                    "no recent messages yet"
                };
                lines.push(Line::from(vec![
                    Span::styled(
                        format!(" {}", t("detail.chat")),
                        Style::default()
                            .fg(theme.title)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        truncate_str(
                            &format!(" · {reason}"),
                            (header_area.width as usize).saturating_sub(5),
                        ),
                        Style::default().fg(theme.inactive_fg),
                    ),
                ]));
            }
            f.render_widget(Paragraph::new(lines), header_area);
        }

        // Layout below the session header:
        //   - file audit focus (F): full-width file audit
        //   - timeline focus (L): full-width timeline
        //   - default chat: full-width chat only
        //   - wide terminal with left detail + tool calls: split lower area
        //   - wide terminal with only tool calls: full-width timeline
        //   - otherwise: children/subagents only (or nothing)
        if let Some(lower) = lower_area {
            if file_audit_focused {
                draw_file_audit(f, session, lower, theme);
            } else if timeline_focused || timeline_full_width {
                draw_timeline(f, session, lower, theme, app.timeline_scroll);
            } else if chat_full_width {
                draw_chat_history(f, app, session, lower, theme);
            } else {
                let (left_area, right_detail_area) = if timeline_side_by_side {
                    let split = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                        .split(lower);
                    (split[0], Some(split[1]))
                } else {
                    (lower, None)
                };

                if let Some(detail_area) = right_detail_area {
                    draw_timeline(f, session, detail_area, theme, app.timeline_scroll);
                }

                if has_children || has_subagents {
                    let body_chunks = if has_children && has_subagents {
                        if left_area.width < 90 {
                            Layout::default()
                                .direction(Direction::Vertical)
                                .constraints([
                                    Constraint::Percentage(50),
                                    Constraint::Percentage(50),
                                ])
                                .split(left_area)
                        } else {
                            Layout::default()
                                .direction(Direction::Horizontal)
                                .constraints([
                                    Constraint::Percentage(45),
                                    Constraint::Percentage(55),
                                ])
                                .split(left_area)
                        }
                    } else {
                        Layout::default()
                            .direction(Direction::Horizontal)
                            .constraints([Constraint::Percentage(100)])
                            .split(left_area)
                    };

                    // Children (left side)
                    if has_children {
                        let children_area = body_chunks[0];
                        let mut lines = Vec::new();
                        lines.push(Line::from(Span::styled(
                            format!(" {}", t("detail.children").as_str()),
                            Style::default()
                                .fg(theme.title)
                                .add_modifier(Modifier::BOLD),
                        )));
                        for child in &session.children {
                            let cmd_short = child
                                .command
                                .split_whitespace()
                                .take(3)
                                .collect::<Vec<_>>()
                                .join(" ");
                            let port_str =
                                child.port.map(|p| format!(" :{}", p)).unwrap_or_default();
                            let max_cmd = (children_area.width as usize).saturating_sub(18);
                            lines.push(Line::from(vec![
                                Span::styled(
                                    format!(" {:<6}", child.pid),
                                    Style::default().fg(theme.main_fg),
                                ),
                                Span::styled(
                                    truncate_str(&cmd_short, max_cmd),
                                    Style::default().fg(theme.graph_text),
                                ),
                                Span::styled(
                                    format!(" {:>5}", fmt_mem_kb(child.mem_kb)),
                                    Style::default().fg(theme.graph_text),
                                ),
                                Span::styled(port_str, Style::default().fg(theme.proc_misc)),
                            ]));
                        }
                        f.render_widget(Paragraph::new(lines), children_area);
                    }

                    // Subagents (right side, or full width if no children)
                    if has_subagents {
                        let sa_area = if has_children {
                            body_chunks[1]
                        } else {
                            body_chunks[0]
                        };

                        let mut lines = Vec::new();
                        lines.push(Line::from(Span::styled(
                            format!(" {}", t("detail.subagents").as_str()),
                            Style::default()
                                .fg(theme.title)
                                .add_modifier(Modifier::BOLD),
                        )));

                        let col_w = sa_area.width as usize;
                        let use_two_cols = session.subagents.len() > 6 && col_w >= 50;

                        if use_two_cols {
                            let half_w = col_w / 2;
                            let name_w = half_w.saturating_sub(12);
                            let mid = session.subagents.len().div_ceil(2);
                            let left_agents = &session.subagents[..mid];
                            let right_agents = &session.subagents[mid..];

                            for (row_idx, sa) in left_agents.iter().enumerate() {
                                let mut spans = Vec::new();
                                // Left column
                                let icon = if sa.status == "working" { "●" } else { "✓" };
                                let fg = if sa.status == "working" {
                                    theme.main_fg
                                } else {
                                    theme.graph_text
                                };
                                spans.push(Span::styled(
                                    format!(
                                        "  {} {:<w$}",
                                        icon,
                                        truncate_str(&sa.name, name_w),
                                        w = name_w
                                    ),
                                    Style::default().fg(fg),
                                ));
                                spans.push(Span::styled(
                                    format!("{:>6}", fmt_tokens(sa.tokens)),
                                    Style::default().fg(theme.graph_text),
                                ));

                                // Right column
                                if let Some(sa_r) = right_agents.get(row_idx) {
                                    let icon_r = if sa_r.status == "working" {
                                        "●"
                                    } else {
                                        "✓"
                                    };
                                    let fg_r = if sa_r.status == "working" {
                                        theme.main_fg
                                    } else {
                                        theme.graph_text
                                    };
                                    spans.push(Span::styled(
                                        format!(
                                            "  {} {:<w$}",
                                            icon_r,
                                            truncate_str(&sa_r.name, name_w),
                                            w = name_w
                                        ),
                                        Style::default().fg(fg_r),
                                    ));
                                    spans.push(Span::styled(
                                        format!("{:>6}", fmt_tokens(sa_r.tokens)),
                                        Style::default().fg(theme.graph_text),
                                    ));
                                }
                                lines.push(Line::from(spans));
                            }
                        } else {
                            let name_w = col_w.saturating_sub(12);
                            for sa in &session.subagents {
                                let icon = if sa.status == "working" { "●" } else { "✓" };
                                let fg = if sa.status == "working" {
                                    theme.main_fg
                                } else {
                                    theme.graph_text
                                };
                                lines.push(Line::from(vec![
                                    Span::styled(
                                        format!(
                                            "  {} {:<w$}",
                                            icon,
                                            truncate_str(&sa.name, name_w),
                                            w = name_w
                                        ),
                                        Style::default().fg(fg),
                                    ),
                                    Span::styled(
                                        format!("{:>6}", fmt_tokens(sa.tokens)),
                                        Style::default().fg(theme.graph_text),
                                    ),
                                ]));
                            }
                        }
                        f.render_widget(Paragraph::new(lines), sa_area);
                    }
                } // end if has_children || has_subagents
            } // end else (not focused)
        }

        // Footer: MEM + version (full width)
        {
            let cpu_grad =
                make_gradient(theme.cpu_grad.start, theme.cpu_grad.mid, theme.cpu_grad.end);
            let mem_color = if session.mem_line_count >= 180 {
                grad_at(&cpu_grad, 100.0)
            } else {
                theme.graph_text
            };
            let mut footer_lines = vec![Line::from("")];
            // MEM line only for Claude Code sessions (Codex has no memory system)
            if session.agent_cli == "claude" {
                footer_lines.push(Line::from(Span::styled(
                    format!(
                        " {} {} · {}/200 · 200 lines",
                        t("detail.mem").as_str(),
                        session.mem_file_count,
                        session.mem_line_count
                    ),
                    Style::default().fg(mem_color),
                )));
            }
            // Context evolution sparkline (if history available)
            if !session.context_history.is_empty() && session.context_window > 0 {
                let normalized: Vec<f64> = session
                    .context_history
                    .iter()
                    .map(|&v| (v as f64 / session.context_window as f64).min(1.0))
                    .collect();
                let spark_w = (detail_footer.width as usize)
                    .saturating_sub(16)
                    .clamp(4, 40);
                let mut ctx_spans = vec![Span::styled(
                    format!(" {} ", t("detail.ctx").as_str()),
                    Style::default().fg(theme.graph_text),
                )];
                ctx_spans.extend(super::braille_sparkline(
                    &normalized,
                    spark_w,
                    &cpu_grad,
                    theme.graph_text,
                ));
                if session.compaction_count > 0 {
                    ctx_spans.push(Span::styled(
                        format!(" C{}", session.compaction_count),
                        Style::default().fg(grad_at(&cpu_grad, 80.0)),
                    ));
                }
                footer_lines.push(Line::from(ctx_spans));
            }
            let effort_part = if session.effort.is_empty() {
                String::new()
            } else {
                format!(" · effort: {}", session.effort)
            };
            footer_lines.push(Line::from(Span::styled(
                format!(
                    " {} · {} · {} turns{}",
                    session.version,
                    session.elapsed_display(),
                    session.turn_count,
                    effort_part,
                ),
                Style::default().fg(theme.inactive_fg),
            )));
            f.render_widget(Paragraph::new(footer_lines), detail_footer);
        }
    }
}

/// Label, label color and text color of one chat row. Tool calls and results
/// reuse theme colors that differ from the conversation: calls get their own
/// label color, results are dimmed and indented by the layout, and errors are
/// warning-colored.
fn chat_row_style(role: &ChatRole, theme: &Theme) -> (&'static str, Color, Color) {
    match role {
        ChatRole::User => ("U", theme.hi_fg, theme.main_fg),
        ChatRole::Assistant => ("A", theme.proc_misc, theme.main_fg),
        ChatRole::ToolCall => ("▸", theme.session_id, theme.main_fg),
        ChatRole::ToolResult => ("└", theme.graph_text, theme.graph_text),
        ChatRole::ToolError => ("✗", theme.warning_fg, theme.warning_fg),
    }
}

/// Render the retained recent items of the session, oldest first, as a
/// read-only chat: user/assistant messages plus tool calls and results while
/// tool details are on (`d`). The view follows the newest item until the user
/// scrolls up (PgUp/PgDn, End to return).
fn draw_chat_history(f: &mut Frame, app: &App, session: &AgentSession, area: Rect, theme: &Theme) {
    let items = app.chat_items(session);
    if items.is_empty() || area.width == 0 || area.height == 0 {
        return;
    }

    let width = area.width as usize;
    let text_w = width.saturating_sub(CHROME_COLS);
    let visible = area.height.saturating_sub(1) as usize;
    let count = items.len();
    let (unit, one) = if items.iter().any(|m| !m.role.is_dialogue()) {
        ("items", "item")
    } else {
        ("messages", "message")
    };
    // The collector keeps the latest 12 messages / 80 items; a full window
    // means older ones were dropped.
    let window_full = session.chat_messages.len() >= MAX_CHAT_ITEMS
        || session
            .chat_messages
            .iter()
            .filter(|m| m.role.is_dialogue())
            .count()
            >= MAX_CHAT_MESSAGES;
    let mut notice = if window_full {
        format!(" · latest {count} {unit} only")
    } else if count == 1 {
        format!(" · 1 {one}")
    } else {
        format!(" · {count} {unit}")
    };
    if !app.show_chat_tools && session.chat_messages.iter().any(|m| !m.role.is_dialogue()) {
        notice.push_str(" · tools hidden (d)");
    }

    let mut body = Vec::new();
    let mut right = Vec::new();
    let mut scrollbar = None;
    if text_w > 0 && visible > 0 {
        let layout = ChatLayout::new(&items, text_w);
        app.chat_view.set(ChatViewMetrics {
            text_width: text_w,
            visible_rows: visible,
        });
        let total = layout.total_rows();
        let offset = app.chat_offset(&layout, visible);
        let end = (offset + visible).min(total);
        for row in &layout.rows[offset..end] {
            let (label, label_color, text_color) = chat_row_style(&items[row.message].role, theme);
            let prefix = if row.first {
                Span::styled(
                    format!(" {label} "),
                    Style::default()
                        .fg(label_color)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::raw("   ")
            };
            body.push(Line::from(vec![
                prefix,
                Span::styled(row.text.clone(), Style::default().fg(text_color)),
            ]));
        }
        if total > visible {
            let at_bottom = end == total;
            let range = format!("{}-{}/{}", offset + 1, end, total);
            // Widest first; the title keeps the first variant that fits.
            right = if at_bottom {
                vec![format!("rows {range} "), format!("{range} ")]
            } else {
                vec![
                    format!("rows {range} · End: latest "),
                    format!("{range} · End: latest "),
                    "End: latest ".to_string(),
                    "↓ ".to_string(),
                ]
            };
            scrollbar = Some((total, offset));
        }
    }

    let left_w = UnicodeWidthStr::width(t("detail.chat").as_str()) + 1;
    let notice_w = UnicodeWidthStr::width(notice.as_str());
    let title_style = Style::default()
        .fg(theme.title)
        .add_modifier(Modifier::BOLD);
    let note_style = Style::default().fg(theme.inactive_fg);
    let mut title = vec![Span::styled(format!(" {}", t("detail.chat")), title_style)];
    if left_w + notice_w <= width {
        title.push(Span::styled(notice, note_style));
    } else {
        title.push(Span::styled(
            truncate_str(&notice, width.saturating_sub(left_w)),
            note_style,
        ));
    }
    let used: usize = title.iter().map(|span| span.width()).sum();
    if let Some(text) = right
        .iter()
        .find(|text| used + UnicodeWidthStr::width(text.as_str()) <= width)
    {
        let pad = width - used - UnicodeWidthStr::width(text.as_str());
        title.push(Span::raw(" ".repeat(pad)));
        title.push(Span::styled(text.clone(), note_style));
    }

    let mut lines = vec![Line::from(title)];
    lines.extend(body);
    f.render_widget(Paragraph::new(lines), area);
    if let Some((total, offset)) = scrollbar {
        draw_chat_scrollbar(
            f,
            Rect {
                x: area.x + area.width - 1,
                y: area.y + 1,
                width: 1,
                height: visible as u16,
            },
            total,
            visible,
            offset,
            theme,
        );
    }
}

/// One-column scrollbar in the style of the session list's.
fn draw_chat_scrollbar(
    f: &mut Frame,
    bar: Rect,
    total: usize,
    visible: usize,
    offset: usize,
    theme: &Theme,
) {
    let bar_h = bar.height as usize;
    if bar_h == 0 || total <= visible {
        return;
    }
    let thumb_size = (((visible as f64 / total as f64) * bar_h as f64).ceil() as usize)
        .max(1)
        .min(bar_h);
    let thumb_pos =
        ((offset as f64 / (total - visible) as f64) * (bar_h - thumb_size) as f64).round() as usize;
    let buf = f.buffer_mut();
    for i in 0..bar_h {
        let (symbol, color) = if i >= thumb_pos && i < thumb_pos + thumb_size {
            ("┃", theme.main_fg)
        } else {
            ("│", theme.div_line)
        };
        buf[(bar.x, bar.y + i as u16)]
            .set_symbol(symbol)
            .set_fg(color);
    }
    if offset > 0 {
        buf[(bar.x, bar.y)].set_symbol("↑").set_fg(theme.cpu_box);
    }
    if offset + visible < total {
        buf[(bar.x, bar.y + bar.height - 1)]
            .set_symbol("↓")
            .set_fg(theme.cpu_box);
    }
}

/// Render the file access audit log in the given area.
fn draw_file_audit(f: &mut Frame, session: &AgentSession, area: Rect, theme: &Theme) {
    use std::collections::HashSet;
    let unique_files: HashSet<&str> = session
        .file_accesses
        .iter()
        .map(|a| a.path.as_str())
        .collect();
    let unique_count = unique_files.len();
    let total_count = session.file_accesses.len();

    let mut lines = Vec::new();
    lines.push(Line::from(Span::styled(
        format!(
            " {} ({} accesses, {} unique files)",
            t("detail.file_audit").as_str(),
            total_count,
            unique_count
        ),
        Style::default()
            .fg(theme.title)
            .add_modifier(Modifier::BOLD),
    )));

    let max_rows = area.height.saturating_sub(1) as usize;
    let max_path_w = (area.width as usize).saturating_sub(5);

    // Show most recent entries first (reverse order), limited to available rows
    for access in session.file_accesses.iter().rev().take(max_rows) {
        let (label, color) = match access.operation {
            FileOp::Read => ("R", theme.session_id), // blue
            FileOp::Edit => ("E", theme.proc_misc),  // yellow
            FileOp::Write => ("W", theme.cpu_box),   // cyan
        };
        let max_path = max_path_w.saturating_sub(4); // room for turn index
        let path_display = truncate_str(&access.path, max_path);
        lines.push(Line::from(vec![
            Span::styled(format!("  {} ", label), Style::default().fg(color)),
            Span::styled(path_display, Style::default().fg(theme.main_fg)),
            Span::styled(
                format!(" t{}", access.turn_index),
                Style::default().fg(theme.inactive_fg),
            ),
        ]));
    }

    f.render_widget(Paragraph::new(lines), area);
}

pub(crate) fn shorten_model(model: &str, is_1m: bool) -> String {
    // "claude-opus-4-6" → "opus4.6", "claude-sonnet-4-6" → "sonnet4.6", "claude-haiku-4-5" → "haiku4.5"
    let s = model.strip_prefix("claude-").unwrap_or(model);
    let s = s.trim_end_matches("[1m]");
    // Extract name and version: "opus-4-6" → ("opus", "4.6")
    let base = if let Some(pos) = s.find(|c: char| c.is_ascii_digit()) {
        let name = s[..pos].trim_end_matches('-');
        let ver = s[pos..].replace('-', ".");
        format!("{}{}", name, ver)
    } else {
        s.to_string()
    };
    if is_1m {
        format!("{}[1m]", base)
    } else {
        base
    }
}

/// Tool name → color mapping for timeline bars, using theme palette.
fn tool_color(name: &str, theme: &Theme) -> Color {
    match name {
        "Read" => theme.session_id, // typically a warm accent
        "Edit" => theme.proc_misc,  // green/active color
        "Write" => theme.cpu_box,   // box/border accent
        "Bash" => theme.hi_fg,      // highlight foreground
        "shell" | "exec_command" | "write_stdin" => theme.hi_fg,
        "apply_patch" => theme.proc_misc,
        "update_plan" => theme.title,
        "spawn_agent" | "send_input" | "wait_agent" => theme.title,
        "view_image" => theme.session_id,
        "Grep" => theme.status_fg,  // status accent
        "Glob" => theme.graph_text, // subtle text
        "find" | "list_mcp_resources" | "read_mcp_resource" => theme.status_fg,
        "Agent" => theme.title,       // title/emphasis
        "Skill" => theme.selected_fg, // selected foreground
        _ => theme.inactive_fg,       // fallback
    }
}

fn tool_label(name: &str) -> &str {
    match name {
        "exec_command" | "shell" => "Exec",
        "write_stdin" => "Input",
        "apply_patch" => "Patch",
        "update_plan" => "Plan",
        "spawn_agent" => "Agent",
        "send_input" => "Send",
        "wait_agent" => "Wait",
        "view_image" => "Image",
        "list_mcp_resources" | "read_mcp_resource" => "MCP",
        other => other,
    }
}

fn fmt_duration(ms: u64) -> String {
    if ms >= 60_000 {
        format!("{}m{:.0}s", ms / 60_000, (ms % 60_000) as f64 / 1000.0)
    } else if ms >= 1000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{}ms", ms)
    }
}

fn draw_timeline(
    f: &mut Frame,
    session: &crate::model::AgentSession,
    area: Rect,
    theme: &Theme,
    scroll: usize,
) {
    let tool_calls = &session.tool_calls;
    let is_thinking = session.thinking_since_ms > 0
        && matches!(
            session.status,
            crate::model::SessionStatus::Thinking
                | crate::model::SessionStatus::Executing
                | crate::model::SessionStatus::Waiting
                | crate::model::SessionStatus::Unknown
        );
    if tool_calls.is_empty() && !is_thinking {
        return;
    }

    // Live duration for any tool still in flight. The collector leaves
    // `duration_ms == 0` on tools whose assistant turn hasn't been closed yet;
    // combined with `pending_since_ms > 0` that means "tool started at
    // pending_since_ms and is still running right now." We compute the elapsed
    // ms on every frame so the bar appears to grow in real time.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let live_duration = |tc: &crate::model::ToolCall| -> u64 {
        if tc.duration_ms > 0 {
            tc.duration_ms
        } else if session.pending_since_ms > 0 {
            now_ms.saturating_sub(session.pending_since_ms)
        } else {
            0
        }
    };
    let is_pending = |tc: &crate::model::ToolCall| -> bool {
        tc.duration_ms == 0 && session.pending_since_ms > 0
    };
    let thinking_duration = if is_thinking {
        now_ms.saturating_sub(session.thinking_since_ms)
    } else {
        0
    };

    let total_duration: u64 = tool_calls.iter().map(live_duration).sum();
    let max_duration = tool_calls
        .iter()
        .map(live_duration)
        .max()
        .unwrap_or(1)
        .max(1);

    let mut lines = Vec::new();

    // Header — note "1 running" / "thinking Xs" if the session is live.
    let pending_count = tool_calls.iter().filter(|tc| is_pending(tc)).count();
    let mut status_notes: Vec<String> = Vec::new();
    if pending_count > 0 {
        status_notes.push(format!("{} running", pending_count));
    }
    if is_thinking {
        status_notes.push(format!("thinking {}", fmt_duration(thinking_duration)));
    }
    let running_note = if status_notes.is_empty() {
        String::new()
    } else {
        format!(", {}", status_notes.join(", "))
    };
    lines.push(Line::from(vec![Span::styled(
        format!(
            " {} ({} calls, {}{})",
            t("detail.timeline").as_str(),
            tool_calls.len(),
            fmt_duration(total_duration),
            running_note,
        ),
        Style::default()
            .fg(theme.title)
            .add_modifier(Modifier::BOLD),
    )]));

    // Available width for the bar: total width - name(7) - arg(22) - duration(8) - padding(5)
    let bar_width = (area.width as usize).saturating_sub(42).max(5);

    // Render each tool call as a row. Reserve the last visible row for the
    // live Thinking row when the model is between turns.
    let header_rows = 1;
    let thinking_rows = if is_thinking { 1 } else { 0 };
    let visible_rows = (area.height as usize).saturating_sub(header_rows + thinking_rows);
    let start = scroll.min(tool_calls.len().saturating_sub(visible_rows));

    for tc in tool_calls.iter().skip(start).take(visible_rows) {
        let duration = live_duration(tc);
        let pending = is_pending(tc);
        let bar_fill = if max_duration > 0 {
            ((duration as f64 / max_duration as f64) * bar_width as f64).ceil() as usize
        } else {
            0
        };
        let bar_fill = bar_fill.min(bar_width);
        let bar_empty = bar_width - bar_fill;

        let is_longest = duration == max_duration && max_duration > 0 && !pending;
        let star = if is_longest { " *" } else { "" };

        let color = tool_color(&tc.name, theme);
        // Prefix running rows with a pulsing ● so they're obvious at a glance.
        // The pulse is cheap: flip between bright/dim on a 2-tick (~4s) cycle
        // using the same clock we used for the live duration.
        let pulse_bright = pending && (now_ms / 500).is_multiple_of(2);
        let name_prefix = if pending { "●" } else { " " };
        let name_style = if pending {
            Style::default().fg(color).add_modifier(if pulse_bright {
                Modifier::BOLD
            } else {
                Modifier::DIM
            })
        } else {
            Style::default().fg(color).add_modifier(Modifier::BOLD)
        };
        let bar_style = if pending {
            Style::default().fg(color).add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(color)
        };

        let duration_label = if pending {
            format!(" {:>5}…", fmt_duration(duration))
        } else {
            format!(" {:>6}{}", fmt_duration(duration), star)
        };
        let duration_color = if is_longest {
            theme.proc_misc
        } else if pending {
            color
        } else {
            theme.graph_text
        };

        let name_label = super::truncate_str(tool_label(&tc.name), 6);
        lines.push(Line::from(vec![
            Span::styled(format!("{}{:<6}", name_prefix, name_label), name_style),
            Span::styled(
                format!(" {:<20}", super::truncate_str(&tc.arg, 20)),
                Style::default().fg(theme.graph_text),
            ),
            Span::styled(" ", Style::default()),
            Span::styled("█".repeat(bar_fill), bar_style),
            Span::styled("░".repeat(bar_empty), Style::default().fg(theme.div_line)),
            Span::styled(duration_label, Style::default().fg(duration_color)),
        ]));
    }

    // Virtual "Thinking" row — the model is generating its next turn, which
    // never shows up as a tool_use in the JSONL. Growth scales against the
    // longest tool so short thinks fill gradually; long thinks cap at full.
    if is_thinking {
        let color = theme.title;
        let pulse_bright = (now_ms / 500).is_multiple_of(2);
        let bar_fill = if max_duration > 0 {
            ((thinking_duration as f64 / max_duration as f64) * bar_width as f64).ceil() as usize
        } else {
            bar_width
        };
        let bar_fill = bar_fill.min(bar_width);
        let bar_empty = bar_width - bar_fill;
        let name_style = Style::default().fg(color).add_modifier(if pulse_bright {
            Modifier::BOLD
        } else {
            Modifier::DIM
        });
        let bar_style = Style::default().fg(color).add_modifier(Modifier::DIM);
        lines.push(Line::from(vec![
            Span::styled("●Think ", name_style),
            Span::styled(
                format!(" {:<20}", "generating reply"),
                Style::default().fg(theme.graph_text),
            ),
            Span::styled(" ", Style::default()),
            Span::styled("█".repeat(bar_fill), bar_style),
            Span::styled("░".repeat(bar_empty), Style::default().fg(theme.div_line)),
            Span::styled(
                format!(" {:>5}…", fmt_duration(thinking_duration)),
                Style::default().fg(color),
            ),
        ]));
    }

    f.render_widget(Paragraph::new(lines), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PanelVisibility;
    use crate::model::{
        ChatMessage, ChatRole, ChildProcess, LaunchSurface, SessionStatus, SubAgent,
        MAX_CHAT_MESSAGES,
    };
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn codex_exec_command_uses_bash_color() {
        let theme = Theme::default();
        assert_eq!(
            tool_color("exec_command", &theme),
            tool_color("Bash", &theme)
        );
    }

    #[test]
    fn codex_tool_labels_fit_timeline_name_column() {
        assert_eq!(tool_label("exec_command"), "Exec");
        assert_eq!(tool_label("update_plan"), "Plan");
        assert!(tool_label("exec_command").len() <= 6);
    }

    #[test]
    fn codex_non_1m_context_window_does_not_show_1m_suffix() {
        let mut app = App::new_with_config(Theme::default(), &[], PanelVisibility::default());
        app.sessions.push(AgentSession {
            agent_cli: "codex",
            launch_surface: LaunchSurface::Cli,
            pid: 42,
            jump_pid: Some(42),
            session_id: "codex-session".into(),
            session_name: "Named conversation".into(),
            cwd: "/tmp/project".into(),
            project_name: "project".into(),
            started_at: 0,
            status: SessionStatus::Waiting,
            model: "gpt-5".into(),
            effort: String::new(),
            context_percent: 58.7,
            total_input_tokens: 1_000,
            total_output_tokens: 500,
            total_cache_read: 0,
            total_cache_create: 0,
            turn_count: 1,
            current_tasks: vec!["waiting for input".into()],
            mem_mb: 0,
            version: String::new(),
            git_branch: String::new(),
            git_added: 0,
            git_modified: 0,
            token_history: Vec::new(),
            context_history: Vec::new(),
            compaction_count: 0,
            context_window: 258_400,
            subagents: Vec::new(),
            mem_file_count: 0,
            mem_line_count: 0,
            children: Vec::new(),
            initial_prompt: "prompt".into(),
            first_assistant_text: String::new(),
            chat_messages: Vec::new(),
            tool_calls: Vec::new(),
            pending_since_ms: 0,
            thinking_since_ms: 0,
            file_accesses: Vec::new(),
            config_root: String::new(),
        });

        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                draw_sessions_panel(
                    f,
                    &app,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 120,
                        height: 20,
                    },
                    &app.theme,
                )
            })
            .unwrap();
        let text = format!("{}", terminal.backend());

        assert!(
            text.contains("gpt5"),
            "model should render in session row\n{text}"
        );
        assert!(text.contains("Codex"), "Codex label should render\n{text}");
        assert!(
            !text.contains("[1m]"),
            "non-1M Codex context windows must not be labeled as 1M\n{text}"
        );
    }

    #[test]
    fn session_table_shows_requested_columns_in_order() {
        let mut app = App::new_with_config(Theme::default(), &[], PanelVisibility::default());
        let mut session = test_session("sid", "project");
        session.session_name = "Conversation title".into();
        app.sessions.push(session);

        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                draw_sessions_panel(
                    f,
                    &app,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 120,
                        height: 20,
                    },
                    &app.theme,
                )
            })
            .unwrap();
        let text = format!("{}", terminal.backend());
        let header = text.lines().nth(1).unwrap();
        let mut previous = 0;
        for label in ["AI", "Status", "Session Name", "Model", "Context", "Tokens"] {
            let position = header
                .find(label)
                .unwrap_or_else(|| panic!("missing {label} in {header}"));
            assert!(position >= previous, "{label} is out of order in {header}");
            previous = position + label.len();
        }
        assert!(!header.contains("Project"));
        assert!(!header.contains("Summary"));
        assert!(text.contains("Conversation title"));
    }

    #[test]
    fn session_table_shows_working_reply_and_wait_without_rate() {
        for (status, expected) in [
            (SessionStatus::Thinking, "Working"),
            (SessionStatus::Executing, "Working"),
            (SessionStatus::AwaitingInput, "Reply"),
            (SessionStatus::Waiting, "Wait"),
            (SessionStatus::RateLimited, "Wait"),
        ] {
            let mut app = App::new_with_config(Theme::default(), &[], PanelVisibility::default());
            let mut session = test_session("sid", "project");
            session.status = status;
            app.sessions.push(session);
            let backend = TestBackend::new(120, 20);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|f| {
                    draw_sessions_panel(
                        f,
                        &app,
                        Rect {
                            x: 0,
                            y: 0,
                            width: 120,
                            height: 20,
                        },
                        &app.theme,
                    )
                })
                .unwrap();
            let text = format!("{}", terminal.backend());
            let row = text.lines().nth(2).unwrap();
            assert!(row.contains(expected), "missing {expected} in {text}");
            assert!(
                !row.contains("Rate"),
                "Rate status should not render\n{text}"
            );
        }
    }

    #[test]
    fn agent_label_flags_app_and_ide_launch_surfaces() {
        let render = |surface: LaunchSurface| {
            let mut app = App::new_with_config(Theme::default(), &[], PanelVisibility::default());
            let mut session = test_session("sid", "proj");
            session.launch_surface = surface;
            app.sessions.push(session);

            let backend = TestBackend::new(120, 20);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|f| {
                    draw_sessions_panel(
                        f,
                        &app,
                        Rect {
                            x: 0,
                            y: 0,
                            width: 120,
                            height: 20,
                        },
                        &app.theme,
                    )
                })
                .unwrap();
            format!("{}", terminal.backend())
        };

        let cli_text = render(LaunchSurface::Cli);
        assert!(
            cli_text.contains("Claude"),
            "plain CLI session should render the full name\n{cli_text}"
        );
        assert!(!cli_text.contains("Claude a") && !cli_text.contains("Claude i"));

        let app_text = render(LaunchSurface::App);
        assert!(
            app_text.contains("Claude a"),
            "Claude desktop app session should get the 'a' suffix\n{app_text}"
        );

        let ide_text = render(LaunchSurface::Ide);
        assert!(
            ide_text.contains("Claude i"),
            "IDE-extension session should get the 'i' suffix\n{ide_text}"
        );
    }

    #[test]
    fn session_table_gives_third_of_model_width_to_session_name() {
        let mut app = App::new_with_config(Theme::default(), &[], PanelVisibility::default());
        let mut session = test_session("sid", "project");
        session.model = "ocx-claude-native--gpt-6.1-sol".into();
        session.context_window = 200_000;
        app.sessions.push(session);
        let model = shorten_model(&app.sessions[0].model, false);

        for (width, name_width, model_width) in
            [(80, 23, 28), (100, 33, 31), (120, 38, 42), (160, 56, 64)]
        {
            let text = render_panel(&app, width, 20);
            let header = text.lines().nth(1).unwrap();
            let row = text.lines().nth(2).unwrap();
            let name_start = header.find("Session Name").unwrap();
            let model_start = header.find("Model").unwrap();
            let context_start = header
                .find("Ctx")
                .or_else(|| header.find("Context"))
                .unwrap();
            assert_eq!(model_start - name_start, name_width + 1, "{header}");
            assert_eq!(context_start - model_start, model_width + 1, "{header}");
            assert!(
                row.contains(&truncate_str(&model, model_width)),
                "model should use its allocated width at width {width}\n{text}"
            );
            assert!(
                header.contains("Tokens"),
                "Tokens should stay visible\n{text}"
            );
            assert!(row.contains("10%"), "context should stay visible\n{text}");
        }
    }

    #[test]
    fn session_table_paints_theme_background_after_clearing() {
        // Clear resets cells to the terminal default; on a light terminal with a
        // dark theme the table must still use the theme background.
        for name in crate::theme::THEME_NAMES {
            let theme = Theme::by_name(name).unwrap();
            let mut app = App::new_with_config(theme, &[], PanelVisibility::default());
            app.sessions = vec![
                test_session("first", "first"),
                test_session("second", "second"),
            ];
            app.selected = 0;
            let backend = TestBackend::new(120, 20);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|f| draw_sessions_panel(f, &app, Rect::new(0, 0, 120, 20), &app.theme))
                .unwrap();
            let buffer = terminal.backend().buffer();
            // Row 1 is the table header and row 3 the unselected session.
            for y in [1, 3] {
                for x in 1..119 {
                    assert_eq!(buffer[(x, y)].bg, app.theme.main_bg, "{name} at ({x}, {y})");
                }
            }
        }
    }

    #[test]
    fn session_table_uses_one_row_per_session() {
        let mut app = App::new_with_config(Theme::default(), &[], PanelVisibility::default());
        let mut first = test_session("first", "project");
        first.session_name = "First session".into();
        first.current_tasks = vec!["UNIQUE_TASK_TEXT".into()];
        let mut second = test_session("second", "project");
        second.session_name = "Second session".into();
        app.sessions = vec![first, second];

        let text = render_panel(&app, 120, 20);
        let lines: Vec<_> = text.lines().collect();
        assert!(lines[2].contains("First session"), "{text}");
        assert!(lines[3].contains("Second session"), "{text}");
        assert!(
            lines[4].contains("────"),
            "divider should follow the sessions\n{text}"
        );
        assert!(!lines[2..4]
            .iter()
            .any(|row| row.contains("UNIQUE_TASK_TEXT")));
    }

    #[test]
    fn session_table_clears_rows_when_selection_scrolls() {
        let mut app = App::new_with_config(Theme::default(), &[], PanelVisibility::default());
        app.sessions = (0..8)
            .map(|i| {
                let mut session = test_session(&format!("sid{i}"), "project");
                session.session_name = format!("Session {i}");
                session
            })
            .collect();
        app.selected = 7;

        let backend = TestBackend::new(120, 14);
        let mut terminal = Terminal::new(backend).unwrap();
        let area = Rect {
            x: 0,
            y: 0,
            width: 120,
            height: 14,
        };

        terminal
            .draw(|f| {
                let lines = (0..area.height)
                    .map(|_| Line::from("STALE".repeat(24)))
                    .collect::<Vec<_>>();
                f.render_widget(Paragraph::new(lines), area);
            })
            .unwrap();

        terminal
            .draw(|f| draw_sessions_panel(f, &app, area, &app.theme))
            .unwrap();

        app.selected = 6;
        terminal
            .draw(|f| draw_sessions_panel(f, &app, area, &app.theme))
            .unwrap();

        let text = format!("{}", terminal.backend());
        assert!(
            !text.contains("STALE") && !text.contains("Session 7"),
            "offscreen session row should be cleared after selection scroll\n{text}"
        );
    }

    #[test]
    fn session_table_repaints_selection_marker_when_moving() {
        let mut app = App::new_with_config(Theme::default(), &[], PanelVisibility::default());
        app.sessions = vec![
            test_session("first111", "first"),
            test_session("second22", "second"),
        ];

        let backend = TestBackend::new(120, 14);
        let mut terminal = Terminal::new(backend).unwrap();
        let area = Rect {
            x: 0,
            y: 0,
            width: 120,
            height: 14,
        };

        app.selected = 0;
        terminal
            .draw(|f| draw_sessions_panel(f, &app, area, &app.theme))
            .unwrap();

        app.selected = 1;
        terminal
            .draw(|f| draw_sessions_panel(f, &app, area, &app.theme))
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(1, 2)].symbol(), " ");
        assert_eq!(buffer[(1, 3)].symbol(), "►");
    }

    fn chat(role: ChatRole, text: &str) -> ChatMessage {
        ChatMessage {
            role,
            text: text.into(),
        }
    }

    fn render_panel(app: &App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let area = Rect {
            x: 0,
            y: 0,
            width,
            height,
        };
        terminal
            .draw(|f| draw_sessions_panel(f, app, area, &app.theme))
            .unwrap();
        format!("{}", terminal.backend())
    }

    /// Rendered cell symbols of the sessions panel, one vector per terminal row.
    fn render_cells(app: &App, width: u16, height: u16) -> Vec<Vec<String>> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let area = Rect {
            x: 0,
            y: 0,
            width,
            height,
        };
        terminal
            .draw(|f| draw_sessions_panel(f, app, area, &app.theme))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect()
            })
            .collect()
    }

    /// Rendered rows of the sessions panel joined into strings.
    fn render_rows(app: &App, width: u16, height: u16) -> Vec<String> {
        render_cells(app, width, height)
            .into_iter()
            .map(|cells| cells.concat())
            .collect()
    }

    /// The chat title row and the visible chat rows. The panel border and the
    /// scrollbar column are stripped from the rows so content can be compared
    /// while the scrollbar thumb moves.
    fn chat_view(app: &App, width: u16, height: u16) -> (String, Vec<String>) {
        let rows = render_rows(app, width, height);
        let title = rows
            .iter()
            .position(|row| row.contains(" CHAT"))
            .expect("chat title");
        let visible = app.chat_view.get().visible_rows;
        let inner = |row: &String, drop_right: usize| {
            let chars: Vec<char> = row.chars().collect();
            chars[1..chars.len() - 1 - drop_right]
                .iter()
                .collect::<String>()
                .trim_end()
                .to_string()
        };
        (
            inner(&rows[title], 0),
            rows[title + 1..title + 1 + visible]
                .iter()
                .map(|row| inner(row, 1))
                .collect(),
        )
    }

    fn app_with(messages: Vec<ChatMessage>) -> App {
        let mut app = App::new_with_config(Theme::default(), &[], PanelVisibility::default());
        let mut session = test_session("sid", "project");
        session.chat_messages = messages;
        app.sessions.push(session);
        app
    }

    #[test]
    fn chat_detail_lists_retained_messages_oldest_first_with_role_labels() {
        let mut app = app_with(vec![
            chat(ChatRole::User, "old question"),
            chat(ChatRole::Assistant, "old answer"),
            chat(ChatRole::User, "latest question"),
            chat(ChatRole::Assistant, "progress note"),
            chat(ChatRole::Assistant, "final answer"),
        ]);
        app.sessions[0].initial_prompt = "first prompt".into();

        let (title, rows) = chat_view(&app, 120, 24);
        assert!(title.contains("CHAT · 5 messages"), "{title}");
        let position = |needle: &str| {
            rows.iter()
                .position(|row| row.contains(needle))
                .unwrap_or_else(|| panic!("{needle} missing: {rows:?}"))
        };
        let order = [
            position(" U old question"),
            position(" A old answer"),
            position(" U latest question"),
            position(" A progress note"),
            position(" A final answer"),
        ];
        assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{order:?}");
        let text = render_panel(&app, 120, 24);
        assert!(!text.contains("waiting for response"), "{text}");
        assert!(!text.contains("not in recent history"), "{text}");
    }

    #[test]
    fn chat_detail_labels_use_theme_colors() {
        let app = app_with(vec![
            chat(ChatRole::User, "question"),
            chat(ChatRole::Assistant, "answer"),
        ]);
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let area = Rect::new(0, 0, 100, 24);
        terminal
            .draw(|f| draw_sessions_panel(f, &app, area, &app.theme))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let mut found = Vec::new();
        for y in 0..24 {
            let row: String = (0..100).map(|x| buffer[(x, y)].symbol()).collect();
            if row.contains(" U question") {
                found.push(("U", buffer[(2, y)].fg));
            }
            if row.contains(" A answer") {
                found.push(("A", buffer[(2, y)].fg));
            }
        }
        assert_eq!(
            found,
            vec![("U", app.theme.hi_fg), ("A", app.theme.proc_misc)]
        );
    }

    #[test]
    fn chat_detail_notes_when_only_the_latest_messages_are_kept() {
        let full: Vec<_> = (0..MAX_CHAT_MESSAGES)
            .map(|i| chat(ChatRole::User, &format!("message {i}")))
            .collect();
        let app = app_with(full);
        let (title, _) = chat_view(&app, 120, 24);
        assert!(title.contains("latest 12 messages only"), "{title}");

        let app = app_with(vec![chat(ChatRole::User, "only one")]);
        let (title, _) = chat_view(&app, 120, 24);
        assert!(
            title.contains("1 message") && !title.contains("only"),
            "{title}"
        );
    }

    #[test]
    fn chat_detail_hides_children_and_subagents_on_wide_terminal() {
        let mut app = App::new_with_config(Theme::default(), &[], PanelVisibility::default());
        let mut session = test_session("sid", "project");
        session.chat_messages = vec![
            chat(ChatRole::User, "question"),
            chat(ChatRole::Assistant, "answer"),
        ];
        session.children = vec![ChildProcess {
            pid: 7401,
            command: "cargo build".into(),
            mem_kb: 1024,
            port: None,
        }];
        session.subagents = vec![SubAgent {
            name: "explore-data".into(),
            status: "working".into(),
            tokens: 12_000,
        }];
        app.sessions.push(session);

        let text = render_panel(&app, 160, 20);
        assert!(text.contains(" A answer"), "{text}");
        for hidden in ["CHILDREN", "SUBAGENTS", "cargo build", "explore-data"] {
            assert!(!text.contains(hidden), "{hidden} should be hidden\n{text}");
        }
    }

    #[test]
    fn chat_detail_shows_a_trailing_user_message_without_placeholder() {
        let app = app_with(vec![
            chat(ChatRole::User, "old question"),
            chat(ChatRole::Assistant, "old answer"),
            chat(ChatRole::User, "latest question"),
        ]);
        let (_, rows) = chat_view(&app, 120, 24);
        assert!(rows.iter().any(|row| row.contains(" U latest question")));
        assert!(rows.iter().any(|row| row.contains(" A old answer")));
    }

    #[test]
    fn chat_detail_wraps_long_reply_across_rows() {
        let reply = "가나다라마바사아자차".repeat(30);
        let app = app_with(vec![
            chat(ChatRole::User, "question"),
            chat(ChatRole::Assistant, &reply),
        ]);
        let text = render_panel(&app, 120, 20);
        let reply_rows = text.lines().filter(|l| l.contains("가나다라")).count();
        assert!(reply_rows > 1, "long reply should wrap\n{text}");
    }

    #[test]
    fn chat_text_never_reaches_the_scrollbar_column_for_latin_or_wide_text() {
        let app = app_with(vec![
            chat(ChatRole::User, &"x".repeat(400)),
            chat(ChatRole::Assistant, &"가나다라마바사".repeat(60)),
            chat(ChatRole::Assistant, &"ab가".repeat(80)),
        ]);
        for width in [60u16, 61, 80, 120] {
            let cells = render_cells(&app, width, 24);
            let title = cells
                .iter()
                .position(|row| row.concat().contains(" CHAT"))
                .unwrap();
            let visible = app.chat_view.get().visible_rows;
            // Panel border at 0 and width-1; the chat area spans 1..width-1 and
            // keeps its last column for the scrollbar.
            let scrollbar_x = (width - 2) as usize;
            for row in &cells[title + 1..title + 1 + visible] {
                assert!(
                    matches!(row[scrollbar_x].as_str(), " " | "│" | "┃" | "↑" | "↓"),
                    "width {width}: {}",
                    row.concat()
                );
            }
            // The wrapped rows themselves never exceed the text column.
            let text_w = app.chat_view.get().text_width;
            assert_eq!(text_w, width as usize - 6);
            for row in ChatLayout::new(&app.sessions[0].chat_messages, text_w).rows {
                assert!(UnicodeWidthStr::width(row.text.as_str()) <= text_w);
            }
        }
    }

    fn numbered_rows(prefix: &str, count: usize) -> String {
        (1..=count)
            .map(|i| format!("{prefix}-{i:02}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn chat_scrolls_by_page_with_overlap_and_end_returns_to_the_newest_message() {
        let mut app = app_with(vec![
            chat(ChatRole::User, "question"),
            chat(ChatRole::Assistant, &numbered_rows("row", 40)),
        ]);

        // Follows the newest rows until the user scrolls.
        let (title, bottom) = chat_view(&app, 120, 20);
        let visible = app.chat_view.get().visible_rows;
        assert!(visible >= 5, "fixture too small: {visible}");
        assert!(bottom.last().unwrap().contains("row-40"), "{bottom:?}");
        assert!(
            title.contains(&format!("{}-41/41", 41 - visible + 1)),
            "{title}"
        );
        assert!(!title.contains("End: latest"), "{title}");
        // Nothing below the newest row: PageDown changes nothing.
        app.scroll_chat_down();
        assert_eq!(chat_view(&app, 120, 20).1, bottom);

        app.scroll_chat_up();
        let (title, page_up) = chat_view(&app, 120, 20);
        assert!(title.contains("End: latest"), "{title}");
        // One row of overlap between pages.
        assert_eq!(page_up.last(), bottom.first());

        app.scroll_chat_down();
        let (title, back) = chat_view(&app, 120, 20);
        assert_eq!(back, bottom);
        assert!(!title.contains("End: latest"), "{title}");

        for _ in 0..20 {
            app.scroll_chat_up();
        }
        let (title, top) = chat_view(&app, 120, 20);
        assert!(top[0].contains(" U question"), "{top:?}");
        assert!(title.contains(&format!("1-{visible}/41")), "{title}");
        app.scroll_chat_up();
        assert_eq!(chat_view(&app, 120, 20).1, top);

        app.scroll_chat_to_end();
        let (title, end) = chat_view(&app, 120, 20);
        assert_eq!(end, bottom);
        assert!(!title.contains("End: latest"), "{title}");
    }

    #[test]
    fn chat_follows_new_messages_only_while_at_the_bottom() {
        let message = |i: usize| chat(ChatRole::Assistant, &format!("m{i:02}\nline a\nline b"));
        let mut app = app_with((1..=12).map(message).collect());
        let mut next = 13;
        // Mimics the collector window: the oldest message drops out.
        let mut push = |app: &mut App| {
            let session = &mut app.sessions[0];
            session.chat_messages.remove(0);
            session.chat_messages.push(message(next));
            next += 1;
        };

        let (_, rows) = chat_view(&app, 120, 20);
        assert!(rows.iter().any(|row| row.contains("m12")), "{rows:?}");
        push(&mut app);
        let (_, rows) = chat_view(&app, 120, 20);
        assert!(rows.iter().any(|row| row.contains("m13")), "{rows:?}");

        // Scrolled up: new messages and dropped old ones keep the same rows.
        app.scroll_chat_up();
        let (title, held) = chat_view(&app, 120, 20);
        assert!(title.contains("End: latest"), "{title}");
        push(&mut app);
        push(&mut app);
        let (title, rows) = chat_view(&app, 120, 20);
        assert_eq!(rows, held);
        assert!(title.contains("End: latest"), "{title}");

        // Back at the bottom the view follows again.
        app.scroll_chat_down();
        app.scroll_chat_down();
        push(&mut app);
        let (title, rows) = chat_view(&app, 120, 20);
        assert!(rows.iter().any(|row| row.contains("m16")), "{rows:?}");
        assert!(!title.contains("End: latest"), "{title}");
    }

    #[test]
    fn chat_scroll_position_belongs_to_one_session() {
        let mut app = app_with(vec![
            chat(ChatRole::User, "first"),
            chat(ChatRole::Assistant, &numbered_rows("a", 40)),
        ]);
        let mut other = test_session("other", "project");
        other.chat_messages = vec![
            chat(ChatRole::User, "second"),
            chat(ChatRole::Assistant, &numbered_rows("b", 40)),
        ];
        app.sessions.push(other);

        chat_view(&app, 120, 20);
        app.scroll_chat_up();
        assert!(chat_view(&app, 120, 20).0.contains("End: latest"));

        // Another session starts at its newest message.
        app.selected = 1;
        let (title, rows) = chat_view(&app, 120, 20);
        assert!(!title.contains("End: latest"), "{title}");
        assert!(rows.last().unwrap().contains("b-40"), "{rows:?}");

        // Explicit reset (done when the selection moves) follows again.
        app.selected = 0;
        assert!(chat_view(&app, 120, 20).0.contains("End: latest"));
        app.reset_chat_scroll();
        assert!(!chat_view(&app, 120, 20).0.contains("End: latest"));
    }

    #[test]
    fn chat_scroll_ignores_page_keys_outside_chat_view() {
        let mut app = app_with(vec![
            chat(ChatRole::User, "question"),
            chat(ChatRole::Assistant, &numbered_rows("row", 40)),
        ]);
        app.sessions[0].tool_calls = vec![crate::model::ToolCall {
            name: "Read".into(),
            arg: "src/main.rs".into(),
            duration_ms: 10,
        }];
        app.toggle_timeline();
        render_panel(&app, 120, 20);
        assert!(!app.chat_view_active());
        app.scroll_chat_up();
        app.scroll_chat_up();
        app.toggle_timeline();
        let (title, rows) = chat_view(&app, 120, 20);
        assert!(!title.contains("End: latest"), "{title}");
        assert!(rows.last().unwrap().contains("row-40"), "{rows:?}");
    }

    #[test]
    fn chat_scrollbar_only_shows_when_rows_overflow() {
        let column = |app: &App| {
            let rows = render_rows(app, 120, 20);
            let title = rows.iter().position(|r| r.contains(" CHAT")).unwrap();
            let visible = app.chat_view.get().visible_rows;
            rows[title + 1..title + 1 + visible]
                .iter()
                .map(|row| row.chars().nth(118).unwrap())
                .collect::<String>()
        };
        let overflow = app_with(vec![chat(ChatRole::Assistant, &numbered_rows("row", 60))]);
        let glyphs = column(&overflow);
        assert!(glyphs.contains('┃') && glyphs.contains('↑'), "{glyphs}");
        assert!(!glyphs.contains('↓'), "at the bottom: {glyphs}");
        let short = app_with(vec![chat(ChatRole::Assistant, "short")]);
        assert!(column(&short).chars().all(|c| c == ' '));
    }

    #[test]
    fn empty_chat_shows_a_notice_for_claude_and_opencode() {
        let mut app = App::new_with_config(Theme::default(), &[], PanelVisibility::default());
        app.sessions.push(test_session("claude-sid", "project"));
        let text = render_panel(&app, 120, 20);
        assert!(text.contains("CHAT · no recent messages yet"), "{text}");
        assert!(!app.chat_view_active());

        let mut opencode = test_session("oc-sid", "project");
        opencode.agent_cli = "opencode";
        app.sessions = vec![opencode];
        let text = render_panel(&app, 120, 20);
        assert!(
            text.contains("CHAT · conversation text is not collected for OpenCode sessions"),
            "{text}"
        );
        // Timeline mode replaces the chat area, so no notice there.
        app.toggle_timeline();
        let text = render_panel(&app, 120, 20);
        assert!(
            !text.contains("conversation text is not collected"),
            "{text}"
        );
    }

    #[test]
    fn chat_view_survives_compact_desktop_and_tiny_areas() {
        let mut app = app_with(vec![
            chat(ChatRole::User, &"질문입니다 ".repeat(40)),
            chat(ChatRole::Assistant, &numbered_rows("row", 30)),
        ]);
        for (width, height) in [(60u16, 18u16), (70, 20), (80, 24), (120, 40), (200, 50)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|f| crate::ui::draw(f, &app)).unwrap();
            let text = format!("{}", terminal.backend());
            assert!(text.contains(" CHAT"), "{width}x{height}\n{text}");
            assert!(app.chat_view_active(), "{width}x{height}");
            // The key paths must work at every size too.
            app.scroll_chat_up();
            terminal.draw(|f| crate::ui::draw(f, &app)).unwrap();
            app.scroll_chat_to_end();
        }

        // Directly into areas that are barely larger than the title row.
        for (width, height) in [(1u16, 1u16), (4, 2), (5, 3), (10, 1), (60, 2), (7, 9)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            let session = &app.sessions[0];
            terminal
                .draw(|f| {
                    draw_chat_history(f, &app, session, Rect::new(0, 0, width, height), &app.theme)
                })
                .unwrap();
        }
    }

    /// Title row of `draw_chat_history` drawn alone into a `width`-column area.
    fn chat_title_at(app: &App, width: u16) -> String {
        let backend = TestBackend::new(width, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                draw_chat_history(
                    f,
                    app,
                    &app.sessions[0],
                    Rect::new(0, 0, width, 6),
                    &app.theme,
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..width)
            .map(|x| buffer[(x, 0)].symbol().to_string())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn chat_title_keeps_the_notice_and_drops_hints_that_do_not_fit() {
        let mut messages = vec![chat(ChatRole::Assistant, &numbered_rows("row", 80))];
        messages.extend((1..MAX_CHAT_MESSAGES).map(|i| chat(ChatRole::User, &format!("u{i}"))));
        let mut app = app_with(messages);
        chat_title_at(&app, 120);
        app.scroll_chat_up();
        let wide = chat_title_at(&app, 120);
        assert!(wide.contains("latest 12 messages only"), "{wide}");
        assert!(
            wide.contains("rows") && wide.contains("End: latest"),
            "{wide}"
        );
        // Narrower titles drop the longer hints but keep the notice while it fits.
        let mid = chat_title_at(&app, 55);
        assert!(mid.contains("latest 12 messages only"), "{mid}");
        assert!(
            mid.contains("End: latest") && !mid.contains("rows"),
            "{mid}"
        );
        // Nothing may exceed the area at any width.
        for width in [120u16, 80, 60, 45, 40, 30, 20, 12, 8] {
            let title = chat_title_at(&app, width);
            assert!(UnicodeWidthStr::width(title.as_str()) <= width as usize);
            assert!(title.starts_with(" CHAT") || width < 5, "{width}: {title}");
        }
    }

    fn tool_chat() -> Vec<ChatMessage> {
        vec![
            chat(ChatRole::User, "fix the build"),
            chat(ChatRole::Assistant, "Looking at the file."),
            chat(ChatRole::ToolCall, "Read src/a.rs"),
            chat(ChatRole::ToolResult, "12 lines, 300 B · content hidden"),
            chat(ChatRole::ToolCall, "Bash cargo test"),
            chat(ChatRole::ToolError, "exit code 1\nboom"),
            chat(ChatRole::Assistant, "Fixed."),
        ]
    }

    #[test]
    fn chat_detail_interleaves_tool_calls_and_results_in_order() {
        let app = app_with(tool_chat());
        let (title, rows) = chat_view(&app, 120, 24);
        assert!(title.contains("CHAT · 7 items"), "{title}");
        let position = |needle: &str| {
            rows.iter()
                .position(|row| row.contains(needle))
                .unwrap_or_else(|| panic!("{needle} missing: {rows:?}"))
        };
        let order = [
            position(" U fix the build"),
            position(" A Looking at the file."),
            position(" ▸ Read src/a.rs"),
            // Results sit two columns right of their call text.
            position(" └   12 lines, 300 B · content hidden"),
            position(" ▸ Bash cargo test"),
            position(" ✗   exit code 1"),
            position("     boom"),
            position(" A Fixed."),
        ];
        assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{order:?}");
    }

    #[test]
    fn chat_detail_styles_tool_rows_apart_from_the_conversation() {
        let app = app_with(tool_chat());
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| draw_sessions_panel(f, &app, Rect::new(0, 0, 100, 24), &app.theme))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let theme = &app.theme;
        // (needle, label cell fg, text cell fg) for the first row of each kind.
        let expected = [
            (" U fix the build", theme.hi_fg, theme.main_fg),
            (" ▸ Read src/a.rs", theme.session_id, theme.main_fg),
            (" └   12 lines", theme.graph_text, theme.graph_text),
            (" ✗   exit code 1", theme.warning_fg, theme.warning_fg),
        ];
        for (needle, label_fg, text_fg) in expected {
            let (y, row) = (0..24u16)
                .map(|y| {
                    (
                        y,
                        (0..100)
                            .map(|x| buffer[(x, y)].symbol())
                            .collect::<String>(),
                    )
                })
                .find(|(_, row)| row.contains(needle))
                .unwrap_or_else(|| panic!("{needle} missing"));
            let col = row
                .chars()
                .position(|c| c == needle.chars().nth(1).unwrap())
                .unwrap();
            assert_eq!(buffer[(col as u16, y)].fg, label_fg, "{needle} label");
            let text_col = col + needle.chars().count() - 2;
            assert_eq!(buffer[(text_col as u16, y)].fg, text_fg, "{needle} text");
        }
        assert_ne!(theme.main_fg, theme.graph_text);
        assert_ne!(theme.session_id, theme.hi_fg);
    }

    #[test]
    fn chat_detail_wraps_wide_tool_text_and_keeps_the_result_indent() {
        let items = vec![
            chat(
                ChatRole::ToolCall,
                &format!("Bash echo {}", "한글출력".repeat(20)),
            ),
            chat(ChatRole::ToolResult, &"가나다라마바사".repeat(12)),
            chat(ChatRole::ToolError, &"오류발생".repeat(15)),
        ];
        let app = app_with(items.clone());
        let (_, rows) = chat_view(&app, 60, 24);
        assert!(rows[0].starts_with(" ▸ Bash echo"), "{rows:?}");
        let text_w = app.chat_view.get().text_width;
        assert!(text_w > 0);
        let layout = ChatLayout::new(&items, text_w);
        let mut per_message = [0usize; 3];
        for row in &layout.rows {
            per_message[row.message] += 1;
            assert!(
                UnicodeWidthStr::width(row.text.as_str()) <= text_w,
                "{:?}",
                row.text
            );
            match items[row.message].role {
                ChatRole::ToolCall => assert!(!row.text.starts_with(' '), "{:?}", row.text),
                _ => assert!(
                    row.text.starts_with("  ") && !row.text.starts_with("   "),
                    "{:?}",
                    row.text
                ),
            }
        }
        assert!(per_message.iter().all(|rows| *rows >= 2), "{per_message:?}");
    }

    #[test]
    fn chat_tool_toggle_hides_tool_rows_and_survives_a_session_switch() {
        let mut app = app_with(tool_chat());
        let mut other = test_session("other", "project");
        other.chat_messages = tool_chat();
        app.sessions.push(other);

        let (title, rows) = chat_view(&app, 120, 24);
        assert!(app.show_chat_tools, "tool details default to on");
        assert!(rows.iter().any(|row| row.contains("▸ Read src/a.rs")));
        assert!(!title.contains("tools hidden"), "{title}");

        app.toggle_chat_tools();
        let (title, rows) = chat_view(&app, 120, 24);
        assert!(title.contains("CHAT · 3 messages"), "{title}");
        assert!(title.contains("tools hidden"), "{title}");
        for row in &rows {
            assert!(
                !row.contains('▸') && !row.contains('└') && !row.contains('✗'),
                "{row}"
            );
        }
        assert!(rows.iter().any(|row| row.contains(" A Fixed.")));

        // The choice is app-wide: another session keeps tool details hidden.
        app.selected = 1;
        app.reset_chat_scroll();
        let (_, rows) = chat_view(&app, 120, 24);
        assert!(!rows.iter().any(|row| row.contains('▸')), "{rows:?}");

        app.toggle_chat_tools();
        let (title, rows) = chat_view(&app, 120, 24);
        assert!(title.contains("CHAT · 7 items"), "{title}");
        assert!(rows.iter().any(|row| row.contains("▸ Bash cargo test")));
    }

    #[test]
    fn chat_with_only_tool_items_falls_back_to_the_empty_notice_when_hidden() {
        let mut app = app_with(vec![
            chat(ChatRole::ToolCall, "Read a.rs"),
            chat(ChatRole::ToolResult, "1 line, 3 B · content hidden"),
        ]);
        assert!(render_panel(&app, 120, 24).contains("▸ Read a.rs"));
        app.toggle_chat_tools();
        let text = render_panel(&app, 120, 24);
        assert!(text.contains("no recent messages yet"), "{text}");
        assert!(!text.contains("▸ Read a.rs"), "{text}");
    }

    #[test]
    fn chat_notice_counts_items_and_flags_a_full_window() {
        let mut items = Vec::new();
        for i in 0..40 {
            items.push(chat(ChatRole::ToolCall, &format!("Read f{i}")));
            items.push(chat(ChatRole::ToolResult, "1 line, 3 B · content hidden"));
        }
        let app = app_with(items);
        let (title, _) = chat_view(&app, 120, 24);
        assert!(title.contains("latest 80 items only"), "{title}");
    }

    #[test]
    fn chat_scroll_position_holds_while_tool_items_arrive_and_old_ones_drop() {
        let item = |i: usize| {
            if i.is_multiple_of(2) {
                chat(ChatRole::ToolCall, &format!("Bash run-{i:02}"))
            } else {
                chat(ChatRole::ToolResult, &format!("out-{i:02}\nmore\nlines"))
            }
        };
        let mut app = app_with((0..40).map(item).collect());
        let mut next = 40;
        let mut push = |app: &mut App| {
            let session = &mut app.sessions[0];
            session.chat_messages.remove(0);
            session.chat_messages.push(item(next));
            next += 1;
        };
        let (_, rows) = chat_view(&app, 120, 20);
        assert!(rows.iter().any(|row| row.contains("out-39")), "{rows:?}");

        app.scroll_chat_up();
        let (title, held) = chat_view(&app, 120, 20);
        assert!(title.contains("End: latest"), "{title}");
        push(&mut app);
        push(&mut app);
        let (_, rows) = chat_view(&app, 120, 20);
        assert_eq!(rows, held);

        app.scroll_chat_to_end();
        push(&mut app);
        let (title, rows) = chat_view(&app, 120, 20);
        assert!(!title.contains("End: latest"), "{title}");
        assert!(rows.iter().any(|row| row.contains("run-42")), "{rows:?}");
    }

    #[test]
    fn chat_toggle_returns_to_following_the_newest_item() {
        let mut app = app_with(vec![
            chat(ChatRole::User, "question"),
            chat(ChatRole::ToolCall, "Bash ls"),
            chat(ChatRole::ToolResult, &numbered_rows("row", 40)),
            chat(ChatRole::Assistant, "done"),
        ]);
        chat_view(&app, 120, 20);
        app.scroll_chat_up();
        assert!(chat_view(&app, 120, 20).0.contains("End: latest"));
        app.toggle_chat_tools();
        assert!(!chat_view(&app, 120, 20).0.contains("End: latest"));
        app.toggle_chat_tools();
        let (title, rows) = chat_view(&app, 120, 20);
        assert!(!title.contains("End: latest"), "{title}");
        assert!(rows.last().unwrap().contains("done"), "{rows:?}");
    }

    #[test]
    fn chat_with_tool_items_survives_compact_and_tiny_areas() {
        let mut app = app_with(tool_chat());
        for (width, height) in [(60u16, 18u16), (70, 20), (80, 24), (120, 40), (200, 50)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|f| crate::ui::draw(f, &app)).unwrap();
            let text = format!("{}", terminal.backend());
            assert!(text.contains(" CHAT"), "{width}x{height}\n{text}");
        }
        app.toggle_chat_tools();
        for (width, height) in [(1u16, 1u16), (4, 2), (5, 3), (13, 4), (60, 2)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            let session = &app.sessions[0];
            terminal
                .draw(|f| {
                    draw_chat_history(f, &app, session, Rect::new(0, 0, width, height), &app.theme)
                })
                .unwrap();
        }
    }

    fn test_session(session_id: &str, project_name: &str) -> AgentSession {
        AgentSession {
            agent_cli: "claude",
            launch_surface: LaunchSurface::Cli,
            pid: 42,
            jump_pid: Some(42),
            session_id: session_id.into(),
            session_name: String::new(),
            cwd: format!("/tmp/{project_name}"),
            project_name: project_name.into(),
            started_at: 0,
            status: SessionStatus::Waiting,
            model: "claude-opus-4-6".into(),
            effort: String::new(),
            context_percent: 10.0,
            total_input_tokens: 1_000,
            total_output_tokens: 500,
            total_cache_read: 0,
            total_cache_create: 0,
            turn_count: 1,
            current_tasks: vec!["waiting for input".into()],
            mem_mb: 0,
            version: String::new(),
            git_branch: String::new(),
            git_added: 0,
            git_modified: 0,
            token_history: Vec::new(),
            context_history: Vec::new(),
            compaction_count: 0,
            context_window: 200_000,
            subagents: Vec::new(),
            mem_file_count: 0,
            mem_line_count: 0,
            children: Vec::new(),
            initial_prompt: "prompt".into(),
            first_assistant_text: String::new(),
            chat_messages: Vec::new(),
            tool_calls: Vec::new(),
            pending_since_ms: 0,
            thinking_since_ms: 0,
            file_accesses: Vec::new(),
            config_root: "~/.claude".into(),
        }
    }
}
