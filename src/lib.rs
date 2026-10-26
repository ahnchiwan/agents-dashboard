//! Agents Dashboard (library crate abtop) — AI agent monitor.
//!
//! This crate is both a binary (the TUI, entered via [`run`]) and a library.
//! The library surface exists so a separate local tool (e.g. a web UI) can
//! reuse the data-collection layer in-process and serialize it via
//! [`snapshot::Snapshot`] / [`app::App::to_snapshot`], without reimplementing
//! session discovery and without depending on the terminal frontend.
//!
//! # Public API for library consumers
//!
//! The stable surface for in-process consumers is [`app`] (notably
//! [`App::to_snapshot`](app::App::to_snapshot) and
//! [`App::tick_no_summaries`](app::App::tick_no_summaries)), [`snapshot`],
//! [`config`], [`demo`], [`host_info`], and the data types in [`model`]. The
//! [`collector`], [`locale`], [`setup`], [`theme`], and [`ui`] modules are
//! published mainly to support the bundled TUI binary and may change without a
//! semver-major bump — depend on them at your own risk.
//!
//! Enum wire formats are part of the snapshot contract: variants such as
//! [`model::SessionStatus`] serialize as their CamelCase names (`"Thinking"`,
//! `"Executing"`, …) and chat roles serialize as `"user"` / `"assistant"`.
//! These strings are stable and won't be renamed without a major version bump.
//!
//! # Threading model
//!
//! [`App`] is **not** `Send`: it owns boxed collector trait objects
//! and must stay on the thread that created it. Don't move it between threads
//! or share it with request handlers — instead, run the collector loop on one
//! thread, serialize each [`snapshot::Snapshot`] to JSON, and hand the *string*
//! to other threads.
//!
//! # Typical usage
//!
//! ```no_run
//! use abtop::app::App;
//! use abtop::{config, theme::Theme};
//!
//! let cfg = config::load_config();
//! let mut app = App::new_with_config_and_claude_dirs(
//!     Theme::default(),
//!     &cfg.hidden_agents,
//!     cfg.panels,
//!     &cfg.claude_config_dirs,
//! );
//! loop {
//!     app.tick_no_summaries();                // refresh without spawning `claude --print`
//!     let snap = app.to_snapshot(2_000);      // pure read → JSON-friendly DTO
//!     let json = serde_json::to_string(&snap).unwrap();
//!     // ... serve `json`, sleep for the interval, repeat ...
//!     # break;
//! }
//! ```

pub mod app;
pub mod archive;
mod chat_view;
pub mod collector;
pub mod config;
pub mod demo;
pub mod host_info;
pub mod jump;
mod launcher;
pub mod locale;
pub mod model;
pub mod setup;
pub mod snapshot;
pub mod theme;
pub mod ui;
pub mod workspace;

use app::{App, JumpOutcome};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::prelude::*;
use std::io::{self, stdout};
use std::time::Duration;

/// Construct a headless `App` from loaded config + theme. Shared by the
/// `--json` and `--once` entry points.
fn build_app(theme: theme::Theme, cfg: &config::AppConfig) -> App {
    App::new_with_config_and_claude_dirs(
        theme,
        &cfg.hidden_agents,
        cfg.panels,
        &cfg.claude_config_dirs,
    )
}

