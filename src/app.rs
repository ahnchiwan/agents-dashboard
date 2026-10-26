use crate::chat_view::{ChatAnchor, ChatLayout, ChatViewMetrics};
use crate::collector::{read_rate_limits, McpServer, MultiCollector};
use crate::host_info::{AgentAggregate, HostMetrics, HostSampler};
use crate::model::{AgentSession, ChatMessage, OrphanPort, RateLimitInfo, SessionStatus};
use crate::theme::Theme;
use std::borrow::Cow;
use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Instant;

/// Maximum data points kept for the live token-rate graph.
const GRAPH_HISTORY_LEN: usize = 200;
/// Max concurrent summary jobs.
const MAX_SUMMARY_JOBS: usize = 3;
/// Max summary attempts per session before giving up.
const MAX_SUMMARY_RETRIES: u32 = 2;

/// Scroll state of the chat history under the selected session. `anchor` is
/// `None` while the view follows the newest message.
#[derive(Clone, Debug, Default)]
struct ChatScroll {
    /// (agent_cli, session_id) the anchor belongs to; any other session
    /// follows its newest message.
    session: Option<(String, String)>,
    anchor: Option<ChatAnchor>,
}

/// Produce a terminal-safe fallback summary from a raw prompt.
fn sanitize_fallback(prompt: &str, max_len: usize) -> String {
    prompt
        .chars()
        .filter(|c| !c.is_control() || *c == ' ')
        .take(max_len)
        .collect()
}

/// Outcome of an Enter-key jump attempt. Distinct from `Option<String>` so
/// callers (notably `--exit-on-jump`) can tell a real terminal jump apart from
/// a no-op (unsupported terminal, or empty session list).
#[derive(Debug, PartialEq, Eq)]
pub enum JumpOutcome {
    /// Actually switched to a terminal pane/tab/window.
    Jumped,
    /// Tried to jump through an applicable backend, but the focus command failed.
    Failed(String),
    /// Unsupported terminal, or nothing selected — nothing happened.
    NoOp,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NarrowTab {
    Work,
    Usage,
}

impl NarrowTab {
    pub const ALL: [Self; 2] = [Self::Work, Self::Usage];

    pub fn label(self) -> &'static str {
        match self {
            Self::Work => "Work",
            Self::Usage => "Usage",
        }
    }

