mod footer;
mod header;
mod help;
mod quota;
mod sessions;
mod view_menu;
mod workspace;

use crate::app::{App, NarrowSection, NarrowTab};
use crate::locale::t;
use crate::theme::Theme;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

// ── braille graph symbols — from btop_draw.cpp ──────────────────────────────
// 5x5 lookup: [prev_val * 5 + cur_val], values 0-4
pub(crate) const BRAILLE_UP: [&str; 25] = [
    " ", "⢀", "⢠", "⢰", "⢸", "⡀", "⣀", "⣠", "⣰", "⣸", "⡄", "⣄", "⣤", "⣴", "⣼", "⡆", "⣆", "⣦", "⣶",
    "⣾", "⡇", "⣇", "⣧", "⣷", "⣿",
];

// ── gradient interpolation (btop-faithful: linear RGB, 101 steps) ────────────

/// Generate 101-step gradient from start→mid→end, matching btop's generateGradients().
pub(crate) fn make_gradient(
    start: (u8, u8, u8),
    mid: (u8, u8, u8),
    end: (u8, u8, u8),
) -> [Color; 101] {
    let mut out = [Color::Reset; 101];
    #[allow(clippy::needless_range_loop)]
    for i in 0..=100 {
        let (s, e, offset, range) = if i <= 50 {
            (start, mid, 0, 50)
        } else {
            (mid, end, 50, 50)
        };
        let t = i - offset;
        let r = s.0 as i32 + t as i32 * (e.0 as i32 - s.0 as i32) / range;
        let g = s.1 as i32 + t as i32 * (e.1 as i32 - s.1 as i32) / range;
        let b = s.2 as i32 + t as i32 * (e.2 as i32 - s.2 as i32) / range;
        out[i] = Color::Rgb(
            r.clamp(0, 255) as u8,
            g.clamp(0, 255) as u8,
            b.clamp(0, 255) as u8,
        );
    }
    out
}

/// Pick color from a gradient at a given percentage.
pub(crate) fn grad_at(gradient: &[Color; 101], pct: f64) -> Color {
    let idx = (pct.clamp(0.0, 100.0)).round() as usize;
    gradient[idx.min(100)]
}

/// Meter bar showing remaining quota: filled = remaining, color reflects urgency.
/// When remaining is high → green (safe), when low → red (danger).
pub(crate) fn remaining_bar(
    remaining_pct: f64,
    width: usize,
    gradient: &[Color; 101],
    meter_bg: Color,
) -> Vec<Span<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let clamped = remaining_pct.clamp(0.0, 100.0);
    let filled = ((clamped / 100.0) * width as f64).round() as usize;
    let used_pct = 100.0 - clamped;
    let mut spans = Vec::new();
    for i in 0..width {
        if i < filled {
            let cell_pct = used_pct;
            spans.push(Span::styled(
                "■",
                Style::default().fg(grad_at(gradient, cell_pct)),
            ));
        } else {
            spans.push(Span::styled("■", Style::default().fg(meter_bg)));
        }
    }
    spans
}

// ── braille sparkline ────────────────────────────────────────────────────────

/// Render a braille sparkline from data points (0.0–1.0), colored with gradient.
pub(crate) fn braille_sparkline(
    data: &[f64],
    width: usize,
    gradient: &[Color; 101],
    graph_text: Color,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    if data.is_empty() || width == 0 {
        for _ in 0..width {
            spans.push(Span::styled(" ", Style::default().fg(graph_text)));
        }
        return spans;
    }

    // We need pairs of data points per braille char (prev, cur)
    // Pad or sample data to fit width * 2 points
    let needed = width * 2;
    let sampled: Vec<f64> = if data.len() >= needed {
        data[data.len() - needed..].to_vec()
    } else {
        let mut v = vec![0.0; needed - data.len()];
        v.extend_from_slice(data);
        v
    };

    for i in 0..width {
        let prev = (sampled[i * 2].clamp(0.0, 1.0) * 4.0).round() as usize;
        let cur = (sampled[i * 2 + 1].clamp(0.0, 1.0) * 4.0).round() as usize;
        let idx = prev * 5 + cur;
        let pct = (sampled[i * 2 + 1] * 100.0).round() as usize;
        let color = grad_at(gradient, pct as f64);
        spans.push(Span::styled(
            BRAILLE_UP[idx.min(24)].to_string(),
            Style::default().fg(color),
        ));
    }
    spans
}