pub fn run() -> io::Result<()> {
    // --version / -V flag: print version and exit
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("agents-dashboard {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    // --update flag: this fork has no release installer; explain how to update
    if std::env::args().any(|a| a == "--update") {
        return run_update();
    }

    // --setup-reply installs permission-status hooks without changing StatusLine.
    if std::env::args().any(|a| a == "--setup-reply") {
        setup::run_reply_setup()?;
        return Ok(());
    }

    // --setup flag: configure StatusLine hook and exit
    if std::env::args().any(|a| a == "--setup") {
        setup::run_setup();
        return Ok(());
    }

    // Load config once; it drives both the default theme and the hidden-agents list.
    let cfg = config::load_config();

    // --theme flag > config file > default
    let initial_theme = std::env::args()
        .position(|a| a == "--theme")
        .map(|pos| {
            let val = std::env::args().nth(pos + 1);
            match val {
                Some(name) if !name.starts_with('-') => name,
                Some(name) => {
                    eprintln!("--theme requires a theme name, got '{}'", name);
                    eprintln!("available: {}", theme::THEME_NAMES.join(", "));
                    std::process::exit(1);
                }
                None => {
                    eprintln!("--theme requires a theme name");
                    eprintln!("available: {}", theme::THEME_NAMES.join(", "));
                    std::process::exit(1);
                }
            }
        })
        .map(|name| {
            theme::Theme::by_name(&name).unwrap_or_else(|| {
                eprintln!(
                    "unknown theme '{}'. available: {}",
                    name,
                    theme::THEME_NAMES.join(", ")
                );
                std::process::exit(1);
            })
        })
        .or_else(|| theme::Theme::by_name(&cfg.theme));

    let demo_mode = std::env::args().any(|a| a == "--demo");
    let exit_on_jump = std::env::args().any(|a| a == "--exit-on-jump");
    let mouse_capture = should_enable_mouse_capture(std::env::args());

    // --json flag: print a machine-readable JSON snapshot and exit.
    // Single tick, no summary subprocesses. Useful for scripting and as a
    // manual check of the web snapshot API; the web tool uses the library
    // `App::to_snapshot` directly rather than shelling out to this.
    if std::env::args().any(|a| a == "--json") {
        let mut app = build_app(initial_theme.unwrap_or_default(), &cfg);
        if demo_mode {
            demo::populate_demo(&mut app);
        } else {
            app.tick_no_summaries();
        }
        match serde_json::to_string_pretty(&app.to_snapshot(2000)) {
            Ok(json) => {
                println!("{}", json);
                return Ok(());
            }
            Err(e) => {
                eprintln!("failed to serialize snapshot: {}", e);
                std::process::exit(1);
            }
        }
    }

    // --once flag: print snapshot and exit
    if std::env::args().any(|a| a == "--once") {
        let mut app = build_app(initial_theme.unwrap_or_default(), &cfg);
        if demo_mode {
            demo::populate_demo(&mut app);
        } else {
            app.tick();
            // Wait for summaries: retry-aware budget (up to 30s total to allow 2 × 10s attempts + slack)
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            while std::time::Instant::now() < deadline {
                app.drain_and_retry_summaries();
                if !app.has_pending_summaries() && !app.has_retryable_summaries() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }
        print_snapshot(&app);
        return Ok(());
    }

    // Setup terminal
    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    if mouse_capture {
        stdout().execute(EnableMouseCapture)?;
    }
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;

    let app_result = run_app(
        &mut terminal,
        demo_mode,
        initial_theme,
        exit_on_jump,
        &cfg.hidden_agents,
        cfg.panels,
        &cfg.claude_config_dirs,
    );

    // Always attempt both cleanup steps regardless of app result
    let r1 = if mouse_capture {
        stdout().execute(DisableMouseCapture).map(|_| ())
    } else {
        Ok(())
    };
    let r2 = disable_raw_mode();
    let r3 = stdout().execute(LeaveAlternateScreen).map(|_| ());

    // Return app error first, then cleanup errors
    app_result.and(r1).and(r2).and(r3)
}

fn should_enable_mouse_capture<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    args.into_iter()
        .fold(false, |enabled, arg| match arg.as_ref() {
            "--mouse" => true,
            "--no-mouse" => false,
            _ => enabled,
        })
}

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    demo_mode: bool,
    initial_theme: Option<theme::Theme>,
    exit_on_jump: bool,
    hidden_agents: &[String],
    panels: config::PanelVisibility,
    claude_config_dirs: &[std::path::PathBuf],
) -> io::Result<()> {
    let mut app = App::new_with_config_and_claude_dirs(
        initial_theme.unwrap_or_default(),
        hidden_agents,
        panels,
        claude_config_dirs,
    );
    // The TUI shows no session summaries, so it never spawns `claude --print`.
    if demo_mode {
        demo::populate_demo(&mut app);
    } else {
        app.tick_no_summaries();
    }

    let mut last_tick = std::time::Instant::now();
    let tick_interval = Duration::from_secs(2);
    let render_interval = Duration::from_millis(500);

    loop {
        app.workspace.poll(&app.sessions);
        terminal.draw(|f| ui::draw(f, &app))?;

        // Poll at 500ms for smooth animations; data tick every 2s
        let had_input = if event::poll(render_interval)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    handle_key_press(&mut app, key, demo_mode, exit_on_jump, |app| {
                        app.jump_to_session()
                    })
                }
                Event::Mouse(mouse) => {
                    let size = terminal.size()?;
                    let area = Rect::new(0, 0, size.width, size.height);
                    handle_mouse_event(&mut app, mouse, area);
                }
                _ => {}
            }
            true
        } else {
            false
        };

        if demo_mode {
            // Rotate token rates to animate the sparkline
            if let Some(front) = app.token_rates.pop_front() {
                app.token_rates.push_back(front);
            }
        } else if !had_input && last_tick.elapsed() >= tick_interval {
            // Data tick every 2s — skip when handling input to avoid lag
            app.tick_no_summaries();
            last_tick = std::time::Instant::now();
        }

        if app.should_quit {
            break;
        }
    }

    Ok(())
}

