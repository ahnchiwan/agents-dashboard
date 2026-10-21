use crate::app::App;
use crate::theme::Theme;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

/// Color of the `|` separators and the selected tab: black on light themes,
/// the theme's title color on dark ones so the header stays legible.
fn header_fg(theme: &Theme) -> Color {
    match theme.main_bg {
        Color::Rgb(r, g, b)
            if u32::from(r) * 299 + u32::from(g) * 587 + u32::from(b) * 114 >= 128_000 =>
        {
            Color::Black
        }
        _ => theme.title,
    }
}

pub(crate) fn draw_header(f: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    let emphasis = Style::default().fg(header_fg(theme));
    let mut tabs = vec![Span::styled("|", emphasis)];
    for tab in crate::workspace::MainTab::ALL {
        let style = if app.workspace.tab == tab {
            emphasis
        } else {
            Style::default().fg(theme.inactive_fg)
        };
        tabs.push(Span::styled(format!(" {} ", tab.label()), style));
        tabs.push(Span::styled("|", emphasis));
    }
    f.render_widget(Paragraph::new(Line::from(tabs)), area);
}