    pub fn shortcut(self) -> char {
        match self {
            Self::Work => 'w',
            Self::Usage => 'u',
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NarrowSection {
    Sessions,
    Quota,
}

impl NarrowSection {
    pub fn tab(self) -> NarrowTab {
        match self {
            Self::Sessions => NarrowTab::Work,
            Self::Quota => NarrowTab::Usage,
        }
    }
}

pub struct App {
    pub workspace: crate::workspace::Workspace,
    pub sessions: Vec<AgentSession>,
    pub selected: usize,
    pub should_quit: bool,
    /// Token rate per tick (delta). Ring buffer for the braille graph.
    pub token_rates: VecDeque<f64>,
    /// Account-level rate limits (Claude, Codex, etc.)
    pub rate_limits: Vec<RateLimitInfo>,
    /// Per-session previous token totals, keyed by (agent_cli, session_id).
    prev_tokens: HashMap<(String, String), u64>,
    /// Rate limit poll counter (read every 5 ticks = 10s)
    rate_limit_counter: u32,
    collector: MultiCollector,
    /// Cached LLM-generated summaries, keyed by session_id.
    pub summaries: HashMap<String, String>,
    /// Session IDs currently being summarized.
    pending_summaries: HashSet<String>,
    /// Per-session retry count for failed summary attempts.
    summary_retries: HashMap<String, u32>,
    /// Channel to receive completed summaries from background threads.
    /// Tuple: (session_id, prompt, maybe_summary).
    summary_rx: mpsc::Receiver<(String, String, Option<String>)>,
    summary_tx: mpsc::Sender<(String, String, Option<String>)>,
    /// Ports left open by processes whose parent sessions have ended.
    pub orphan_ports: Vec<OrphanPort>,
    /// Transient status message shown in the footer (auto-clears after 3s).
    pub status_msg: Option<(String, Instant)>,
    /// Kill confirmation: (session ID, PID, timestamp). Expires after 2s.
    kill_confirm: Option<(String, u32, Instant)>,
    pub theme: Theme,
    pub show_quota: bool,
    pub show_sessions: bool,
    pub narrow_tab: NarrowTab,
    pub active_narrow_section: Option<NarrowSection>,
    pub maximized_narrow_section: Option<NarrowSection>,
    /// MCP servers detected on the most recent tick (sourced from
    /// MultiCollector).
    pub mcp_servers: Vec<McpServer>,
    /// When true (default), mcp-server-owned rollouts are hidden from
    /// the sessions panel. Toggle with Shift+M.
    pub mcp_suppress_sessions: bool,
    pub tree_view: bool,
    pub filter_text: String,
    pub filter_active: bool,
    pub show_timeline: bool,
    pub timeline_scroll: usize,
    /// Scroll position of the selected session's chat history.
    chat_scroll: ChatScroll,
    /// Geometry of the chat view in the last frame, set on each render. Zero
    /// when no chat view is on screen.
    pub(crate) chat_view: Cell<ChatViewMetrics>,
    /// Show tool calls and results in the chat view (default on). Kept across
    /// session switches; toggled with `d`.
    pub show_chat_tools: bool,
    pub show_file_audit: bool,
    /// Host vitals sampler (CPU% delta needs prior snapshot).
    host_sampler: HostSampler,
    /// Latest host metrics snapshot (None until first valid sample).
    pub host_metrics: Option<HostMetrics>,
    /// Aggregate metrics across all sessions (recomputed each tick).
    pub agent_aggregate: AgentAggregate,
    /// Help overlay (`?`) visibility.
    pub help_open: bool,
    /// View leader overlay (`v`) visibility.
    pub view_open: bool,
}

impl App {
    #[cfg(test)]
    pub fn new_with_config(
        theme: Theme,
        hidden_agents: &[String],
        panels: crate::config::PanelVisibility,
    ) -> Self {
        Self::new_with_config_and_claude_dirs(theme, hidden_agents, panels, &[])
    }

    pub fn new_with_config_and_claude_dirs(
        theme: Theme,
        hidden_agents: &[String],
        panels: crate::config::PanelVisibility,
        claude_config_dirs: &[PathBuf],
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let summaries = load_summary_cache();
        let mut collector =
            MultiCollector::with_hidden_and_claude_config_dirs(hidden_agents, claude_config_dirs);
        collector.set_mcp_suppress(true);
        let mut workspace = crate::workspace::Workspace::default();
        workspace.claude_dirs = claude_config_dirs.to_vec();
        Self {
            workspace,
            sessions: Vec::new(),
            selected: 0,
            should_quit: false,
            token_rates: VecDeque::with_capacity(GRAPH_HISTORY_LEN),
            rate_limits: Vec::new(),
            prev_tokens: HashMap::new(),
            rate_limit_counter: 5,
            collector,
            summaries,
            pending_summaries: HashSet::new(),
            summary_retries: HashMap::new(),
            summary_rx: rx,
            summary_tx: tx,
            orphan_ports: Vec::new(),
            status_msg: None,
            kill_confirm: None,
            theme,
            show_quota: panels.quota,
            show_sessions: panels.sessions,
            narrow_tab: NarrowTab::Work,
            active_narrow_section: Some(NarrowSection::Sessions),
            maximized_narrow_section: None,
            mcp_servers: Vec::new(),
            mcp_suppress_sessions: true,
            tree_view: false,
            filter_text: String::new(),
            filter_active: false,
            show_timeline: false,
            timeline_scroll: 0,
            chat_scroll: ChatScroll::default(),
            chat_view: Cell::new(ChatViewMetrics::default()),
            show_chat_tools: true,
            show_file_audit: false,
            host_sampler: HostSampler::new(),
            host_metrics: None,
            agent_aggregate: AgentAggregate::default(),
            help_open: false,
            view_open: false,
        }
    }

    pub(crate) fn collector_config_dirs(&self) -> Vec<PathBuf> {
        self.collector.all_config_dirs()
    }

    pub fn toggle_help(&mut self) {
        self.help_open = !self.help_open;
        if self.help_open {
            self.view_open = false;
        }
    }

    pub fn toggle_view_menu(&mut self) {
        self.view_open = !self.view_open;
        if self.view_open {
            self.help_open = false;
        }
    }

    pub fn toggle_panel(&mut self, panel: u8) {
        match panel {
            1 => self.show_quota = !self.show_quota,
            2 => self.show_sessions = !self.show_sessions,
            _ => return,
        }
        self.persist_panel_visibility();
        self.clamp_narrow_tab();
    }

    /// Toggle whether mcp-server-owned rollouts are hidden from the
    /// sessions panel. Default is on; turning it off restores upstream
    /// behavior so the user can see exactly what mcp-server fd holding
    /// produces (mostly stale "Done" rows).
    pub fn toggle_mcp_session_suppression(&mut self) {
        self.mcp_suppress_sessions = !self.mcp_suppress_sessions;
        let label = if self.mcp_suppress_sessions {
            "on"
        } else {
            "off"
        };
        self.set_status(format!("mcp session suppression: {}", label));
    }

    fn persist_panel_visibility(&mut self) {
        let panels = crate::config::PanelVisibility {
            quota: self.show_quota,
            sessions: self.show_sessions,
        };
        if let Err(e) = crate::config::save_panel_visibility(&panels) {
            self.set_status(format!("panels save failed: {}", e));
        }
    }

    pub fn toggle_file_audit(&mut self) {
        self.show_file_audit = !self.show_file_audit;
    }

    pub fn narrow_tab_visible(&self, tab: NarrowTab) -> bool {
        match tab {
            NarrowTab::Work => self.show_sessions,
            NarrowTab::Usage => self.show_quota,
        }
    }

    pub fn visible_narrow_tabs(&self) -> Vec<NarrowTab> {
        NarrowTab::ALL
            .into_iter()
            .filter(|&tab| self.narrow_tab_visible(tab))
            .collect()
    }

    pub fn active_narrow_tab(&self) -> Option<NarrowTab> {
        if self.narrow_tab_visible(self.narrow_tab) {
            Some(self.narrow_tab)
        } else {
            NarrowTab::ALL
                .into_iter()
                .find(|&tab| self.narrow_tab_visible(tab))
        }
    }

    pub fn set_narrow_tab(&mut self, tab: NarrowTab) {
        if self.narrow_tab_visible(tab) {
            self.narrow_tab = tab;
            self.clamp_narrow_section();
        }
    }

    pub fn select_next_narrow_tab(&mut self) {
        let tabs = self.visible_narrow_tabs();
        if tabs.is_empty() {
            return;
        }
        let current = self.active_narrow_tab().unwrap_or(tabs[0]);
        let pos = tabs.iter().position(|&tab| tab == current).unwrap_or(0);
        self.narrow_tab = tabs[(pos + 1) % tabs.len()];
        self.clamp_narrow_section();
    }

    pub fn select_prev_narrow_tab(&mut self) {
        let tabs = self.visible_narrow_tabs();
        if tabs.is_empty() {
            return;
        }
        let current = self.active_narrow_tab().unwrap_or(tabs[0]);
        let pos = tabs.iter().position(|&tab| tab == current).unwrap_or(0);
        self.narrow_tab = tabs[(pos + tabs.len() - 1) % tabs.len()];
        self.clamp_narrow_section();
    }

    fn clamp_narrow_tab(&mut self) {
        if let Some(tab) = self.active_narrow_tab() {
            self.narrow_tab = tab;
        }
        self.clamp_narrow_section();
    }

    pub fn narrow_section_visible(&self, section: NarrowSection) -> bool {
        match section {
            NarrowSection::Sessions => self.show_sessions,
            NarrowSection::Quota => self.show_quota,
        }
    }

    pub fn visible_narrow_sections(&self, tab: NarrowTab) -> Vec<NarrowSection> {
        let sections: &[NarrowSection] = match tab {
            NarrowTab::Work => &[NarrowSection::Sessions],
            NarrowTab::Usage => &[NarrowSection::Quota],
        };
        sections
            .iter()
            .copied()
            .filter(|&section| self.narrow_section_visible(section))
            .collect()
    }

    pub fn active_narrow_section(&self) -> Option<NarrowSection> {
        let tab = self.active_narrow_tab()?;
        if let Some(section) = self.active_narrow_section {
            if section.tab() == tab && self.narrow_section_visible(section) {
                return Some(section);
            }
        }
        self.visible_narrow_sections(tab).into_iter().next()
    }

    pub fn set_active_narrow_section(&mut self, section: NarrowSection) {
        if self.narrow_section_visible(section) {
            self.narrow_tab = section.tab();
            self.active_narrow_section = Some(section);
            self.clamp_narrow_section();
        }
    }

    pub fn maximized_narrow_section(&self) -> Option<NarrowSection> {
        let section = self.maximized_narrow_section?;
        if self.active_narrow_tab() == Some(section.tab()) && self.narrow_section_visible(section) {
            Some(section)
        } else {
            None
        }
    }

    pub fn toggle_narrow_section_zoom(&mut self, section: NarrowSection) {
        if !self.narrow_section_visible(section) {
            return;
        }
        self.set_active_narrow_section(section);
        self.maximized_narrow_section = if self.maximized_narrow_section() == Some(section) {
            None
        } else {
            Some(section)
        };
    }

    pub fn maximize_active_narrow_section(&mut self) {
        if let Some(section) = self.active_narrow_section() {
            self.maximized_narrow_section = Some(section);
        }
    }

    pub fn restore_narrow_sections(&mut self) {
        self.maximized_narrow_section = None;
    }

    fn clamp_narrow_section(&mut self) {
        self.active_narrow_section = self.active_narrow_section();
        if self.maximized_narrow_section().is_none() {
            self.maximized_narrow_section = None;
        }
    }

    pub fn toggle_timeline(&mut self) {
        self.show_timeline = !self.show_timeline;
        self.timeline_scroll = 0;
    }

    /// Chat items of `session` that the chat view lists: everything, or only
    /// user/assistant messages while tool details are off.
    pub(crate) fn chat_items<'a>(&self, session: &'a AgentSession) -> Cow<'a, [ChatMessage]> {
        if self.show_chat_tools {
            Cow::Borrowed(&session.chat_messages)
        } else {
            Cow::Owned(
                session
                    .chat_messages
                    .iter()
                    .filter(|m| m.role.is_dialogue())
                    .cloned()
                    .collect(),
            )
        }
    }

    /// Show or hide tool calls and results in the chat view. The layout
    /// changes, so the view goes back to following the newest item.
    pub fn toggle_chat_tools(&mut self) {
        self.show_chat_tools = !self.show_chat_tools;
        self.reset_chat_scroll();
        let state = if self.show_chat_tools { "on" } else { "off" };
        self.set_status(format!("chat tool details: {state}"));
    }

    /// True when the chat view was drawn in the last frame, so the chat scroll
    /// keys have something to move.
    pub(crate) fn chat_view_active(&self) -> bool {
        self.chat_view.get().visible_rows > 0
    }

    /// Row of the chat viewport top: the anchored row while the user has
    /// scrolled up, otherwise the bottom so the newest message stays visible.
    pub(crate) fn chat_offset(&self, layout: &ChatLayout, visible_rows: usize) -> usize {
        let max = layout.total_rows().saturating_sub(visible_rows);
        let applies = match (&self.chat_scroll.session, self.sessions.get(self.selected)) {
            (Some((cli, id)), Some(session)) => {
                *cli == session.agent_cli && *id == session.session_id
            }
            _ => false,
        };
        match &self.chat_scroll.anchor {
            Some(anchor) if applies => layout.resolve(anchor).min(max),
            _ => max,
        }
    }

    /// Follow the newest message again, e.g. after the selection moved.
    pub fn reset_chat_scroll(&mut self) {
        self.chat_scroll = ChatScroll::default();
    }

    pub fn scroll_chat_up(&mut self) {
        self.scroll_chat(true);
    }

    pub fn scroll_chat_down(&mut self) {
        self.scroll_chat(false);
    }

    /// Jump to the newest message and resume following new ones.
    pub fn scroll_chat_to_end(&mut self) {
        self.reset_chat_scroll();
    }

    /// Move the chat history by one page, keeping one row of overlap. Reaching
    /// the bottom resumes following new messages; anywhere above it the
    /// position stays on the same message when new ones arrive.
    fn scroll_chat(&mut self, up: bool) {
        let metrics = self.chat_view.get();
        if metrics.visible_rows == 0 || metrics.text_width == 0 {
            return;
        }
        let Some(session) = self.sessions.get(self.selected) else {
            return;
        };
        let layout = ChatLayout::new(&self.chat_items(session), metrics.text_width);
        let max = layout.total_rows().saturating_sub(metrics.visible_rows);
        if max == 0 {
            return;
        }
        let step = metrics.visible_rows.saturating_sub(1).max(1);
        let offset = self.chat_offset(&layout, metrics.visible_rows);
        let target = if up {
            offset.saturating_sub(step)
        } else {
            (offset + step).min(max)
        };
        self.chat_scroll = if target >= max {
            ChatScroll::default()
        } else {
            ChatScroll {
                session: Some((session.agent_cli.to_string(), session.session_id.clone())),
                anchor: layout.anchor_at(target),
            }
        };
    }

    pub fn cycle_theme(&mut self) {
        let names = crate::theme::THEME_NAMES;
        let current = names
            .iter()
            .position(|&n| n == self.theme.name)
            .unwrap_or(0);
        let next = (current + 1) % names.len();
        self.theme = Theme::by_name(names[next]).unwrap_or_default();
        if let Err(e) = crate::config::save_theme(names[next]) {
            self.set_status(format!("theme: {} (save failed: {})", names[next], e));
        } else {
            self.set_status(format!("theme: {}", names[next]));
        }
    }

    /// Set a transient status message that auto-clears after 3 seconds.
    pub fn set_status(&mut self, msg: String) {
        self.status_msg = Some((msg, Instant::now()));
    }

    /// Full refresh used by `--once`: collect monitored data, then generate and
    /// retry session summaries. Equivalent to [`App::tick_no_summaries`] followed
    /// by [`App::drain_and_retry_summaries`].
    pub fn tick(&mut self) {
        self.tick_no_summaries();
        self.drain_and_retry_summaries();
    }

    /// Refresh all monitored data WITHOUT spawning background summary jobs.
    ///
    /// `tick` additionally calls [`App::drain_and_retry_summaries`], which
    /// shells out to `claude --print` to generate session titles. Headless
    /// consumers (e.g. the web snapshot API) call this variant so they never
    /// spawn subprocesses or consume the user's Claude quota.
    pub fn tick_no_summaries(&mut self) {
        self.collector.set_mcp_suppress(self.mcp_suppress_sessions);
        // Git stats only fed the removed projects panel.
        self.collector.set_collect_git(false);
        self.sessions = self.collector.collect();
        self.orphan_ports = self.collector.orphan_ports.clone();
        self.mcp_servers = self.collector.mcp_servers.clone();
        self.host_metrics = self.host_sampler.sample();
        self.agent_aggregate = AgentAggregate::from_sessions(&self.sessions);
        if self.selected >= self.sessions.len() && !self.sessions.is_empty() {
            self.selected = self.sessions.len() - 1;
        }
        self.clamp_selection_to_visible();

        // Compute rate as sum of per-session deltas (stable across session churn).
        // Update prev_tokens in place; stale entries are harmless (bounded by
        // total unique sessions ever seen) and keeping them avoids false spikes
        // when a session transiently disappears from one poll.
        let mut rate: f64 = 0.0;
        for s in &self.sessions {
            let key = (s.agent_cli.to_string(), s.session_id.clone());
            let total = s.active_tokens();
            let prev = self.prev_tokens.get(&key).copied().unwrap_or(total);
            rate += total.saturating_sub(prev) as f64;
            self.prev_tokens.insert(key, total);
        }

        self.token_rates.push_back(rate);
        if self.token_rates.len() > GRAPH_HISTORY_LEN {
            self.token_rates.pop_front();
        }

        // Poll rate limits: first tick immediately, then every 5 ticks ≈ 10s
        if self.rate_limits.is_empty() || self.rate_limit_counter >= 5 {
            self.rate_limit_counter = 0;
            let extra_dirs = self.collector.all_config_dirs();
            self.rate_limits = read_rate_limits(&extra_dirs);
            // Merge live rate limits from agent collectors (e.g. Codex JSONL parsing)
            self.rate_limits.extend(self.collector.agent_rate_limits());
        } else {
            self.rate_limit_counter += 1;
        }
    }

    /// Drain completed summary results and spawn retries. Does NOT recollect
    /// sessions, so it is safe for `--once` mode (stable snapshot).
    pub fn drain_and_retry_summaries(&mut self) {
        while let Ok((sid, prompt, maybe_summary)) = self.summary_rx.try_recv() {
            self.pending_summaries.remove(&sid);
            match maybe_summary {
                Some(summary) => {
                    self.summary_retries.remove(&sid);
                    self.summaries.insert(sid, summary);
                    save_summary_cache(&self.summaries);
                }
                None => {
                    let count = self.summary_retries.entry(sid.clone()).or_insert(0);
                    *count += 1;
                    if *count >= MAX_SUMMARY_RETRIES {
                        // Exhausted — store sanitized fallback using prompt from worker
                        self.summaries.insert(sid, sanitize_fallback(&prompt, 80));
                        save_summary_cache(&self.summaries);
                    }
                }
            }
        }

        // Spawn summary jobs for sessions that need one
        for s in &self.sessions {
            let retries = self
                .summary_retries
                .get(&s.session_id)
                .copied()
                .unwrap_or(0);
            let has_input = !s.initial_prompt.is_empty() || !s.first_assistant_text.is_empty();
            if has_input
                && !self.summaries.contains_key(&s.session_id)
                && !self.pending_summaries.contains(&s.session_id)
                && self.pending_summaries.len() < MAX_SUMMARY_JOBS
                && retries < MAX_SUMMARY_RETRIES
            {
                self.pending_summaries.insert(s.session_id.clone());
                let sid = s.session_id.clone();
                let prompt = s.initial_prompt.clone();
                let assistant_text = s.first_assistant_text.clone();
                let tx = self.summary_tx.clone();
                std::thread::spawn(move || {
                    let result = generate_summary(&prompt, &assistant_text);
                    let fallback_text = if prompt.is_empty() {
                        assistant_text
                    } else {
                        prompt
                    };
                    let _ = tx.send((sid, fallback_text, result));
                });
            }
        }
    }

    pub fn has_pending_summaries(&self) -> bool {
        !self.pending_summaries.is_empty()
    }

    /// True if any session still qualifies for a summary retry.
    pub fn has_retryable_summaries(&self) -> bool {
        self.sessions.iter().any(|s| {
            (!s.initial_prompt.is_empty() || !s.first_assistant_text.is_empty())
                && !self.summaries.contains_key(&s.session_id)
                && !self.pending_summaries.contains(&s.session_id)
                && self
                    .summary_retries
                    .get(&s.session_id)
                    .copied()
                    .unwrap_or(0)
                    < MAX_SUMMARY_RETRIES
        })
    }

    /// Returns indices of sessions matching the current filter.
    pub fn visible_indices(&self) -> Vec<usize> {
        if self.filter_text.is_empty() {
            return (0..self.sessions.len()).collect();
        }
        let query = self.filter_text.to_lowercase();
        self.sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| Self::session_matches(s, &query))
            .map(|(i, _)| i)
            .collect()
    }

