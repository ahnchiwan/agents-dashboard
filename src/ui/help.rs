use crate::locale::t;
use crate::theme::Theme;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;

// ENTRIES now uses locale keys
fn get_entries() -> Vec<(String, String)> {
    vec![
        (t("help.navigation"), String::new()),
        ("  ↑↓ / j k".to_string(), t("help.select_session")),
        ("  PgUp PgDn".to_string(), t("help.scroll_chat")),
        ("  ↵".to_string(), t("help.jump_tmux")),
        ("  /".to_string(), t("help.filter")),
        ("  Esc".to_string(), t("help.clear_filter")),
        (t("help.actions"), String::new()),
        ("  x".to_string(), t("help.kill_session")),
        ("  r".to_string(), t("help.refresh")),
        ("  q".to_string(), t("help.quit")),
        (t("help.views"), String::new()),
        ("  v".to_string(), t("help.view_menu")),
        ("  Tab / S-Tab".to_string(), "Switch main tabs".into()),
        ("  ←→".to_string(), "Work/Usage or field selection".into()),
        ("  t / T".to_string(), t("help.cycle_theme")),
        ("  l".to_string(), t("help.toggle_timeline")),
        ("  f".to_string(), t("help.toggle_file_audit")),
        ("  d".to_string(), t("help.toggle_chat_tools")),
        ("  1-2".to_string(), t("help.toggle_panels")),
        ("  M".to_string(), t("help.mcp_suppress")),
        (t("help.help"), String::new()),
        ("  ?".to_string(), t("help.this_help")),
        (t("help.legend"), String::new()),
        ("  a / i".to_string(), t("help.launch_surface_suffix")),
    ]
}

pub(crate) fn draw_help_overlay(f: &mut Frame, theme: &Theme) {
    let entries = get_entries();
    let area = f.area();
    let popup_w = 60u16.min(area.width.saturating_sub(4));
    let popup_h = (entries.len() as u16 + 4).min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(popup_w)) / 2;
    let y = (area.height.saturating_sub(popup_h)) / 2;
    let popup = Rect::new(x, y, popup_w, popup_h);

    f.render_widget(Clear, popup);

    let help_title = t("help.title");
    let block = Block::default()
        .style(Style::default().bg(theme.main_bg))
        .title(
            Line::from(vec![Span::styled(
                help_title.clone(),
                Style::default()
                    .fg(theme.title)
                    .add_modifier(Modifier::BOLD),
            )])
            .alignment(Alignment::Center),
        )
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.cpu_box));
    f.render_widget(block, popup);

    let inner = Rect::new(
        popup.x + 2,
        popup.y + 1,
        popup.width.saturating_sub(4),
        popup.height.saturating_sub(2),
    );

    let mut lines: Vec<Line> = Vec::with_capacity(entries.len() + 2);
    for (key, desc) in entries {
        if desc.is_empty() {
            lines.push(Line::from(Span::styled(
                key,
                Style::default()
                    .fg(theme.title)
                    .add_modifier(Modifier::BOLD),
            )));
        } else {
            lines.push(Line::from(vec![
                Span::styled(format!("{:<14}", key), Style::default().fg(theme.hi_fg)),
                Span::styled(desc, Style::default().fg(theme.main_fg)),
            ]));
        }
    }
    lines.push(Line::from(""));
    let help_press_key = t("help.press_key");
    lines.push(Line::from(Span::styled(
        help_press_key,
        Style::default().fg(theme.graph_text),
    )));

    f.render_widget(Paragraph::new(lines), inner);
}
