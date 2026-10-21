use crate::app::App;
use crate::locale::t;
use crate::model::RateLimitInfo;
use crate::theme::Theme;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use super::{btop_block_active, fmt_tokens, grad_at, make_gradient, remaining_bar, styled_label};

/// Data considered "stale" when its updated_at is older than this many seconds.
const STALE_SECS: u64 = 600;

/// Fixed source order so columns stay stable across runs.
const SOURCES: &[&str] = &["claude", "codex"];

pub(crate) fn draw_quota_panel(f: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    draw_quota_panel_active(f, app, area, theme, false);
}

pub(crate) fn draw_quota_panel_active(
    f: &mut Frame,
    app: &App,
    area: Rect,
    theme: &Theme,
    active: bool,
) {
    let cpu_grad = make_gradient(theme.cpu_grad.start, theme.cpu_grad.mid, theme.cpu_grad.end);

    let block = btop_block_active("quota", theme.proc_box, theme, active);
    f.render_widget(block, area);

    let inner = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };

    // Bottom summary: total tokens + rate
    let total_tokens: u64 = app.sessions.iter().map(|s| s.total_tokens()).sum();
    let rates = &app.token_rates;
    let ticks_per_min = 30usize;
    let tokens_per_min: f64 = rates.iter().rev().take(ticks_per_min).sum();

    // One row per known source (CLAUDE, CODEX) with its windows side by side.
    // Rows are always rendered so the panel layout stays stable even when a
    // source has no data yet.
    let content_h = inner.height.saturating_sub(1); // reserve last row for totals
    let lines: Vec<Line> = SOURCES
        .iter()
        .map(|source| {
            let rl = app
                .rate_limits
                .iter()
                .find(|r| r.source.eq_ignore_ascii_case(source));
            source_line(source, rl, inner.width as usize, &cpu_grad, theme)
        })
        .collect();
    f.render_widget(
        Paragraph::new(lines),
        Rect {
            height: content_h,
            ..inner
        },
    );

    // Total tokens summary on last row (full width)
    let bottom_area = Rect {
        x: inner.x,
        y: inner.y + content_h,
        width: inner.width,
        height: 1,
    };
    let total_label = t("quota.total");
    f.render_widget(
        Paragraph::new(vec![Line::from(vec![
            Span::styled(
                format!(" {} {}", total_label, fmt_tokens(total_tokens)),
                Style::default().fg(theme.main_fg),
            ),
            Span::styled(
                format!(" {}/min", fmt_tokens(tokens_per_min as u64)),
                Style::default().fg(theme.graph_text),
            ),
        ])]),
        bottom_area,
    );
}