    fn session_matches(s: &AgentSession, query: &str) -> bool {
        s.project_name.to_lowercase().contains(query)
            || s.model.to_lowercase().contains(query)
            || s.session_id.to_lowercase().contains(query)
            || s.initial_prompt.to_lowercase().contains(query)
            || s.cwd.to_lowercase().contains(query)
            || format!("{:?}", s.status).to_lowercase().contains(query)
    }

    /// Ensure `selected` points to a session included in the current filter.
    /// No-op when no sessions match; otherwise snaps to the first visible.
    fn clamp_selection_to_visible(&mut self) {
        let visible = self.visible_indices();
        if visible.is_empty() {
            return;
        }
        if !visible.contains(&self.selected) {
            self.selected = visible[0];
        }
    }

    pub fn filter_push(&mut self, c: char) {
        self.filter_text.push(c);
        self.clamp_selection_to_visible();
    }

    pub fn filter_pop(&mut self) {
        self.filter_text.pop();
        self.clamp_selection_to_visible();
    }

    pub fn clear_filter(&mut self) {
        self.filter_active = false;
        self.filter_text.clear();
    }

    pub fn select_next(&mut self) {
        let visible = self.visible_indices();
        if visible.is_empty() {
            return;
        }
        if let Some(pos) = visible.iter().position(|&i| i == self.selected) {
            if pos + 1 < visible.len() {
                self.selected = visible[pos + 1];
            }
        } else {
            self.selected = visible[0];
        }
    }