fn handle_key_press(
    app: &mut App,
    key: KeyEvent,
    demo_mode: bool,
    exit_on_jump: bool,
    jump_to_session: impl FnOnce(&mut App) -> JumpOutcome,
) {
    let selected = app.selected;
    if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
        for root in app.collector_config_dirs() {
            if !app.workspace.claude_dirs.contains(&root) {
                app.workspace.claude_dirs.push(root);
            }
        }
        app.workspace
            .switch(key.code == KeyCode::BackTab, &app.sessions);
        app.filter_active = false;
        app.help_open = false;
        app.view_open = false;
        return;
    }
    if app.workspace.tab != workspace::MainTab::Dashboard {
        if key.code == KeyCode::Char('q')
            && !app.workspace.searching
            && !app.workspace.new.editing
            && app.workspace.resume.is_none()
        {
            app.quit();
        } else {
            app.workspace.handle_key(key.code, &app.sessions, demo_mode);
        }
        return;
    }
    if app.help_open {
        // Any key dismisses help.
        app.help_open = false;
    } else if app.view_open {
        match key.code {
            KeyCode::Esc | KeyCode::Char('v') => app.view_open = false,
            KeyCode::Char('T') => app.tree_view = !app.tree_view,
            KeyCode::Char('l') => app.toggle_timeline(),
            KeyCode::Char('f') => app.toggle_file_audit(),
            KeyCode::Char('d') => app.toggle_chat_tools(),
            KeyCode::Char(c @ '1'..='2') => app.toggle_panel(c as u8 - b'0'),
            KeyCode::Char('M') => app.toggle_mcp_session_suppression(),
            KeyCode::Char('t') => app.cycle_theme(),
            _ => {}
        }
    } else if app.filter_active {
        match key.code {
            KeyCode::Esc => app.clear_filter(),
            KeyCode::Enter => app.filter_active = false,
            KeyCode::Backspace => app.filter_pop(),
            KeyCode::Down => app.select_next(),
            KeyCode::Up => app.select_prev(),
            KeyCode::Char(c) => app.filter_push(c),
            _ => {}
        }
    } else {
        match key.code {
            KeyCode::Char('q') => app.quit(),
            KeyCode::Char('r') if !demo_mode => app.tick_no_summaries(),
            KeyCode::Down | KeyCode::Char('j') => app.select_next(),
            KeyCode::Up | KeyCode::Char('k') => app.select_prev(),
            KeyCode::PageUp => app.scroll_chat_up(),
            KeyCode::PageDown => app.scroll_chat_down(),
            KeyCode::End => app.scroll_chat_to_end(),
            KeyCode::Right => app.select_next_narrow_tab(),
            KeyCode::Left => app.select_prev_narrow_tab(),
            KeyCode::Char('w') => app.set_narrow_tab(app::NarrowTab::Work),
            KeyCode::Char('u') => app.set_narrow_tab(app::NarrowTab::Usage),
            KeyCode::Char('+') | KeyCode::Char('=') => app.maximize_active_narrow_section(),
            KeyCode::Char('-') => app.restore_narrow_sections(),
            KeyCode::Char('x') if !demo_mode => app.kill_selected(),
            KeyCode::Char('t') => app.cycle_theme(),
            KeyCode::Char('T') => app.tree_view = !app.tree_view,
            KeyCode::Char('l') | KeyCode::Char('L') => app.toggle_timeline(),
            KeyCode::Char(c @ '1'..='2') => app.toggle_panel(c as u8 - b'0'),
            KeyCode::Char('M') => app.toggle_mcp_session_suppression(),
            KeyCode::Char('v') => app.toggle_view_menu(),
            KeyCode::Char('?') => app.toggle_help(),
            KeyCode::Char('/') => app.filter_active = true,
            KeyCode::Esc if !app.filter_text.is_empty() => app.clear_filter(),
            KeyCode::Char('f') | KeyCode::Char('F') => app.toggle_file_audit(),
            KeyCode::Char('d') | KeyCode::Char('D') => app.toggle_chat_tools(),
            KeyCode::Enter if !demo_mode => match jump_to_session(app) {
                JumpOutcome::Jumped if exit_on_jump => app.quit(),
                JumpOutcome::Failed(msg) => app.set_status(msg),
                JumpOutcome::Jumped | JumpOutcome::NoOp => {}
            },
            _ => {}
        }
    }
    if app.selected != selected {
        app.reset_chat_scroll();
    }
}