/// Render one source's rate-limit windows on a single row.
fn source_line(
    source: &str,
    rl: Option<&RateLimitInfo>,
    width: usize,
    cpu_grad: &[ratatui::style::Color; 101],
    theme: &Theme,
) -> Line<'static> {
    let label = format!(" {:<6}", source.to_uppercase());

    let Some(rl) = rl else {
        let hint = if source.eq_ignore_ascii_case("claude") {
            t("quota.abtop_setup")
        } else {
            t("quota.run_codex")
        };
        return Line::from(vec![
            Span::styled(
                label,
                Style::default()
                    .fg(theme.title)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  — {}", t("quota.no_data")),
                Style::default().fg(theme.inactive_fg),
            ),
            Span::styled(
                format!(" · {}", hint),
                Style::default().fg(theme.graph_text),
            ),
        ]);
    };

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let is_stale = rl
        .updated_at
        .is_some_and(|ts| now.saturating_sub(ts) > STALE_SECS);

    // Stale data → dim the source name. Keeping a distinct freshness color
    // but no explicit age is enough to signal "values may be out of date".
    let source_color = if is_stale {
        theme.inactive_fg
    } else {
        theme.title
    };

    // Show future reset timestamps even for stale data, but past/missing
    // timestamps are omitted because the next reset cannot be inferred.
    let windows: Vec<(String, f64, String)> = [
        (
            rl.five_hour_pct,
            rl.five_hour_resets_at,
            rl.five_hour_window_minutes,
            t("quota.5h"),
        ),
        (
            rl.seven_day_pct,
            rl.seven_day_resets_at,
            rl.seven_day_window_minutes,
            t("quota.7d"),
        ),
    ]
    .into_iter()
    .filter_map(|(pct, resets_at, window_minutes, fallback)| {
        let used_pct = pct?;
        let reset = resets_at.map(format_reset_time).unwrap_or_default();
        Some((
            format_window_label(window_minutes, fallback),
            used_pct,
            reset,
        ))
    })
    .collect();

    // Each window is "  <label> <bar> 100%" plus an optional reset time.
    // Narrow rows drop reset times before shrinking the bars.
    let n = windows.len().max(1);
    let fixed = UnicodeWidthStr::width(label.as_str())
        + windows
            .iter()
            .map(|(window_label, _, _)| 3 + UnicodeWidthStr::width(window_label.as_str()) + 5)
            .sum::<usize>();
    let reset_col_w = windows
        .iter()
        .map(|(_, _, reset)| UnicodeWidthStr::width(reset.as_str()))
        .max()
        .unwrap_or(0);
    let show_reset = reset_col_w > 0 && fixed + n * (8 + 1 + reset_col_w) <= width;
    let reset_w = if show_reset { n * (1 + reset_col_w) } else { 0 };
    let bar_w = (width.saturating_sub(fixed + reset_w) / n).clamp(2, 8);
    let reset_color = if is_stale {
        theme.inactive_fg
    } else {
        theme.graph_text
    };

    let mut spans = vec![Span::styled(
        label,
        Style::default()
            .fg(source_color)
            .add_modifier(Modifier::BOLD),
    )];
    for (window_label, used_pct, reset) in windows {
        let remaining = (100.0 - used_pct).clamp(0.0, 100.0);
        spans.push(styled_label(
            format!("  {} ", window_label).as_str(),
            theme.graph_text,
        ));
        spans.extend(remaining_bar(remaining, bar_w, cpu_grad, theme.meter_bg));
        spans.push(Span::styled(
            format!(" {:>3.0}%", remaining),
            Style::default().fg(grad_at(cpu_grad, used_pct)),
        ));
        if show_reset {
            spans.push(Span::styled(
                format!(" {:<w$}", reset, w = reset_col_w),
                Style::default().fg(reset_color),
            ));
        }
    }
    Line::from(spans)
}

fn format_window_label(window_minutes: Option<u64>, fallback: String) -> String {
    let Some(minutes) = window_minutes else {
        return fallback;
    };

    if minutes == 0 {
        fallback
    } else if minutes % (24 * 60) == 0 {
        format!("{}{}", minutes / (24 * 60), t("time.d"))
    } else if minutes % 60 == 0 {
        format!("{}{}", minutes / 60, t("time.h"))
    } else {
        format!("{}{}", minutes, t("time.m"))
    }
}

/// Format a future reset timestamp in local time. Include the date when the
/// reset is not today. Returns an empty string when the timestamp is invalid
/// or already past; callers do not know enough to infer the next reset.
pub(crate) fn format_reset_time(reset_ts: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format_reset_time_at(reset_ts, now)
}