    pub fn select_prev(&mut self) {
        let visible = self.visible_indices();
        if visible.is_empty() {
            return;
        }
        if let Some(pos) = visible.iter().position(|&i| i == self.selected) {
            if pos > 0 {
                self.selected = visible[pos - 1];
            }
        } else {
            self.selected = *visible.last().unwrap();
        }
    }

    pub fn select_session(&mut self, index: usize) {
        if index < self.sessions.len() && self.visible_indices().contains(&index) {
            self.selected = index;
        }
    }

    pub fn kill_selected(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        let session = &self.sessions[self.selected];
        if matches!(session.status, SessionStatus::Done) {
            return;
        }
        if session.pid == 0 || matches!(session.status, SessionStatus::Unknown) {
            self.set_status(
                "Could not uniquely match this session to an agent process".to_string(),
            );
            return;
        }

        // Check if we have a pending confirmation for this exact session
        if let Some((id, confirmed_pid, ts)) = self.kill_confirm.take() {
            if id == session.session_id
                && confirmed_pid == session.pid
                && ts.elapsed().as_secs() < 2
            {
                // Confirmed — verify PID still runs a killable agent before killing
                let pid = session.pid;
                let verified = std::process::Command::new("ps")
                    .args(["-p", &pid.to_string(), "-o", "command="])
                    .output()
                    .ok()
                    .map(|output| {
                        let cmd = String::from_utf8_lossy(&output.stdout).trim().to_string();
                        is_killable_agent_command(&cmd)
                    })
                    .unwrap_or(false);
                if !verified {
                    self.set_status(format!("PID {} is no longer a known agent process", pid));
                    return;
                }
                let result = std::process::Command::new("kill")
                    .args(["-9", &pid.to_string()])
                    .output();
                match result {
                    Ok(output) if output.status.success() => {}
                    Ok(output) => {
                        self.set_status(format!(
                            "Could not kill PID {}: {}",
                            pid,
                            String::from_utf8_lossy(&output.stderr).trim()
                        ));
                        return;
                    }
                    Err(error) => {
                        self.set_status(format!("Could not kill PID {}: {}", pid, error));
                        return;
                    }
                }
                self.tick_no_summaries();
                return;
            }
        }

        // First press — ask for confirmation
        let name = self
            .summaries
            .get(&session.session_id)
            .cloned()
            .unwrap_or_else(|| format!("PID {}", session.pid));
        self.kill_confirm = Some((session.session_id.clone(), session.pid, Instant::now()));
        self.set_status(format!("Press x again to kill: {}", name));
    }