// ── btop-style block with notch title: ──┐¹title┌────── ─────────────────────

pub(crate) fn btop_block_active(
    title: &str,
    box_color: Color,
    theme: &Theme,
    active: bool,
) -> Block<'static> {
    let title = if active {
        format!("{title}(*)")
    } else {
        title.to_string()
    };
    Block::default()
        .title(Line::from(vec![
            Span::styled("┐", Style::default().fg(box_color)),
            Span::styled(
                title,
                Style::default()
                    .fg(theme.title)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("┌", Style::default().fg(box_color)),
        ]))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(box_color))
}

pub(crate) fn styled_label(text: &str, graph_text: Color) -> Span<'static> {
    Span::styled(text.to_string(), Style::default().fg(graph_text))
}

// ── main draw ────────────────────────────────────────────────────────────────

const MIN_WIDTH: u16 = 60;
const MIN_HEIGHT: u16 = 18;
pub(crate) const DESKTOP_WIDTH: u16 = 100;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ClickTarget {
    MainTab(crate::workspace::MainTab),
    NarrowTab(NarrowTab),
    NarrowSection(NarrowSection),
    NarrowZoom(NarrowSection),
    Session(usize),
}

struct DesktopLayout {
    header: Rect,
    mid: Vec<(NarrowSection, Rect)>,
    sessions: Option<Rect>,
    footer: Rect,
}