fn handle_mouse_event(app: &mut App, mouse: MouseEvent, area: Rect) {
    if app.help_open || app.view_open || app.filter_active {
        return;
    }

    if app.workspace.tab != workspace::MainTab::Dashboard {
        let code = match mouse.kind {
            MouseEventKind::ScrollDown => Some(KeyCode::Down),
            MouseEventKind::ScrollUp => Some(KeyCode::Up),
            MouseEventKind::ScrollRight => Some(KeyCode::Right),
            MouseEventKind::ScrollLeft => Some(KeyCode::Left),
            _ => None,
        };
        if let Some(code) = code {
            app.workspace.handle_key(code, &app.sessions, true);
            return;
        }
    }
    let selected = app.selected;
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if let Some(target) = ui::click_target(app, area, mouse.column, mouse.row) {
                match target {
                    ui::ClickTarget::MainTab(tab) => {
                        for root in app.collector_config_dirs() {
                            if !app.workspace.claude_dirs.contains(&root) {
                                app.workspace.claude_dirs.push(root);
                            }
                        }
                        app.workspace.set_tab(tab, &app.sessions);
                    }
                    ui::ClickTarget::NarrowTab(tab) => app.set_narrow_tab(tab),
                    ui::ClickTarget::NarrowSection(section) => {
                        app.set_active_narrow_section(section);
                    }
                    ui::ClickTarget::NarrowZoom(section) => {
                        app.toggle_narrow_section_zoom(section);
                    }
                    ui::ClickTarget::Session(index) => {
                        app.select_session(index);
                        app.set_active_narrow_section(app::NarrowSection::Sessions);
                    }
                }
            }
        }
        MouseEventKind::ScrollDown => app.select_next(),
        MouseEventKind::ScrollUp => app.select_prev(),
        MouseEventKind::ScrollRight => app.select_next_narrow_tab(),
        MouseEventKind::ScrollLeft => app.select_prev_narrow_tab(),
        _ => {}
    }
    if app.selected != selected {
        app.reset_chat_scroll();
    }
}

/// Strip control characters (including ANSI escapes) and Unicode bidi
/// overrides from a string for safe terminal output. Defeats CVE-2021-42574
/// (Trojan Source) style attacks via RTLO/LRO/PDF/isolate characters.
fn sanitize_output(s: &str) -> String {
    s.chars()
        .filter(|c| {
            !c.is_control()
                && !matches!(*c,
                '\u{202A}'..='\u{202E}'
                | '\u{2066}'..='\u{2069}'
                | '\u{200E}'
                | '\u{200F}')
        })
        .collect()
}

fn print_snapshot(app: &App) {
    println!(
        "Agents Dashboard — {} sessions, {} mcp servers\n",
        app.sessions.len(),
        app.mcp_servers.len()
    );
    if !app.mcp_servers.is_empty() {
        let now = std::time::SystemTime::now();
        for server in &app.mcp_servers {
            let active = server.active_count(now, collector::mcp::ACTIVE_MTIME_SECS);
            let total = server.rollouts.len();
            let last_age = server
                .latest_mtime()
                .and_then(|m| now.duration_since(m).ok())
                .map(|d| {
                    if d.as_secs() < 60 {
                        format!("{}s", d.as_secs())
                    } else if d.as_secs() < 3600 {
                        format!("{}m", d.as_secs() / 60)
                    } else {
                        format!("{}h", d.as_secs() / 3600)
                    }
                })
                .unwrap_or_else(|| "—".to_string());
            let profile = server.profile.as_deref().unwrap_or("default");
            println!(
                "  mcp pid={} parent={} profile={:<16} active={}/{} last={}",
                server.pid, server.parent_cli, profile, active, total, last_age
            );
        }
        println!();
    }
    for session in &app.sessions {
        let status = match &session.status {
            model::SessionStatus::Thinking => "◉ Working",
            model::SessionStatus::Executing => "◉ Working",
            model::SessionStatus::AwaitingInput => "● Reply",
            model::SessionStatus::Waiting | model::SessionStatus::RateLimited => "◌ Wait",
            model::SessionStatus::Unknown => "? Unknown",
            model::SessionStatus::Done => "✓ Done",
        };
        let sid_short = if session.session_id.len() >= 7 {
            &session.session_id[..7]
        } else {
            &session.session_id
        };
        let project_label = format!("{}({})", session.project_name, sid_short);
        let summary = sanitize_output(&app.session_summary(session));
        println!(
            "  {} {:<20} {} {} {:<10} CTX:{:>3.0}% Tok:{} Mem:{}M {}",
            session.pid,
            sanitize_output(&project_label),
            summary,
            status,
            session.model.replace("claude-", ""),
            session.context_percent,
            fmt_tok(session.total_tokens()),
            session.mem_mb,
            session.elapsed_display(),
        );
        if let Some(task) = session.current_tasks.last() {
            println!("       └─ {}", sanitize_output(task));
        }
        for child in &session.children {
            let port = child.port.map(|p| format!(":{}", p)).unwrap_or_default();
            println!(
                "       {} {} {}K {}",
                child.pid,
                sanitize_output(
                    &child
                        .command
                        .split_whitespace()
                        .take(3)
                        .collect::<Vec<_>>()
                        .join(" ")
                ),
                child.mem_kb / 1024,
                port,
            );
        }
    }
}