    /// Kill all orphan port processes (Shift+X).
    /// Does a fresh port scan and validates PID identity + port ownership
    /// immediately before sending any signals to avoid PID reuse / stale cache issues.
    pub fn kill_orphan_ports(&mut self) {
        use crate::collector::process::get_listening_ports;

        // Fresh port scan right now — don't rely on cached data
        let fresh_ports = get_listening_ports();

        for orphan in &self.orphan_ports {
            // 1. Verify PID still listens on the expected port
            let still_listening = fresh_ports
                .get(&orphan.pid)
                .is_some_and(|ports| ports.contains(&orphan.port));
            if !still_listening {
                continue;
            }
            // 2. Verify PID still runs the expected command (full match, not substring)
            if let Ok(output) = std::process::Command::new("ps")
                .args(["-p", &orphan.pid.to_string(), "-o", "command="])
                .output()
            {
                let current_cmd = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if current_cmd == orphan.command {
                    let _ = std::process::Command::new("kill")
                        .args([&orphan.pid.to_string()])
                        .output();
                }
            }
        }
        // Re-collect to reflect changes
        self.tick();
    }

    pub fn quit(&mut self) {
        self.should_quit = true;
    }

    /// Jump to the terminal running the selected session's agent process.
    /// Delegates to the terminal-jumper registry (cmux / tmux / iTerm2);
    /// see [`crate::jump`]. No-op when nothing is selected; unmatched clients
    /// and unsupported terminals return an error for the status line.
    pub fn jump_to_session(&mut self) -> JumpOutcome {
        if self.sessions.is_empty() {
            return JumpOutcome::NoOp;
        }
        let session = &self.sessions[self.selected];
        let Some(target_pid) = session.jump_pid else {
            return JumpOutcome::Failed(
                "Could not uniquely match this session to a terminal process".to_string(),
            );
        };
        match crate::jump::run_jump(target_pid) {
            JumpOutcome::NoOp => JumpOutcome::Failed(
                "No supported terminal found for this session; use tmux, cmux, or iTerm2"
                    .to_string(),
            ),
            outcome => outcome,
        }
    }