pub fn draw(f: &mut Frame, app: &App) {
    // The chat view sets its metrics again when it is drawn this frame; until
    // then the chat scroll keys and footer hint see no chat view.
    app.chat_view
        .set(crate::chat_view::ChatViewMetrics::default());
    let theme = &app.theme;
    let area = f.area();
    let w = area.width;
    let h = area.height;

    // Paint the entire frame with the theme's background so the app renders
    // correctly on light-background terminals (where text would otherwise be
    // light-on-light and unreadable).
    f.render_widget(
        Block::default().style(Style::default().bg(theme.main_bg).fg(theme.main_fg)),
        area,
    );

    if w < MIN_WIDTH || h < MIN_HEIGHT {
        let msg = vec![
            Line::from(Span::styled(
                t("term.too_small"),
                Style::default()
                    .fg(theme.main_fg)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(vec![
                Span::styled(
                    format!("{} ", t("term.width")),
                    Style::default().fg(theme.main_fg),
                ),
                Span::styled(
                    w.to_string(),
                    Style::default().fg(if w < MIN_WIDTH {
                        Color::Red
                    } else {
                        Color::Green
                    }),
                ),
                Span::styled(
                    format!(" {} ", t("term.height")),
                    Style::default().fg(theme.main_fg),
                ),
                Span::styled(
                    h.to_string(),
                    Style::default().fg(if h < MIN_HEIGHT {
                        Color::Red
                    } else {
                        Color::Green
                    }),
                ),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                t("term.needed"),
                Style::default()
                    .fg(theme.main_fg)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                format!(
                    "{} {}  {} {}",
                    t("term.width"),
                    MIN_WIDTH,
                    t("term.height"),
                    MIN_HEIGHT
                ),
                Style::default().fg(theme.main_fg),
            )),
        ];
        let block = Paragraph::new(msg).alignment(Alignment::Center);
        let y = h / 2 - 2;
        let msg_area = Rect {
            x: 0,
            y,
            width: w,
            height: 5.min(h.saturating_sub(y)),
        };
        f.render_widget(block, msg_area);
        return;
    }

    if app.workspace.tab != crate::workspace::MainTab::Dashboard {
        workspace::draw(f, app, area);
        return;
    }

    if w < DESKTOP_WIDTH {
        draw_narrow(f, app, area, theme);
        draw_overlays(f, app, theme);
        return;
    }

    let layout = desktop_layout(app, area);
    header::draw_header(f, app, layout.header, theme);

    for (section, area) in layout.mid {
        match section {
            NarrowSection::Quota => quota::draw_quota_panel(f, app, area, theme),
            NarrowSection::Sessions => {}
        }
    }

    if let Some(area) = layout.sessions {
        sessions::draw_sessions_panel(f, app, area, theme);
    }
    footer::draw_footer(f, app, layout.footer, theme);

    draw_overlays(f, app, theme);
}

fn desktop_layout(app: &App, area: Rect) -> DesktopLayout {
    const FIXED: u16 = 2; // header + footer
                          // Quota renders Claude and Codex rows plus the total in a short panel.
    const QUOTA_H: u16 = 5;

    let available = area.height.saturating_sub(FIXED);
    let quota_h = if app.show_quota {
        QUOTA_H.min(available)
    } else {
        0
    };
    let sessions_h = if app.show_sessions {
        available.saturating_sub(quota_h)
    } else {
        0
    };

    let mut constraints = vec![Constraint::Length(1)];
    if quota_h > 0 {
        constraints.push(Constraint::Length(quota_h));
    }
    if sessions_h > 0 {
        constraints.push(Constraint::Min(sessions_h));
    } else {
        constraints.push(Constraint::Min(0));
    }
    constraints.push(Constraint::Length(1));

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    let mut idx = 1;
    let mid = if quota_h > 0 {
        idx += 1;
        vec![(NarrowSection::Quota, chunks[1])]
    } else {
        Vec::new()
    };
    let sessions = if sessions_h > 0 {
        Some(chunks[idx])
    } else {
        None
    };

    DesktopLayout {
        header: chunks[0],
        mid,
        sessions,
        footer: chunks[chunks.len() - 1],
    }
}

fn draw_narrow(f: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);

    header::draw_header(f, app, chunks[0], theme);

    let body = chunks[1];
    draw_active_narrow_panel(f, app, body, theme);

    draw_narrow_tabs(f, app, chunks[2], theme);
    footer::draw_footer(f, app, chunks[3], theme);
}

fn draw_narrow_tabs(f: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    let active = app.active_narrow_tab();
    let tab_areas = narrow_tab_layout(app, area);
    let used = narrow_tab_group_width(&tab_areas);
    let pad = area.width.saturating_sub(used) as usize;
    let mut spans: Vec<Span> = Vec::new();
    if pad > 0 {
        spans.push(Span::styled(
            " ".repeat(pad),
            Style::default().bg(theme.main_bg),
        ));
    }
    for (i, (tab, _)) in tab_areas.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" ", Style::default().bg(theme.main_bg)));
        }
        let selected = Some(tab) == active;
        let mut style = Style::default()
            .bg(if selected {
                theme.selected_bg
            } else {
                theme.main_bg
            })
            .fg(if selected {
                theme.selected_fg
            } else {
                theme.inactive_fg
            });
        if selected {
            style = style.add_modifier(Modifier::BOLD);
        };
        spans.push(Span::styled(narrow_tab_label(tab), style));
    }

    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(theme.main_bg)),
        area,
    );
}

fn narrow_tab_label(tab: NarrowTab) -> String {
    format!(" {}({}) ", tab.label(), tab.shortcut())
}

fn narrow_tab_width(tab: NarrowTab) -> u16 {
    narrow_tab_label(tab).chars().count() as u16
}

fn narrow_tab_group_width(tab_areas: &[(NarrowTab, Rect)]) -> u16 {
    let labels = tab_areas.iter().map(|(_, area)| area.width).sum::<u16>();
    labels + tab_areas.len().saturating_sub(1) as u16
}

fn draw_active_narrow_panel(f: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    let Some(tab) = app.active_narrow_tab() else {
        return;
    };

    for (section, section_area) in narrow_section_areas(app, tab, area) {
        draw_narrow_section(f, app, section_area, theme, section);
        draw_narrow_zoom_button(f, app, section_area, theme, section);
    }
}