fn format_reset_time_at(reset_ts: u64, now_ts: u64) -> String {
    if reset_ts <= now_ts {
        return String::new();
    }
    let Some(reset_utc) = i64::try_from(reset_ts)
        .ok()
        .and_then(|timestamp| chrono::DateTime::<chrono::Utc>::from_timestamp(timestamp, 0))
    else {
        return String::new();
    };
    let Some(now_utc) = i64::try_from(now_ts)
        .ok()
        .and_then(|timestamp| chrono::DateTime::<chrono::Utc>::from_timestamp(timestamp, 0))
    else {
        return String::new();
    };
    let reset_local = reset_utc.with_timezone(&chrono::Local);
    let now_local = now_utc.with_timezone(&chrono::Local);
    if reset_local.date_naive() == now_local.date_naive() {
        reset_local.format("%H:%M").to_string()
    } else {
        use chrono::{Datelike, Timelike};
        format!(
            "{}/{} {:02}:{:02}",
            reset_local.month(),
            reset_local.day(),
            reset_local.hour(),
            reset_local.minute()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PanelVisibility;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn render(app: &App, width: u16) -> Vec<String> {
        let backend = TestBackend::new(width, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| draw_quota_panel(f, app, Rect::new(0, 0, width, 5), &app.theme))
            .unwrap();
        format!("{}", terminal.backend())
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn limits(source: &str) -> RateLimitInfo {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        RateLimitInfo {
            source: source.into(),
            five_hour_pct: Some(35.0),
            five_hour_resets_at: Some(now + 2 * 3600),
            seven_day_pct: Some(12.0),
            seven_day_resets_at: Some(now + 3 * 86400),
            updated_at: Some(now),
            ..Default::default()
        }
    }

    #[test]
    fn quota_shows_each_source_windows_on_one_row() {
        let mut app = App::new_with_config(Theme::default(), &[], PanelVisibility::default());
        let claude = limits("claude");
        let codex = limits("codex");
        let resets = [
            claude.five_hour_resets_at.unwrap(),
            claude.seven_day_resets_at.unwrap(),
        ];
        app.rate_limits = vec![claude, codex];

        let rows = render(&app, 80);
        for (row, source) in [(1, "CLAUDE"), (2, "CODEX")] {
            let line = &rows[row];
            assert!(
                line.contains(source) && line.contains("5h") && line.contains("7d"),
                "{source} row should hold both windows\n{}",
                rows.join("\n")
            );
            assert!(line.contains("65%") && line.contains("88%"), "{line}");
            assert!(line.contains(&format_reset_time(resets[0])), "{line}");
            assert!(line.contains(&format_reset_time(resets[1])), "{line}");
            assert!(!line.contains(" in "), "{line}");
        }
        assert!(rows[3].contains("total"), "{}", rows.join("\n"));
    }

    #[test]
    fn narrow_quota_drops_reset_times_but_keeps_both_windows() {
        let mut app = App::new_with_config(Theme::default(), &[], PanelVisibility::default());
        app.rate_limits = vec![limits("claude")];

        let rows = render(&app, 50);
        let line = &rows[1];
        assert!(line.contains("5h") && line.contains("7d"), "{line}");
        assert!(
            !line.contains(&format_reset_time(
                app.rate_limits[0].five_hour_resets_at.unwrap()
            )),
            "{line}"
        );
        assert!(
            rows[2].contains("CODEX") && rows[2].contains("no data"),
            "{}",
            rows[2]
        );
    }

    #[test]
    fn stale_quota_still_shows_future_reset_times() {
        let mut app = App::new_with_config(Theme::default(), &[], PanelVisibility::default());
        let mut stale = limits("claude");
        stale.updated_at = Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                .saturating_sub(STALE_SECS + 1),
        );
        let expected = format_reset_time(stale.five_hour_resets_at.unwrap());
        app.rate_limits = vec![stale];

        let rows = render(&app, 80);
        assert!(rows[1].contains(&expected), "{}", rows.join("\n"));
    }

    #[test]
    fn reset_time_uses_local_clock_and_hides_past_timestamps() {
        use chrono::{Datelike, Timelike};

        let now = chrono::NaiveDate::from_ymd_opt(2026, 1, 2)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp() as u64;
        let reset = now + 3600;
        let local = chrono::DateTime::<chrono::Utc>::from_timestamp(reset as i64, 0)
            .unwrap()
            .with_timezone(&chrono::Local);

        let expected = if local.date_naive()
            == chrono::DateTime::<chrono::Utc>::from_timestamp(now as i64, 0)
                .unwrap()
                .with_timezone(&chrono::Local)
                .date_naive()
        {
            format!("{:02}:{:02}", local.hour(), local.minute())
        } else {
            format!(
                "{}/{} {:02}:{:02}",
                local.month(),
                local.day(),
                local.hour(),
                local.minute()
            )
        };
        assert_eq!(format_reset_time_at(reset, now), expected);
        assert_eq!(format_reset_time_at(now, now), "");
    }
}