    /// Get the display summary for a session: LLM summary > "..." if pending > raw prompt > "—"
    /// Done sessions skip pending state to avoid stuck "..." display.
    pub fn session_summary(&self, session: &AgentSession) -> String {
        if let Some(summary) = self.summaries.get(&session.session_id) {
            summary.clone()
        } else if matches!(session.status, SessionStatus::Done) {
            // Done sessions: don't wait for pending summary, show fallback immediately
            if !session.initial_prompt.is_empty() {
                sanitize_fallback(&session.initial_prompt, 80)
            } else if !session.first_assistant_text.is_empty() {
                sanitize_fallback(&session.first_assistant_text, 80)
            } else {
                "—".to_string()
            }
        } else if self.pending_summaries.contains(&session.session_id) {
            // Animate dots: . → .. → ... (cycles every ~1.5s at 2s tick)
            let dots = match (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                / 500)
                % 3
            {
                0 => ".",
                1 => "..",
                _ => "...",
            };
            dots.to_string()
        } else if !session.initial_prompt.is_empty() {
            sanitize_fallback(&session.initial_prompt, 80)
        } else if !session.first_assistant_text.is_empty() {
            sanitize_fallback(&session.first_assistant_text, 80)
        } else {
            "—".to_string()
        }
    }
}