/// Printed by `--update`. Nothing is downloaded or executed: this fork ships no
/// release installer, and the upstream one would install the original abtop.
const UPDATE_HELP: &str = "\
agents-dashboard does not update itself: it is installed from source and has no release installer.

To update, run this in your agents-dashboard checkout:
  git pull
  cargo install --path .";

fn run_update() -> io::Result<()> {
    eprintln!(
        "agents-dashboard {}\n\n{UPDATE_HELP}",
        env!("CARGO_PKG_VERSION")
    );
    std::process::exit(1);
}

fn fmt_tok(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{}", n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};
    use ratatui::backend::TestBackend;

    #[test]
    fn mouse_capture_defaults_off_and_last_explicit_option_wins() {
        assert!(!should_enable_mouse_capture(["abtop"]));
        assert!(should_enable_mouse_capture(["abtop", "--mouse"]));
        assert!(!should_enable_mouse_capture(["abtop", "--no-mouse"]));
        assert!(!should_enable_mouse_capture([
            "abtop",
            "--mouse",
            "--no-mouse"
        ]));
        assert!(should_enable_mouse_capture([
            "abtop",
            "--no-mouse",
            "--mouse"
        ]));
        assert!(!should_enable_mouse_capture([
            "abtop",
            "--no-mouse",
            "--demo"
        ]));
    }

    #[test]
    fn actual_left_click_events_switch_main_tabs_at_wide_and_compact_sizes() {
        for width in [80, 140] {
            let mut app = App::new_with_config(
                theme::Theme::default(),
                &[],
                config::PanelVisibility::default(),
            );
            app.workspace.loaded = true;
            for tab in [
                workspace::MainTab::Old,
                workspace::MainTab::New,
                workspace::MainTab::Dashboard,
            ] {
                let area = Rect::new(0, 0, width, 40);
                let (column, row) = (0..area.height).flat_map(|y| (0..area.width).map(move |x| (x, y)))
                    .find(|(x, y)| matches!(ui::click_target(&app, area, *x, *y), Some(ui::ClickTarget::MainTab(t)) if t == tab))
                    .expect("Rendered tab must have a click target");
                handle_mouse_event(
                    &mut app,
                    MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column,
                        row,
                        modifiers: KeyModifiers::NONE,
                    },
                    area,
                );
                assert_eq!(app.workspace.tab, tab);
            }
        }
    }

    #[test]
    fn enter_jump_failure_renders_footer_status() {
        let mut app = App::new_with_config(
            theme::Theme::default(),
            &[],
            config::PanelVisibility::default(),
        );
        demo::populate_demo(&mut app);

        handle_key_press(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            false,
            false,
            |_| JumpOutcome::Failed("cmux: socket broken; restart cmux".to_string()),
        );

        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| ui::draw(f, &app)).unwrap();
        let text = format!("{}", terminal.backend());

        assert!(text.contains("cmux: socket broken; restart cmux"));
        assert!(!text.contains("Broken pipe"));
        assert!(!text.contains("select-workspace"));
    }

    #[test]
    fn archive_path_edit_accepts_q_and_tab_cancels_without_quitting() {
        let temp = tempfile::tempdir().unwrap();
        let mut app = App::new_with_config(
            theme::Theme::default(),
            &[],
            config::PanelVisibility::default(),
        );
        app.workspace.tab = workspace::MainTab::Old;
        app.workspace.loaded = true;
        app.workspace.archive.push(archive::ArchivedSession {
            agent: archive::Agent::Claude,
            id: "archived-session".into(),
            title: "Moved project".into(),
            cwd: temp.path().join("missing"),
            root: temp.path().into(),
            modified: 0,
        });
        let press = |app: &mut App, code| {
            handle_key_press(
                app,
                KeyEvent::new(code, KeyModifiers::NONE),
                false,
                false,
                |_| panic!("Archive must not jump to a live session"),
            )
        };
        press(&mut app, KeyCode::Enter);
        let path = app.workspace.resume.as_ref().unwrap().path.clone();
        press(&mut app, KeyCode::Char('q'));
        assert_eq!(
            app.workspace.resume.as_ref().unwrap().path,
            format!("{path}q")
        );
        assert!(!app.should_quit);
        press(&mut app, KeyCode::BackTab);
        assert_eq!(app.workspace.tab, workspace::MainTab::Dashboard);
        assert!(app.workspace.resume.is_none());
        assert!(!app.should_quit);
    }

    #[test]
    fn main_tabs_are_separate_from_work_usage_and_c_is_not_a_shortcut() {
        let mut app = App::new_with_config(
            theme::Theme::default(),
            &[],
            config::PanelVisibility::default(),
        );
        app.workspace.loaded = true;
        let press = |app: &mut App, code| {
            handle_key_press(
                app,
                KeyEvent::new(code, KeyModifiers::NONE),
                true,
                false,
                |_| panic!("History must not use the live-session jumper"),
            )
        };
        press(&mut app, KeyCode::Right);
        assert_eq!(app.narrow_tab, app::NarrowTab::Usage);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.workspace.tab, workspace::MainTab::Old);
        assert_eq!(app.narrow_tab, app::NarrowTab::Usage);
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.workspace.tab, workspace::MainTab::New);
        press(&mut app, KeyCode::BackTab);
        assert_eq!(app.workspace.tab, workspace::MainTab::Old);
        press(&mut app, KeyCode::BackTab);
        assert_eq!(app.workspace.tab, workspace::MainTab::Dashboard);
        press(&mut app, KeyCode::Left);
        assert_eq!(app.narrow_tab, app::NarrowTab::Work);
    }

    /// Demo app whose sessions all carry a long conversation, drawn once so the
    /// chat view has its geometry.
    fn chat_app() -> (App, Terminal<TestBackend>) {
        let mut app = App::new_with_config(
            theme::Theme::default(),
            &[],
            config::PanelVisibility::default(),
        );
        demo::populate_demo(&mut app);
        let reply = (1..=80)
            .map(|i| format!("row-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        for session in &mut app.sessions {
            session.chat_messages = vec![
                model::ChatMessage {
                    role: model::ChatRole::User,
                    text: "question".into(),
                },
                model::ChatMessage {
                    role: model::ChatRole::Assistant,
                    text: reply.clone(),
                },
            ];
        }
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| ui::draw(f, &app)).unwrap();
        (app, terminal)
    }

    fn press_demo(app: &mut App, code: KeyCode) {
        handle_key_press(
            app,
            KeyEvent::new(code, KeyModifiers::NONE),
            true,
            false,
            |_| JumpOutcome::NoOp,
        );
    }

    /// Redraw and report whether the chat title shows the scrolled-up hint.
    fn scrolled_up(app: &App, terminal: &mut Terminal<TestBackend>) -> bool {
        terminal.draw(|f| ui::draw(f, app)).unwrap();
        format!("{}", terminal.backend()).contains("End: latest")
    }

    #[test]
    fn moving_selection_resets_chat_scroll() {
        let (mut app, mut terminal) = chat_app();
        assert!(!scrolled_up(&app, &mut terminal));
        press_demo(&mut app, KeyCode::PageUp);
        assert!(scrolled_up(&app, &mut terminal));
        press_demo(&mut app, KeyCode::Down);
        press_demo(&mut app, KeyCode::Up);
        assert!(!scrolled_up(&app, &mut terminal));
    }

    #[test]
    fn page_keys_and_end_scroll_the_chat_history() {
        let (mut app, mut terminal) = chat_app();
        press_demo(&mut app, KeyCode::PageUp);
        assert!(scrolled_up(&app, &mut terminal));
        press_demo(&mut app, KeyCode::PageUp);
        press_demo(&mut app, KeyCode::PageDown);
        assert!(scrolled_up(&app, &mut terminal));
        // End returns to the newest message and resumes following.
        press_demo(&mut app, KeyCode::End);
        assert!(!scrolled_up(&app, &mut terminal));
        assert!(format!("{}", terminal.backend()).contains("row-80"));
        // Paging down to the bottom also resumes following.
        press_demo(&mut app, KeyCode::PageUp);
        for _ in 0..20 {
            press_demo(&mut app, KeyCode::PageDown);
        }
        assert!(!scrolled_up(&app, &mut terminal));
    }

    #[test]
    fn chat_keys_are_ignored_by_overlays_filter_and_other_tabs() {
        let (mut app, mut terminal) = chat_app();
        // Help consumes the key instead of scrolling.
        press_demo(&mut app, KeyCode::Char('?'));
        assert!(app.help_open);
        press_demo(&mut app, KeyCode::PageUp);
        assert!(!app.help_open);
        assert!(!scrolled_up(&app, &mut terminal));
        // The filter prompt owns the keyboard.
        press_demo(&mut app, KeyCode::Char('/'));
        assert!(app.filter_active);
        press_demo(&mut app, KeyCode::PageUp);
        press_demo(&mut app, KeyCode::End);
        press_demo(&mut app, KeyCode::Esc);
        assert!(!scrolled_up(&app, &mut terminal));
        // Other main tabs handle their own keys.
        press_demo(&mut app, KeyCode::Tab);
        assert_ne!(app.workspace.tab, workspace::MainTab::Dashboard);
        press_demo(&mut app, KeyCode::PageUp);
        press_demo(&mut app, KeyCode::BackTab);
        assert_eq!(app.workspace.tab, workspace::MainTab::Dashboard);
        assert!(!scrolled_up(&app, &mut terminal));
        // The timeline replaces the chat view, so the keys do nothing there.
        for session in &mut app.sessions {
            session.tool_calls = vec![model::ToolCall {
                name: "Read".into(),
                arg: "src/main.rs".into(),
                duration_ms: 10,
            }];
        }
        press_demo(&mut app, KeyCode::Char('l'));
        terminal.draw(|f| ui::draw(f, &app)).unwrap();
        assert!(!app.chat_view_active());
        press_demo(&mut app, KeyCode::PageUp);
        press_demo(&mut app, KeyCode::Char('l'));
        assert!(!scrolled_up(&app, &mut terminal));
    }

    #[test]
    fn existing_dashboard_keys_are_unchanged_by_chat_scrolling() {
        let (mut app, _terminal) = chat_app();
        assert_eq!(app.selected, 0);
        press_demo(&mut app, KeyCode::Down);
        assert_eq!(app.selected, 1);
        press_demo(&mut app, KeyCode::Char('j'));
        assert_eq!(app.selected, 2);
        press_demo(&mut app, KeyCode::Up);
        press_demo(&mut app, KeyCode::Char('k'));
        assert_eq!(app.selected, 0);

        // Enter still jumps to the terminal, and the scroll keys never do.
        let mut jumps = 0;
        let mut press = |app: &mut App, code: KeyCode| {
            handle_key_press(
                app,
                KeyEvent::new(code, KeyModifiers::NONE),
                false,
                false,
                |_| {
                    jumps += 1;
                    JumpOutcome::NoOp
                },
            );
        };
        press(&mut app, KeyCode::PageUp);
        press(&mut app, KeyCode::PageDown);
        press(&mut app, KeyCode::End);
        press(&mut app, KeyCode::Enter);
        assert_eq!(jumps, 1);
    }

    fn with_tool_items(app: &mut App) {
        for session in &mut app.sessions {
            session.chat_messages = vec![
                model::ChatMessage {
                    role: model::ChatRole::User,
                    text: "question".into(),
                },
                model::ChatMessage {
                    role: model::ChatRole::ToolCall,
                    text: "Bash cargo test".into(),
                },
                model::ChatMessage {
                    role: model::ChatRole::ToolResult,
                    text: "TOOLSECRET output line".into(),
                },
                model::ChatMessage {
                    role: model::ChatRole::Assistant,
                    text: "answer".into(),
                },
            ];
        }
    }

    #[test]
    fn d_toggles_chat_tool_details_and_the_choice_follows_the_selection() {
        let (mut app, mut terminal) = chat_app();
        with_tool_items(&mut app);
        let draw = |app: &App, terminal: &mut Terminal<TestBackend>| {
            terminal.draw(|f| ui::draw(f, app)).unwrap();
            format!("{}", terminal.backend())
        };
        assert!(app.show_chat_tools, "tool details default to on");
        let text = draw(&app, &mut terminal);
        assert!(text.contains("▸ Bash cargo test"), "{text}");
        assert!(text.contains("TOOLSECRET output line"), "{text}");

        press_demo(&mut app, KeyCode::Char('d'));
        assert!(!app.show_chat_tools);
        let text = draw(&app, &mut terminal);
        assert!(!text.contains("▸ Bash cargo test"), "{text}");
        assert!(text.contains("tools hidden"), "{text}");
        assert!(text.contains("chat tool details: off"), "{text}");

        // Moving to another session keeps the choice.
        press_demo(&mut app, KeyCode::Down);
        assert!(!app.show_chat_tools);
        assert!(!draw(&app, &mut terminal).contains("▸ Bash cargo test"));

        press_demo(&mut app, KeyCode::Char('D'));
        assert!(app.show_chat_tools);
        assert!(draw(&app, &mut terminal).contains("▸ Bash cargo test"));
    }

    #[test]
    fn d_is_scoped_to_the_dashboard_and_the_view_menu() {
        let (mut app, _terminal) = chat_app();
        // The filter prompt owns the keyboard, so `d` is just text there.
        press_demo(&mut app, KeyCode::Char('/'));
        press_demo(&mut app, KeyCode::Char('d'));
        assert_eq!(app.filter_text, "d");
        assert!(app.show_chat_tools);
        press_demo(&mut app, KeyCode::Esc);
        // Help swallows it.
        press_demo(&mut app, KeyCode::Char('?'));
        press_demo(&mut app, KeyCode::Char('d'));
        assert!(app.show_chat_tools && !app.help_open);
        // Other main tabs do not toggle it.
        press_demo(&mut app, KeyCode::Tab);
        press_demo(&mut app, KeyCode::Char('d'));
        assert!(app.show_chat_tools);
        press_demo(&mut app, KeyCode::BackTab);
        // The view menu lists it and accepts its key.
        press_demo(&mut app, KeyCode::Char('v'));
        assert!(app.view_open);
        press_demo(&mut app, KeyCode::Char('d'));
        assert!(!app.show_chat_tools && app.view_open);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| ui::draw(f, &app)).unwrap();
        let text = format!("{}", terminal.backend());
        assert!(text.contains("chat tool details"), "{text}");
    }

    #[test]
    fn toggling_chat_tool_details_does_not_change_the_json_snapshot() {
        let (mut app, _terminal) = chat_app();
        with_tool_items(&mut app);
        let snapshot = |app: &App| {
            let mut value = serde_json::to_value(app.to_snapshot(2_000)).unwrap();
            value.as_object_mut().unwrap().remove("generated_at_ms");
            value
        };
        let before = snapshot(&app);
        press_demo(&mut app, KeyCode::Char('d'));
        let after = snapshot(&app);
        assert_eq!(before, after);
        let text = after.to_string();
        assert!(!text.contains("TOOLSECRET"), "tool output leaked");
        assert!(!text.contains("Bash cargo test"), "tool call leaked");
        assert!(!text.contains("show_chat_tools"));
        for session in after["sessions"].as_array().unwrap() {
            let roles: Vec<&str> = session["chat_messages"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["role"].as_str().unwrap())
                .collect();
            assert_eq!(roles, ["user", "assistant"]);
        }
    }

    #[test]
    fn chat_scrolling_does_not_change_the_json_snapshot() {
        let (mut app, _terminal) = chat_app();
        // Everything but the generation time must match.
        let snapshot = |app: &App| {
            let mut value = serde_json::to_value(app.to_snapshot(2_000)).unwrap();
            value.as_object_mut().unwrap().remove("generated_at_ms");
            value
        };
        let before = snapshot(&app);
        press_demo(&mut app, KeyCode::PageUp);
        press_demo(&mut app, KeyCode::End);
        let after = snapshot(&app);
        assert_eq!(before, after);
        let text = before.to_string();
        assert!(!text.contains("chat_scroll") && !text.contains("chat_view"));
        let sessions = before["sessions"].as_array().unwrap();
        assert!(!sessions.is_empty());
        for session in sessions {
            assert_eq!(session["chat_messages"].as_array().unwrap().len(), 2);
        }
    }
}
