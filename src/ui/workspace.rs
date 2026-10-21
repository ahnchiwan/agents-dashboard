use crate::app::App;
use crate::archive::{display, Agent};
use crate::workspace::MainTab;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};
use ratatui::Frame;

fn border_color(tab: MainTab) -> Color {
    match tab {
        MainTab::Old => Color::Rgb(95, 158, 110),
        _ => Color::Rgb(105, 175, 199),
    }
}

// Match the agent labels in the live sessions panel without changing its theme.
fn agent_color(agent: Agent) -> Color {
    match agent {
        Agent::Claude => Color::Rgb(217, 119, 87),
        Agent::Codex => Color::Rgb(122, 157, 255),
    }
}

fn selected_style(tab: MainTab) -> Style {
    let background = match tab {
        MainTab::Old => Color::Rgb(220, 235, 221),
        _ => Color::Rgb(221, 239, 245),
    };
    Style::default()
        .bg(background)
        .fg(Color::Black)
        .remove_modifier(Modifier::DIM | Modifier::BOLD)
}

pub(super) fn draw(f: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let ws = &app.workspace;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(2),
            Constraint::Length(1),
        ])
        .split(area);
    super::header::draw_header(f, app, chunks[0], theme);
    let inner = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color(ws.tab)));
    let body = inner.inner(chunks[1]);
    f.render_widget(inner, chunks[1]);
    if ws.tab == MainTab::Old {
        let visible = ws.visible(&app.sessions);
        let filter = ["All", "Claude", "Codex"][ws.agent_filter];
        let info = format!(
            "Agent: < {filter} >  /{}{}  {} sessions{}{}",
            display(&ws.query),
            if ws.searching { "_" } else { "" },
            visible.len(),
            if ws.loading { " (scanning...)" } else { "" },
            if ws.limited {
                " [scan limit reached]"
            } else {
                ""
            }
        );
        f.render_widget(
            Paragraph::new(info).style(Style::default().fg(theme.title)),
            Rect::new(body.x, body.y, body.width, 1),
        );
        let list_height = body.height.saturating_sub(6) as usize;
        let offset = ws.selected.saturating_sub(list_height.saturating_sub(1));
        let mut lines = Vec::new();
        for (index, session) in visible.iter().enumerate().skip(offset).take(list_height) {
            let date = chrono::DateTime::from_timestamp(session.modified as i64, 0)
                .map(|d| {
                    d.with_timezone(&chrono::Local)
                        .format("%Y-%m-%d %H:%M")
                        .to_string()
                })
                .unwrap_or_default();
            let title = if session.title.is_empty() {
                &session.id
            } else {
                &session.title
            };
            let selected = index == ws.selected;
            let style = if selected {
                selected_style(ws.tab)
            } else {
                Style::default().fg(theme.main_fg)
            };
            let mut line = Line::from(vec![
                Span::raw(format!("{} {date} ", if selected { "▶" } else { " " })),
                Span::styled(
                    format!("{:6}", session.agent.label()),
                    if selected {
                        style
                    } else {
                        Style::default().fg(agent_color(session.agent))
                    },
                ),
                Span::raw(format!(" {title}")),
            ])
            .style(style);
            if selected {
                line.push_span(" ".repeat((body.width as usize).saturating_sub(line.width())));
            }
            lines.push(line);
        }
        if visible.is_empty() {
            lines.push(Line::from(if ws.loading {
                "Scanning local main sessions..."
            } else {
                "No matching historical sessions. Press r to refresh."
            }));
        }
        f.render_widget(
            Paragraph::new(lines),
            Rect::new(body.x, body.y + 1, body.width, list_height as u16),
        );
        if body.height >= 5 {
            f.render_widget(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(Style::default().fg(border_color(ws.tab))),
                Rect::new(body.x, body.y + body.height - 5, body.width, 1),
            );
        }
        if let Some(session) = visible.get(ws.selected) {
            let detail = vec![
                Line::from(format!("ID: {}", session.id)),
                Line::from(format!(
                    "CWD: {}{}",
                    display(&session.cwd.to_string_lossy()),
                    if session.cwd.is_dir() {
                        ""
                    } else {
                        " [missing/deleted; resume disabled]"
                    }
                )),
                Line::from(format!(
                    "Config: {}",
                    display(&session.root.to_string_lossy())
                )),
                Line::from("Enter: resume in a new tmux window (never splits a pane)"),
            ];
            f.render_widget(
                Paragraph::new(detail),
                Rect::new(
                    body.x,
                    body.y + body.height.saturating_sub(4),
                    body.width,
                    4.min(body.height),
                ),
            );
        }
    } else {
        let form = &ws.new;
        let path = if form.editing {
            format!("{}▏", display(&form.path))
        } else {
            display(&form.path)
        };
        let mut lines = vec![
            Line::from("Create new session"),
            Line::from(""),
            Line::from("Agent:"),
        ];
        let mut focus_row = 3;
        let mut add_option = |lines: &mut Vec<Line<'static>>,
                              label: String,
                              field: usize,
                              chosen: bool,
                              color: Color| {
            let focused = form.field == field && chosen;
            let style = if focused {
                selected_style(ws.tab)
            } else {
                Style::default().fg(color)
            };
            if focused {
                focus_row = lines.len();
            }
            let text = format!("{} {label}", if chosen { ">" } else { " " });
            let padding = (body.width as usize).saturating_sub(text.chars().count());
            // Style the span itself, not only the line, so no inherited gray can override black.
            lines.push(Line::from(Span::styled(
                format!("{text}{}", " ".repeat(padding)),
                style,
            )));
        };
        for agent in [Agent::Claude, Agent::Codex] {
            add_option(
                &mut lines,
                agent.label().into(),
                0,
                agent == form.agent,
                agent_color(agent),
            );
        }
        lines.push(Line::from(""));
        lines.push(Line::from("Model:"));
        lines.push(Line::from(format!(
            "Source: {}",
            display(&form.model_source)
        )));
        for (index, model) in form.models.iter().enumerate() {
            add_option(
                &mut lines,
                display(model),
                1,
                index == form.model,
                theme.main_fg,
            );
        }
        lines.push(Line::from(""));
        add_option(&mut lines, format!("Path: {path}"), 2, true, theme.main_fg);
        lines.push(Line::from(""));
        add_option(
            &mut lines,
            "[ Create session ]".into(),
            3,
            true,
            theme.main_fg,
        );
        lines.push(Line::from(""));
        lines.push(Line::from(format!(
            "Config: {}",
            display(&crate::archive::default_root(form.agent).to_string_lossy())
        )));
        lines.push(Line::from(
            "Default uses CLI defaults. Local/cached models are not availability checks.",
        ));
        // Keep all options in the form; follow the chosen option when it exceeds the viewport.
        let offset = focus_row.saturating_sub(body.height.saturating_sub(1) as usize);
        f.render_widget(
            Paragraph::new(lines).scroll((offset.min(u16::MAX as usize) as u16, 0)),
            body,
        );
    }
    f.render_widget(
        Paragraph::new(display(&ws.message))
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(theme.status_fg)),
        chunks[2],
    );
    super::footer::draw_workspace_footer(f, app, chunks[3]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PanelVisibility;
    use crate::theme::Theme;
    use ratatui::{backend::TestBackend, Terminal};
    fn render(app: &App) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|f| super::super::draw(f, app)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn text_position(buffer: &ratatui::buffer::Buffer, text: &str) -> (u16, u16) {
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width.saturating_sub(text.len() as u16) {
                if text
                    .chars()
                    .enumerate()
                    .all(|(offset, ch)| buffer[(x + offset as u16, y)].symbol() == ch.to_string())
                {
                    return (x, y);
                }
            }
        }
        panic!("Missing text: {text}");
    }

    fn assert_selection(buffer: &ratatui::buffer::Buffer, y: u16, background: Color) {
        for x in 1..buffer.area.width - 1 {
            let cell = &buffer[(x, y)];
            assert_eq!(cell.bg, background, "at ({x}, {y})");
            assert_eq!(cell.fg, Color::Black, "at ({x}, {y})");
            assert!(
                !cell.modifier.intersects(Modifier::DIM | Modifier::BOLD),
                "at ({x}, {y})"
            );
        }
    }

    #[test]
    fn old_agent_colors_and_selected_row_override_are_theme_independent() {
        for name in crate::theme::THEME_NAMES {
            let mut app = App::new_with_config(
                Theme::by_name(name).unwrap(),
                &[],
                PanelVisibility::default(),
            );
            app.workspace.tab = MainTab::Old;
            app.workspace.archive = [Agent::Claude, Agent::Codex]
                .into_iter()
                .map(|agent| crate::archive::ArchivedSession {
                    agent,
                    id: agent.cli().into(),
                    title: "Archived work".into(),
                    cwd: "/project".into(),
                    root: "/config".into(),
                    modified: 0,
                })
                .collect();
            for selected in 0..2 {
                app.workspace.selected = selected;
                let buffer = render(&app);
                for (index, agent, color) in [
                    (0, Agent::Claude, Color::Rgb(217, 119, 87)),
                    (1, Agent::Codex, Color::Rgb(122, 157, 255)),
                ] {
                    let (x, y) = text_position(&buffer, agent.label());
                    if index == selected {
                        assert_selection(&buffer, y, Color::Rgb(220, 235, 221));
                    } else {
                        for offset in 0..agent.label().len() as u16 {
                            assert_eq!(buffer[(x + offset, y)].fg, color);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn new_agent_colors_yield_to_each_focused_field_selection() {
        for name in crate::theme::THEME_NAMES {
            let mut app = App::new_with_config(
                Theme::by_name(name).unwrap(),
                &[],
                PanelVisibility::default(),
            );
            app.workspace.tab = MainTab::New;
            for (agent, color) in [
                (Agent::Claude, Color::Rgb(217, 119, 87)),
                (Agent::Codex, Color::Rgb(122, 157, 255)),
            ] {
                app.workspace.new.agent = agent;
                for (field, label) in ["Agent:", "Model:", "Path:", "Create session"]
                    .into_iter()
                    .enumerate()
                {
                    app.workspace.new.field = field;
                    let buffer = render(&app);
                    let selected_label = match field {
                        0 => agent.label(),
                        1 => "Default",
                        _ => label,
                    };
                    let (_, y) = text_position(&buffer, selected_label);
                    assert_selection(&buffer, y, Color::Rgb(221, 239, 245));
                    let (x, y) = text_position(&buffer, agent.label());
                    for offset in 0..agent.label().len() as u16 {
                        assert_eq!(
                            buffer[(x + offset, y)].fg,
                            if field == 0 { Color::Black } else { color }
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn old_and_new_boxes_have_rounded_accent_borders() {
        for name in crate::theme::THEME_NAMES {
            let mut app = App::new_with_config(
                Theme::by_name(name).unwrap(),
                &[],
                PanelVisibility::default(),
            );
            for tab in [MainTab::Old, MainTab::New] {
                app.workspace.tab = tab;
                let buffer = render(&app);
                for (x, y, symbol) in [(0, 1, "╭"), (99, 1, "╮"), (0, 20, "╰"), (99, 20, "╯")]
                {
                    assert_eq!(buffer[(x, y)].symbol(), symbol);
                    assert_eq!(
                        buffer[(x, y)].fg,
                        if tab == MainTab::Old {
                            Color::Rgb(95, 158, 110)
                        } else {
                            Color::Rgb(105, 175, 199)
                        }
                    );
                }
            }
        }
    }

    #[test]
    fn old_session_divider_matches_its_border() {
        let mut app = App::new_with_config(Theme::default(), &[], PanelVisibility::default());
        app.workspace.tab = MainTab::Old;
        let buffer = render(&app);
        for x in 1..99 {
            assert_eq!(buffer[(x, 15)].symbol(), "─");
            assert_eq!(buffer[(x, 15)].fg, Color::Rgb(95, 158, 110));
        }
    }

    #[test]
    fn main_tabs_highlight_selection_without_background_and_stay_legible() {
        for name in crate::theme::THEME_NAMES {
            let theme = Theme::by_name(name).unwrap();
            // Black on the light themes, the theme's title color on dark ones.
            let emphasis = if matches!(*name, "light" | "white") {
                Color::Black
            } else {
                theme.title
            };
            assert_ne!(emphasis, theme.main_bg, "{name}");
            let mut app = App::new_with_config(theme, &[], PanelVisibility::default());
            for selected in MainTab::ALL {
                app.workspace.tab = selected;
                let buffer = render(&app);
                assert_eq!(buffer[(0, 0)].fg, emphasis, "{name} separator");
                for tab in MainTab::ALL {
                    let (x, y) = text_position(&buffer, tab.label());
                    for offset in 0..tab.label().len() as u16 {
                        let cell = &buffer[(x + offset, y)];
                        assert_eq!(
                            cell.fg,
                            if tab == selected {
                                emphasis
                            } else {
                                app.theme.inactive_fg
                            },
                            "{name}"
                        );
                        assert_eq!(cell.bg, buffer[(99, 0)].bg);
                    }
                }
            }
        }
    }

    #[test]
    fn model_source_and_freshness_render_without_provider_details() {
        let mut app = App::new_with_config(Theme::default(), &[], PanelVisibility::default());
        app.workspace.tab = MainTab::New;
        for source in [
            "Gateway cache (stale >24h; last-known, not verified)",
            "CLI cc cache (fresh; account/flags unverified)",
            "Configured only (gateway cache missing/invalid/provider mismatch)",
        ] {
            app.workspace.new.model_source = source.into();
            let buffer = render(&app);
            text_position(&buffer, &format!("Source: {source}"));
        }
    }

    #[test]
    fn expanded_choices_scroll_with_keyboard_selection() {
        use crossterm::event::KeyCode;
        let mut app = App::new_with_config(Theme::default(), &[], PanelVisibility::default());
        app.workspace.tab = MainTab::New;
        app.workspace.new.models = vec![
            "Default".into(),
            "first-model".into(),
            "second-model".into(),
        ];
        let buffer = render(&app);
        for label in [
            "Create new session",
            "Claude",
            "Codex",
            "Default",
            "first-model",
            "second-model",
            "Path:",
            "Create session",
        ] {
            text_position(&buffer, label);
        }
        app.workspace
            .new
            .models
            .extend((0..40).map(|n| format!("catalog-model-{n:02}")));
        app.workspace.handle_key(KeyCode::Down, &[], true);
        for _ in 0..app.workspace.new.models.len() - 1 {
            app.workspace.handle_key(KeyCode::Right, &[], true);
            let buffer = render(&app);
            let label = &app.workspace.new.models[app.workspace.new.model];
            let (_, y) = text_position(&buffer, label);
            assert!(y > 1 && y < 20);
            assert_selection(&buffer, y, Color::Rgb(221, 239, 245));
        }
        for label in ["Path:", "Create session"] {
            app.workspace.handle_key(KeyCode::Down, &[], true);
            let buffer = render(&app);
            let (_, y) = text_position(&buffer, label);
            assert_selection(&buffer, y, Color::Rgb(221, 239, 245));
        }
        app.workspace.handle_key(KeyCode::Down, &[], true);
        let buffer = render(&app);
        text_position(&buffer, "Agent:");
        text_position(&buffer, "Codex");
    }

    #[test]
    fn header_pipes_and_hit_targets_match_visible_tabs() {
        let mut app = App::new_with_config(Theme::default(), &[], PanelVisibility::default());
        app.workspace.tab = MainTab::New;
        let buffer = render(&app);
        let expected = "| Agents Dashboard | Old Sessions | New Session |";
        let header: String = (0..expected.len() as u16)
            .map(|x| buffer[(x, 0)].symbol())
            .collect();
        assert_eq!(header, expected);
        for width in [35, 60, 100] {
            let area = Rect::new(3, 2, width, 24);
            let mut x = area.x + 1;
            for tab in MainTab::ALL {
                assert_eq!(super::super::click_target(&app, area, x - 1, area.y), None);
                for column in x..x + tab.label().len() as u16 + 2 {
                    let target = super::super::click_target(&app, area, column, area.y);
                    assert_eq!(
                        target,
                        if column < area.right() {
                            Some(super::super::ClickTarget::MainTab(tab))
                        } else {
                            None
                        }
                    );
                }
                x += tab.label().len() as u16 + 3;
            }
        }
    }

    #[test]
    fn workspace_footer_uses_dashboard_key_and_description_styles() {
        for name in crate::theme::THEME_NAMES {
            let mut app = App::new_with_config(
                Theme::by_name(name).unwrap(),
                &[],
                PanelVisibility::default(),
            );
            for tab in [MainTab::Old, MainTab::New] {
                app.workspace.tab = tab;
                let buffer = render(&app);
                for (label, color) in [
                    ("↑↓", app.theme.hi_fg),
                    (
                        if tab == MainTab::Old {
                            "select"
                        } else {
                            "focus"
                        },
                        app.theme.main_fg,
                    ),
                    ("Tab/S-Tab", app.theme.hi_fg),
                ] {
                    let (x, y) = text_position(&buffer, label);
                    assert_eq!(y, 23);
                    for offset in 0..label.chars().count() as u16 {
                        assert_eq!(buffer[(x + offset, y)].fg, color);
                    }
                }
            }
        }
    }

    #[test]
    fn old_and_new_render_at_compact_and_desktop_widths() {
        for width in [60, 80, 120] {
            for tab in [MainTab::Old, MainTab::New] {
                let mut app =
                    App::new_with_config(Theme::default(), &[], PanelVisibility::default());
                app.workspace.tab = tab;
                let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
                terminal.draw(|f| super::super::draw(f, &app)).unwrap();
                let text: String = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|c| c.symbol())
                    .collect();
                assert!(text.contains("Old Sessions"));
                assert!(text.contains("New Session"));
                if tab == MainTab::New {
                    assert!(text.contains("Create session"));
                    assert!(!text.contains("Enter edits path"));
                    assert!(!text.contains("arrows choose recent projects"));
                }
            }
        }
    }
}