/// Call `claude --print` via stdin pipe to summarize a prompt.
/// Returns `None` on timeout so the caller can retry later.
fn generate_summary(prompt: &str, assistant_text: &str) -> Option<String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    use std::time::Duration;

    // Build input from user prompt and/or first assistant response
    let user_part: String = prompt.chars().take(200).collect();
    let assistant_part: String = assistant_text.chars().take(200).collect();

    let context = if !user_part.is_empty() && !assistant_part.is_empty() {
        format!(
            "User message: {}\n\nAssistant response: {}",
            user_part, assistant_part
        )
    } else if !assistant_part.is_empty() {
        format!("Assistant response: {}", assistant_part)
    } else {
        format!("User message: {}", user_part)
    };

    let request = format!(
        "You are a conversation title generator. Given the conversation below, create a short title (3-5 words) that describes the session's main topic. Be specific and actionable. Do NOT output generic titles like 'New conversation' or 'Initial setup'. Output ONLY the title, no quotes, no explanation.\n\n{}",
        context
    );

    let mut child = match Command::new("claude")
        .args(["--print", "-"])
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return Some(sanitize_fallback(prompt, 80)),
    };

    // Write prompt via stdin (no shell injection)
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(request.as_bytes());
    }

    // Run wait_with_output in a helper thread so we can apply a bounded timeout.
    // This drains stdout internally, avoiding pipe-full deadlock.
    let child_pid = child.id();
    let (wo_tx, wo_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = wo_tx.send(child.wait_with_output());
    });

    let result = match wo_rx.recv_timeout(Duration::from_secs(10)) {
        Ok(r) => r,
        Err(_) => {
            // Timeout or disconnected — kill the child so the helper thread can exit.
            let _ = std::process::Command::new("kill")
                .args(["-9", &child_pid.to_string()])
                .status();
            return None;
        }
    };

    let fallback = sanitize_fallback(prompt, 80);

    match result {
        Ok(output) if output.status.success() => {
            let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let lower = raw.to_lowercase();
            // Reject empty, too long, generic, or prompt-echo outputs
            if raw.is_empty()
                || raw.chars().count() > 80
                || raw.contains("Summarize")
                || raw.starts_with("- ")
                || lower.contains("new conversation")
                || lower.contains("initial setup")
                || lower.contains("initial project")
                || lower.contains("initial conversation")
                || lower.starts_with("greeting")
            {
                Some(fallback)
            } else {
                Some(raw.trim_matches('"').trim_matches('\'').to_string())
            }
        }
        _ => Some(fallback),
    }
}