fn narrow_section_areas(app: &App, tab: NarrowTab, area: Rect) -> Vec<(NarrowSection, Rect)> {
    let sections = if let Some(section) = app.maximized_narrow_section() {
        if section.tab() == tab {
            vec![section]
        } else {
            app.visible_narrow_sections(tab)
        }
    } else {
        app.visible_narrow_sections(tab)
    };
    if sections.is_empty() {
        return Vec::new();
    }

    if sections.len() == 1 {
        return vec![(sections[0], area)];
    }

    let count = sections.len() as u32;
    let constraints: Vec<Constraint> = (0..count).map(|_| Constraint::Ratio(1, count)).collect();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    sections.into_iter().zip(chunks.iter().copied()).collect()
}

fn draw_narrow_section(
    f: &mut Frame,
    app: &App,
    area: Rect,
    theme: &Theme,
    section: NarrowSection,
) {
    let active = app.active_narrow_section() == Some(section);
    match section {
        NarrowSection::Sessions => {
            sessions::draw_sessions_panel_active(f, app, area, theme, active)
        }
        NarrowSection::Quota => quota::draw_quota_panel_active(f, app, area, theme, active),
    }
}

fn draw_narrow_zoom_button(
    f: &mut Frame,
    app: &App,
    area: Rect,
    theme: &Theme,
    section: NarrowSection,
) {
    let Some(button_area) = zoom_button_area(area) else {
        return;
    };
    let label = if app.maximized_narrow_section() == Some(section) {
        " - "
    } else {
        " + "
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            label,
            Style::default()
                .bg(theme.main_bg)
                .fg(theme.hi_fg)
                .add_modifier(Modifier::BOLD),
        ))),
        button_area,
    );
}

fn draw_overlays(f: &mut Frame, app: &App, theme: &Theme) {
    if app.view_open {
        view_menu::draw_view_overlay(f, app, theme);
    }
    if app.help_open {
        help::draw_help_overlay(f, theme);
    }
}

pub(crate) fn click_target(app: &App, area: Rect, column: u16, row: u16) -> Option<ClickTarget> {
    if row == area.y {
        let mut x = area.x.saturating_add(1);
        for tab in crate::workspace::MainTab::ALL {
            let end = x + tab.label().len() as u16 + 2;
            if column >= x && column < end && column < area.right() {
                return Some(ClickTarget::MainTab(tab));
            }
            x = end.saturating_add(1);
        }
    }
    if app.workspace.tab != crate::workspace::MainTab::Dashboard {
        return None;
    }
    if area.width >= DESKTOP_WIDTH {
        let layout = desktop_layout(app, area);
        if let Some(sessions_area) = layout.sessions {
            if contains(sessions_area, column, row) {
                return session_at(app, sessions_area, row).map(ClickTarget::Session);
            }
        }
        return None;
    }

    let chunks = narrow_chunks(area);
    if contains(chunks[2], column, row) {
        return narrow_tab_at(app, chunks[2], column).map(ClickTarget::NarrowTab);
    }

    let tab = app.active_narrow_tab()?;

    if contains(chunks[1], column, row) {
        for (section, section_area) in narrow_section_areas(app, tab, chunks[1]) {
            if !contains(section_area, column, row) {
                continue;
            }
            if zoom_button_at(section_area, column, row) {
                return Some(ClickTarget::NarrowZoom(section));
            }
            if section == NarrowSection::Sessions {
                if let Some(index) = session_at(app, section_area, row) {
                    return Some(ClickTarget::Session(index));
                }
            }
            return Some(ClickTarget::NarrowSection(section));
        }
    }

    None
}

fn zoom_button_area(area: Rect) -> Option<Rect> {
    if area.width < 5 || area.height == 0 {
        return None;
    }
    Some(Rect {
        x: area.x + area.width - 4,
        y: area.y,
        width: 3,
        height: 1,
    })
}

fn zoom_button_at(area: Rect, column: u16, row: u16) -> bool {
    zoom_button_area(area).is_some_and(|button| contains(button, column, row))
}

