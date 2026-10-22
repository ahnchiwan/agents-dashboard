use crate::app::App;
use crate::locale::t;
use crate::theme::Theme;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::truncate_str;

pub(super) fn key_hint(key: &str, description: &str, theme: &Theme) -> [Span<'static>; 2] {
    [
        Span::styled(key.to_owned(), Style::default().fg(theme.hi_fg)),
        Span::styled(
            format!(" {description} "),
            Style::default().fg(theme.main_fg),
        ),
    ]
}

pub(super) fn draw_workspace_footer(f: &mut Frame, app: &App, area: Rect) {
    let compact = area.width <= 80;
    let ultra_compact = area.width <= 70;
    let hints = if app.workspace.resume.is_some() {
        vec![(" ↵", "resume here"), ("Esc", "cancel edit")]
    } else if app.workspace.tab == crate::workspace::MainTab::Old && app.workspace.searching {
        vec![(" ↵", "accept search"), ("Esc", "clear search")]
    } else if app.workspace.tab == crate::workspace::MainTab::Old {
        let mut hints = vec![
            (" ↑↓", "select"),
            ("↵", "resume"),
            ("←→", "agent"),
            ("/", "search"),
        ];
        if !ultra_compact {
            hints.push(("r", "refresh"));
        }
        hints
    } else if app.workspace.new.editing {
        vec![(" ↵", "accept path"), ("Esc", "cancel edit")]
    } else {
        let mut hints = vec![(" ↑↓", "focus"), ("←→", "choose"), ("↵", "edit/create")];
        if !ultra_compact {
            hints.push(("Esc", "cancel"));
        }
        hints
    };
    let mut spans = hints
        .into_iter()
        .flat_map(|(key, description)| key_hint(key, description, &app.theme))
        .collect::<Vec<_>>();
    spans.extend(key_hint(
        if compact { "Tab" } else { "Tab/S-Tab" },
        "tabs",
        &app.theme,
    ));
    if !app.workspace.new.editing && !app.workspace.searching && app.workspace.resume.is_none() {
        spans.extend(key_hint("q", "quit", &app.theme));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

pub(crate) fn draw_footer(f: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    // Filter input mode: show filter bar instead of normal keybindings
    if app.filter_active {
        let visible_count = app.visible_indices().len();
        let count = format!(
            "{}/{} {}",
            visible_count,
            app.sessions.len(),
            t("footer.sessions")
        );
        let suffix = if area.width <= 80 {
            format!("_  {}", count)
        } else {
            format!(
                "_  {}  (Esc {}, Enter {})",
                count,
                t("footer.esc_clear")
                    .split(',')
                    .next()
                    .unwrap_or(&t("footer.esc_clear")),
                t("footer.esc_clear")
                    .split(',')
                    .nth(1)
                    .unwrap_or("keep")
                    .trim()
            )
        };
        let filter_w = (area.width as usize).saturating_sub(2 + suffix.chars().count());
        let spans = vec![
            Span::styled(" /", Style::default().fg(theme.hi_fg)),
            Span::styled(
                truncate_str(&app.filter_text, filter_w),
                Style::default().fg(theme.title),
            ),
            Span::styled(suffix, Style::default().fg(theme.inactive_fg)),
        ];
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }

    let compact = area.width <= 80;
    let ultra_compact = area.width <= 70;

    let mut spans = key_hint(" ↑↓", &t("footer.select"), theme).to_vec();
    if !ultra_compact {
        spans.push(Span::styled("↵", Style::default().fg(theme.hi_fg)));
        spans.push(Span::styled(
            format!(" {} ", t("footer.jump")),
            Style::default().fg(theme.main_fg),
        ));
    }
    if compact {
        spans.push(Span::styled("←→", Style::default().fg(theme.hi_fg)));
        spans.push(Span::styled(" tabs ", Style::default().fg(theme.main_fg)));
    }
    if !ultra_compact {
        spans.push(Span::styled("x", Style::default().fg(theme.hi_fg)));
        spans.push(Span::styled(
            format!(" {} ", t("footer.kill")),
            Style::default().fg(theme.main_fg),
        ));
    }
    spans.push(Span::styled("/", Style::default().fg(theme.hi_fg)));
    spans.push(Span::styled(
        format!(" {} ", t("footer.filter")),
        Style::default().fg(theme.main_fg),
    ));
    if !ultra_compact {
        spans.push(Span::styled("v", Style::default().fg(theme.hi_fg)));
        spans.push(Span::styled(
            format!(" {} ", t("footer.view")),
            Style::default().fg(theme.main_fg),
        ));
        if !compact {
            spans.push(Span::styled("Tab", Style::default().fg(theme.hi_fg)));
            spans.push(Span::styled(
                " main tabs ",
                Style::default().fg(theme.main_fg),
            ));
        }
        spans.push(Span::styled("?", Style::default().fg(theme.hi_fg)));
        spans.push(Span::styled(
            format!(" {} ", t("footer.help")),
            Style::default().fg(theme.main_fg),
        ));
    }
    // Chat scroll hint, only while a chat view is on screen and only when it
    // leaves room for the quit hint that follows.
    if !ultra_compact && app.chat_view_active() {
        let quit_hint_w = format!("q {} ", t("footer.quit")).chars().count();
        let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
        let chat_hint = key_hint("PgUp/PgDn", "chat", theme);
        let tools_hint = key_hint("d", "tools", theme);
        let width_of = |hint: &[Span<'static>; 2]| -> usize {
            hint.iter().map(|s| s.content.chars().count()).sum()
        };
        let chat_hint_w = width_of(&chat_hint);
        let tools_hint_w = width_of(&tools_hint);
        // The tools hint also yields to a visible filter or status message.
        let message_w = if !app.filter_text.is_empty() {
            app.filter_text.chars().count() + 3
        } else {
            app.status_msg
                .as_ref()
                .filter(|(_, when)| when.elapsed().as_secs() < 3)
                .map_or(0, |(msg, _)| msg.chars().count() + 2)
        };
        if used + chat_hint_w + tools_hint_w + quit_hint_w + message_w <= area.width as usize {
            spans.extend(chat_hint);
            spans.extend(tools_hint);
        } else if used + chat_hint_w + quit_hint_w <= area.width as usize {
            spans.extend(chat_hint);
        }
    }
    spans.push(Span::styled("q", Style::default().fg(theme.hi_fg)));
    spans.push(Span::styled(
        format!(" {} ", t("footer.quit")),
        Style::default().fg(theme.main_fg),
    ));

    // Show active filter or transient status
    if !compact && !app.filter_text.is_empty() {
        spans.push(Span::styled(
            format!(" /{} ", app.filter_text),
            Style::default().fg(theme.status_fg),
        ));
    } else if !compact {
        let status_text = app
            .status_msg
            .as_ref()
            .filter(|(_, when)| when.elapsed().as_secs() < 3)
            .map(|(msg, _)| msg.as_str());
        if let Some(msg) = status_text {
            spans.push(Span::styled(
                format!(" {msg} "),
                Style::default().fg(theme.status_fg),
            ));
        } else {
            spans.push(Span::styled(
                t("footer.auto"),
                Style::default().fg(theme.inactive_fg),
            ));
        }
    }

    let visible_count = app.visible_indices().len();
    let sessions_label = t("footer.sessions");
    let count_label = if visible_count < app.sessions.len() {
        format!(
            "{}/{} {}",
            visible_count,
            app.sessions.len(),
            sessions_label
        )
    } else {
        format!("{} {}", app.sessions.len(), sessions_label)
    };
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let count_w = count_label.chars().count();
    if used + count_w < area.width as usize {
        let remaining = (area.width as usize).saturating_sub(used + count_w);
        spans.push(Span::styled(
            format!("{:>width$}", count_label, width = remaining),
            Style::default().fg(theme.graph_text),
        ));
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PanelVisibility;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn footer_renders_concise_cmux_socket_failure() {
        let mut app = App::new_with_config(Theme::default(), &[], PanelVisibility::default());
        app.set_status("cmux: socket broken; restart cmux".to_string());

        let backend = TestBackend::new(120, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                draw_footer(
                    f,
                    &app,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 120,
                        height: 1,
                    },
                    &app.theme,
                )
            })
            .unwrap();
        let text = format!("{}", terminal.backend());

        assert!(text.contains("cmux: socket broken; restart cmux"));
        assert!(!text.contains("Broken pipe"));
        assert!(!text.contains("select-workspace"));
    }

    fn footer_text(app: &App, width: u16) -> String {
        let backend = TestBackend::new(width, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| draw_footer(f, app, Rect::new(0, 0, width, 1), &app.theme))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..width).map(|x| buffer[(x, 0)].symbol()).collect()
    }

    #[test]
    fn footer_shows_chat_scroll_hint_only_while_the_chat_view_is_visible() {
        let app = App::new_with_config(Theme::default(), &[], PanelVisibility::default());
        for width in [60u16, 70, 80, 100, 120, 200] {
            let text = footer_text(&app, width);
            assert!(!text.contains("PgUp/PgDn"), "{width}: {text}");
        }

        app.chat_view.set(crate::chat_view::ChatViewMetrics {
            text_width: 20,
            visible_rows: 4,
        });
        assert!(footer_text(&app, 120).contains("PgUp/PgDn chat"));
        assert!(footer_text(&app, 200).contains("PgUp/PgDn chat"));
        // Ultra compact footers keep only the essentials.
        assert!(!footer_text(&app, 70).contains("PgUp/PgDn"));
        // The hint is dropped, never the hints around it, when space runs out.
        for width in 60u16..=140 {
            let text = footer_text(&app, width);
            assert!(text.contains("q quit"), "{width}: {text}");
            assert!(text.contains("select"), "{width}: {text}");
            if text.contains("PgUp/PgDn chat") {
                assert!(text.contains("? help") || width <= 70, "{width}: {text}");
            }
        }
    }

    #[test]
    fn footer_shows_the_tools_hint_next_to_the_chat_hint_when_there_is_room() {
        let mut app = App::new_with_config(Theme::default(), &[], PanelVisibility::default());
        assert!(!footer_text(&app, 200).contains("d tools"));

        app.chat_view.set(crate::chat_view::ChatViewMetrics {
            text_width: 20,
            visible_rows: 4,
        });
        assert!(footer_text(&app, 200).contains("PgUp/PgDn chat d tools"));
        assert!(!footer_text(&app, 70).contains("d tools"));
        for width in 60u16..=140 {
            let text = footer_text(&app, width);
            assert!(text.contains("q quit"), "{width}: {text}");
            if text.contains("d tools") {
                assert!(text.contains("PgUp/PgDn chat"), "{width}: {text}");
            }
        }
        // A visible status message wins over the optional hint.
        app.set_status("cmux: socket broken; restart cmux".to_string());
        let text = footer_text(&app, 120);
        assert!(text.contains("cmux: socket broken; restart cmux"), "{text}");
        assert!(!text.contains("d tools"), "{text}");
    }
}