/// Cache directory: ~/.cache/abtop/
fn cache_dir() -> std::path::PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".cache"))
        .join("abtop")
}

fn cache_path() -> std::path::PathBuf {
    cache_dir().join("summaries.json")
}

fn load_summary_cache() -> HashMap<String, String> {
    let path = cache_path();
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let mut cache: HashMap<String, String> =
                serde_json::from_str(&content).unwrap_or_default();
            // Purge polluted or old truncated-fallback entries so they regenerate
            let before = cache.len();
            cache.retain(|_, v| !v.contains("You are a conversation tit") && !v.ends_with('…'));
            if cache.len() < before {
                // Persist cleaned cache
                let _ = std::fs::create_dir_all(cache_dir());
                let _ = std::fs::write(&path, serde_json::to_string(&cache).unwrap_or_default());
            }
            cache
        }
        Err(_) => HashMap::new(),
    }
}

fn save_summary_cache(summaries: &HashMap<String, String>) {
    let path = cache_path();
    let _ = std::fs::create_dir_all(cache_dir());
    if let Ok(json) = serde_json::to_string(summaries) {
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, &json).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

fn is_supported_agent_command(cmd: &str) -> bool {
    crate::collector::process::cmd_has_binary(cmd, "claude")
        || crate::collector::process::cmd_has_binary(cmd, "codex")
        || crate::collector::process::cmd_has_binary(cmd, "opencode")
}

fn is_killable_agent_command(cmd: &str) -> bool {
    is_supported_agent_command(cmd)
        && !(crate::collector::process::cmd_has_binary(cmd, "codex") && cmd.contains(" app-server"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_agent_command_accepts_opencode() {
        assert!(is_supported_agent_command("/usr/local/bin/claude"));
        assert!(is_supported_agent_command("codex --resume abc"));
        assert!(is_supported_agent_command("/opt/homebrew/bin/opencode"));
        assert!(!is_supported_agent_command("node server.js"));
    }

    #[test]
    fn killable_agent_command_rejects_codex_app_server() {
        assert!(is_killable_agent_command("codex --resume abc"));
        assert!(is_killable_agent_command("/usr/local/bin/claude"));
        assert!(!is_killable_agent_command(
            "/Applications/Codex.app/Contents/Resources/codex app-server --analytics-default-enabled"
        ));
    }
}