fn narrow_chunks(area: Rect) -> Vec<Rect> {
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area)
        .to_vec()
}

fn narrow_tab_at(app: &App, area: Rect, column: u16) -> Option<NarrowTab> {
    for (tab, tab_area) in narrow_tab_layout(app, area) {
        if contains(tab_area, column, area.y) {
            return Some(tab);
        }
    }
    None
}

fn narrow_tab_layout(app: &App, area: Rect) -> Vec<(NarrowTab, Rect)> {
    let tabs = app.visible_narrow_tabs();
    if tabs.is_empty() {
        return Vec::new();
    }
    let labels_width = tabs.iter().map(|&tab| narrow_tab_width(tab)).sum::<u16>();
    let gaps = tabs.len().saturating_sub(1) as u16;
    let total = labels_width.saturating_add(gaps).min(area.width);
    let mut x = area.x + area.width.saturating_sub(total);
    let mut out = Vec::with_capacity(tabs.len());
    for (i, tab) in tabs.into_iter().enumerate() {
        if i > 0 {
            x = x.saturating_add(1);
        }
        let width = narrow_tab_width(tab).min(area.x + area.width - x);
        if width == 0 {
            break;
        }
        out.push((
            tab,
            Rect {
                x,
                y: area.y,
                width,
                height: 1,
            },
        ));
        x = x.saturating_add(width);
        if x >= area.x + area.width {
            break;
        }
    }
    out
}

fn session_at(app: &App, area: Rect, row: u16) -> Option<usize> {
    if area.height < 4 || row <= area.y + 1 {
        return None;
    }

    let inner_h = area.height.saturating_sub(2);
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
        (inner_h * 2 / 3).min(inner_h.saturating_sub(5))
    } else if inner_h <= 12 {
        6.min(inner_h.saturating_sub(3))
    } else {
        10.min(inner_h / 2)
    };
    let max_table = inner_h.saturating_sub(detail_reserve);
    let table_h = (1 + session_rows).min(max_table);
    let table_y = area.y + 1;
    if row >= table_y.saturating_add(table_h) {
        return None;
    }

    let visible_rows = table_h.saturating_sub(1) as usize;
    let selected_pos = visible.iter().position(|&i| i == app.selected).unwrap_or(0);
    let selected_row_start: usize = visible
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
    let scroll_offset = (selected_row_start + selected_session_rows).saturating_sub(visible_rows);
    let target_row = scroll_offset + row.saturating_sub(table_y + 1) as usize;
    let mut offset = 0usize;
    for &idx in &visible {
        let rows = if app.tree_view {
            1 + app.sessions[idx].subagents.len()
        } else {
            1
        };
        if target_row >= offset && target_row < offset + rows {
            return Some(idx);
        }
        offset += rows;
    }

    None
}

fn contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x
        && column < area.x.saturating_add(area.width)
        && row >= area.y
        && row < area.y.saturating_add(area.height)
}

// ── utility functions ────────────────────────────────────────────────────────

pub(crate) fn fmt_mem_kb(kb: u64) -> String {
    if kb >= 1_048_576 {
        format!("{:.1}G", kb as f64 / 1_048_576.0)
    } else if kb >= 1024 {
        format!("{}M", kb / 1024)
    } else {
        format!("{}K", kb)
    }
}

pub(crate) fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        let v = n as f64 / 1_000_000.0;
        if v == v.floor() {
            format!("{}M", v as u64)
        } else {
            format!("{:.1}M", v)
        }
    } else if n >= 1_000 {
        let v = n as f64 / 1_000.0;
        if v == v.floor() {
            format!("{}k", v as u64)
        } else {
            format!("{:.1}k", v)
        }
    } else {
        format!("{}", n)
    }
}

pub(crate) fn truncate_str(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(s) <= max {
        s.to_string()
    } else {
        let mut width = 0;
        let mut truncated = String::new();
        for ch in s.chars() {
            let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
            if width + ch_width >= max {
                break;
            }
            truncated.push(ch);
            width += ch_width;
        }
        format!("{}…", truncated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PanelVisibility;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn truncate_str_respects_terminal_display_width() {
        assert_eq!(truncate_str("ＡＢ123", 6), "ＡＢ1…");
    }

    #[test]
    fn desktop_default_detail_shows_chat_instead_of_timeline() {
        let mut app = App::new_with_config(Theme::default(), &[], PanelVisibility::default());
        crate::demo::populate_demo(&mut app);
        app.sessions[app.selected].children.clear();
        app.sessions[app.selected].subagents.clear();

        let backend = TestBackend::new(160, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = format!("{}", terminal.backend());

        assert!(
            text.contains("CHAT"),
            "chat should render by default\n{text}"
        );
        assert!(
            text.contains("webhook signatures"),
            "recent chat tail should render selected session messages\n{text}"
        );
        assert!(
            !text.contains("TIMELINE"),
            "timeline should be opt-in via l toggle\n{text}"
        );
    }

    #[test]
    fn desktop_renders_only_quota_and_sessions() {
        let text = render_demo(120, 40);
        assert!(text.contains("Agents Dashboard"), "{text}");
        assert!(
            text.contains("quota") && text.contains("sessions"),
            "{text}"
        );
        for removed in ["context", "tokens", "projects", "ports", "mcp"] {
            assert!(
                !text.contains(removed),
                "{removed} panel should be gone\n{text}"
            );
        }
    }

    #[test]
    fn compact_renders_work_and_usage_tabs_only() {
        let text = render_demo(80, 24);
        assert!(text.contains("Work") && text.contains("Usage"), "{text}");
        assert!(!text.contains("System"), "{text}");
        assert!(text.contains("sessions"), "{text}");
    }

    #[test]
    fn desktop_click_targets_sessions() {
        let mut app = App::new_with_config(Theme::default(), &[], PanelVisibility::default());
        crate::demo::populate_demo(&mut app);
        let area = Rect::new(0, 0, 120, 40);
        let sessions_area = desktop_layout(&app, area).sessions.unwrap();
        assert_eq!(
            click_target(&app, area, sessions_area.x + 2, sessions_area.y + 2),
            Some(ClickTarget::Session(0))
        );
    }

    #[test]
    fn session_click_rows_match_rendering_with_scroll_and_tree() {
        for tree_view in [false, true] {
            let mut app = App::new_with_config(Theme::default(), &[], PanelVisibility::default());
            crate::demo::populate_demo(&mut app);
            let template = app.sessions[0].clone();
            app.sessions = (0..8)
                .map(|i| {
                    let mut session = template.clone();
                    session.session_name = format!("Session {i}");
                    session.subagents = vec![crate::model::SubAgent {
                        name: format!("Worker {i}"),
                        status: "working".into(),
                        tokens: 100,
                    }];
                    session
                })
                .collect();
            app.tree_view = tree_view;
            app.selected = 7;
            let area = Rect::new(0, 0, 120, 14);
            let backend = TestBackend::new(area.width, area.height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|f| sessions::draw_sessions_panel(f, &app, area, &app.theme))
                .unwrap();

            let buffer = terminal.backend().buffer();
            let mut matched = 0;
            for y in 2..7 {
                let row: String = (0..area.width).map(|x| buffer[(x, y)].symbol()).collect();
                for i in 0..8 {
                    if row.contains(&format!("Session {i}")) || row.contains(&format!("Worker {i}"))
                    {
                        assert_eq!(session_at(&app, area, y), Some(i), "{row}");
                        matched += 1;
                    }
                }
            }
            assert_eq!(matched, 5, "every visible row should map to its session");
            assert_eq!(session_at(&app, area, 1), None);
            assert_eq!(session_at(&app, area, 7), None);
        }
    }

    fn render_demo(width: u16, height: u16) -> String {
        let mut app = App::new_with_config(Theme::default(), &[], PanelVisibility::default());
        crate::demo::populate_demo(&mut app);

        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        format!("{}", terminal.backend())
    }
}
