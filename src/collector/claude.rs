use super::chat_tools::{self, ChatTools};
use super::process::{self, ProcInfo};
use crate::model::{
    trim_chat_window, AgentSession, ChatMessage, ChatRole, ChildProcess, FileAccess, FileOp,
    LaunchSurface, SessionFile, SessionStatus, SubAgent, MAX_CHAT_TEXT_CHARS, MAX_FILE_ACCESSES,
};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
#[cfg(all(
    not(target_os = "linux"),
    not(target_vendor = "apple"),
    not(target_os = "windows")
))]
use std::process::Command;

/// A single Claude config directory (sessions + projects + transcripts).
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
struct ConfigDir {
    sessions_dir: PathBuf,
    projects_dir: PathBuf,
}

impl ConfigDir {
    fn new(base: PathBuf) -> Self {
        Self {
            sessions_dir: base.join("sessions"),
            projects_dir: base.join("projects"),
        }
    }

    fn base_dir(&self) -> PathBuf {
        self.sessions_dir
            .parent()
            .unwrap_or(Path::new("."))
            .to_path_buf()
    }
}

#[derive(Debug, Default)]
struct ProcessOpenPaths {
    cwd: Option<PathBuf>,
    paths: Vec<PathBuf>,
}

pub struct ClaudeCollector {
    /// All known config directories to scan for sessions.
    config_dirs: Vec<ConfigDir>,
    /// User-configured Claude config directories from abtop config.
    configured_config_dirs: Vec<PathBuf>,
    /// Cached transcript parse results keyed by session_id.
    /// On each tick, only new bytes since `new_offset` are parsed.
    transcript_cache: HashMap<String, TranscriptResult>,
}

impl ClaudeCollector {
    #[cfg(test)]
    pub fn new() -> Self {
        Self::with_configured_dirs(Vec::new())
    }

    pub fn with_configured_dirs(configured_config_dirs: Vec<PathBuf>) -> Self {
        Self {
            config_dirs: Vec::new(),
            configured_config_dirs,
            transcript_cache: HashMap::new(),
        }
    }

    /// Discover unique Claude config directories from defaults, configured
    /// profile roots, home sibling profiles, and process environment/open-file
    /// signals when the platform exposes them.
    fn refresh_config_dirs(&mut self, process_info: &HashMap<u32, process::ProcInfo>) {
        // BTreeSet for deterministic iteration order across runs.
        let mut seen = std::collections::BTreeSet::new();

        // Always include the default directory
        let default = dirs::home_dir().unwrap_or_default().join(".claude");
        seen.insert(default);

        if let Some(home) = dirs::home_dir() {
            seen.extend(discover_home_claude_config_dirs(&home));
        }

        for dir in &self.configured_config_dirs {
            if is_claude_config_root(dir) {
                seen.insert(dir.clone());
            }
        }

        // Include CLAUDE_CONFIG_DIR from abtop's own environment
        if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
            let p = PathBuf::from(dir);
            if p.is_dir() {
                seen.insert(p);
            }
        }

        // Discover from running Claude processes via /proc/<pid>/environ
        for (pid, info) in process_info {
            if !process::cmd_has_binary(&info.command, "claude") {
                continue;
            }
            if let Some(dir) = read_env_var_from_proc(*pid, "CLAUDE_CONFIG_DIR") {
                let p = PathBuf::from(dir);
                if p.is_dir() {
                    seen.insert(p);
                }
            }
        }

        self.config_dirs = seen.into_iter().map(ConfigDir::new).collect();
    }

    fn collect_sessions(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        // Refresh config dirs on slow ticks only (every ~10s) or on first run.
        // Scanning /proc/<pid>/environ for every Claude process is expensive
        // to do every 2s; config dirs change rarely.
        if shared.slow_tick || self.config_dirs.is_empty() {
            self.refresh_config_dirs(&shared.process_info);
        }

        let self_pid = std::process::id();
        let active_session_paths =
            self.discover_active_session_paths(&shared.process_info, self_pid);
        let active_config_dirs: Vec<ConfigDir> = active_session_paths
            .iter()
            .map(|(_, config)| config.clone())
            .collect();
        self.merge_config_dirs(active_config_dirs);

        // Collect all session file paths first to avoid borrowing self
        // immutably (config_dirs) and mutably (load_session) at the same time.
        let mut session_paths: Vec<(PathBuf, ConfigDir)> = Vec::new();
        session_paths.extend(active_session_paths);

        for config in &self.config_dirs {
            let session_files = match fs::read_dir(&config.sessions_dir) {
                Ok(entries) => entries,
                Err(_) => continue,
            };

            for entry in session_files.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                session_paths.push((path, config.clone()));
            }
        }

        let discovery_ctx = build_discovery_context(&session_paths, &shared.process_info, self_pid);

        let mut sessions = self.load_session_paths(
            &session_paths,
            &shared.process_info,
            &shared.children_map,
            &shared.ports,
            &discovery_ctx,
        );

        self.evict_stale_cache(&sessions);

        sessions.sort_by_key(|s| std::cmp::Reverse(s.started_at));
        sessions
    }

    /// Drop `transcript_cache` entries for session_ids that are no longer
    /// in the active set. After `/clear`, the old sid leaves the active
    /// set and its cache entry (with stale token counters) is removed on
    /// the very next tick — without this, counters would persist forever.
    fn evict_stale_cache(&mut self, sessions: &[AgentSession]) {
        let active_ids: std::collections::HashSet<&str> =
            sessions.iter().map(|s| s.session_id.as_str()).collect();
        self.transcript_cache
            .retain(|sid, _| active_ids.contains(sid.as_str()));
    }

    fn load_session_paths(
        &mut self,
        session_paths: &[(PathBuf, ConfigDir)],
        process_info: &HashMap<u32, ProcInfo>,
        children_map: &HashMap<u32, Vec<u32>>,
        ports: &HashMap<u32, Vec<u16>>,
        ctx: &DiscoveryContext,
    ) -> Vec<AgentSession> {
        let mut sessions = Vec::new();
        let mut seen_ids = std::collections::HashSet::new();
        for (path, config) in session_paths {
            if let Some(session) =
                self.load_session(path, config, process_info, children_map, ports, ctx)
            {
                if seen_ids.insert(session.session_id.clone()) {
                    sessions.push(session);
                }
            }
        }
        sessions
    }

    fn merge_config_dirs(&mut self, dirs: Vec<ConfigDir>) {
        let mut seen: std::collections::BTreeSet<ConfigDir> =
            self.config_dirs.iter().cloned().collect();
        seen.extend(dirs);
        self.config_dirs = seen.into_iter().collect();
    }

    fn discover_active_session_paths(
        &self,
        process_info: &HashMap<u32, process::ProcInfo>,
        self_pid: u32,
    ) -> Vec<(PathBuf, ConfigDir)> {
        let pids = Self::find_claude_pids(process_info, self_pid);
        if pids.is_empty() {
            return Vec::new();
        }

        let open_paths = Self::map_pid_to_open_paths(&pids);
        Self::session_paths_from_open_paths(&pids, &open_paths)
    }

    fn session_paths_from_open_paths(
        pids: &[u32],
        open_paths: &HashMap<u32, ProcessOpenPaths>,
    ) -> Vec<(PathBuf, ConfigDir)> {
        let mut paths = Vec::new();
        let mut seen = std::collections::BTreeSet::new();

        for &pid in pids {
            let Some(info) = open_paths.get(&pid) else {
                continue;
            };
            for config in config_dirs_from_open_paths(info) {
                let Some(path) = find_session_file_for_pid(&config.sessions_dir, pid) else {
                    continue;
                };
                if seen.insert(path.clone()) {
                    paths.push((path, config));
                }
            }
        }

        paths
    }

    /// Collect PIDs of all live `claude` processes that are NOT descendants
    /// of abtop itself. Other users' non-interactive (`claude --print`)
    /// invocations are still surfaced — only abtop's own summary children
    /// are filtered out.
    fn find_claude_pids(process_info: &HashMap<u32, process::ProcInfo>, self_pid: u32) -> Vec<u32> {
        let mut pids = Vec::new();
        for (pid, info) in process_info {
            if !process::cmd_has_binary(&info.command, "claude") {
                continue;
            }
            if process::is_descendant_of(*pid, self_pid, process_info) {
                continue;
            }
            pids.push(*pid);
        }
        pids
    }

    /// Classify which surface launched a `claude` process, from the resolved
    /// executable path in its full command line (as reported by `ps`/
    /// `/proc/{pid}/cmdline`/sysinfo — see `process::ProcInfo::command`).
    ///
    /// - The Claude desktop app bundles its own Claude Code binary under a
    ///   per-user `Claude/claude-code/<version>/` directory (Electron's
    ///   userData layout: `~/Library/Application Support/Claude/...` on
    ///   macOS, `%APPDATA%\Claude\...` on Windows, `~/.config/Claude/...`
    ///   on Linux) — distinct from a plain PATH install.
    /// - Editor extensions (VS Code, Cursor, Windsurf, ...) vendor the
    ///   binary under `<editor-extensions-dir>/anthropic.claude-code-<ver>/`.
    /// - Anything else (homebrew/npm/native install, the auto-updater's
    ///   `claude/versions/<ver>` layout) is a plain CLI invocation.
    fn detect_launch_surface(cmd: &str) -> LaunchSurface {
        // Match against the whole command string rather than the first
        // whitespace-split token: unlike `cmd_has_binary`'s binary-name
        // check, these are fixed path fragments with no ambiguity, and the
        // desktop app's own path already contains an unquoted space
        // ("Application Support") on macOS that a naive first-token split
        // would cut through.
        let normalized = cmd.replace('\\', "/");
        if normalized.contains("/Claude/claude-code/") {
            LaunchSurface::App
        } else if normalized.contains("/extensions/anthropic.claude-code") {
            LaunchSurface::Ide
        } else {
            LaunchSurface::Cli
        }
    }

    fn map_pid_to_open_paths(pids: &[u32]) -> HashMap<u32, ProcessOpenPaths> {
        if pids.is_empty() {
            return HashMap::new();
        }

        #[cfg(target_os = "linux")]
        {
            map_pid_to_proc_open_paths(pids)
        }

        #[cfg(target_vendor = "apple")]
        {
            map_pid_to_libproc_open_paths(pids)
        }

        #[cfg(target_os = "windows")]
        {
            map_pid_to_sysinfo_open_paths(pids)
        }

        #[cfg(all(
            not(target_os = "linux"),
            not(target_vendor = "apple"),
            not(target_os = "windows")
        ))]
        {
            map_pid_to_lsof_open_paths(pids)
        }
    }

    fn load_session(
        &mut self,
        path: &Path,
        config: &ConfigDir,
        process_info: &HashMap<u32, ProcInfo>,
        children_map: &HashMap<u32, Vec<u32>>,
        ports: &HashMap<u32, Vec<u16>>,
        ctx: &DiscoveryContext,
    ) -> Option<AgentSession> {
        let content = fs::read_to_string(path).ok()?;
        let mut sf: SessionFile = serde_json::from_str(&content).ok()?;
        sf.sanitize();
        if sf.is_helper_run() {
            return None;
        }

        // Resolve the project dir that actually holds this session's
        // transcripts. For worktree sessions the on-disk dir does not match
        // `encode_cwd_path(cwd)`, so locate it via the original sid first.
        let project_dir = resolve_project_dir(config, &sf.cwd, &sf.session_id);

        // `/clear` mints a new sessionId + transcript without rewriting
        // `sessions/{PID}.json`. Override with the most recent transcript in
        // the project dir so counters/status track the live session instead
        // of the stale one. (#68)
        //
        // Skip the override when multiple active claude PIDs share this
        // cwd: we can't tell which PID owns a freshly-created jsonl, and
        // picking the wrong one would cross-contaminate counters. Also
        // exclude sids already claimed by OTHER session files (a sibling
        // PID's transcript must not be adopted as this PID's live sid).
        let siblings = ctx.pids_per_cwd.get(&sf.cwd).copied().unwrap_or(1);
        if siblings <= 1 {
            let excluded: std::collections::HashSet<&str> = ctx
                .claimed_sids_by_pid
                .iter()
                .filter(|&(p, _)| *p != sf.pid)
                .map(|(_, s)| s.as_str())
                .collect();
            if let Some(live_sid) =
                find_live_session_id(project_dir.as_deref(), sf.started_at, &excluded)
            {
                if live_sid != sf.session_id {
                    sf.session_id = live_sid;
                }
            }
        }

        let proc_cmd = process_info.get(&sf.pid).map(|p| p.command.as_str());
        let pid_alive = proc_cmd
            .map(|c| process::cmd_has_binary(c, "claude"))
            .unwrap_or(false);
        let launch_surface = proc_cmd
            .map(Self::detect_launch_surface)
            .unwrap_or(LaunchSurface::Cli);

        // Skip sessions whose PID is a descendant of abtop itself —
        // those are the `claude --print` summary children spawned by
        // `generate_summary` in app.rs. User-launched non-interactive
        // sessions (`claude --print` in another shell) are NOT filtered.
        // Only checked while the process is alive (ppid visible); dead
        // sessions are cleaned up when the session file disappears.
        if process::is_descendant_of(sf.pid, ctx.self_pid, process_info) {
            return None;
        }

        let project_name = process::last_path_segment(&sf.cwd)
            .unwrap_or("?")
            .to_string();

        let proc = process_info.get(&sf.pid);
        let mem_mb = proc.map(|p| p.rss_kb / 1024).unwrap_or(0);

        // Use the already-resolved project_dir so a post-/clear sid lookup
        // lands in the same (possibly worktree) directory as the original.
        let transcript_path = project_dir.as_ref().and_then(|pd| {
            let p = pd.join(format!("{}.jsonl", sf.session_id));
            if p.exists() && !is_symlink(&p) {
                Some(p)
            } else {
                None
            }
        });

        if let Some(ref tp) = transcript_path {
            let cached = self.transcript_cache.remove(&sf.session_id);
            // Detect file replacement: if the file identity changed, reparse from scratch.
            let identity_changed = cached
                .as_ref()
                .map(|c| c.file_identity != file_identity(tp))
                .unwrap_or(false);
            let from_offset = if identity_changed {
                0
            } else {
                cached.as_ref().map(|c| c.new_offset).unwrap_or(0)
            };

            let (initial_context_tokens, initial_cache_read) = if from_offset > 0 {
                cached
                    .as_ref()
                    .map(|c| (c.last_context_tokens, c.prev_cache_read))
                    .unwrap_or((0, 0))
            } else {
                (0, 0)
            };
            let delta = parse_transcript_with_state(
                tp,
                from_offset,
                initial_context_tokens,
                initial_cache_read,
                cached.as_ref().map(|c| &c.pending_question_ids),
                cached.as_ref().map(|c| &c.chat_tools),
            );

            if let Some(mut prev) = cached {
                // File replaced, shrank, or first parse — replace entirely
                if identity_changed || from_offset == 0 || delta.new_offset < from_offset {
                    self.transcript_cache.insert(sf.session_id.clone(), delta);
                } else {
                    // Merge delta into cached result
                    if delta.model != "-" {
                        prev.model = delta.model;
                    }
                    if !delta.session_name.is_empty() {
                        prev.session_name = delta.session_name;
                    }
                    prev.total_input += delta.total_input;
                    prev.total_output += delta.total_output;
                    prev.total_cache_read += delta.total_cache_read;
                    prev.total_cache_create += delta.total_cache_create;
                    if delta.last_context_tokens > 0 {
                        prev.last_context_tokens = delta.last_context_tokens;
                    }
                    if delta.max_context_tokens > prev.max_context_tokens {
                        prev.max_context_tokens = delta.max_context_tokens;
                    }
                    if delta.last_context_tokens > 0 {
                        prev.prev_cache_read = delta.prev_cache_read;
                    }
                    prev.compaction_count += delta.compaction_count;
                    prev.turn_count += delta.turn_count;
                    // Always update current_task from delta — empty means
                    // latest assistant turn had no tool_use (task cleared)
                    if delta.turn_count > 0 {
                        prev.current_task = delta.current_task;
                    }
                    if !delta.version.is_empty() {
                        prev.version = delta.version;
                    }
                    if !delta.git_branch.is_empty() {
                        prev.git_branch = delta.git_branch;
                    }
                    if delta.last_activity > prev.last_activity {
                        prev.last_activity = delta.last_activity;
                    }
                    prev.token_history.extend(delta.token_history);
                    if prev.tool_calls.len() < 500 {
                        let remaining = 500 - prev.tool_calls.len();
                        prev.tool_calls
                            .extend(delta.tool_calls.into_iter().take(remaining));
                    }
                    prev.chat_messages.extend(delta.chat_messages);
                    trim_chat_window(&mut prev.chat_messages);
                    // The delta started from the cached tool ids, so it
                    // already holds the full, bounded set.
                    prev.chat_tools = delta.chat_tools;
                    // Only overwrite turn-state when the delta actually
                    // observed new user/assistant lines. A no-op tick (file
                    // didn't grow) returns an empty delta whose zeroed
                    // timestamps would otherwise wipe the live markers and
                    // break the timeline animation between polls.
                    if delta.saw_turn {
                        prev.last_assistant_ts_ms = delta.last_assistant_ts_ms;
                        prev.last_user_ts_ms = delta.last_user_ts_ms;
                    }
                    if prev.initial_prompt.is_empty() && !delta.initial_prompt.is_empty() {
                        prev.initial_prompt = delta.initial_prompt;
                    }
                    prev.file_accesses.extend(delta.file_accesses);
                    // Sliding window: drop OLDEST entries past the cap so the
                    // cache always holds the most recent MAX_FILE_ACCESSES.
                    // `truncate` would have kept the oldest and silently
                    // dropped every new access once the cache hit the cap.
                    let len = prev.file_accesses.len();
                    if len > MAX_FILE_ACCESSES {
                        prev.file_accesses.drain(..len - MAX_FILE_ACCESSES);
                    }
                    prev.pending_question_ids = delta.pending_question_ids;
                    prev.tool_events.extend(delta.tool_events);
                    let len = prev.tool_events.len();
                    if len > MAX_TOOL_EVENTS {
                        prev.tool_events.drain(..len - MAX_TOOL_EVENTS);
                    }
                    prev.new_offset = delta.new_offset;
                    self.transcript_cache.insert(sf.session_id.clone(), prev);
                }
            } else {
                // First parse — store full result
                self.transcript_cache.insert(sf.session_id.clone(), delta);
            }
        }

        let empty_result = TranscriptResult {
            model: "-".to_string(),
            total_input: 0,
            total_output: 0,
            total_cache_read: 0,
            total_cache_create: 0,
            last_context_tokens: 0,
            max_context_tokens: 0,
            prev_cache_read: 0,
            context_history: Vec::new(),
            compaction_count: 0,
            turn_count: 0,
            current_task: String::new(),
            version: String::new(),
            git_branch: String::new(),
            last_activity: std::time::UNIX_EPOCH,
            new_offset: 0,
            file_identity: (0, 0),
            token_history: Vec::new(),
            initial_prompt: String::new(),
            session_name: String::new(),
            first_assistant_text: String::new(),
            chat_messages: Vec::new(),
            tool_calls: Vec::new(),
            last_assistant_ts_ms: 0,
            last_user_ts_ms: 0,
            saw_turn: false,
            pending_question_ids: HashSet::new(),
            chat_tools: Vec::new(),
            tool_events: Vec::new(),
            file_accesses: Vec::new(),
        };
        let cached = self
            .transcript_cache
            .get(&sf.session_id)
            .unwrap_or(&empty_result);

        let model = cached.model.clone();
        let total_input = cached.total_input;
        let total_output = cached.total_output;
        let total_cache_read = cached.total_cache_read;
        let total_cache_create = cached.total_cache_create;
        let last_context_tokens = cached.last_context_tokens;
        let max_context_tokens = cached.max_context_tokens;
        let turn_count = cached.turn_count;
        let current_task = cached.current_task.clone();
        let version = cached.version.clone();
        let git_branch = cached.git_branch.clone();
        let token_history = cached.token_history.clone();
        let context_history = cached.context_history.clone();
        let compaction_count = cached.compaction_count;
        let initial_prompt = cached.initial_prompt.clone();
        let first_assistant_text = cached.first_assistant_text.clone();
        let chat_messages = cached.chat_messages.clone();
        let tool_calls = cached.tool_calls.clone();
        let file_accesses = cached.file_accesses.clone();

        if !pid_alive {
            return None;
        }

        // Derive the project directory from the transcript path (handles worktree sessions),
        // falling back to the encoded cwd.
        let project_dir = transcript_path
            .as_ref()
            .and_then(|tp| tp.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| config.projects_dir.join(encode_cwd_path(&sf.cwd)));

        // Collect subagents before deriving the parent status so asynchronous
        // Agent work keeps the parent active after its tool_result has returned.
        let subagents_dir = project_dir.join(&sf.session_id).join("subagents");
        let subagents = Self::collect_subagents(&subagents_dir);
        let has_working_subagent = subagents.iter().any(|agent| agent.status == "working");

        // Status is best-effort. Signals we trust:
        //   1. Active descendant CPU → tool is running.
        //   2. current_task non-empty → latest assistant turn left a
        //      tool_use unanswered. Catches I/O-bound tools (Read, Edit)
        //      whose descendants stay under 5% CPU, so the CPU heuristic
        //      alone would flicker to Waiting while the tool runs.
        //   3. A working subagent → an async Agent call is still running even
        //      though its tool_result has already returned to the parent.
        //   4. last_user_ts_ms > 0 → trailing transcript line is a real
        //      user prompt with no assistant reply yet, so the model is
        //      generating. tool_result wrappers are skipped at the
        //      parser level so this only fires for actual prompts.
        //
        // We drop the mtime freshness gate intentionally: Claude Code
        // writes the assistant turn atomically when it lands, so during
        // a long streamed reply the file isn't touched and mtime would
        // go stale. Without the gate the status now matches the live
        // "Think" row in the timeline (both keyed off last_user_ts_ms),
        // and an idle session can't get stuck Thinking because the
        // tool_result skip means last_user only flips on real prompts.
        // Permission prompts come from the hook marker, not from the transcript
        // timestamps: the marker may exist without a matching tool_use. A denied
        // or interrupted prompt has no completion hook, so a leftover marker is
        // closed by later transcript tool_result events instead (see
        // `permission_claim_open`); it must not keep the session in Reply.
        let waiting_for_permission = read_permission_claims(config, &sf.session_id, sf.started_at)
            .iter()
            .any(|claim| permission_claim_open(claim, &cached.tool_events));
        let waiting_for_question = !cached.pending_question_ids.is_empty();
        let status = {
            let has_active_descendant =
                process::has_active_descendant(sf.pid, children_map, process_info, 5.0);
            // Non-empty current_task = latest assistant turn left a tool_use
            // unanswered. Catches fast tools (`Bash rm ...`) that finish
            // between CPU samples, so has_active_descendant alone misses them.
            let pending_tool = !cached.current_task.is_empty();
            let model_generating = cached.last_user_ts_ms > 0;
            if waiting_for_permission || waiting_for_question {
                SessionStatus::AwaitingInput
            } else if has_active_descendant || pending_tool || has_working_subagent {
                SessionStatus::Executing
            } else if model_generating {
                SessionStatus::Thinking
            } else {
                SessionStatus::Waiting
            }
        };

        let configured_model = read_configured_model(&sf.cwd);
        let context_window = crate::collector::context_window_for_model(
            &model,
            &configured_model,
            max_context_tokens,
        );
        let context_percent = if context_window > 0 {
            (last_context_tokens as f64 / context_window as f64) * 100.0
        } else {
            0.0
        };

        let current_tasks = if waiting_for_permission {
            vec!["waiting for permission".to_string()]
        } else if waiting_for_question {
            vec!["waiting for answer".to_string()]
        } else if !current_task.is_empty() {
            vec![current_task]
        } else if !pid_alive {
            vec!["finished".to_string()]
        } else if matches!(status, SessionStatus::Waiting) {
            vec!["waiting for input".to_string()]
        } else {
            vec!["thinking...".to_string()]
        };

        let mut children = Vec::new();
        // Collect all descendants (not just direct children) so we catch
        // grandchild processes that listen on ports (e.g. Claude → shell → node).
        let mut stack: Vec<u32> = children_map.get(&sf.pid).cloned().unwrap_or_default();
        let mut visited = std::collections::HashSet::new();
        while let Some(cpid) = stack.pop() {
            if !visited.insert(cpid) {
                continue;
            }
            if let Some(cproc) = process_info.get(&cpid) {
                let port = ports.get(&cpid).and_then(|v| v.first().copied());
                children.push(ChildProcess {
                    pid: cpid,
                    command: cproc.command.clone(),
                    mem_kb: cproc.rss_kb,
                    port,
                });
            }
            // Add grandchildren to the stack
            if let Some(grandchildren) = children_map.get(&cpid) {
                stack.extend(grandchildren);
            }
        }

        // Git stats: populated by MultiCollector on slow ticks
        let (git_added, git_modified) = (0, 0);

        // Memory status
        let memory_dir = project_dir.join("memory");
        let (mem_file_count, mem_line_count) = Self::collect_memory_status(&memory_dir);

        // Effort level (persistent `effortLevel` from settings files).
        // Note: the `/effort` slash command is session-scoped and does NOT persist,
        // so this only reflects settings.json — not live in-session overrides.
        let effort = read_effort_level(&sf.cwd);

        Some(AgentSession {
            agent_cli: "claude",
            launch_surface,
            pid: sf.pid,
            jump_pid: Some(sf.pid),
            session_id: sf.session_id,
            session_name: cached.session_name.clone(),
            cwd: sf.cwd,
            project_name,
            started_at: sf.started_at,
            status,
            model,
            effort,
            context_percent,
            total_input_tokens: total_input,
            total_output_tokens: total_output,
            total_cache_read,
            total_cache_create,
            turn_count,
            current_tasks,
            mem_mb,
            version,
            git_branch,
            git_added,
            git_modified,
            token_history,
            context_history,
            compaction_count,
            context_window,
            subagents,
            mem_file_count,
            mem_line_count,
            children,
            initial_prompt,
            first_assistant_text,
            chat_messages,
            tool_calls,
            pending_since_ms: cached.last_assistant_ts_ms,
            thinking_since_ms: cached.last_user_ts_ms,
            file_accesses,
            config_root: super::abbrev_path(&config.base_dir()),
        })
    }

    fn collect_subagents(subagents_dir: &Path) -> Vec<SubAgent> {
        let mut subagents = Vec::new();

        let entries = match fs::read_dir(subagents_dir) {
            Ok(e) => e,
            Err(_) => return subagents,
        };

        // Collect meta files and their corresponding jsonl files
        let mut meta_files: Vec<PathBuf> = Vec::new();
        for entry in entries.flatten() {
            if entry.file_type().map(|ft| ft.is_symlink()).unwrap_or(true) {
                continue;
            }
            let path = entry.path();
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.ends_with(".meta.json") {
                    meta_files.push(path);
                }
            }
        }

        for meta_path in meta_files {
            let meta_name = match meta_path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };

            // Parse meta JSON
            let meta_content = match fs::read_to_string(&meta_path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let meta_val: Value = match serde_json::from_str(&meta_content) {
                Ok(v) => v,
                Err(_) => continue,
            };

            let description = meta_val
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("agent")
                .to_string();

            // Derive jsonl path: agent-{hash}.meta.json -> agent-{hash}.jsonl
            let jsonl_name = meta_name.replace(".meta.json", ".jsonl");
            let jsonl_path = meta_path.with_file_name(&jsonl_name);

            let mut tokens = 0u64;
            let mut last_activity = std::time::UNIX_EPOCH;

            if jsonl_path.exists() {
                // Get file mtime for status
                if let Ok(metadata) = fs::metadata(&jsonl_path) {
                    if let Ok(mtime) = metadata.modified() {
                        last_activity = mtime;
                    }
                }

                // Parse jsonl for token totals
                let transcript = parse_transcript(&jsonl_path, 0);
                tokens = transcript.total_input
                    + transcript.total_output
                    + transcript.total_cache_read
                    + transcript.total_cache_create;
            }

            let status = {
                let since = std::time::SystemTime::now()
                    .duration_since(last_activity)
                    .unwrap_or_default();
                if since.as_secs() < 30 {
                    "working".to_string()
                } else {
                    "done".to_string()
                }
            };

            // Use description as name, shorten if needed
            let name = truncate(&description, 30);

            subagents.push(SubAgent {
                name,
                status,
                tokens,
            });
        }

        subagents
    }

    fn collect_memory_status(memory_dir: &Path) -> (u32, u32) {
        let mut file_count = 0u32;
        let mut line_count = 0u32;

        if let Ok(entries) = fs::read_dir(memory_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() {
                    file_count += 1;
                }
            }
        }

        let memory_md = memory_dir.join("MEMORY.md");
        if let Ok(content) = fs::read_to_string(&memory_md) {
            line_count = content.lines().count() as u32;
        }

        (file_count, line_count)
    }
}

impl Default for ClaudeCollector {
    fn default() -> Self {
        Self::with_configured_dirs(Vec::new())
    }
}

impl super::AgentCollector for ClaudeCollector {
    fn collect(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        self.collect_sessions(shared)
    }

    fn discovered_config_dirs(&self) -> Vec<PathBuf> {
        self.config_dirs.iter().map(ConfigDir::base_dir).collect()
    }
}

#[cfg(target_os = "linux")]
fn map_pid_to_proc_open_paths(pids: &[u32]) -> HashMap<u32, ProcessOpenPaths> {
    let mut map = HashMap::new();

    for &pid in pids {
        let cwd = fs::read_link(format!("/proc/{}/cwd", pid)).ok();
        let entries = match fs::read_dir(format!("/proc/{}/fd", pid)) {
            Ok(entries) => entries,
            Err(_) => {
                if cwd.is_some() {
                    map.insert(
                        pid,
                        ProcessOpenPaths {
                            cwd,
                            paths: Vec::new(),
                        },
                    );
                }
                continue;
            }
        };

        let paths = entries
            .flatten()
            .filter_map(|entry| fs::read_link(entry.path()).ok())
            .filter(|path| path.is_absolute())
            .collect();
        map.insert(pid, ProcessOpenPaths { cwd, paths });
    }

    map
}

#[cfg(target_vendor = "apple")]
fn map_pid_to_libproc_open_paths(pids: &[u32]) -> HashMap<u32, ProcessOpenPaths> {
    use proc_pidinfo::{
        proc_pidfdinfo, proc_pidinfo_list, Pid, ProcFDInfo, ProcFDType, VnodeFdInfoWithPath,
    };

    let mut map = HashMap::new();

    for &raw_pid in pids {
        let pid = Pid(raw_pid);
        let fds = match proc_pidinfo_list::<ProcFDInfo>(pid) {
            Ok(fds) => fds,
            Err(_) => continue,
        };

        let paths = fds
            .into_iter()
            .filter(|fd| fd.fd_type() == Ok(ProcFDType::VNODE))
            .filter_map(|fd| proc_pidfdinfo::<VnodeFdInfoWithPath>(pid, fd.proc_fd).ok())
            .flatten()
            .filter_map(|vnode| vnode.path().ok().map(PathBuf::from))
            .collect();

        map.insert(raw_pid, ProcessOpenPaths { cwd: None, paths });
    }

    map
}

#[cfg(target_os = "windows")]
fn map_pid_to_sysinfo_open_paths(pids: &[u32]) -> HashMap<u32, ProcessOpenPaths> {
    use std::sync::{Mutex, OnceLock};

    static SYS: OnceLock<Mutex<sysinfo::System>> = OnceLock::new();
    let sys_mutex = SYS.get_or_init(|| Mutex::new(sysinfo::System::new()));
    let mut sys = sys_mutex.lock().expect("open-paths system mutex poisoned");

    let pids_sys: Vec<sysinfo::Pid> = pids
        .iter()
        .copied()
        .map(|p| sysinfo::Pid::from(p as usize))
        .collect();
    sys.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::Some(&pids_sys),
        true,
        sysinfo::ProcessRefreshKind::new().with_memory(),
    );

    // sysinfo 0.32 exposes cwd but not open file descriptors, so the `paths`
    // fallback used by lsof/libproc on other platforms isn't available here.
    // Claude session discovery still works via cwd plus the session-file index.
    let mut map: HashMap<u32, ProcessOpenPaths> = HashMap::new();
    for &pid_u32 in pids {
        let pid = sysinfo::Pid::from(pid_u32 as usize);
        if let Some(proc_) = sys.process(pid) {
            let cwd = proc_.cwd().map(PathBuf::from);
            map.insert(pid_u32, ProcessOpenPaths { cwd, paths: vec![] });
        }
    }
    map
}

#[cfg(all(
    not(target_os = "linux"),
    not(target_vendor = "apple"),
    not(target_os = "windows")
))]
fn map_pid_to_lsof_open_paths(pids: &[u32]) -> HashMap<u32, ProcessOpenPaths> {
    let pid_args: Vec<String> = pids.iter().map(|p| format!("-p{}", p)).collect();
    let mut args = vec!["-F", "ftn"];
    for pa in &pid_args {
        args.push(pa);
    }

    let output = Command::new("lsof").args(&args).output().ok();
    output
        .map(|out| parse_lsof_process_info(&String::from_utf8_lossy(&out.stdout)))
        .unwrap_or_default()
}

#[cfg_attr(
    any(target_os = "linux", target_vendor = "apple", target_os = "windows"),
    allow(dead_code)
)]
fn parse_lsof_process_info(output: &str) -> HashMap<u32, ProcessOpenPaths> {
    let mut map: HashMap<u32, ProcessOpenPaths> = HashMap::new();
    let mut current_pid: Option<u32> = None;
    let mut current_fd = String::new();

    for line in output.lines() {
        if let Some(pid_str) = line.strip_prefix('p') {
            current_pid = pid_str.parse::<u32>().ok();
            if let Some(pid) = current_pid {
                map.entry(pid).or_default();
            }
            current_fd.clear();
        } else if let Some(fd) = line.strip_prefix('f') {
            current_fd = fd.to_string();
        } else if let Some(name) = line.strip_prefix('n') {
            let Some(pid) = current_pid else {
                continue;
            };
            if name.is_empty() || name.starts_with('[') {
                continue;
            }
            let path = PathBuf::from(name);
            let info = map.entry(pid).or_default();
            if current_fd == "cwd" {
                info.cwd = Some(path.clone());
            }
            info.paths.push(path);
        }
    }

    map
}

fn config_dirs_from_open_paths(info: &ProcessOpenPaths) -> Vec<ConfigDir> {
    let mut candidates = Vec::new();
    if let Some(cwd) = &info.cwd {
        candidates.push(cwd.clone());
    }
    candidates.extend(info.paths.iter().cloned());

    let mut roots = std::collections::BTreeSet::new();
    for path in candidates {
        for root in candidate_config_roots_from_path(&path) {
            if is_claude_config_root(&root) {
                roots.insert(root);
            }
        }
    }

    roots.into_iter().map(ConfigDir::new).collect()
}

fn candidate_config_roots_from_path(path: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    roots.push(path.to_path_buf());

    let mut cursor = path;
    while let Some(parent) = cursor.parent() {
        if cursor.file_name().and_then(|n| n.to_str()) == Some("sessions")
            || cursor.file_name().and_then(|n| n.to_str()) == Some("projects")
        {
            roots.push(parent.to_path_buf());
        }
        cursor = parent;
    }

    roots
}

fn is_claude_config_root(path: &Path) -> bool {
    path.join("sessions").is_dir() && path.join("projects").is_dir()
}

fn discover_home_claude_config_dirs(home: &Path) -> Vec<PathBuf> {
    let mut roots = std::collections::BTreeSet::new();
    let Ok(entries) = fs::read_dir(home) else {
        return Vec::new();
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name != ".claude" && !name.starts_with(".claude-") {
            continue;
        }
        let path = entry.path();
        if is_claude_config_root(&path) {
            roots.insert(path);
        }
    }

    roots.into_iter().collect()
}

/// Per-tick session-discovery state shared across all `load_session` calls
/// in a single `collect_sessions` pass. Pre-parsed so each PID can reason
/// about the sids claimed by its neighbors without re-reading every
/// session file.
#[derive(Default)]
struct DiscoveryContext {
    /// PID → sid currently recorded in its `sessions/{PID}.json`.
    claimed_sids_by_pid: HashMap<u32, String>,
    /// cwd → number of active session files pointing at it.
    pids_per_cwd: HashMap<String, usize>,
    /// abtop's own PID, threaded through so `load_session` can self-filter
    /// without growing an extra arg. Set by `build_discovery_context`.
    self_pid: u32,
}

fn build_discovery_context(
    session_paths: &[(PathBuf, ConfigDir)],
    process_info: &HashMap<u32, ProcInfo>,
    self_pid: u32,
) -> DiscoveryContext {
    let mut claimed_sids_by_pid: HashMap<u32, String> = HashMap::new();
    let mut pids_per_cwd: HashMap<String, usize> = HashMap::new();
    let mut seen_pids: std::collections::HashSet<u32> = std::collections::HashSet::new();
    for (path, _) in session_paths {
        let Ok(content) = fs::read_to_string(path) else {
            continue;
        };
        let Ok(mut sf) = serde_json::from_str::<SessionFile>(&content) else {
            continue;
        };
        sf.sanitize();
        if !seen_pids.insert(sf.pid) {
            continue;
        }
        // Only count PIDs that are alive AND actually claude AND not
        // descended from abtop itself. Stale `sessions/{PID}.json` files
        // (crashed sessions) and abtop's own `claude --print` summary
        // children would otherwise inflate `pids_per_cwd` and silently
        // suppress the /clear sid override for the real session sharing
        // that cwd.
        let Some(info) = process_info.get(&sf.pid) else {
            continue;
        };
        if !process::cmd_has_binary(&info.command, "claude") {
            continue;
        }
        if process::is_descendant_of(sf.pid, self_pid, process_info) {
            continue;
        }
        *pids_per_cwd.entry(sf.cwd.clone()).or_insert(0) += 1;
        claimed_sids_by_pid.insert(sf.pid, sf.session_id);
    }
    DiscoveryContext {
        claimed_sids_by_pid,
        pids_per_cwd,
        self_pid,
    }
}

/// Resolve the project directory that holds this session's transcripts.
/// Prefers `encode_cwd_path(cwd)` but falls back to any sibling subdir
/// containing `{original_sid}.jsonl` — worktree sessions live under a dir
/// keyed by the branch name, not the encoded cwd, so both the transcript
/// path and the live-sid lookup must resolve through this fallback.
fn resolve_project_dir(config: &ConfigDir, cwd: &str, original_sid: &str) -> Option<PathBuf> {
    let encoded = encode_cwd_path(cwd);
    let primary = config.projects_dir.join(&encoded);
    let jsonl_name = format!("{}.jsonl", original_sid);

    let primary_has_original = {
        let p = primary.join(&jsonl_name);
        p.exists() && !is_symlink(&p)
    };
    if primary_has_original {
        return Some(primary);
    }

    if let Ok(entries) = fs::read_dir(&config.projects_dir) {
        for entry in entries.flatten() {
            if entry.file_type().map(|ft| ft.is_symlink()).unwrap_or(true) {
                continue;
            }
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let candidate = path.join(&jsonl_name);
            if candidate.exists() && !is_symlink(&candidate) {
                return Some(path);
            }
        }
    }

    // Original transcript is missing (deleted, or never flushed yet). Fall
    // back to the encoded-cwd dir if it exists so live-sid lookup still
    // has somewhere to scan.
    if primary.is_dir() {
        return Some(primary);
    }
    None
}

/// Find the currently-live session_id by scanning the project directory
/// for the most recently modified transcript.
///
/// `/clear` mints a new sessionId and a new `{sid}.jsonl` without rewriting
/// `sessions/{PID}.json`, so the session file's sid goes stale. The fresh
/// transcript is however always present on disk in the same project dir.
/// `started_at_ms` filters out transcripts older than this PID's lifetime
/// (prior runs or sibling claudes that left files behind). `excluded`
/// skips sids already claimed by other active session files.
fn find_live_session_id(
    project_dir: Option<&Path>,
    started_at_ms: u64,
    excluded: &std::collections::HashSet<&str>,
) -> Option<String> {
    let project_dir = project_dir?;
    let entries = fs::read_dir(project_dir).ok()?;

    // Allow a small grace window (5s) before started_at to tolerate clock
    // skew between the session file's startedAt and jsonl creation mtime
    // (FS mtime granularity is 1-2s on some platforms, and startedAt is
    // captured before Claude Code flushes the transcript's first line).
    let min_mtime = std::time::UNIX_EPOCH
        + std::time::Duration::from_millis(started_at_ms.saturating_sub(5_000));

    let mut best: Option<(std::time::SystemTime, String)> = None;
    for entry in entries.flatten() {
        if entry.file_type().map(|ft| ft.is_symlink()).unwrap_or(true) {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if excluded.contains(stem) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(mtime) = meta.modified() else { continue };
        if mtime < min_mtime {
            continue;
        }
        match &best {
            Some((best_mtime, _)) if mtime <= *best_mtime => {}
            _ => best = Some((mtime, stem.to_string())),
        }
    }

    best.map(|(_, sid)| sid)
}

fn find_session_file_for_pid(sessions_dir: &Path, pid: u32) -> Option<PathBuf> {
    let direct = sessions_dir.join(format!("{}.json", pid));
    if direct.exists() && !is_symlink(&direct) {
        return Some(direct);
    }

    let entries = fs::read_dir(sessions_dir).ok()?;
    for entry in entries.flatten() {
        if entry.file_type().map(|ft| ft.is_symlink()).unwrap_or(true) {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        // Skip files we can't read or parse — one bad file shouldn't abort
        // the whole fallback search (previously `?` bubbled out of the loop).
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(session) = serde_json::from_str::<SessionFile>(&content) else {
            continue;
        };
        if session.pid == pid {
            return Some(path);
        }
    }

    None
}

struct TranscriptResult {
    model: String,
    total_input: u64,
    total_output: u64,
    total_cache_read: u64,
    total_cache_create: u64,
    /// Last assistant turn's input context size (for context % calculation)
    last_context_tokens: u64,
    /// High-water mark: largest context seen in any turn (for 1M detection)
    max_context_tokens: u64,
    /// cache_read from the previous turn (for compaction detection)
    prev_cache_read: u64,
    /// Per-turn context sizes for evolution visualization.
    context_history: Vec<u64>,
    /// Detected compaction events (context dropped > 30% between consecutive turns).
    compaction_count: u32,
    turn_count: u32,
    current_task: String,
    version: String,
    git_branch: String,
    last_activity: std::time::SystemTime,
    new_offset: u64,
    /// File identity: (inode, mtime_ns). Used to detect file replacement
    /// even when the new file is the same size or larger.
    file_identity: (u64, u64),
    token_history: Vec<u64>,
    initial_prompt: String,
    session_name: String,
    /// First assistant response text (text blocks only, no tool_use)
    first_assistant_text: String,
    /// Recent chat items: real user/assistant messages plus display-only tool
    /// call and result lines (see `chat_tools`). Tool inputs and results are
    /// reduced there; file bodies and tool_result wrappers never reach this list.
    chat_messages: Vec<ChatMessage>,
    /// Result kind of recent tool calls by id, so a result parsed in a later
    /// incremental read still knows whether its output may be shown.
    chat_tools: ChatTools,
    /// Tool call timeline extracted from transcript.
    tool_calls: Vec<crate::model::ToolCall>,
    /// Timestamp of the last assistant turn (epoch ms), used to compute tool duration.
    last_assistant_ts_ms: u64,
    /// Timestamp (epoch ms) of the most recent `user` line that has not been
    /// followed by an assistant turn. Zero when the latest entry was an
    /// assistant turn. Mutually exclusive with `last_assistant_ts_ms`.
    last_user_ts_ms: u64,
    /// True when this parse observed at least one `user` or `assistant`
    /// line. When false the timestamp fields above are just defaults and
    /// must not overwrite cached state - otherwise a no-new-data tick
    /// would clear the live pending/thinking markers.
    saw_turn: bool,
    /// Unanswered AskUserQuestion tool IDs, independent of task/timestamp heuristics.
    /// Only correlated tool results close questions; ordinary prompts, local
    /// commands, and assistant chunks do not prove cancellation. Session/cache
    /// replacement or transcript truncation rebuilds this state from scratch.
    pending_question_ids: HashSet<String>,
    /// Bounded, in-order ledger of tool_use / tool_result events with their
    /// transcript timestamps. Permission hook markers are evaluated against it
    /// (see `permission_claim_open`); it is independent of
    /// `last_assistant_ts_ms`, so a marker is judged even when the transcript
    /// holds no matching tool_use. Incremental parses append to the cached
    /// ledger; file replacement or truncation rebuilds it from scratch.
    tool_events: Vec<ToolEvent>,
    /// File access audit log extracted from tool_use entries.
    file_accesses: Vec<FileAccess>,
}

/// Number of tool lifecycle events kept per transcript (oldest dropped first).
const MAX_TOOL_EVENTS: usize = 256;
/// Tool ids longer than this are ignored instead of being stored.
const MAX_TOOL_ID_BYTES: usize = 128;

/// One tool lifecycle event from the transcript. `ts_ms` is the transcript
/// line timestamp in epoch milliseconds, or 0 when the line had none.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ToolEvent {
    Use {
        id: String,
        name: String,
        ts_ms: u64,
    },
    Result {
        id: String,
        ts_ms: u64,
    },
}

fn push_tool_event(events: &mut Vec<ToolEvent>, event: ToolEvent) {
    events.push(event);
    let len = events.len();
    if len > MAX_TOOL_EVENTS {
        events.drain(..len - MAX_TOOL_EVENTS);
    }
}

/// Check if a path is a symlink without following it.
/// Defaults to true on error (fail-closed: skip if we can't verify).
fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(true)
}

/// Largest marker file the collector accepts. The hook script keeps every
/// marker it writes within this size (see `scripts/claude-reply-hook.sh`).
const MAX_PERMISSION_MARKER_BYTES: u64 = 128;
/// Tool-name length cap applied by the hook script (`MAX_TOOL_NAME`). A marker
/// name of exactly this length may have been truncated and is matched by prefix.
const PERMISSION_MARKER_TOOL_CAP: usize = 32;
/// Epoch values below this are not millisecond timestamps (for example plain
/// seconds from a minimal `date`), so they are ignored in favor of the mtime.
const MIN_EPOCH_MS: u64 = 1_000_000_000_000;

/// What a permission marker says about the prompt it stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ClaimScope {
    /// `tool_use_id` supplied by the hook payload. Claude Code documents it for
    /// PostToolUse/PostToolUseFailure/PermissionDenied but not for
    /// PermissionRequest or Notification, so this is optional and never assumed.
    Id(String),
    /// Tool name from the hook payload. `truncated` is set when the name has
    /// the hook's length cap and is therefore matched as a prefix.
    Name { name: String, truncated: bool },
    /// No identity: an empty Notification marker or an old-format empty marker.
    Any,
}

/// "A permission prompt was shown at `since_ms`" as recorded by the hook.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PermissionClaim {
    scope: ClaimScope,
    since_ms: u64,
}

/// Parse a `u64` epoch-ms value from a marker line, rejecting non-ms values.
fn parse_marker_epoch_ms(value: &str) -> Option<u64> {
    value
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|ms| *ms >= MIN_EPOCH_MS)
}

/// Read the permission marker written by `scripts/claude-reply-hook.sh`.
///
/// File format, one field per line (everything after line 1 is optional and
/// unknown keys are ignored, so the format can grow):
///
/// ```text
/// <tool_name>        line 1, may be empty (legacy markers hold only this line)
/// ev=<kind>          permission | notification | unknown (diagnostic only)
/// at=<epoch_ms>      when the prompt was marked
/// id=<tool_use_id>   only when the hook payload carried one
/// nt=<epoch_ms>      latest identity-less Notification seen while a named
///                    marker was already present
/// ```
///
/// Returns up to two claims: the primary one (identified by id, else name,
/// else nothing) and an `Any` claim for `nt`. A legacy or malformed marker
/// yields one identity-less claim stamped with the file mtime. Symlinks,
/// non-regular files, oversized files, invalid session ids and markers older
/// than the session start yield no claims.
fn read_permission_claims(
    config: &ConfigDir,
    session_id: &str,
    started_at: u64,
) -> Vec<PermissionClaim> {
    if session_id.is_empty()
        || session_id.len() > 128
        || !session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Vec::new();
    }
    let marker = config.base_dir().join("abtop-reply-state").join(session_id);
    let Ok(metadata) = fs::symlink_metadata(&marker) else {
        return Vec::new();
    };
    if !metadata.file_type().is_file() || metadata.len() > MAX_PERMISSION_MARKER_BYTES {
        return Vec::new();
    }
    let Some(mtime_ms) = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|time| time.as_millis() as u64)
        .filter(|time| *time >= started_at)
    else {
        return Vec::new();
    };

    let mut raw = Vec::new();
    if fs::File::open(&marker)
        .and_then(|file| {
            file.take(MAX_PERMISSION_MARKER_BYTES + 1)
                .read_to_end(&mut raw)
        })
        .is_err()
    {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&raw);
    let mut lines = text.lines();
    let tool_name = lines.next().unwrap_or("").trim().to_string();
    let (mut at, mut id, mut notification_at) = (None, None, None);
    for line in lines {
        match line.split_once('=') {
            Some(("at", value)) => at = parse_marker_epoch_ms(value),
            Some(("id", value)) if !value.trim().is_empty() => {
                id = Some(value.trim().to_string());
            }
            Some(("nt", value)) => notification_at = parse_marker_epoch_ms(value),
            _ => {}
        }
    }

    let scope = if let Some(id) = id {
        ClaimScope::Id(id)
    } else if !tool_name.is_empty() {
        ClaimScope::Name {
            truncated: tool_name.len() >= PERMISSION_MARKER_TOOL_CAP,
            name: tool_name,
        }
    } else {
        ClaimScope::Any
    };
    let mut claims = vec![PermissionClaim {
        scope,
        since_ms: at.unwrap_or(mtime_ms),
    }];
    if let Some(since_ms) = notification_at {
        claims.push(PermissionClaim {
            scope: ClaimScope::Any,
            since_ms,
        });
    }
    // A claim stamped before this process started belongs to an earlier run of
    // the same session id.
    claims.retain(|claim| claim.since_ms >= started_at);
    claims
}

/// Decide whether a hook-recorded permission prompt is still unanswered, using
/// only transcript events that happened after the marker was written.
///
/// Events are compared with the marker time, never with the latest assistant
/// timestamp, so the answer does not depend on the transcript holding a
/// matching tool_use. Rules, with `since` = marker time:
///
/// * A tool_result at or before `since` never closes the claim; it answered an
///   earlier request. Results without a parsable timestamp cannot be ordered
///   against the marker, so they neither close nor keep a claim open.
/// * `Id`: closed by the tool_result carrying that id.
/// * `Name`: closed by a tool_result of a same-name call once every same-name
///   call that was still open at `since` has a result. Results of other tools do
///   not close it. Calls that share a name cannot be told apart by name alone,
///   so the claim waits for all of them instead of closing on the first.
/// * `Any` (empty Notification marker): closed by any tool_result once every
///   call open at `since` has a result. This is the weakest fallback: if no call
///   was open at `since` (the transcript has no matching tool_use) the first
///   later tool_result of any tool closes it, so a result from an unrelated
///   parallel call can close a still-pending prompt early. A newer marker always
///   reopens the state because it carries a newer time.
/// * Safety net for `Id` and `Name`: when at least one call was open at `since`
///   and all of them have a result, the turn that raised the prompt is over, so
///   the claim is closed even if the prompt's own call never appears in this
///   transcript (for example a subagent call whose result lands elsewhere).
///
/// Normal approvals are normally cleared by the PostToolUse hook; denials and
/// interruptions have no hook and rely on the transcript rules above.
fn permission_claim_open(claim: &PermissionClaim, events: &[ToolEvent]) -> bool {
    let since = claim.since_ms;
    let result_after = |wanted: &str| {
        events.iter().any(|event| {
            matches!(event, ToolEvent::Result { id, ts_ms } if id == wanted && *ts_ms > since)
        })
    };
    let result_at_or_before = |wanted: &str| {
        events.iter().any(|event| {
            matches!(event, ToolEvent::Result { id, ts_ms } if id == wanted && *ts_ms <= since)
        })
    };
    // Calls that had been issued but not answered when the marker was written.
    let open_at_marker: Vec<(&str, &str)> = events
        .iter()
        .filter_map(|event| match event {
            ToolEvent::Use { id, name, ts_ms } if *ts_ms <= since && !result_at_or_before(id) => {
                Some((id.as_str(), name.as_str()))
            }
            _ => None,
        })
        .collect();
    let all_resolved = open_at_marker.iter().all(|(id, _)| result_after(id));
    let turn_complete = !open_at_marker.is_empty() && all_resolved;

    let closed = match &claim.scope {
        ClaimScope::Id(wanted) => result_after(wanted) || turn_complete,
        ClaimScope::Name { name, truncated } => {
            let same_name = |candidate: &str| {
                candidate == name || (*truncated && candidate.starts_with(name.as_str()))
            };
            let named_result_after = events.iter().any(|event| match event {
                ToolEvent::Result { id, ts_ms } if *ts_ms > since => events.iter().any(|other| {
                    matches!(other, ToolEvent::Use { id: use_id, name: use_name, .. }
                        if use_id == id && same_name(use_name))
                }),
                _ => false,
            });
            let same_name_resolved = open_at_marker
                .iter()
                .filter(|(_, open_name)| same_name(open_name))
                .all(|(id, _)| result_after(id));
            (named_result_after && same_name_resolved) || turn_complete
        }
        ClaimScope::Any => {
            let any_result_after = events
                .iter()
                .any(|event| matches!(event, ToolEvent::Result { ts_ms, .. } if *ts_ms > since));
            any_result_after && all_resolved
        }
    };
    !closed
}

/// Get file identity as (device, inode) for detecting file replacement.
#[cfg(unix)]
fn file_identity(path: &Path) -> (u64, u64) {
    fs::metadata(path)
        .ok()
        .map(|m| (m.dev(), m.ino()))
        .unwrap_or((0, 0))
}

#[cfg(windows)]
fn file_identity(path: &Path) -> (u64, u64) {
    fs::metadata(path)
        .ok()
        .map(|m| {
            let size = m.len();
            let mtime_ns = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            (size, mtime_ns)
        })
        .unwrap_or((0, 0))
}

fn parse_transcript(path: &Path, from_offset: u64) -> TranscriptResult {
    parse_transcript_with_previous(path, from_offset, 0, 0, None)
}

fn parse_transcript_with_previous(
    path: &Path,
    from_offset: u64,
    initial_context_tokens: u64,
    initial_cache_read: u64,
    initial_question_ids: Option<&HashSet<String>>,
) -> TranscriptResult {
    parse_transcript_with_state(
        path,
        from_offset,
        initial_context_tokens,
        initial_cache_read,
        initial_question_ids,
        None,
    )
}

fn parse_transcript_with_state(
    path: &Path,
    from_offset: u64,
    initial_context_tokens: u64,
    initial_cache_read: u64,
    initial_question_ids: Option<&HashSet<String>>,
    initial_chat_tools: Option<&ChatTools>,
) -> TranscriptResult {
    let identity = file_identity(path);
    let mut result = TranscriptResult {
        model: "-".to_string(),
        total_input: 0,
        total_output: 0,
        total_cache_read: 0,
        total_cache_create: 0,
        last_context_tokens: 0,
        max_context_tokens: 0,
        prev_cache_read: 0,
        context_history: Vec::new(),
        compaction_count: 0,
        turn_count: 0,
        current_task: String::new(),
        version: String::new(),
        git_branch: String::new(),
        last_activity: std::time::UNIX_EPOCH,
        new_offset: from_offset,
        file_identity: identity,
        token_history: Vec::new(),
        initial_prompt: String::new(),
        session_name: String::new(),
        first_assistant_text: String::new(),
        chat_messages: Vec::new(),
        tool_calls: Vec::new(),
        last_assistant_ts_ms: 0,
        last_user_ts_ms: 0,
        saw_turn: false,
        pending_question_ids: if from_offset > 0 {
            initial_question_ids.cloned().unwrap_or_default()
        } else {
            HashSet::new()
        },
        chat_tools: if from_offset > 0 {
            initial_chat_tools.cloned().unwrap_or_default()
        } else {
            Vec::new()
        },
        tool_events: Vec::new(),
        file_accesses: Vec::new(),
    };

    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return result,
    };

    let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if file_len == from_offset {
        // No new data
        result.new_offset = file_len;
        return result;
    }
    // File shrank (truncation/rotation) — reset and reparse from start
    let effective_offset = if file_len < from_offset {
        result.pending_question_ids.clear();
        result.chat_tools.clear();
        0
    } else {
        from_offset
    };
    let from_offset = effective_offset;
    let mut prev_context_tokens = if from_offset > 0 {
        initial_context_tokens
    } else {
        0
    };
    let mut prev_cache_read = if from_offset > 0 {
        initial_cache_read
    } else {
        0
    };

    let mut reader = BufReader::new(file);
    if from_offset > 0 {
        let _ = reader.seek(SeekFrom::Start(from_offset));
    }

    let mtime = fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .unwrap_or(std::time::UNIX_EPOCH);
    result.last_activity = mtime;

    const MAX_LINE_BYTES: usize = 10 * 1024 * 1024; // 10 MB

    let mut bytes_read = from_offset;
    let mut line_buf = String::new();
    loop {
        line_buf.clear();
        // Bounded read: take(MAX+1) physically caps the allocation. Without
        // this, a malformed or hostile transcript with an unbounded line
        // would OOM the process before any length check could fire.
        match reader
            .by_ref()
            .take(MAX_LINE_BYTES as u64 + 1)
            .read_line(&mut line_buf)
        {
            Ok(0) => break,
            Ok(n) => {
                // Cap hit without a newline — malformed/hostile line. Skip
                // to end of file; we'll re-evaluate when file_identity changes.
                if line_buf.len() > MAX_LINE_BYTES && !line_buf.ends_with('\n') {
                    bytes_read = file_len;
                    break;
                }
                let has_newline = line_buf.ends_with('\n');
                let line = line_buf.trim();
                if line.is_empty() {
                    if has_newline {
                        bytes_read += n as u64;
                    }
                    continue;
                }
                // Try to parse as JSON. If incomplete (no newline) and
                // parse fails, defer to next poll. If parse succeeds,
                // accept the record even without trailing newline.
                let val = match serde_json::from_str::<Value>(line) {
                    Ok(v) => v,
                    Err(_) => {
                        if has_newline {
                            // Complete line but invalid JSON — skip it
                            bytes_read += n as u64;
                        }
                        // Incomplete line with parse error — defer
                        if !has_newline {
                            break;
                        }
                        continue;
                    }
                };
                bytes_read += n as u64;
                {
                    // Parse timestamp from any entry (for tool duration calculation)
                    let entry_ts_ms = val
                        .get("timestamp")
                        .and_then(|t| t.as_str())
                        .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
                        .map(|dt| dt.timestamp_millis().max(0) as u64)
                        .unwrap_or(0);

                    match val.get("type").and_then(|t| t.as_str()) {
                        Some("custom-title") => {
                            if let Some(title) = val.get("customTitle").and_then(|v| v.as_str()) {
                                result.session_name = clean_chat_text(title, 200);
                            }
                        }
                        Some("assistant") => {
                            result.turn_count += 1;
                            // Clear previous task on each new turn so stale tasks
                            // don't persist when latest turn has no tool_use
                            result.current_task = String::new();
                            if let Some(msg) = val.get("message") {
                                if let Some(m) = msg.get("model").and_then(|m| m.as_str()) {
                                    result.model = m.to_string();
                                }
                                if let Some(usage) = msg.get("usage") {
                                    let inp = usage
                                        .get("input_tokens")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0);
                                    let out = usage
                                        .get("output_tokens")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0);
                                    let cr = usage
                                        .get("cache_read_input_tokens")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0);
                                    let cc = usage
                                        .get("cache_creation_input_tokens")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0);
                                    result.total_input += inp;
                                    result.total_output += out;
                                    result.total_cache_read += cr;
                                    result.total_cache_create += cc;
                                    // Context = input_tokens + cache_read (excludes cache_creation, #54)
                                    // Exception: when cache_read = 0 but cache_creation > 0,
                                    // this is a fresh session creating cache for the first time,
                                    // so cache_creation represents the actual context size.
                                    let prev_context = prev_context_tokens;
                                    let current_context = if cr == 0 && cc > 0 {
                                        inp + cc
                                    } else {
                                        inp + cr
                                    };
                                    result.last_context_tokens = current_context;
                                    if result.last_context_tokens > result.max_context_tokens {
                                        result.max_context_tokens = result.last_context_tokens;
                                    }
                                    // Detect compaction: context drops > 30% between turns
                                    // AND cache_read drops hard (old cache invalidated).
                                    // This avoids false positives from normal cache hit rate
                                    // fluctuations where total context varies but no
                                    // actual conversation truncation occurred.
                                    if prev_context > 0
                                        && result.last_context_tokens < prev_context * 7 / 10
                                        && prev_cache_read > 1000
                                        && cr < prev_cache_read / 5
                                    {
                                        result.compaction_count += 1;
                                    }
                                    prev_context_tokens = current_context;
                                    prev_cache_read = cr;
                                    result.prev_cache_read = cr;
                                    if result.context_history.len() < 10_000 {
                                        result.context_history.push(result.last_context_tokens);
                                    }
                                    if result.token_history.len() < 10_000 {
                                        result.token_history.push(inp + out + cr + cc);
                                    }
                                }
                                // Extract first assistant text (text blocks only) for summary fallback
                                if result.first_assistant_text.is_empty() {
                                    if let Some(content) =
                                        msg.get("content").and_then(|c| c.as_array())
                                    {
                                        let texts: Vec<&str> = content
                                            .iter()
                                            .filter_map(|block| {
                                                if block.get("type").and_then(|t| t.as_str())
                                                    == Some("text")
                                                {
                                                    block.get("text").and_then(|t| t.as_str())
                                                } else {
                                                    None
                                                }
                                            })
                                            .collect();
                                        if !texts.is_empty() {
                                            let joined = texts.join(" ");
                                            let normalized: String = joined
                                                .lines()
                                                .map(|l| l.trim())
                                                .filter(|l| !l.is_empty())
                                                .collect::<Vec<_>>()
                                                .join(" ");
                                            result.first_assistant_text =
                                                truncate(&normalized, 200);
                                        }
                                    }
                                }
                                let assistant_text = extract_chat_text(msg);
                                if !assistant_text.is_empty() {
                                    push_chat_message(
                                        &mut result.chat_messages,
                                        ChatRole::Assistant,
                                        assistant_text,
                                    );
                                }
                                // Extract all tool_use entries: timeline + current_task + file access audit
                                let mut has_tool_use = false;
                                if let Some(content) = msg.get("content").and_then(|c| c.as_array())
                                {
                                    for item in content {
                                        if item.get("type").and_then(|t| t.as_str())
                                            == Some("tool_use")
                                        {
                                            has_tool_use = true;
                                            let tool = item
                                                .get("name")
                                                .and_then(|n| n.as_str())
                                                .unwrap_or("?");
                                            if tool == "AskUserQuestion" {
                                                if let Some(id) = item
                                                    .get("id")
                                                    .and_then(Value::as_str)
                                                    .filter(|id| !id.is_empty())
                                                {
                                                    result
                                                        .pending_question_ids
                                                        .insert(id.to_string());
                                                }
                                            }
                                            if let Some(id) = item
                                                .get("id")
                                                .and_then(Value::as_str)
                                                .filter(|id| {
                                                    !id.is_empty() && id.len() <= MAX_TOOL_ID_BYTES
                                                })
                                            {
                                                push_tool_event(
                                                    &mut result.tool_events,
                                                    ToolEvent::Use {
                                                        id: id.to_string(),
                                                        name: tool.to_string(),
                                                        ts_ms: entry_ts_ms,
                                                    },
                                                );
                                            }
                                            if let Some(id) = item.get("id").and_then(Value::as_str)
                                            {
                                                chat_tools::remember_tool(
                                                    &mut result.chat_tools,
                                                    id,
                                                    chat_tools::claude_result_kind(tool),
                                                );
                                            }
                                            push_chat_message(
                                                &mut result.chat_messages,
                                                ChatRole::ToolCall,
                                                chat_tools::call_text(
                                                    tool,
                                                    item.get("input").unwrap_or(&Value::Null),
                                                ),
                                            );
                                            let arg = extract_tool_arg(item);
                                            // Last tool_use in forward order wins current_task
                                            result.current_task = format!("{} {}", tool, arg);
                                            if result.tool_calls.len() < 500 {
                                                result.tool_calls.push(crate::model::ToolCall {
                                                    name: tool.to_string(),
                                                    arg: truncate(&arg, 40),
                                                    duration_ms: 0, // filled on next user turn
                                                });
                                            }
                                            // Extract file access audit entries.
                                            // Cap is applied once at the end of
                                            // parse_transcript via a sliding window so
                                            // the latest accesses always survive.
                                            if let Some(file_path) = item
                                                .get("input")
                                                .and_then(|i| i.get("file_path"))
                                                .and_then(|f| f.as_str())
                                            {
                                                let op = match tool {
                                                    "Read" => Some(FileOp::Read),
                                                    "Edit" => Some(FileOp::Edit),
                                                    "Write" => Some(FileOp::Write),
                                                    _ => None,
                                                };
                                                if let Some(op) = op {
                                                    result.file_accesses.push(FileAccess {
                                                        path: file_path.to_string(),
                                                        operation: op,
                                                        turn_index: result.turn_count,
                                                    });
                                                }
                                            }
                                        }
                                    }
                                }
                                // Save timestamp for duration calculation — only when
                                // tool_use is present, so we can detect "tools pending".
                                if entry_ts_ms > 0 && has_tool_use {
                                    result.last_assistant_ts_ms = entry_ts_ms;
                                }
                                // Any assistant turn closes the prior "thinking" window.
                                result.last_user_ts_ms = 0;
                                result.saw_turn = true;
                            }
                        }
                        Some("user") => {
                            // Correlate before filtering synthetic user wrappers: answers,
                            // errors, and denials all close the matching invocation.
                            if let Some(content) = val
                                .get("message")
                                .and_then(|msg| msg.get("content"))
                                .and_then(Value::as_array)
                            {
                                for block in content {
                                    if block.get("type").and_then(Value::as_str)
                                        == Some("tool_result")
                                    {
                                        if let Some(id) =
                                            block.get("tool_use_id").and_then(Value::as_str)
                                        {
                                            result.pending_question_ids.remove(id);
                                            let is_error = block
                                                .get("is_error")
                                                .and_then(Value::as_bool)
                                                .unwrap_or(false);
                                            let kind =
                                                chat_tools::tool_kind(&result.chat_tools, id);
                                            push_chat_message(
                                                &mut result.chat_messages,
                                                if is_error {
                                                    ChatRole::ToolError
                                                } else {
                                                    ChatRole::ToolResult
                                                },
                                                chat_tools::result_text(
                                                    &tool_result_text(block.get("content")),
                                                    kind,
                                                    is_error,
                                                ),
                                            );
                                            if !id.is_empty() && id.len() <= MAX_TOOL_ID_BYTES {
                                                push_tool_event(
                                                    &mut result.tool_events,
                                                    ToolEvent::Result {
                                                        id: id.to_string(),
                                                        ts_ms: entry_ts_ms,
                                                    },
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                            // Compute tool call duration: time from assistant turn to this user turn
                            if entry_ts_ms > 0 && result.last_assistant_ts_ms > 0 {
                                let duration =
                                    entry_ts_ms.saturating_sub(result.last_assistant_ts_ms);
                                // Distribute duration across tool calls from that assistant turn
                                // (approximation: divide equally among pending zero-duration calls)
                                let pending: Vec<usize> = result
                                    .tool_calls
                                    .iter()
                                    .enumerate()
                                    .rev()
                                    .take_while(|(_, tc)| tc.duration_ms == 0)
                                    .map(|(i, _)| i)
                                    .collect();
                                if !pending.is_empty() {
                                    let per_call = duration / pending.len() as u64;
                                    for idx in pending {
                                        result.tool_calls[idx].duration_ms = per_call;
                                    }
                                }
                                result.last_assistant_ts_ms = 0;
                            }
                            // Mark the start of a thinking window — next assistant
                            // turn clears it. **Skip synthetic user lines**:
                            // Claude Code serializes both real prompts and a number
                            // of non-prompt entries (tool_result wrappers, slash
                            // commands like /plugin, ! bash invocations, meta
                            // caveats) as `user`-role lines. Only real prompts
                            // mean "model has been asked, no reply yet"; the
                            // others either belong inside an existing turn or
                            // are pure local operations that never invoke the
                            // model. Treating them as a thinking window pins
                            // the session in Thinking forever (e.g. /plugin
                            // update flushes 3 user-role lines and no
                            // assistant reply ever arrives to clear them).
                            let synthetic = is_synthetic_user_msg(&val);
                            if entry_ts_ms > 0 && !synthetic {
                                result.last_user_ts_ms = entry_ts_ms;
                            }
                            result.saw_turn = true;
                            if let Some(v) = val.get("version").and_then(|v| v.as_str()) {
                                result.version = v.to_string();
                            }
                            if let Some(b) = val.get("gitBranch").and_then(|b| b.as_str()) {
                                result.git_branch = b.to_string();
                            }
                            // Extract first user prompt as session title — also
                            // skip synthetic lines so the title isn't
                            // "<command-name>/plugin..." or a bash stdout dump.
                            if result.initial_prompt.is_empty() && !synthetic {
                                if let Some(msg) = val.get("message") {
                                    result.initial_prompt = extract_prompt_text(msg);
                                }
                            }
                            if !synthetic {
                                if let Some(msg) = val.get("message") {
                                    let user_text = extract_chat_text(msg);
                                    if !user_text.is_empty() {
                                        push_chat_message(
                                            &mut result.chat_messages,
                                            ChatRole::User,
                                            user_text,
                                        );
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            Err(_) => break,
        }
    }

    // Sliding window cap: keep the most recent MAX_FILE_ACCESSES, drop
    // the oldest. Applied once after the parse loop so peak memory stays
    // bounded by the file size, not by the number of historical accesses.
    let len = result.file_accesses.len();
    if len > MAX_FILE_ACCESSES {
        result.file_accesses.drain(..len - MAX_FILE_ACCESSES);
    }

    result.new_offset = bytes_read;
    result
}

/// Extract a short summary from the first user message content.
/// Handles both string content and array-of-blocks content.
/// Encode a cwd path to match Claude Code's project directory naming.
/// Claude Code replaces '/', '_', and '.' with '-'.
/// True iff a `user`-role transcript entry is *synthetic* — i.e. not a
/// real human prompt that the model still owes a reply for. Three forms:
///
/// 1. `isMeta: true` — Claude Code's explicit marker for non-prompt
///    entries (e.g. the `<local-command-caveat>` line).
/// 2. `content` is an array and every block is `tool_result` — the
///    user-role wrapper Claude Code uses to feed tool output back to
///    the model. A mixed block message is treated as a real prompt so
///    we never silently swallow user input.
/// 3. `content` is a string opening with a known local-command tag —
///    `/plugin`, `/exit`, `!bash`, etc. all serialize as user-role
///    lines like `<command-name>...`, `<local-command-stdout>...`,
///    `<bash-input>...`. These are pure local operations that never
///    invoke the model, so treating them as a prompt would leave
///    `last_user_ts_ms` stuck and pin the session in Thinking forever.
fn is_synthetic_user_msg(entry: &Value) -> bool {
    if entry
        .get("isMeta")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return true;
    }
    let Some(message) = entry.get("message") else {
        return false;
    };
    match message.get("content") {
        Some(Value::Array(arr)) => {
            !arr.is_empty()
                && arr
                    .iter()
                    .all(|block| block.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
        }
        Some(Value::String(s)) => {
            let t = s.trim_start();
            t.starts_with("<local-command-stdout>")
                || t.starts_with("<local-command-stderr>")
                || t.starts_with("<local-command-caveat>")
                || t.starts_with("<command-name>")
                || t.starts_with("<bash-input>")
                || t.starts_with("<bash-stdout>")
                || t.starts_with("<bash-stderr>")
        }
        _ => false,
    }
}

fn push_chat_message(messages: &mut Vec<ChatMessage>, role: ChatRole, text: String) {
    if text.is_empty() {
        return;
    }
    messages.push(ChatMessage { role, text });
    trim_chat_window(messages);
}

/// Text of a `tool_result` block's `content`, which is either a string or an
/// array of blocks. Only text blocks count; images and other blocks are skipped.
fn tool_result_text(content: Option<&Value>) -> std::borrow::Cow<'_, str> {
    match content {
        Some(Value::String(s)) => std::borrow::Cow::Borrowed(s),
        Some(Value::Array(blocks)) => std::borrow::Cow::Owned(
            blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => std::borrow::Cow::Borrowed(""),
    }
}

fn extract_chat_text(message: &Value) -> String {
    let raw = match message.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|block| {
                if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                    block
                        .get("text")
                        .and_then(|t| t.as_str())
                        .map(|s| s.to_string())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    };
    clean_chat_message(&raw)
}

/// Like `clean_chat_text`, but keeps line breaks for the chat detail view.
fn clean_chat_message(raw: &str) -> String {
    let lines = super::sanitize_chat_lines(&strip_image_markers(raw.to_string()));
    let redacted = super::redact_secrets(&lines);
    truncate(&redacted, MAX_CHAT_TEXT_CHARS)
}

fn clean_chat_text(raw: &str, max: usize) -> String {
    let cleaned = raw
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with("```"))
        .collect::<Vec<_>>()
        .join(" ");

    let without_images = strip_image_markers(cleaned);
    let terminal_safe = super::sanitize_terminal_text(without_images.trim());
    let redacted = super::redact_secrets(&terminal_safe);
    truncate(&redacted, max)
}

fn strip_image_markers(text: String) -> String {
    let mut without_images = text;
    while let Some(start) = without_images.find("[Image") {
        if let Some(end) = without_images[start..].find(']') {
            without_images = format!(
                "{}{}",
                &without_images[..start],
                without_images[start + end + 1..].trim_start_matches(' ')
            );
        } else {
            break;
        }
    }
    without_images
}

fn encode_cwd_path(cwd: &str) -> String {
    cwd.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '_' | '.' => '-',
            _ => c,
        })
        .collect()
}

fn extract_prompt_text(message: &Value) -> String {
    let raw = match message.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => {
            // Find first text block
            arr.iter()
                .filter_map(|block| {
                    if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                        block
                            .get("text")
                            .and_then(|t| t.as_str())
                            .map(|s| s.to_string())
                    } else {
                        None
                    }
                })
                .next()
                .unwrap_or_default()
        }
        _ => return String::new(),
    };

    // Clean up: remove image markers, code blocks, markdown headers
    let cleaned: String = raw
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with("```"))
        .collect::<Vec<_>>()
        .join(" ");

    // Remove [Image #N] markers
    let mut result = cleaned;
    while let Some(start) = result.find("[Image") {
        if let Some(end) = result[start..].find(']') {
            result = format!(
                "{}{}",
                &result[..start],
                result[start + end + 1..].trim_start()
            );
        } else {
            break;
        }
    }

    let clean = result.trim().to_string();
    if clean.is_empty() {
        return String::new();
    }
    // Skip prompts generated by abtop's own summary generation (claude --print)
    if clean.contains("You are a conversation title generator") {
        return String::new();
    }
    truncate(&clean, 50)
}

fn extract_tool_arg(tool_use: &Value) -> String {
    if let Some(input) = tool_use.get("input") {
        // Edit/Read: file_path
        if let Some(fp) = input.get("file_path").and_then(|f| f.as_str()) {
            return shorten_path(fp);
        }
        // Bash: command (first 40 chars, redact secrets)
        if let Some(cmd) = input.get("command").and_then(|c| c.as_str()) {
            let short = cmd.lines().next().unwrap_or(cmd);
            return super::redact_secrets(&truncate(short, 40));
        }
        // Grep/Glob: pattern
        if let Some(pat) = input.get("pattern").and_then(|p| p.as_str()) {
            return truncate(pat, 40);
        }
    }
    String::new()
}

fn shorten_path(path: &str) -> String {
    #[cfg(windows)]
    let parts: Vec<&str> = path.rsplit(['/', '\\']).collect();
    #[cfg(not(windows))]
    let parts: Vec<&str> = path.rsplit('/').collect();
    if parts.len() <= 2 {
        path.to_string()
    } else {
        format!("{}/{}", parts[1], parts[0])
    }
}

fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max - 1).collect();
        format!("{}…", truncated)
    }
}

/// Returns the ordered list of Claude Code settings files to check, from
/// highest to lowest priority, matching Claude Code's own resolution order:
/// 1. `{cwd}/.claude/settings.local.json`
/// 2. `{cwd}/.claude/settings.json`
/// 3. `~/.claude/settings.local.json`
/// 4. `~/.claude/settings.json`
fn settings_candidate_paths(cwd: &str) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    let cwd_path = PathBuf::from(cwd);
    candidates.push(cwd_path.join(".claude").join("settings.local.json"));
    candidates.push(cwd_path.join(".claude").join("settings.json"));
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".claude").join("settings.local.json"));
        candidates.push(home.join(".claude").join("settings.json"));
    }
    candidates
}

/// Read the persistent `effortLevel` for a Claude Code session.
///
/// Precedence (highest wins), matching Claude Code's own resolution order:
/// 1. `CLAUDE_CODE_EFFORT_LEVEL` env var (abtop's own env — only visible when
///    set in the user's shell before launching both abtop and claude)
/// 2. `{cwd}/.claude/settings.local.json`
/// 3. `{cwd}/.claude/settings.json`
/// 4. `~/.claude/settings.local.json`
/// 5. `~/.claude/settings.json`
///
/// Returns an empty string when no `effortLevel` is set. This does NOT capture
/// in-session `/effort` changes — those are ephemeral and not written to disk.
fn read_effort_level(cwd: &str) -> String {
    if let Ok(v) = std::env::var("CLAUDE_CODE_EFFORT_LEVEL") {
        let trimmed = v.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    for path in settings_candidate_paths(cwd) {
        if let Some(level) = read_effort_from_settings(&path) {
            return level;
        }
    }
    String::new()
}

fn read_effort_from_settings(path: &Path) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    let val: Value = serde_json::from_str(&content).ok()?;
    let level = val.get("effortLevel")?.as_str()?.trim();
    if level.is_empty() {
        None
    } else {
        Some(level.to_string())
    }
}

/// Read the configured model from Claude Code's settings files.
///
/// Precedence (highest wins), matching Claude Code's own resolution order:
/// 1. `CLAUDE_CODE_MODEL` env var (abtop's own env — only visible when
///    set in the user's shell before launching both abtop and claude)
/// 2. `{cwd}/.claude/settings.local.json`
/// 3. `{cwd}/.claude/settings.json`
/// 4. `~/.claude/settings.local.json`
/// 5. `~/.claude/settings.json`
///
/// Returns an empty string when no model is configured. The value may include
/// the `[1m]` suffix (e.g. `"sonnet[1m]"`) which is used to detect 1M context.
fn read_configured_model(cwd: &str) -> String {
    if let Ok(v) = std::env::var("CLAUDE_CODE_MODEL") {
        let trimmed = v.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    for path in settings_candidate_paths(cwd) {
        if let Some(model) = read_model_from_settings(&path) {
            return model;
        }
    }
    String::new()
}

fn read_model_from_settings(path: &Path) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    let val: Value = serde_json::from_str(&content).ok()?;
    let model = val.get("model")?.as_str()?.trim();
    if model.is_empty() {
        None
    } else {
        Some(model.to_string())
    }
}

/// Parse a NUL-separated environ blob to extract a single variable's value.
/// Only invoked from the Linux-gated `read_env_var_from_proc`; kept available
/// to unit tests on all platforms, so suppress dead_code on non-Linux.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_environ_var(data: &[u8], var_name: &str) -> Option<String> {
    let prefix = format!("{}=", var_name);
    data.split(|&b| b == 0)
        .filter_map(|entry| std::str::from_utf8(entry).ok())
        .find(|entry| entry.starts_with(&prefix))
        .map(|entry| entry[prefix.len()..].to_string())
}

/// Read a single environment variable from a running process via /proc/<pid>/environ.
/// Returns None if the process is inaccessible or the variable is not set.
#[cfg(target_os = "linux")]
fn read_env_var_from_proc(pid: u32, var_name: &str) -> Option<String> {
    let data = fs::read(format!("/proc/{}/environ", pid)).ok()?;
    parse_environ_var(&data, var_name)
}

/// Stub for non-Linux platforms where /proc is not available.
/// Windows has no equivalent way to read another process's environment block
/// without elevated privileges, so per-process `CLAUDE_CONFIG_DIR` overrides
/// can't be detected. Configured roots, home sibling profile discovery, and
/// abtop's own env are the portable signals there.
///
/// On macOS, `ps eww`/`KERN_PROCARGS2` are unreliable: the kernel truncates
/// the env block to ~120 chars for non-root callers, so `CLAUDE_CONFIG_DIR`
/// is rarely visible. Discovery of profile sessions can still use configured
/// roots, home sibling profiles, and `libproc` open-FD inspection when Claude
/// exposes session/project paths.
#[cfg(not(target_os = "linux"))]
fn read_env_var_from_proc(_pid: u32, _var_name: &str) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // ---- detect_launch_surface ----

    #[test]
    fn detect_launch_surface_desktop_app_macos() {
        let cmd = "/Users/a/Library/Application Support/Claude/claude-code/2.1.266/claude.app/Contents/MacOS/claude";
        assert_eq!(
            ClaudeCollector::detect_launch_surface(cmd),
            LaunchSurface::App
        );
    }

    #[test]
    fn detect_launch_surface_desktop_app_windows_backslashes() {
        let cmd = r#"C:\Users\a\AppData\Roaming\Claude\claude-code\2.1.266\claude.exe"#;
        assert_eq!(
            ClaudeCollector::detect_launch_surface(cmd),
            LaunchSurface::App
        );
    }

    #[test]
    fn detect_launch_surface_vscode_extension() {
        let cmd = "/Users/a/.vscode/extensions/anthropic.claude-code-2.1.269-darwin-arm64/resources/native-binary/claude";
        assert_eq!(
            ClaudeCollector::detect_launch_surface(cmd),
            LaunchSurface::Ide
        );
    }

    #[test]
    fn detect_launch_surface_plain_cli_install() {
        assert_eq!(
            ClaudeCollector::detect_launch_surface("/usr/local/bin/claude"),
            LaunchSurface::Cli
        );
        assert_eq!(
            ClaudeCollector::detect_launch_surface("claude --session-id abc"),
            LaunchSurface::Cli
        );
    }

    #[test]
    fn detect_launch_surface_autoupdater_versions_layout_is_cli() {
        // The auto-updater's `<name>/versions/<ver>` layout (see
        // `cmd_has_binary_autoupdater_layout` in process.rs) is a plain CLI
        // install, not the desktop app — it must not match on "claude" alone.
        assert_eq!(
            ClaudeCollector::detect_launch_surface(
                "/Users/a/.local/share/claude/versions/2.1.121 --allow-dangerously-skip-permissions",
            ),
            LaunchSurface::Cli
        );
    }

    fn write_lines(file: &mut tempfile::NamedTempFile, lines: &[&str]) {
        for line in lines {
            writeln!(file, "{}", line).unwrap();
        }
        file.flush().unwrap();
    }

    fn write_session_file(path: &Path, pid: u32, session_id: &str, cwd: &Path) {
        // Serialize via serde_json so Windows backslash paths are escaped
        // correctly instead of producing invalid JSON.
        std::fs::write(
            path,
            serde_json::json!({
                "pid": pid,
                "sessionId": session_id,
                "cwd": cwd.to_str().unwrap(),
                "startedAt": 1774715116826u64,
            })
            .to_string(),
        )
        .unwrap();
    }

    fn write_transcript(projects: &Path, cwd: &Path, session_id: &str, prompt: &str) -> PathBuf {
        let transcript_dir = projects.join(encode_cwd_path(cwd.to_str().unwrap()));
        std::fs::create_dir_all(&transcript_dir).unwrap();
        let transcript = transcript_dir.join(format!("{}.jsonl", session_id));
        std::fs::write(
            &transcript,
            format!(
                r#"{{"type":"user","timestamp":"2026-03-28T15:00:00Z","message":{{"role":"user","content":"{}"}}}}
{{"type":"assistant","timestamp":"2026-03-28T15:00:05Z","message":{{"model":"claude-sonnet-4-6","usage":{{"input_tokens":12,"output_tokens":6,"cache_read_input_tokens":3,"cache_creation_input_tokens":0}},"content":[{{"type":"text","text":"done"}}]}}}}
"#,
                prompt
            ),
        )
        .unwrap();
        transcript
    }

    fn make_proc_info(pid: u32, command: &str) -> HashMap<u32, ProcInfo> {
        let mut process_info = HashMap::new();
        process_info.insert(
            pid,
            ProcInfo {
                pid,
                ppid: 1,
                rss_kb: 2048,
                cpu_pct: 0.0,
                command: command.to_string(),
            },
        );
        process_info
    }

    #[test]
    fn test_parse_transcript_no_new_bytes_does_not_set_saw_turn() {
        // Regression for the merge fix: a no-op poll (from_offset == file_len)
        // must return saw_turn=false so the cached pending/thinking markers
        // aren't clobbered by the default-zero timestamps.
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                r#"{"type":"user","timestamp":"2026-03-28T15:00:00Z","message":{"role":"user","content":"hi"}}"#,
                r#"{"type":"assistant","timestamp":"2026-03-28T15:00:05Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"tool_use","name":"Edit","id":"t1","input":{"file_path":"x"}}]}}"#,
            ],
        );
        let file_len = std::fs::metadata(file.path()).unwrap().len();

        let result = parse_transcript(file.path(), file_len);

        assert!(!result.saw_turn);
        assert_eq!(result.last_user_ts_ms, 0);
        assert_eq!(result.last_assistant_ts_ms, 0);
        assert_eq!(result.new_offset, file_len);
    }

    #[cfg(unix)]
    #[test]
    fn test_file_identity_is_stable_across_append() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[r#"{"type":"assistant","message":{"usage":{"input_tokens":1}}}"#],
        );
        let before = file_identity(file.path());

        std::thread::sleep(std::time::Duration::from_millis(20));
        write_lines(
            &mut file,
            &[r#"{"type":"assistant","message":{"usage":{"input_tokens":2}}}"#],
        );
        let after = file_identity(file.path());

        assert_eq!(
            before, after,
            "appending to an existing transcript must keep the same file identity so the collector can tail new bytes"
        );
    }

    #[test]
    fn test_parse_transcript_non_turn_lines_do_not_set_saw_turn() {
        // A delta that only processes non-user/non-assistant entries (e.g.
        // `summary` lines emitted on compaction) must also leave saw_turn
        // false, so the merge step preserves the cached turn state.
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[r#"{"type":"summary","summary":"compaction marker","leafUuid":"abc"}"#],
        );

        let result = parse_transcript(file.path(), 0);

        assert!(!result.saw_turn);
        assert_eq!(result.last_user_ts_ms, 0);
        assert_eq!(result.last_assistant_ts_ms, 0);
        assert!(result.new_offset > 0, "non-turn lines still advance offset");
    }

    #[test]
    fn test_parse_transcript_tool_result_does_not_open_thinking_window() {
        // Regression: status used to flicker Think ↔ Wait during tool
        // loops because tool_result lines come back as `user`-role
        // messages, and the parser treated them as the start of a new
        // thinking window. Real flow:
        //   1. assistant(tool_use) → last_user cleared (Wait)
        //   2. user(tool_result)   → last_user set    (Think) ← bug
        //   3. assistant(next)     → last_user cleared (Wait)
        // Fix: skip tool_result wrappers when updating last_user_ts_ms.
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                r#"{"type":"user","timestamp":"2026-03-28T15:00:00Z","message":{"role":"user","content":"hi"}}"#,
                r#"{"type":"assistant","timestamp":"2026-03-28T15:00:01Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"tool_use","name":"Bash","id":"t1","input":{"command":"ls"}}]}}"#,
                r#"{"type":"user","timestamp":"2026-03-28T15:00:02Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"a\nb"}]}}"#,
            ],
        );

        let result = parse_transcript(file.path(), 0);

        assert!(result.saw_turn);
        // After the tool_result line, last_user_ts_ms must STILL be 0
        // — the assistant turn at 15:00:01 cleared it, and the
        // tool_result wrapper at 15:00:02 must not re-open the window.
        assert_eq!(
            result.last_user_ts_ms, 0,
            "tool_result user-role line must not reopen the thinking window",
        );
    }

    #[test]
    fn test_parse_transcript_user_then_assistant_clears_thinking_marker() {
        // Sanity check on the mutual exclusion: after an assistant turn
        // closes a thinking window, last_user_ts_ms must be zero and
        // last_assistant_ts_ms must carry the assistant timestamp.
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                r#"{"type":"user","timestamp":"2026-03-28T15:00:00Z","message":{"role":"user","content":"hi"}}"#,
                r#"{"type":"assistant","timestamp":"2026-03-28T15:00:05Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"tool_use","name":"Edit","id":"t1","input":{"file_path":"x"}}]}}"#,
            ],
        );

        let result = parse_transcript(file.path(), 0);

        assert!(result.saw_turn);
        assert_eq!(result.last_user_ts_ms, 0);
        assert!(result.last_assistant_ts_ms > 0);
    }

    #[test]
    fn test_parse_transcript_trailing_user_marks_thinking_window() {
        // When the latest line is a user turn (prompt or tool_result),
        // last_user_ts_ms should carry its timestamp so the UI can render
        // the live Think row.
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                r#"{"type":"assistant","timestamp":"2026-03-28T15:00:00Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"ok"}]}}"#,
                r#"{"type":"user","timestamp":"2026-03-28T15:00:10Z","message":{"role":"user","content":"next"}}"#,
            ],
        );

        let result = parse_transcript(file.path(), 0);

        assert!(result.saw_turn);
        assert!(result.last_user_ts_ms > 0);
        assert_eq!(result.last_assistant_ts_ms, 0);
    }

    #[test]
    fn test_parse_transcript_text_only_assistant_does_not_set_pending_ts() {
        // Regression: a terminal text-only assistant turn (final "done"
        // message with no tool_use) used to leave last_assistant_ts_ms
        // set to its timestamp, which leaked into pending_since_ms and
        // made the UI tool-duration bar render a phantom growing
        // duration after the model had finished. Fix: only record the
        // assistant timestamp when the turn contains tool_use blocks.
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                r#"{"type":"user","timestamp":"2026-03-28T15:00:00Z","message":{"role":"user","content":"hi"}}"#,
                r#"{"type":"assistant","timestamp":"2026-03-28T15:00:05Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"all done"}]}}"#,
            ],
        );

        let result = parse_transcript(file.path(), 0);

        assert!(result.saw_turn);
        assert_eq!(
            result.last_assistant_ts_ms, 0,
            "text-only assistant turn must not record a pending timestamp",
        );
    }

    #[test]
    fn test_parse_transcript_fresh_session_uses_cache_creation_for_context() {
        // Regression: on a fresh session's first turn the usage block
        // reports cache_read=0 and cache_creation>0 (the prompt is being
        // cached for the first time). Using `inp + cr` would underreport
        // the context as just the input tokens, hiding the real prompt
        // size from the gauge. Fall back to `inp + cc` in that case.
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                r#"{"type":"user","timestamp":"2026-03-28T15:00:00Z","message":{"role":"user","content":"hi"}}"#,
                r#"{"type":"assistant","timestamp":"2026-03-28T15:00:05Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":4,"output_tokens":2,"cache_read_input_tokens":0,"cache_creation_input_tokens":12000},"content":[{"type":"text","text":"hello"}]}}"#,
            ],
        );

        let result = parse_transcript(file.path(), 0);

        assert_eq!(result.last_context_tokens, 12_004);
    }

    #[test]
    fn test_parse_transcript_slash_command_does_not_open_thinking_window() {
        // Regression: `/plugin update` (and other pure-local slash commands)
        // flush 2-3 user-role lines into the transcript and never produce an
        // assistant reply. Before the fix, the trailing `<local-command-stdout>`
        // line set last_user_ts_ms and the session was pinned in Thinking
        // forever (Think generating reply). All three line shapes must be
        // treated as synthetic.
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                // Real prompt + assistant reply to seed a clean Wait state.
                r#"{"type":"user","timestamp":"2026-03-28T15:00:00Z","message":{"role":"user","content":"hi"}}"#,
                r#"{"type":"assistant","timestamp":"2026-03-28T15:00:01Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"hello"}]}}"#,
                // The three user-role lines `/plugin update` writes:
                r#"{"type":"user","timestamp":"2026-03-28T15:01:00Z","isMeta":true,"message":{"role":"user","content":"<local-command-caveat>Caveat: ...</local-command-caveat>"}}"#,
                r#"{"type":"user","timestamp":"2026-03-28T15:01:00Z","message":{"role":"user","content":"<command-name>/plugin</command-name>\n<command-args>update foo</command-args>"}}"#,
                r#"{"type":"user","timestamp":"2026-03-28T15:01:00Z","message":{"role":"user","content":"<local-command-stdout>Updated foo</local-command-stdout>"}}"#,
            ],
        );

        let result = parse_transcript(file.path(), 0);

        assert!(result.saw_turn);
        assert_eq!(
            result.last_user_ts_ms, 0,
            "/plugin local-command lines must not reopen the thinking window",
        );
        // Title must come from the real prompt, not the synthetic lines.
        assert_eq!(result.initial_prompt, "hi");
    }

    #[test]
    fn test_parse_transcript_bash_input_does_not_open_thinking_window() {
        // `!ls` and friends serialize as <bash-input>/<bash-stdout> user-role
        // lines with no assistant reply. Same failure mode as /plugin.
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                r#"{"type":"assistant","timestamp":"2026-03-28T15:00:00Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"ok"}]}}"#,
                r#"{"type":"user","timestamp":"2026-03-28T15:00:05Z","message":{"role":"user","content":"<bash-input>ls</bash-input>"}}"#,
                r#"{"type":"user","timestamp":"2026-03-28T15:00:05Z","message":{"role":"user","content":"<bash-stdout>a\nb</bash-stdout><bash-stderr></bash-stderr>"}}"#,
            ],
        );

        let result = parse_transcript(file.path(), 0);

        assert!(result.saw_turn);
        assert_eq!(
            result.last_user_ts_ms, 0,
            "<bash-input>/<bash-stdout> lines must not reopen the thinking window",
        );
    }

    #[test]
    fn test_parse_lsof_process_info_captures_multiple_pids_and_cwd() {
        let output = "\
p111
fcwd
tDIR
n/Users/alice/project
f15
tDIR
n/Users/alice/.claude-work
p222
fcwd
tDIR
n/Users/bob/project
f20
tREG
n/Users/bob/.claude-alt/projects/-Users-bob-project/session.jsonl
";

        let parsed = parse_lsof_process_info(output);

        assert_eq!(parsed.len(), 2);
        assert_eq!(
            parsed.get(&111).unwrap().cwd.as_deref(),
            Some(Path::new("/Users/alice/project")),
        );
        assert!(parsed.get(&222).unwrap().paths.contains(&PathBuf::from(
            "/Users/bob/.claude-alt/projects/-Users-bob-project/session.jsonl"
        )));
    }

    #[test]
    fn test_config_dirs_from_open_paths_finds_profile_root_and_ignores_project_claude_dir() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".claude-work");
        std::fs::create_dir_all(profile.join("sessions")).unwrap();
        std::fs::create_dir_all(profile.join("projects")).unwrap();

        let project_claude = temp.path().join("repo").join(".claude");
        std::fs::create_dir_all(&project_claude).unwrap();
        std::fs::write(project_claude.join("settings.local.json"), "{}").unwrap();

        let info = ProcessOpenPaths {
            cwd: Some(temp.path().join("repo")),
            paths: vec![
                project_claude,
                profile.clone(),
                profile
                    .join("projects")
                    .join("-tmp-repo")
                    .join("session.jsonl"),
            ],
        };

        let configs = config_dirs_from_open_paths(&info);

        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].base_dir(), profile);
    }

    #[test]
    fn test_config_dirs_from_open_paths_finds_profile_root_without_cwd() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".claude-work");
        std::fs::create_dir_all(profile.join("sessions")).unwrap();
        let projects = profile.join("projects");
        std::fs::create_dir_all(&projects).unwrap();

        let info = ProcessOpenPaths {
            cwd: None,
            paths: vec![projects.join("-tmp-repo").join("session.jsonl")],
        };

        let configs = config_dirs_from_open_paths(&info);

        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].base_dir(), profile);
    }

    #[test]
    fn test_discover_home_claude_config_dirs_finds_profile_siblings() {
        let temp = tempfile::tempdir().unwrap();
        let personal = temp.path().join(".claude-personal");
        let work = temp.path().join(".claude-work-team");
        let incomplete = temp.path().join(".claude-cache");
        std::fs::create_dir_all(personal.join("sessions")).unwrap();
        std::fs::create_dir_all(personal.join("projects")).unwrap();
        std::fs::create_dir_all(work.join("sessions")).unwrap();
        std::fs::create_dir_all(work.join("projects")).unwrap();
        std::fs::create_dir_all(incomplete.join("sessions")).unwrap();

        let discovered = discover_home_claude_config_dirs(temp.path());

        assert!(discovered.contains(&personal));
        assert!(discovered.contains(&work));
        assert!(!discovered.contains(&incomplete));
    }

    #[test]
    fn test_refresh_config_dirs_includes_configured_profile_roots() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".claude-personal");
        std::fs::create_dir_all(profile.join("sessions")).unwrap();
        std::fs::create_dir_all(profile.join("projects")).unwrap();

        let mut collector = ClaudeCollector::with_configured_dirs(vec![profile.clone()]);
        collector.refresh_config_dirs(&HashMap::new());

        assert!(
            collector
                .config_dirs
                .iter()
                .any(|config| config.base_dir() == profile),
            "configured Claude profile root was not retained"
        );
    }

    #[test]
    fn test_session_paths_from_open_paths_maps_pid_to_session_file() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".claude-work");
        let sessions = profile.join("sessions");
        let projects = profile.join("projects");
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();

        let pid = 4242;
        let session_path = sessions.join(format!("{}.json", pid));
        write_session_file(&session_path, pid, "session-4242", &cwd);

        let mut open_paths = HashMap::new();
        open_paths.insert(
            pid,
            ProcessOpenPaths {
                cwd: None,
                paths: vec![projects.join("-tmp-repo").join("session-4242.jsonl")],
            },
        );

        let discovered = ClaudeCollector::session_paths_from_open_paths(&[pid], &open_paths);

        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].0, session_path);
        assert_eq!(discovered[0].1.base_dir(), profile);
    }

    #[test]
    fn test_session_paths_from_open_paths_deduplicates_same_session_path() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".claude-work");
        let sessions = profile.join("sessions");
        let projects = profile.join("projects");
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();

        let pid = 4242;
        let session_path = sessions.join(format!("{}.json", pid));
        write_session_file(&session_path, pid, "session-4242", &cwd);

        let mut open_paths = HashMap::new();
        open_paths.insert(
            pid,
            ProcessOpenPaths {
                cwd: None,
                paths: vec![
                    projects.join("-tmp-repo").join("session-4242.jsonl"),
                    sessions.join(format!("{}.json", pid)),
                    profile.clone(),
                ],
            },
        );

        let discovered = ClaudeCollector::session_paths_from_open_paths(&[pid], &open_paths);

        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].0, session_path);
    }

    #[test]
    fn test_collect_sessions_deduplicates_active_and_scanned_session_id() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".claude-work");
        let sessions = profile.join("sessions");
        let projects = profile.join("projects");
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();

        let pid = 4242;
        let session_id = "session-4242";
        let session_path = sessions.join(format!("{}.json", pid));
        write_session_file(&session_path, pid, session_id, &cwd);
        write_transcript(&projects, &cwd, session_id, "dedup prompt");

        let config = ConfigDir::new(profile.clone());
        let process_info = make_proc_info(pid, "claude");
        let mut collector = ClaudeCollector::new();
        collector.config_dirs = vec![config.clone()];
        let session_paths = vec![
            (session_path.clone(), config.clone()),
            (session_path.clone(), config),
        ];
        let ctx = build_discovery_context(&session_paths, &process_info, 0);
        let sessions = collector.load_session_paths(
            &session_paths,
            &process_info,
            &HashMap::new(),
            &HashMap::new(),
            &ctx,
        );

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, session_id);
    }

    #[test]
    fn test_find_session_file_for_pid_falls_back_to_embedded_pid() {
        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        let session_path = sessions.join("custom-name.json");
        write_session_file(&session_path, 4242, "session-4242", &cwd);

        assert_eq!(
            find_session_file_for_pid(&sessions, 4242).as_deref(),
            Some(session_path.as_path()),
        );
    }

    #[test]
    fn test_find_session_file_for_pid_skips_bad_files_and_continues() {
        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();

        // A malformed JSON file that appears before the real one on-disk.
        // Previously the `?` operator made one bad file abort the whole
        // fallback search; now the loop must skip it and still find the match.
        std::fs::write(sessions.join("aaa-broken.json"), "not json at all").unwrap();
        let target = sessions.join("zzz-valid.json");
        write_session_file(&target, 9999, "session-9999", &cwd);

        assert_eq!(
            find_session_file_for_pid(&sessions, 9999).as_deref(),
            Some(target.as_path()),
        );
    }

    #[test]
    fn test_find_session_file_for_pid_prefers_direct_pid_file() {
        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        let direct = sessions.join("4242.json");
        let fallback = sessions.join("custom-name.json");
        write_session_file(&direct, 4242, "direct", &cwd);
        write_session_file(&fallback, 4242, "fallback", &cwd);

        assert_eq!(
            find_session_file_for_pid(&sessions, 4242).as_deref(),
            Some(direct.as_path()),
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_find_session_file_for_pid_rejects_symlinked_session_files() {
        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();

        let real_direct = temp.path().join("real-direct.json");
        write_session_file(&real_direct, 4242, "direct", &cwd);
        std::os::unix::fs::symlink(&real_direct, sessions.join("4242.json")).unwrap();

        let real_fallback = temp.path().join("real-fallback.json");
        write_session_file(&real_fallback, 4242, "fallback", &cwd);
        std::os::unix::fs::symlink(&real_fallback, sessions.join("fallback.json")).unwrap();

        assert!(find_session_file_for_pid(&sessions, 4242).is_none());
    }

    #[test]
    fn test_find_claude_pids_excludes_self_spawned_print_sessions() {
        // Simulate abtop running as PID 99 with a `claude --print` summary
        // child it spawned (PID 11, ppid=99) AND an unrelated user-launched
        // `claude --print` session (PID 13, ppid=1). The self-spawned child
        // must be filtered; the user-launched non-interactive session must
        // be tracked.
        let abtop_pid = 99u32;
        let mut process_info = HashMap::new();
        process_info.insert(
            abtop_pid,
            ProcInfo {
                pid: abtop_pid,
                ppid: 1,
                rss_kb: 1,
                cpu_pct: 0.0,
                command: "abtop".to_string(),
            },
        );
        process_info.insert(
            10,
            ProcInfo {
                pid: 10,
                ppid: 1,
                rss_kb: 1,
                cpu_pct: 0.0,
                command: "claude".to_string(),
            },
        );
        process_info.insert(
            11,
            ProcInfo {
                pid: 11,
                ppid: abtop_pid,
                rss_kb: 1,
                cpu_pct: 0.0,
                command: "claude --print summarize".to_string(),
            },
        );
        process_info.insert(
            12,
            ProcInfo {
                pid: 12,
                ppid: 1,
                rss_kb: 1,
                cpu_pct: 0.0,
                command: "codex".to_string(),
            },
        );
        process_info.insert(
            13,
            ProcInfo {
                pid: 13,
                ppid: 1,
                rss_kb: 1,
                cpu_pct: 0.0,
                command: "claude --print user-script".to_string(),
            },
        );

        let mut got = ClaudeCollector::find_claude_pids(&process_info, abtop_pid);
        got.sort_unstable();
        assert_eq!(got, vec![10, 13]);
    }

    #[test]
    fn test_resolve_project_dir_uses_worktree_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".claude-work");
        let projects = profile.join("projects");
        std::fs::create_dir_all(profile.join("sessions")).unwrap();
        std::fs::create_dir_all(&projects).unwrap();

        // The transcript lives in a dir that does NOT match encode_cwd_path.
        let worktree_dir = projects.join("actual-worktree");
        std::fs::create_dir_all(&worktree_dir).unwrap();
        std::fs::write(worktree_dir.join("session-1.jsonl"), "{}\n").unwrap();

        let config = ConfigDir::new(profile);

        assert_eq!(
            resolve_project_dir(&config, "/tmp/repo", "session-1").as_deref(),
            Some(worktree_dir.as_path()),
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_resolve_project_dir_rejects_symlinked_matches() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".claude-work");
        let projects = profile.join("projects");
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(profile.join("sessions")).unwrap();
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();

        let session_id = "session-1";
        let real_exact = temp.path().join("real-exact.jsonl");
        std::fs::write(&real_exact, "{}\n").unwrap();
        let exact_dir = projects.join(encode_cwd_path(cwd.to_str().unwrap()));
        std::fs::create_dir_all(&exact_dir).unwrap();
        std::os::unix::fs::symlink(&real_exact, exact_dir.join(format!("{}.jsonl", session_id)))
            .unwrap();

        let real_fallback = temp.path().join("real-fallback.jsonl");
        std::fs::write(&real_fallback, "{}\n").unwrap();
        let fallback_dir = projects.join("actual-worktree");
        std::fs::create_dir_all(&fallback_dir).unwrap();
        std::os::unix::fs::symlink(
            &real_fallback,
            fallback_dir.join(format!("{}.jsonl", session_id)),
        )
        .unwrap();

        let config = ConfigDir::new(profile);

        // Both exact and fallback are symlinks and must be rejected. The
        // encoded-cwd dir exists though, so the final fallback returns it.
        assert_eq!(
            resolve_project_dir(&config, cwd.to_str().unwrap(), session_id).as_deref(),
            Some(exact_dir.as_path()),
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_open_paths_without_cwd_loads_session_from_same_config_root() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".claude-work");
        let sessions = profile.join("sessions");
        let projects = profile.join("projects");
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();

        let pid = 5151;
        let session_id = "session-5151";
        let session_path = sessions.join(format!("{}.json", pid));
        write_session_file(&session_path, pid, session_id, &cwd);
        let transcript = write_transcript(&projects, &cwd, session_id, "libproc prompt");

        let mut open_paths = HashMap::new();
        open_paths.insert(
            pid,
            ProcessOpenPaths {
                cwd: None,
                paths: vec![transcript],
            },
        );
        let discovered = ClaudeCollector::session_paths_from_open_paths(&[pid], &open_paths);
        let process_info = make_proc_info(pid, "claude");
        let mut collector = ClaudeCollector::new();

        assert_eq!(discovered.len(), 1);
        let ctx = build_discovery_context(&discovered, &process_info, 0);
        let session = collector
            .load_session(
                &discovered[0].0,
                &discovered[0].1,
                &process_info,
                &HashMap::new(),
                &HashMap::new(),
                &ctx,
            )
            .unwrap();

        assert_eq!(session.session_id, session_id);
        assert_eq!(session.initial_prompt, "libproc prompt");
        assert_eq!(session.total_input_tokens, 12);
    }

    #[test]
    fn test_load_session_uses_non_default_config_root() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".claude-work");
        let sessions = profile.join("sessions");
        let projects = profile.join("projects");
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();

        let pid = 5151;
        let session_id = "session-5151";
        let session_path = sessions.join(format!("{}.json", pid));
        write_session_file(&session_path, pid, session_id, &cwd);

        let transcript_dir = projects.join(encode_cwd_path(cwd.to_str().unwrap()));
        std::fs::create_dir_all(&transcript_dir).unwrap();
        std::fs::write(
            transcript_dir.join(format!("{}.jsonl", session_id)),
            r#"{"type":"user","timestamp":"2026-03-28T15:00:00Z","version":"2.1.90","gitBranch":"main","message":{"role":"user","content":"profile specific prompt"}}
{"type":"assistant","timestamp":"2026-03-28T15:00:05Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":12,"output_tokens":6,"cache_read_input_tokens":3,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"done"}]}}
"#,
        )
        .unwrap();

        let mut collector = ClaudeCollector::new();
        let process_info = make_proc_info(pid, "claude");
        let children_map = HashMap::new();
        let ports = HashMap::new();
        let config = ConfigDir::new(profile);
        let ctx =
            build_discovery_context(&[(session_path.clone(), config.clone())], &process_info, 0);

        let session = collector
            .load_session(
                &session_path,
                &config,
                &process_info,
                &children_map,
                &ports,
                &ctx,
            )
            .unwrap();

        assert_eq!(session.session_id, session_id);
        assert_eq!(session.cwd, cwd.to_str().unwrap());
        assert_eq!(session.total_input_tokens, 12);
        assert_eq!(session.total_cache_read, 3);
        assert_eq!(session.initial_prompt, "profile specific prompt");
    }

    #[test]
    fn test_load_session_detects_incremental_compaction_across_cached_boundary() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".claude");
        let sessions = profile.join("sessions");
        let projects = profile.join("projects");
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();

        let pid = 5252;
        let session_id = "session-5252";
        let session_path = sessions.join(format!("{}.json", pid));
        write_session_file(&session_path, pid, session_id, &cwd);

        let transcript_dir = projects.join(encode_cwd_path(cwd.to_str().unwrap()));
        std::fs::create_dir_all(&transcript_dir).unwrap();
        let transcript_path = transcript_dir.join(format!("{}.jsonl", session_id));
        std::fs::write(
            &transcript_path,
            r#"{"type":"user","timestamp":"2026-03-28T15:00:00Z","message":{"role":"user","content":"start"}}
{"type":"assistant","timestamp":"2026-03-28T15:00:05Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":100000,"output_tokens":50,"cache_read_input_tokens":100000,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"first"}]}}
"#,
        )
        .unwrap();

        let mut collector = ClaudeCollector::new();
        let process_info = make_proc_info(pid, "claude");
        let children_map = HashMap::new();
        let ports = HashMap::new();
        let config = ConfigDir::new(profile);
        let ctx =
            build_discovery_context(&[(session_path.clone(), config.clone())], &process_info, 0);

        let first = collector
            .load_session(
                &session_path,
                &config,
                &process_info,
                &children_map,
                &ports,
                &ctx,
            )
            .unwrap();
        assert_eq!(first.compaction_count, 0);

        let mut transcript = std::fs::OpenOptions::new()
            .append(true)
            .open(&transcript_path)
            .unwrap();
        writeln!(
            transcript,
            r#"{{"type":"user","timestamp":"2026-03-28T15:01:00Z","message":{{"role":"user","content":"continue"}}}}"#
        )
        .unwrap();
        writeln!(
            transcript,
            r#"{{"type":"assistant","timestamp":"2026-03-28T15:01:05Z","message":{{"model":"claude-sonnet-4-6","usage":{{"input_tokens":20000,"output_tokens":30,"cache_read_input_tokens":100,"cache_creation_input_tokens":0}},"content":[{{"type":"text","text":"after compact"}}]}}}}"#
        )
        .unwrap();
        transcript.flush().unwrap();

        let second = collector
            .load_session(
                &session_path,
                &config,
                &process_info,
                &children_map,
                &ports,
                &ctx,
            )
            .unwrap();

        assert_eq!(second.compaction_count, 1);
    }

    #[test]
    fn test_parse_transcript_basic_tokens() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                r#"{"type":"user","timestamp":"2026-03-28T15:00:00Z","version":"2.1.86","gitBranch":"main","message":{"role":"user","content":"fix the bug"}}"#,
                r#"{"type":"assistant","timestamp":"2026-03-28T15:00:05Z","message":{"role":"assistant","model":"claude-sonnet-4-6","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":200,"cache_creation_input_tokens":30},"content":[{"type":"text","text":"I found the issue."}]}}"#,
            ],
        );
        let result = parse_transcript(file.path(), 0);
        assert_eq!(result.total_input, 100);
        assert_eq!(result.total_output, 50);
        assert_eq!(result.total_cache_read, 200);
        assert_eq!(result.total_cache_create, 30);
        assert_eq!(result.model, "claude-sonnet-4-6");
        assert_eq!(result.turn_count, 1);
        assert_eq!(result.last_context_tokens, 300); // 100 + 200 (cache_creation excluded)
    }

    #[test]
    fn test_parse_transcript_uses_latest_custom_title() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                r#"{"type":"custom-title","customTitle":"Old name"}"#,
                r#"{"type":"custom-title","customTitle":"한국어 대화"}"#,
            ],
        );

        let result = parse_transcript(file.path(), 0);
        assert_eq!(result.session_name, "한국어 대화");
    }

    #[test]
    fn test_parse_transcript_multiple_turns() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                r#"{"type":"user","timestamp":"2026-03-28T15:00:00Z","message":{"role":"user","content":"first prompt"}}"#,
                r#"{"type":"assistant","timestamp":"2026-03-28T15:00:05Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"First response."}]}}"#,
                r#"{"type":"user","timestamp":"2026-03-28T15:01:00Z","message":{"role":"user","content":"second prompt"}}"#,
                r#"{"type":"assistant","timestamp":"2026-03-28T15:01:05Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":200,"output_tokens":80,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"Second response."}]}}"#,
            ],
        );
        let result = parse_transcript(file.path(), 0);
        assert_eq!(result.turn_count, 2);
        assert_eq!(result.total_input, 300); // 100 + 200
        assert_eq!(result.total_output, 130); // 50 + 80
        assert_eq!(result.token_history.len(), 2);
    }

    #[test]
    fn test_parse_transcript_chat_keeps_long_multiline_reply() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let reply = format!("## Result\\n\\n- first\\n- second\\n{}", "x".repeat(1_000));
        let line = format!(
            r#"{{"type":"assistant","timestamp":"2026-03-28T15:00:05Z","message":{{"model":"claude-sonnet-4-6","usage":{{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}},"content":[{{"type":"text","text":"{reply}"}}]}}}}"#
        );
        write_lines(
            &mut file,
            &[
                r#"{"type":"user","timestamp":"2026-03-28T15:00:00Z","message":{"role":"user","content":"look [Image #1]\nplease"}}"#,
                &line,
            ],
        );
        let result = parse_transcript(file.path(), 0);
        assert_eq!(result.chat_messages[0].text, "look\nplease");
        let text = &result.chat_messages[1].text;
        assert!(
            text.starts_with("## Result\n\n- first\n- second\n"),
            "{text}"
        );
        assert_eq!(
            text.chars().count(),
            1_000 + "## Result\n\n- first\n- second\n".len()
        );
    }

    #[test]
    fn test_parse_transcript_chat_tail_skips_tool_results() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                r#"{"type":"user","timestamp":"2026-03-28T15:00:00Z","message":{"role":"user","content":"fi\u0007x login\u202E"}}"#,
                r#"{"type":"assistant","timestamp":"2026-03-28T15:00:05Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"I'll inspect\u0008 auth."},{"type":"tool_use","name":"Read","input":{"file_path":"src/auth.rs"}}]}}"#,
                r#"{"type":"user","timestamp":"2026-03-28T15:00:06Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"secret output"}]}}"#,
                r#"{"type":"assistant","timestamp":"2026-03-28T15:00:10Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"Root cause is the guard."}]}}"#,
            ],
        );
        let result = parse_transcript(file.path(), 0);
        // User, assistant text, the Read call, its (summarized) result, and the
        // final assistant text. The tool_result body itself never appears.
        assert_eq!(result.chat_messages.len(), 5);
        assert_eq!(result.chat_messages[0].role, ChatRole::User);
        assert_eq!(result.chat_messages[0].text, "fix login");
        assert_eq!(result.chat_messages[1].role, ChatRole::Assistant);
        assert_eq!(result.chat_messages[1].text, "I'll inspect auth.");
        assert_eq!(result.chat_messages[2].role, ChatRole::ToolCall);
        assert_eq!(result.chat_messages[2].text, "Read src/auth.rs");
        assert_eq!(result.chat_messages[3].role, ChatRole::ToolResult);
        assert_eq!(result.chat_messages[4].text, "Root cause is the guard.");
        assert!(result
            .chat_messages
            .iter()
            .all(|msg| !msg.text.contains("secret output")));
    }

    #[test]
    fn test_parse_transcript_file_accesses_sliding_window() {
        // Regression: parse_transcript used to gate pushes on a `< MAX`
        // check, so once the cap was hit during a single parse the latest
        // tool_use entries were silently dropped — leaving the audit log
        // frozen on the *oldest* file accesses. The fix moves the cap to
        // a sliding window applied at end-of-parse, so the last
        // MAX_FILE_ACCESSES entries always survive.
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let extra = 5usize;
        let total = MAX_FILE_ACCESSES + extra;
        let mut lines: Vec<String> = Vec::with_capacity(total);
        for i in 0..total {
            lines.push(format!(
                r#"{{"type":"assistant","timestamp":"2026-03-28T15:00:00Z","message":{{"model":"claude-sonnet-4-6","usage":{{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}},"content":[{{"type":"tool_use","name":"Edit","input":{{"file_path":"src/file_{}.rs"}}}}]}}}}"#,
                i
            ));
        }
        let line_refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        write_lines(&mut file, &line_refs);

        let result = parse_transcript(file.path(), 0);

        assert_eq!(result.file_accesses.len(), MAX_FILE_ACCESSES);
        // Oldest `extra` entries must have been dropped, newest must survive.
        assert_eq!(
            result.file_accesses[0].path,
            format!("src/file_{}.rs", extra)
        );
        assert_eq!(
            result.file_accesses[MAX_FILE_ACCESSES - 1].path,
            format!("src/file_{}.rs", total - 1),
        );
    }

    #[test]
    fn test_parse_transcript_tool_use_current_task() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                r#"{"type":"user","timestamp":"2026-03-28T15:00:00Z","message":{"role":"user","content":"fix the bug"}}"#,
                r#"{"type":"assistant","timestamp":"2026-03-28T15:00:05Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"src/main.rs"}}]}}"#,
            ],
        );
        let result = parse_transcript(file.path(), 0);
        assert_eq!(result.current_task, "Edit src/main.rs");
    }

    #[test]
    fn test_parse_transcript_initial_prompt() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                r#"{"type":"user","timestamp":"2026-03-28T15:00:00Z","message":{"role":"user","content":"refactor the auth module"}}"#,
            ],
        );
        let result = parse_transcript(file.path(), 0);
        assert_eq!(result.initial_prompt, "refactor the auth module");
    }

    #[test]
    fn test_parse_transcript_incremental_offset() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                r#"{"type":"user","timestamp":"2026-03-28T15:00:00Z","message":{"role":"user","content":"first prompt"}}"#,
                r#"{"type":"assistant","timestamp":"2026-03-28T15:00:05Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"First response."}]}}"#,
            ],
        );
        let first = parse_transcript(file.path(), 0);
        let offset = first.new_offset;
        assert!(offset > 0);

        // Append a third line (new assistant turn)
        write_lines(
            &mut file,
            &[
                r#"{"type":"assistant","timestamp":"2026-03-28T15:01:05Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":40,"output_tokens":20,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"Third."}]}}"#,
            ],
        );
        let delta = parse_transcript(file.path(), offset);
        assert_eq!(delta.turn_count, 1);
        assert_eq!(delta.total_input, 40);
        assert_eq!(delta.total_output, 20);
    }

    #[test]
    fn test_parse_transcript_empty_file() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let result = parse_transcript(file.path(), 0);
        assert_eq!(result.model, "-");
        assert_eq!(result.total_input, 0);
        assert_eq!(result.turn_count, 0);
    }

    #[test]
    fn test_encode_cwd_path() {
        assert_eq!(encode_cwd_path("/Users/foo/bar"), "-Users-foo-bar");
        assert_eq!(
            encode_cwd_path("/home/user/my_project.v2"),
            "-home-user-my-project-v2"
        );
    }

    #[test]
    fn test_context_window_for_model() {
        // Base model with low token usage → 200K
        assert_eq!(
            crate::collector::context_window_for_model("claude-opus-4-6", "", 50_000),
            200_000
        );
        // Explicit [1m] suffix in transcript model → 1M regardless of token count
        assert_eq!(
            crate::collector::context_window_for_model("claude-opus-4-6[1m]", "", 0),
            1_000_000
        );
        // [1m] in configured model (from settings.json) → 1M even if transcript lacks it
        assert_eq!(
            crate::collector::context_window_for_model("claude-sonnet-4-6", "sonnet[1m]", 0),
            1_000_000
        );
        assert_eq!(
            crate::collector::context_window_for_model("claude-sonnet-4-6", "", 100_000),
            200_000
        );
        assert_eq!(
            crate::collector::context_window_for_model("unknown-model", "", 0),
            200_000
        );
        // Token usage exceeds 200K → must be 1M window
        assert_eq!(
            crate::collector::context_window_for_model("claude-opus-4-6", "", 250_000),
            1_000_000
        );
    }

    #[test]
    fn test_truncate() {
        assert_eq!(truncate("hello world", 5), "hell…");
        assert_eq!(truncate("hi", 5), "hi");
    }

    #[test]
    fn test_shorten_path() {
        assert_eq!(
            shorten_path("src/collector/claude.rs"),
            "collector/claude.rs"
        );
        assert_eq!(shorten_path("main.rs"), "main.rs");
    }

    #[test]
    fn test_parse_transcript_skips_malformed_json() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                r#"{"type":"user","timestamp":"2026-03-28T15:00:00Z","message":{"role":"user","content":"hi"}}"#,
                r#"THIS IS NOT VALID JSON"#,
                r#"{"type":"assistant","timestamp":"2026-03-28T15:00:05Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"response"}]}}"#,
            ],
        );
        let result = parse_transcript(file.path(), 0);
        // Bad line should be skipped, assistant line still parsed
        assert_eq!(result.turn_count, 1);
        assert_eq!(result.total_input, 100);
        assert_eq!(result.initial_prompt, "hi");
    }

    #[test]
    fn test_parse_transcript_file_shrunk_resets() {
        use std::io::Seek;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                r#"{"type":"user","timestamp":"2026-03-28T15:00:00Z","message":{"role":"user","content":"first"}}"#,
                r#"{"type":"assistant","timestamp":"2026-03-28T15:00:05Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"resp"}]}}"#,
            ],
        );
        let first = parse_transcript(file.path(), 0);
        let old_offset = first.new_offset;
        assert!(old_offset > 0);

        // Simulate file rotation: truncate and write shorter content
        file.as_file().set_len(0).unwrap();
        file.seek(std::io::SeekFrom::Start(0)).unwrap();
        write_lines(
            &mut file,
            &[
                r#"{"type":"assistant","timestamp":"2026-03-28T16:00:00Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"new session"}]}}"#,
            ],
        );
        // Pass old offset that is now beyond file length
        let result = parse_transcript(file.path(), old_offset);
        // Should reset to 0 and parse the new content
        assert_eq!(result.turn_count, 1);
        assert_eq!(result.total_input, 10);
    }

    #[test]
    fn test_parse_transcript_current_task_cleared_between_turns() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                // Turn 1: has tool_use
                r#"{"type":"assistant","timestamp":"2026-03-28T15:00:05Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"src/main.rs"}}]}}"#,
                // Turn 2: text only, no tool_use
                r#"{"type":"assistant","timestamp":"2026-03-28T15:01:05Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"Done, all changes applied."}]}}"#,
            ],
        );
        let result = parse_transcript(file.path(), 0);
        assert_eq!(result.turn_count, 2);
        // current_task should be empty because last turn had no tool_use
        assert_eq!(result.current_task, "");
    }

    #[test]
    fn test_parse_transcript_version_and_git_branch() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                r#"{"type":"user","timestamp":"2026-03-28T15:00:00Z","version":"2.1.90","gitBranch":"feat/payments","message":{"role":"user","content":"add stripe"}}"#,
            ],
        );
        let result = parse_transcript(file.path(), 0);
        assert_eq!(result.version, "2.1.90");
        assert_eq!(result.git_branch, "feat/payments");
    }

    #[test]
    fn test_read_effort_from_settings_extracts_value() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file, r#"{{"effortLevel":"high","other":true}}"#).unwrap();
        file.flush().unwrap();
        assert_eq!(
            read_effort_from_settings(file.path()).as_deref(),
            Some("high")
        );
    }

    #[test]
    fn test_read_effort_from_settings_missing_field_returns_none() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file, r#"{{"permissions":{{"deny":[]}}}}"#).unwrap();
        file.flush().unwrap();
        assert!(read_effort_from_settings(file.path()).is_none());
    }

    #[test]
    fn test_read_effort_from_settings_empty_string_returns_none() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file, r#"{{"effortLevel":""}}"#).unwrap();
        file.flush().unwrap();
        assert!(read_effort_from_settings(file.path()).is_none());
    }

    #[test]
    fn test_read_effort_from_settings_nonexistent_file() {
        assert!(read_effort_from_settings(Path::new("/nonexistent/nowhere.json")).is_none());
    }

    #[test]
    fn test_parse_environ_var_found() {
        let data = b"HOME=/root\0CLAUDE_CONFIG_DIR=/home/user/.claude-pro\0SHELL=/bin/bash\0";
        assert_eq!(
            parse_environ_var(data, "CLAUDE_CONFIG_DIR").as_deref(),
            Some("/home/user/.claude-pro"),
        );
    }

    #[test]
    fn test_parse_environ_var_not_set() {
        let data = b"HOME=/root\0SHELL=/bin/bash\0";
        assert!(parse_environ_var(data, "CLAUDE_CONFIG_DIR").is_none());
    }

    #[test]
    fn test_parse_environ_var_empty_value() {
        let data = b"CLAUDE_CONFIG_DIR=\0OTHER=val\0";
        assert_eq!(
            parse_environ_var(data, "CLAUDE_CONFIG_DIR").as_deref(),
            Some(""),
        );
    }

    #[test]
    fn test_parse_environ_var_no_partial_match() {
        // CLAUDE_CONFIG_DIR_EXTRA should not match CLAUDE_CONFIG_DIR
        let data = b"CLAUDE_CONFIG_DIR_EXTRA=/wrong\0";
        assert!(parse_environ_var(data, "CLAUDE_CONFIG_DIR").is_none());
    }

    fn set_mtime(path: &Path, secs_from_now: i64) {
        let t = if secs_from_now >= 0 {
            std::time::SystemTime::now() + std::time::Duration::from_secs(secs_from_now as u64)
        } else {
            std::time::SystemTime::now() - std::time::Duration::from_secs((-secs_from_now) as u64)
        };
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(t).unwrap();
    }

    #[test]
    fn test_find_live_session_id_picks_newest_jsonl() {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = temp.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();

        let old_path = project_dir.join("old-sid.jsonl");
        let new_path = project_dir.join("new-sid.jsonl");
        std::fs::write(&old_path, "{}\n").unwrap();
        std::fs::write(&new_path, "{}\n").unwrap();
        set_mtime(&old_path, -60);
        set_mtime(&new_path, 0);

        let started_ms = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64)
            .saturating_sub(120_000);

        let sid = find_live_session_id(
            Some(project_dir.as_path()),
            started_ms,
            &std::collections::HashSet::new(),
        );
        assert_eq!(sid.as_deref(), Some("new-sid"));
    }

    #[test]
    fn test_find_live_session_id_filters_by_started_at() {
        // An old jsonl from a prior claude run in the same cwd must not be
        // picked up when started_at is more recent.
        let temp = tempfile::tempdir().unwrap();
        let project_dir = temp.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();

        let stale = project_dir.join("abandoned.jsonl");
        std::fs::write(&stale, "{}\n").unwrap();
        set_mtime(&stale, -3600);

        // Session started 60s ago — the hour-old file must be filtered.
        let started_ms = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64)
            .saturating_sub(60_000);

        let sid = find_live_session_id(
            Some(project_dir.as_path()),
            started_ms,
            &std::collections::HashSet::new(),
        );
        assert!(sid.is_none(), "expected None, got {:?}", sid);
    }

    #[test]
    fn test_find_live_session_id_empty_dir_returns_none() {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = temp.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        assert!(find_live_session_id(
            Some(project_dir.as_path()),
            0,
            &std::collections::HashSet::new()
        )
        .is_none());
    }

    #[test]
    fn test_find_live_session_id_excludes_claimed_sids() {
        // Cross-PID hijack guard: a sibling PID's jsonl (newest on disk)
        // must not be adopted when it's in the `excluded` set.
        let temp = tempfile::tempdir().unwrap();
        let project_dir = temp.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();

        let mine = project_dir.join("mine.jsonl");
        let siblings = project_dir.join("sibling.jsonl");
        std::fs::write(&mine, "{}\n").unwrap();
        std::fs::write(&siblings, "{}\n").unwrap();
        set_mtime(&mine, -60);
        set_mtime(&siblings, 0); // sibling is newer

        let started_ms = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64)
            .saturating_sub(120_000);

        let mut excluded = std::collections::HashSet::new();
        excluded.insert("sibling");
        let sid = find_live_session_id(Some(project_dir.as_path()), started_ms, &excluded);
        assert_eq!(sid.as_deref(), Some("mine"));
    }

    #[test]
    fn test_find_live_session_id_grace_window_boundary() {
        // The 5s grace window must accept a file whose mtime is slightly
        // before started_at (clock skew). One second before started_at is in;
        // ten seconds before is out.
        let temp = tempfile::tempdir().unwrap();
        let project_dir = temp.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();

        let within = project_dir.join("within-grace.jsonl");
        std::fs::write(&within, "{}\n").unwrap();
        set_mtime(&within, -1);

        let outside = project_dir.join("outside-grace.jsonl");
        std::fs::write(&outside, "{}\n").unwrap();
        set_mtime(&outside, -10);

        // started_at = now → 5s grace covers the -1s file but not the -10s one.
        let started_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let sid = find_live_session_id(
            Some(project_dir.as_path()),
            started_ms,
            &std::collections::HashSet::new(),
        );
        assert_eq!(sid.as_deref(), Some("within-grace"));
    }

    #[test]
    fn test_load_session_stale_transcript_is_waiting_even_when_cpu_busy() {
        // Regression: lifetime `%cpu` from ps doesn't tell us whether the
        // agent is doing work *right now* — long-running sessions can
        // average over 1% even when fully idle. Status must drive off
        // recent transcript activity, not lifetime CPU. Here the
        // transcript timestamps are months stale and there is no active
        // descendant; even with cpu_pct=42 the session must read as
        // Waiting.
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".claude");
        let sessions_dir = profile.join("sessions");
        let projects = profile.join("projects");
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();

        let pid = 9101;
        let sid = "stale";
        let session_path = sessions_dir.join(format!("{}.json", pid));
        write_session_file(&session_path, pid, sid, &cwd);
        // write_transcript hardcodes timestamps in 2026-03 → stale by
        // the time the test runs. last_activity reflects the
        // timestamp, not the file mtime, so this is the right way to
        // simulate "no recent activity".
        let _ = write_transcript(&projects, &cwd, sid, "hello");

        let config = ConfigDir::new(profile.clone());
        let mut process_info = make_proc_info(pid, "claude");
        // Lifetime CPU > 1 — would have flipped the previous version
        // to Thinking even though nothing is happening.
        process_info.get_mut(&pid).unwrap().cpu_pct = 42.0;

        let mut collector = ClaudeCollector::new();
        collector.config_dirs = vec![config.clone()];

        let session_paths = vec![(session_path, config)];
        let ctx = build_discovery_context(&session_paths, &process_info, 0);
        let sessions = collector.load_session_paths(
            &session_paths,
            &process_info,
            &HashMap::new(),
            &HashMap::new(),
            &ctx,
        );

        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions[0].status,
            SessionStatus::Waiting,
            "stale transcript + idle descendants must be Waiting regardless of lifetime cpu_pct",
        );
    }

    #[test]
    fn test_load_session_recent_transcript_activity_is_thinking() {
        // Counterpart: when the transcript has just been written
        // (timestamp within the last few seconds), the agent is actually
        // emitting something — Thinking is the correct status.
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".claude");
        let sessions_dir = profile.join("sessions");
        let projects = profile.join("projects");
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();

        let pid = 9102;
        let sid = "fresh";
        let session_path = sessions_dir.join(format!("{}.json", pid));
        write_session_file(&session_path, pid, sid, &cwd);

        // Hand-build a transcript with a "now"-ish timestamp so
        // last_activity lands inside the 5s active window.
        let now_iso = chrono::Utc::now().to_rfc3339();
        let transcript_dir = projects.join(encode_cwd_path(cwd.to_str().unwrap()));
        std::fs::create_dir_all(&transcript_dir).unwrap();
        let transcript = transcript_dir.join(format!("{}.jsonl", sid));
        std::fs::write(
            &transcript,
            format!(
                r#"{{"type":"user","timestamp":"{}","message":{{"role":"user","content":"go"}}}}
"#,
                now_iso
            ),
        )
        .unwrap();

        let config = ConfigDir::new(profile.clone());
        // cpu_pct intentionally 0 — status must come from transcript
        // freshness alone, not CPU.
        let process_info = make_proc_info(pid, "claude");
        let mut collector = ClaudeCollector::new();
        collector.config_dirs = vec![config.clone()];

        let session_paths = vec![(session_path, config)];
        let ctx = build_discovery_context(&session_paths, &process_info, 0);
        let sessions = collector.load_session_paths(
            &session_paths,
            &process_info,
            &HashMap::new(),
            &HashMap::new(),
            &ctx,
        );

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].status, SessionStatus::Thinking);
    }

    #[test]
    fn test_load_session_pending_tool_use_is_executing() {
        // Regression: when the last assistant turn ends with a tool_use that
        // hasn't been answered yet, the agent is still mid-turn even if no
        // descendant is burning CPU (fast tools like `Bash rm` finish before
        // the next sample). Status must read as Executing, not Waiting.
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".claude");
        let sessions_dir = profile.join("sessions");
        let projects = profile.join("projects");
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();

        let pid = 9103;
        let sid = "pending-tool";
        let session_path = sessions_dir.join(format!("{}.json", pid));
        write_session_file(&session_path, pid, sid, &cwd);

        // Trailing assistant tool_use with no following tool_result.
        let transcript_dir = projects.join(encode_cwd_path(cwd.to_str().unwrap()));
        std::fs::create_dir_all(&transcript_dir).unwrap();
        let transcript = transcript_dir.join(format!("{}.jsonl", sid));
        std::fs::write(
            &transcript,
            r#"{"type":"user","timestamp":"2026-03-28T17:00:00Z","message":{"role":"user","content":"go"}}
{"type":"assistant","timestamp":"2026-03-28T17:00:05Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"tool_use","name":"Bash","id":"t1","input":{"command":"rm -v /tmp/x"}}]}}
"#,
        )
        .unwrap();

        let config = ConfigDir::new(profile.clone());
        // cpu_pct = 0 → has_active_descendant is false; Executing must come
        // from the pending-tool branch alone.
        let process_info = make_proc_info(pid, "claude");
        let mut collector = ClaudeCollector::new();
        collector.config_dirs = vec![config.clone()];

        let session_paths = vec![(session_path, config)];
        let ctx = build_discovery_context(&session_paths, &process_info, 0);
        let sessions = collector.load_session_paths(
            &session_paths,
            &process_info,
            &HashMap::new(),
            &HashMap::new(),
            &ctx,
        );

        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions[0].status,
            SessionStatus::Executing,
            "pending tool_use must read as Executing even with idle descendants",
        );

        // The marker is stamped between the tool_use (17:00:05) and its denial
        // (17:00:08); transcript events are ordered against `at`, not the mtime.
        std::fs::create_dir_all(profile.join("abtop-reply-state")).unwrap();
        std::fs::write(
            profile.join("abtop-reply-state").join(sid),
            format!(
                "Bash\nev=permission\nat={}\n",
                epoch_ms("2026-03-28T17:00:06Z")
            ),
        )
        .unwrap();
        let sessions = collector.load_session_paths(
            &session_paths,
            &process_info,
            &HashMap::new(),
            &HashMap::new(),
            &ctx,
        );
        assert_eq!(sessions[0].status, SessionStatus::AwaitingInput);
        assert_eq!(sessions[0].current_tasks, vec!["waiting for permission"]);

        let mut transcript_file = std::fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .unwrap();
        writeln!(
            transcript_file,
            r#"{{"type":"user","timestamp":"2026-03-28T17:00:08Z","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t1","content":"Permission denied"}}]}}}}"#
        )
        .unwrap();
        transcript_file.flush().unwrap();
        let sessions = collector.load_session_paths(
            &session_paths,
            &process_info,
            &HashMap::new(),
            &HashMap::new(),
            &ctx,
        );
        assert_eq!(sessions[0].status, SessionStatus::Executing);

        std::fs::remove_file(profile.join("abtop-reply-state").join(sid)).unwrap();
        writeln!(
            transcript_file,
            r#"{{"type":"assistant","timestamp":"2026-03-28T17:00:10Z","message":{{"model":"claude-sonnet-4-6","usage":{{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}},"content":[{{"type":"tool_use","name":"AskUserQuestion","id":"t2","input":{{"questions":[]}}}}]}}}}"#
        )
        .unwrap();
        transcript_file.flush().unwrap();
        let sessions = collector.load_session_paths(
            &session_paths,
            &process_info,
            &HashMap::new(),
            &HashMap::new(),
            &ctx,
        );
        assert_eq!(sessions[0].status, SessionStatus::AwaitingInput);
        assert_eq!(sessions[0].current_tasks, vec!["waiting for answer"]);

        writeln!(
            transcript_file,
            r#"{{"type":"user","timestamp":"2026-03-28T17:00:12Z","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t2","content":"Option A"}}]}}}}"#
        )
        .unwrap();
        transcript_file.flush().unwrap();
        let sessions = collector.load_session_paths(
            &session_paths,
            &process_info,
            &HashMap::new(),
            &HashMap::new(),
            &ctx,
        );
        assert_eq!(sessions[0].status, SessionStatus::Executing);
    }

    fn question_tool(id: &str) -> Value {
        serde_json::json!({"type":"tool_use", "name":"AskUserQuestion", "id":id,
            "input":{"questions":[]}})
    }

    fn question_entry(kind: &str, content: Value) -> Value {
        serde_json::json!({"type":kind, "timestamp":"2026-03-28T15:00:10Z",
            "message":{"role":kind, "content":content}})
    }

    fn question_result(id: &str, error: bool, text: &str) -> Value {
        question_entry(
            "user",
            serde_json::json!([
                {"type":"tool_result", "tool_use_id":id, "is_error":error, "content":text}
            ]),
        )
    }

    #[test]
    fn pending_questions_match_ids_across_full_incremental_and_noop_parses() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let mut meta = question_entry("user", serde_json::json!("metadata"));
        meta["isMeta"] = Value::Bool(true);
        let steps = vec![
            (
                question_entry(
                    "assistant",
                    serde_json::json!([question_tool("q1"),
                {"type":"tool_use", "name":"Read", "id":"read", "input":{"file_path":"x"}}]),
                ),
                vec!["q1"],
            ),
            (
                question_entry(
                    "assistant",
                    serde_json::json!([{"type":"text", "text":"Progress"}]),
                ),
                vec!["q1"],
            ),
            (question_result("read", false, "ok"), vec!["q1"]),
            (meta, vec!["q1"]),
            (
                question_entry(
                    "user",
                    serde_json::json!("<local-command-stdout>ok</local-command-stdout>"),
                ),
                vec!["q1"],
            ),
            (
                question_entry(
                    "user",
                    serde_json::json!("<command-name>/plugin</command-name>"),
                ),
                vec!["q1"],
            ),
            // Neither a new prompt nor an interruption-looking string proves that
            // a specific tool invocation was cancelled. Wait for its result.
            (
                question_entry("user", serde_json::json!("Please do something else")),
                vec!["q1"],
            ),
            (
                question_entry("user", serde_json::json!("[Request interrupted by user]")),
                vec!["q1"],
            ),
            (
                question_entry(
                    "assistant",
                    serde_json::json!([question_tool("q2"), question_tool("q1")]),
                ),
                vec!["q1", "q2"],
            ),
            (question_result("q1", false, "Option A"), vec!["q2"]),
            (question_result("q1", false, "duplicate"), vec!["q2"]),
            (
                question_result("q2", true, "User cancelled the question"),
                vec![],
            ),
            (
                question_entry(
                    "assistant",
                    serde_json::json!([{"type":"text", "text":"Done"}]),
                ),
                vec![],
            ),
        ];
        let mut previous = parse_transcript(file.path(), 0);
        for (entry, expected) in steps {
            writeln!(file, "{entry}").unwrap();
            file.flush().unwrap();
            let delta = parse_transcript_with_previous(
                file.path(),
                previous.new_offset,
                0,
                0,
                Some(&previous.pending_question_ids),
            );
            let full = parse_transcript(file.path(), 0);
            let expected: HashSet<String> = expected.into_iter().map(String::from).collect();
            assert_eq!(full.pending_question_ids, expected);
            assert_eq!(delta.pending_question_ids, expected);
            let noop = parse_transcript_with_previous(
                file.path(),
                delta.new_offset,
                0,
                0,
                Some(&delta.pending_question_ids),
            );
            assert_eq!(noop.pending_question_ids, expected);
            assert!(!noop.saw_turn);
            previous = delta;
        }
    }

    #[test]
    fn pending_questions_require_exact_tool_name_and_nonempty_id() {
        for tool in [
            serde_json::json!({"type":"tool_use", "name":"AskUserQuestion", "input":{}}),
            question_tool(""),
            serde_json::json!({"type":"tool_use", "name":"namespace.AskUserQuestion", "id":"q", "input":{}}),
            serde_json::json!({"type":"text", "text":"Which option?"}),
        ] {
            let mut file = tempfile::NamedTempFile::new().unwrap();
            writeln!(
                file,
                "{}",
                question_entry("assistant", serde_json::json!([tool]))
            )
            .unwrap();
            file.flush().unwrap();
            assert!(parse_transcript(file.path(), 0)
                .pending_question_ids
                .is_empty());
        }
        // IDs, not timestamps or result success, determine the question lifecycle.
        for (error, text) in [
            (false, "answered"),
            (true, "failed"),
            (true, "Permission denied"),
        ] {
            let mut file = tempfile::NamedTempFile::new().unwrap();
            let mut entry = question_entry("assistant", serde_json::json!([question_tool("q")]));
            entry.as_object_mut().unwrap().remove("timestamp");
            writeln!(file, "{entry}").unwrap();
            file.flush().unwrap();
            let pending = parse_transcript(file.path(), 0);
            assert_eq!(pending.last_assistant_ts_ms, 0);
            assert!(pending.pending_question_ids.contains("q"));
            let mut result = question_result("q", error, text);
            result["isMeta"] = Value::Bool(true);
            writeln!(file, "{result}").unwrap();
            file.flush().unwrap();
            assert!(parse_transcript(file.path(), 0)
                .pending_question_ids
                .is_empty());
        }
    }

    #[test]
    fn pending_question_status_survives_cache_and_resets_with_transcript() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".claude");
        let config = ConfigDir::new(profile.clone());
        let cwd = temp.path().join("repo");
        fs::create_dir_all(&config.sessions_dir).unwrap();
        fs::create_dir_all(&cwd).unwrap();
        let pid = 9200;
        let sid = "pending-question";
        let session_path = config.sessions_dir.join("9200.json");
        write_session_file(&session_path, pid, sid, &cwd);
        let project = config
            .projects_dir
            .join(encode_cwd_path(cwd.to_str().unwrap()));
        fs::create_dir_all(&project).unwrap();
        let transcript = project.join(format!("{sid}.jsonl"));
        let paths = vec![(session_path, config)];
        let mut procs = make_proc_info(pid, "claude");
        procs.insert(
            pid + 1,
            ProcInfo {
                pid: pid + 1,
                ppid: pid,
                rss_kb: 0,
                cpu_pct: 20.0,
                command: "worker".to_string(),
            },
        );
        let children = HashMap::from([(pid, vec![pid + 1])]);
        let ctx = build_discovery_context(&paths, &procs, 0);
        let mut collector = ClaudeCollector::new();
        let load = |collector: &mut ClaudeCollector| {
            collector
                .load_session_paths(&paths, &procs, &children, &HashMap::new(), &ctx)
                .remove(0)
        };
        let question = question_entry("assistant", serde_json::json!([question_tool("q")]));
        fs::write(&transcript, format!("{question}\n")).unwrap();
        let session = load(&mut collector);
        assert_eq!(session.status, SessionStatus::AwaitingInput);
        assert_eq!(session.current_tasks, vec!["waiting for answer"]);
        assert_eq!(load(&mut collector).status, SessionStatus::AwaitingInput);
        let markers = profile.join("abtop-reply-state");
        fs::create_dir_all(&markers).unwrap();
        fs::write(markers.join(sid), "AskUserQuestion\n").unwrap();
        assert_eq!(
            load(&mut collector).current_tasks,
            vec!["waiting for permission"]
        );
        fs::remove_file(markers.join(sid)).unwrap();

        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .unwrap();
        for entry in [
            question_entry(
                "assistant",
                serde_json::json!([{"type":"text", "text":"Progress"}]),
            ),
            question_result("other", false, "ok"),
            question_entry("user", serde_json::json!("<bash-stdout>ok</bash-stdout>")),
        ] {
            writeln!(file, "{entry}").unwrap();
            file.flush().unwrap();
            let session = load(&mut collector);
            assert_eq!(session.status, SessionStatus::AwaitingInput);
            assert_eq!(session.current_tasks, vec!["waiting for answer"]);
        }
        assert_eq!(collector.transcript_cache[sid].last_assistant_ts_ms, 0);
        // Multiple events in one incremental batch must resolve in file order.
        writeln!(
            file,
            "{}\n{}",
            question_entry("assistant", serde_json::json!([question_tool("q2")])),
            question_result("q2", true, "cancelled")
        )
        .unwrap();
        file.flush().unwrap();
        assert_eq!(load(&mut collector).status, SessionStatus::AwaitingInput);
        writeln!(file, "{}", question_result("q", false, "answer")).unwrap();
        file.flush().unwrap();
        assert_ne!(load(&mut collector).status, SessionStatus::AwaitingInput);
        assert_ne!(load(&mut collector).status, SessionStatus::AwaitingInput);

        writeln!(file, "{question}").unwrap();
        file.flush().unwrap();
        assert_eq!(load(&mut collector).status, SessionStatus::AwaitingInput);
        // Shrinking the same file resets seeded question IDs.
        fs::write(&transcript, "{}\n").unwrap();
        assert_ne!(load(&mut collector).status, SessionStatus::AwaitingInput);
        writeln!(file, "{question}").unwrap();
        file.flush().unwrap();
        assert_eq!(load(&mut collector).status, SessionStatus::AwaitingInput);
        // A replacement file can be larger; identity, not size, must reset it.
        let replacement = project.join("replacement");
        fs::write(&replacement, "\n".repeat(1024)).unwrap();
        fs::rename(&replacement, &transcript).unwrap();
        assert_ne!(load(&mut collector).status, SessionStatus::AwaitingInput);
        fs::write(&transcript, format!("{question}\n")).unwrap();
        assert_eq!(load(&mut collector).status, SessionStatus::AwaitingInput);
        collector.evict_stale_cache(&[]);
        assert!(collector.transcript_cache.is_empty());
        fs::write(&transcript, "{}\n").unwrap();
        assert_ne!(load(&mut collector).status, SessionStatus::AwaitingInput);
    }

    fn epoch_ms(timestamp: &str) -> u64 {
        chrono::DateTime::parse_from_rfc3339(timestamp)
            .unwrap()
            .timestamp_millis() as u64
    }

    fn use_event(id: &str, name: &str, timestamp: &str) -> ToolEvent {
        ToolEvent::Use {
            id: id.to_string(),
            name: name.to_string(),
            ts_ms: epoch_ms(timestamp),
        }
    }

    fn result_event(id: &str, timestamp: &str) -> ToolEvent {
        ToolEvent::Result {
            id: id.to_string(),
            ts_ms: epoch_ms(timestamp),
        }
    }

    fn claim_at(scope: ClaimScope, timestamp: &str) -> PermissionClaim {
        PermissionClaim {
            scope,
            since_ms: epoch_ms(timestamp),
        }
    }

    fn named(name: &str) -> ClaimScope {
        ClaimScope::Name {
            name: name.to_string(),
            truncated: false,
        }
    }

    fn assistant_tools(calls: &[(&str, &str)], timestamp: &str) -> Value {
        let content: Vec<Value> = calls
            .iter()
            .map(|(id, name)| {
                serde_json::json!({"type":"tool_use", "name":name, "id":id, "input":{}})
            })
            .collect();
        serde_json::json!({"type":"assistant", "timestamp":timestamp,
            "message":{"model":"claude-sonnet-4-6", "content":content}})
    }

    fn tool_result_at(id: &str, timestamp: &str, error: bool) -> Value {
        serde_json::json!({"type":"user", "timestamp":timestamp,
        "message":{"role":"user", "content":[
            {"type":"tool_result", "tool_use_id":id, "is_error":error, "content":"x"}
        ]}})
    }

    /// Collector-level fixture: a live Claude session whose transcript and
    /// permission marker are written by the test. Session start is
    /// 2026-03-28T16:25:16Z, so transcript times below use 17:xx.
    struct PermissionFixture {
        _temp: tempfile::TempDir,
        profile: PathBuf,
        sid: String,
        transcript: PathBuf,
        paths: Vec<(PathBuf, ConfigDir)>,
        procs: HashMap<u32, ProcInfo>,
        ctx: DiscoveryContext,
        collector: ClaudeCollector,
    }

    impl PermissionFixture {
        fn new(sid: &str) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let profile = temp.path().join(".claude");
            let config = ConfigDir::new(profile.clone());
            let cwd = temp.path().join("repo");
            fs::create_dir_all(&config.sessions_dir).unwrap();
            fs::create_dir_all(&cwd).unwrap();
            let pid = 9300;
            let session_path = config.sessions_dir.join(format!("{pid}.json"));
            write_session_file(&session_path, pid, sid, &cwd);
            let project = config
                .projects_dir
                .join(encode_cwd_path(cwd.to_str().unwrap()));
            fs::create_dir_all(&project).unwrap();
            let transcript = project.join(format!("{sid}.jsonl"));
            fs::write(
                &transcript,
                r#"{"type":"user","timestamp":"2026-03-28T17:00:00Z","message":{"role":"user","content":"go"}}
"#,
            )
            .unwrap();
            let paths = vec![(session_path, config)];
            let procs = make_proc_info(pid, "claude");
            let ctx = build_discovery_context(&paths, &procs, 0);
            let mut collector = ClaudeCollector::new();
            collector.config_dirs = vec![paths[0].1.clone()];
            Self {
                _temp: temp,
                profile,
                sid: sid.to_string(),
                transcript,
                paths,
                procs,
                ctx,
                collector,
            }
        }

        fn load(&mut self) -> AgentSession {
            self.collector
                .load_session_paths(
                    &self.paths,
                    &self.procs,
                    &HashMap::new(),
                    &HashMap::new(),
                    &self.ctx,
                )
                .remove(0)
        }

        fn append(&self, entries: &[Value]) {
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(&self.transcript)
                .unwrap();
            for entry in entries {
                writeln!(file, "{entry}").unwrap();
            }
            file.flush().unwrap();
        }

        fn marker_path(&self) -> PathBuf {
            self.profile.join("abtop-reply-state").join(&self.sid)
        }

        /// Write a marker; `mtime` pins the file mtime (legacy markers have no
        /// `at=` line, so their time is the mtime).
        fn write_marker(&self, body: &str, mtime: Option<&str>) {
            fs::create_dir_all(self.marker_path().parent().unwrap()).unwrap();
            fs::write(self.marker_path(), body).unwrap();
            if let Some(mtime) = mtime {
                fs::File::options()
                    .write(true)
                    .open(self.marker_path())
                    .unwrap()
                    .set_modified(
                        std::time::UNIX_EPOCH + std::time::Duration::from_millis(epoch_ms(mtime)),
                    )
                    .unwrap();
            }
        }

        fn write_new_marker(&self, tool: &str, extra: &str, at: &str) {
            self.write_marker(
                &format!("{tool}\nev=permission\nat={}\n{extra}", epoch_ms(at)),
                None,
            );
        }
    }

    fn is_reply(session: &AgentSession) -> bool {
        session.status == SessionStatus::AwaitingInput
            && session.current_tasks == vec!["waiting for permission"]
    }

    #[test]
    fn empty_notification_marker_after_tool_result_stays_reply_across_polls() {
        // Observed failure: a tool result at :31, then a new empty marker at :55
        // with no matching tool_use in the transcript. The marker alone must
        // show Reply, and unchanged polls must not drop it.
        for legacy in [false, true] {
            let mut fixture = PermissionFixture::new("late-marker");
            fixture.append(&[
                assistant_tools(&[("r1", "Read")], "2026-03-28T17:00:30Z"),
                tool_result_at("r1", "2026-03-28T17:00:31Z", false),
            ]);
            assert!(!is_reply(&fixture.load()));
            if legacy {
                // Old hook output: only a newline, time = file mtime.
                fixture.write_marker("\n", Some("2026-03-28T17:00:55Z"));
            } else {
                fixture.write_marker(
                    &format!(
                        "\nev=notification\nat={}\n",
                        epoch_ms("2026-03-28T17:00:55Z")
                    ),
                    None,
                );
            }
            for _ in 0..3 {
                assert!(is_reply(&fixture.load()), "legacy={legacy}");
            }
            // Text-only assistant chunks and real prompts prove nothing.
            fixture.append(&[
                serde_json::json!({"type":"assistant", "timestamp":"2026-03-28T17:00:57Z",
                    "message":{"content":[{"type":"text", "text":"still working"}]}}),
                serde_json::json!({"type":"user", "timestamp":"2026-03-28T17:00:58Z",
                    "message":{"role":"user", "content":"hello?"}}),
            ]);
            assert!(is_reply(&fixture.load()), "legacy={legacy}");
        }
    }

    #[test]
    fn identity_less_marker_closes_on_the_first_later_result_when_nothing_was_open() {
        // Documented limit of the Notification-only fallback: with no call open
        // at the marker time, the next tool_result of any tool closes it, even if
        // that result belongs to an unrelated parallel call.
        let mut fixture = PermissionFixture::new("any-fallback");
        fixture.write_marker("\n", Some("2026-03-28T17:00:55Z"));
        assert!(is_reply(&fixture.load()));
        fixture.append(&[
            assistant_tools(&[("late", "Read")], "2026-03-28T17:00:56Z"),
            tool_result_at("late", "2026-03-28T17:00:57Z", false),
        ]);
        assert!(!is_reply(&fixture.load()));
        assert!(!is_reply(&fixture.load()));
    }

    #[test]
    fn identity_less_marker_waits_for_the_call_that_was_open() {
        let mut fixture = PermissionFixture::new("any-open-call");
        fixture.append(&[
            assistant_tools(&[("x", "Bash"), ("r", "Read")], "2026-03-28T17:00:40Z"),
            tool_result_at("r", "2026-03-28T17:00:41Z", false),
        ]);
        fixture.write_marker("\n", Some("2026-03-28T17:00:50Z"));
        assert!(is_reply(&fixture.load()));
        // Unrelated results arrive while the prompt for `x` is still showing.
        fixture.append(&[
            assistant_tools(&[("other", "Read")], "2026-03-28T17:00:52Z"),
            tool_result_at("other", "2026-03-28T17:00:53Z", false),
        ]);
        assert!(is_reply(&fixture.load()));
        // Denying `x` writes its tool_result; no completion hook runs.
        fixture.append(&[tool_result_at("x", "2026-03-28T17:01:00Z", true)]);
        assert!(!is_reply(&fixture.load()));
    }

    #[test]
    fn named_marker_ignores_unrelated_results_and_closes_on_approval_or_denial() {
        for denied in [false, true] {
            let mut fixture = PermissionFixture::new("named-marker");
            fixture.append(&[
                assistant_tools(&[("x", "Bash")], "2026-03-28T17:00:40Z"),
                // Unrelated, already finished before the prompt.
                assistant_tools(&[("old", "Read")], "2026-03-28T17:00:41Z"),
                tool_result_at("old", "2026-03-28T17:00:42Z", false),
            ]);
            fixture.write_new_marker("Bash", "", "2026-03-28T17:00:50Z");
            assert!(is_reply(&fixture.load()));
            // A different tool finishing, a new user prompt and a text chunk
            // while the prompt is pending.
            fixture.append(&[
                assistant_tools(&[("r2", "Read")], "2026-03-28T17:00:52Z"),
                tool_result_at("r2", "2026-03-28T17:00:53Z", false),
                serde_json::json!({"type":"user", "timestamp":"2026-03-28T17:00:54Z",
                    "message":{"role":"user", "content":"are you there"}}),
            ]);
            assert!(is_reply(&fixture.load()));
            // Approval or denial both end with the call's own tool_result.
            fixture.append(&[tool_result_at("x", "2026-03-28T17:01:00Z", denied)]);
            for _ in 0..3 {
                let session = fixture.load();
                assert!(!is_reply(&session), "denied={denied}");
                assert_ne!(session.status, SessionStatus::AwaitingInput);
            }
            // The leftover file is stale, not deleted: a newer marker reopens it.
            assert!(fixture.marker_path().exists());
            fixture.write_new_marker("Bash", "", "2026-03-28T17:01:30Z");
            assert!(is_reply(&fixture.load()));
        }
    }

    #[test]
    fn hook_clear_removing_the_marker_ends_reply() {
        let mut fixture = PermissionFixture::new("hook-cleared");
        fixture.append(&[assistant_tools(&[("x", "Bash")], "2026-03-28T17:00:40Z")]);
        fixture.write_new_marker("Bash", "", "2026-03-28T17:00:50Z");
        assert!(is_reply(&fixture.load()));
        fs::remove_file(fixture.marker_path()).unwrap();
        assert!(!is_reply(&fixture.load()));
    }

    #[test]
    fn same_name_concurrent_calls_do_not_close_each_other() {
        // Name-only marker: both Bash calls must finish before it closes.
        let mut fixture = PermissionFixture::new("same-name");
        fixture.append(&[assistant_tools(
            &[("a", "Bash"), ("b", "Bash")],
            "2026-03-28T17:00:40Z",
        )]);
        fixture.write_new_marker("Bash", "", "2026-03-28T17:00:50Z");
        assert!(is_reply(&fixture.load()));
        fixture.append(&[tool_result_at("a", "2026-03-28T17:00:55Z", false)]);
        assert!(is_reply(&fixture.load()));
        fixture.append(&[tool_result_at("b", "2026-03-28T17:00:56Z", false)]);
        assert!(!is_reply(&fixture.load()));

        // Marker with a call id (when a payload provides one): only that call
        // closes it.
        let mut fixture = PermissionFixture::new("same-name-id");
        fixture.append(&[assistant_tools(
            &[("a", "Bash"), ("b", "Bash")],
            "2026-03-28T17:00:40Z",
        )]);
        fixture.write_new_marker("Bash", "id=b\n", "2026-03-28T17:00:50Z");
        assert!(is_reply(&fixture.load()));
        fixture.append(&[tool_result_at("a", "2026-03-28T17:00:55Z", false)]);
        assert!(is_reply(&fixture.load()));
        fixture.append(&[tool_result_at("b", "2026-03-28T17:00:56Z", false)]);
        assert!(!is_reply(&fixture.load()));
    }

    #[test]
    fn identified_marker_for_a_call_missing_from_the_transcript_closes_with_the_turn() {
        // The prompt's own tool_use never appears (for example a subagent call).
        // Once every call that was open at the marker time has a result, the
        // turn is over and the marker must not stay in Reply forever.
        let mut fixture = PermissionFixture::new("foreign-call");
        fixture.append(&[assistant_tools(
            &[("agent", "Agent")],
            "2026-03-28T17:00:40Z",
        )]);
        fixture.write_new_marker("Bash", "id=hidden\n", "2026-03-28T17:00:50Z");
        assert!(is_reply(&fixture.load()));
        fixture.append(&[tool_result_at("agent", "2026-03-28T17:05:00Z", false)]);
        assert!(!is_reply(&fixture.load()));
    }

    #[test]
    fn permission_reason_wins_over_question_and_question_survives_closure() {
        let mut fixture = PermissionFixture::new("perm-and-question");
        fixture.append(&[assistant_tools(
            &[("q", "AskUserQuestion"), ("x", "Bash")],
            "2026-03-28T17:00:40Z",
        )]);
        assert_eq!(fixture.load().current_tasks, vec!["waiting for answer"]);
        fixture.write_new_marker("Bash", "", "2026-03-28T17:00:50Z");
        assert!(is_reply(&fixture.load()));
        fixture.append(&[tool_result_at("x", "2026-03-28T17:00:55Z", true)]);
        let session = fixture.load();
        assert_eq!(session.status, SessionStatus::AwaitingInput);
        assert_eq!(session.current_tasks, vec!["waiting for answer"]);
        fixture.append(&[tool_result_at("q", "2026-03-28T17:01:00Z", false)]);
        assert_ne!(fixture.load().status, SessionStatus::AwaitingInput);
    }

    #[test]
    fn marker_roots_and_sessions_are_isolated() {
        let mut fixture = PermissionFixture::new("session-a");
        // Another session in the same root and the same id in another root.
        let other_session = fixture.profile.join("abtop-reply-state").join("session-b");
        fs::create_dir_all(other_session.parent().unwrap()).unwrap();
        fs::write(&other_session, "\n").unwrap();
        let other_root = fixture._temp.path().join(".claude-other");
        fs::create_dir_all(other_root.join("abtop-reply-state")).unwrap();
        fs::write(other_root.join("abtop-reply-state").join("session-a"), "\n").unwrap();
        assert!(!is_reply(&fixture.load()));
        fs::write(
            fixture.profile.join("abtop-reply-state").join("session-a"),
            "\n",
        )
        .unwrap();
        assert!(is_reply(&fixture.load()));
    }

    #[test]
    fn marker_reader_accepts_legacy_and_extended_formats() {
        let temp = tempfile::tempdir().unwrap();
        let config = ConfigDir::new(temp.path().to_path_buf());
        let dir = temp.path().join("abtop-reply-state");
        fs::create_dir(&dir).unwrap();
        let read = |sid: &str| read_permission_claims(&config, sid, 0);
        let ms = |text: &str| epoch_ms(text);
        let mtime = |sid: &str| {
            fs::metadata(dir.join(sid))
                .unwrap()
                .modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64
        };

        // Legacy: plain tool name, empty line, completely empty file.
        fs::write(dir.join("legacy-name"), "Bash\n").unwrap();
        fs::write(dir.join("legacy-empty-line"), "\n").unwrap();
        fs::write(dir.join("legacy-empty-file"), "").unwrap();
        assert_eq!(
            read("legacy-name"),
            vec![PermissionClaim {
                scope: named("Bash"),
                since_ms: mtime("legacy-name")
            }]
        );
        for sid in ["legacy-empty-line", "legacy-empty-file"] {
            assert_eq!(
                read(sid),
                vec![PermissionClaim {
                    scope: ClaimScope::Any,
                    since_ms: mtime(sid)
                }]
            );
        }

        // Extended: id wins over name, `nt` adds an identity-less claim, unknown
        // keys are ignored.
        fs::write(
            dir.join("extended"),
            format!(
                "Bash\nev=permission\nat={}\nid=toolu_01\nfuture=1\nnt={}\n",
                ms("2026-03-28T17:00:10Z"),
                ms("2026-03-28T17:00:20Z")
            ),
        )
        .unwrap();
        assert_eq!(
            read("extended"),
            vec![
                claim_at(ClaimScope::Id("toolu_01".into()), "2026-03-28T17:00:10Z"),
                claim_at(ClaimScope::Any, "2026-03-28T17:00:20Z"),
            ]
        );

        // A non-millisecond `at` (plain seconds) falls back to the mtime, and a
        // name of the hook's length cap is matched as a prefix.
        let long = "m".repeat(PERMISSION_MARKER_TOOL_CAP);
        fs::write(dir.join("seconds"), format!("{long}\nat=1774717000\n")).unwrap();
        assert_eq!(
            read("seconds"),
            vec![PermissionClaim {
                scope: ClaimScope::Name {
                    name: long,
                    truncated: true
                },
                since_ms: mtime("seconds")
            }]
        );

        // Claims stamped before the session started are dropped, as are
        // oversized markers, non-regular files and unsafe session ids.
        assert!(read_permission_claims(&config, "extended", u64::MAX).is_empty());
        let stale = format!("Bash\nat={}\n", ms("2026-03-28T10:00:00Z"));
        fs::write(dir.join("stale-at"), stale).unwrap();
        assert!(read_permission_claims(&config, "stale-at", ms("2026-03-28T11:00:00Z")).is_empty());
        fs::write(dir.join("big"), "x".repeat(129)).unwrap();
        assert!(read("big").is_empty());
        fs::create_dir(dir.join("directory")).unwrap();
        assert!(read("directory").is_empty());
        assert!(read("../legacy-name").is_empty());
        assert!(read("").is_empty());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.join("legacy-name"), dir.join("link")).unwrap();
            assert!(read("link").is_empty());
        }
    }

    #[test]
    fn claim_rules_order_events_by_marker_time() {
        let marker = "2026-03-28T17:00:50Z";
        // Result before the marker never closes it.
        let before = [
            use_event("x", "Bash", "2026-03-28T17:00:40Z"),
            result_event("x", "2026-03-28T17:00:45Z"),
        ];
        for scope in [named("Bash"), ClaimScope::Id("x".into()), ClaimScope::Any] {
            assert!(permission_claim_open(&claim_at(scope, marker), &before));
        }
        // Results without a timestamp can be ordered against nothing.
        let untimed = [
            use_event("y", "Bash", "2026-03-28T17:00:40Z"),
            ToolEvent::Result {
                id: "y".into(),
                ts_ms: 0,
            },
        ];
        assert!(permission_claim_open(
            &claim_at(named("Bash"), marker),
            &untimed
        ));
        // The result lands exactly on the marker millisecond: not "after".
        let same = [
            use_event("z", "Bash", "2026-03-28T17:00:40Z"),
            result_event("z", marker),
        ];
        assert!(permission_claim_open(
            &claim_at(named("Bash"), marker),
            &same
        ));
        // Prefix matching for names cut at the hook's cap.
        let long_name = format!("{}_tool_tail", "m".repeat(PERMISSION_MARKER_TOOL_CAP));
        let events = [
            use_event("m1", &long_name, "2026-03-28T17:00:40Z"),
            // Still running, so the turn-complete safety net cannot apply.
            use_event("busy", "Read", "2026-03-28T17:00:41Z"),
            result_event("m1", "2026-03-28T17:00:55Z"),
        ];
        let cut = ClaimScope::Name {
            name: "m".repeat(PERMISSION_MARKER_TOOL_CAP),
            truncated: true,
        };
        assert!(!permission_claim_open(&claim_at(cut, marker), &events));
        let exact = ClaimScope::Name {
            name: "m".repeat(PERMISSION_MARKER_TOOL_CAP),
            truncated: false,
        };
        assert!(permission_claim_open(&claim_at(exact, marker), &events));
    }

    #[test]
    fn tool_events_match_between_full_and_incremental_parses_and_stay_bounded() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            file,
            "{}\n{}",
            assistant_tools(&[("a", "Bash")], "2026-03-28T17:00:40Z"),
            tool_result_at("a", "2026-03-28T17:00:41Z", false)
        )
        .unwrap();
        file.flush().unwrap();
        let first = parse_transcript(file.path(), 0);
        writeln!(
            file,
            "{}",
            assistant_tools(&[("b", "Read")], "2026-03-28T17:00:42Z")
        )
        .unwrap();
        file.flush().unwrap();
        let delta = parse_transcript_with_previous(
            file.path(),
            first.new_offset,
            0,
            0,
            Some(&first.pending_question_ids),
        );
        let full = parse_transcript(file.path(), 0);
        let mut merged = first.tool_events.clone();
        merged.extend(delta.tool_events.clone());
        assert_eq!(merged, full.tool_events);
        assert_eq!(
            full.tool_events,
            vec![
                use_event("a", "Bash", "2026-03-28T17:00:40Z"),
                result_event("a", "2026-03-28T17:00:41Z"),
                use_event("b", "Read", "2026-03-28T17:00:42Z"),
            ]
        );
        // A no-op poll adds nothing.
        let noop = parse_transcript_with_previous(file.path(), delta.new_offset, 0, 0, None);
        assert!(noop.tool_events.is_empty());

        let mut many = tempfile::NamedTempFile::new().unwrap();
        for index in 0..MAX_TOOL_EVENTS + 20 {
            writeln!(
                many,
                "{}",
                assistant_tools(&[(&format!("id{index}"), "Read")], "2026-03-28T17:00:40Z")
            )
            .unwrap();
        }
        many.flush().unwrap();
        let capped = parse_transcript(many.path(), 0);
        assert_eq!(capped.tool_events.len(), MAX_TOOL_EVENTS);
        assert_eq!(
            capped.tool_events.last(),
            Some(&use_event(
                &format!("id{}", MAX_TOOL_EVENTS + 19),
                "Read",
                "2026-03-28T17:00:40Z"
            ))
        );
    }

    #[test]
    fn reply_hook_script_writes_markers_the_collector_understands() {
        let jq_available = std::process::Command::new("jq")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success());
        if !jq_available {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let config = ConfigDir::new(temp.path().to_path_buf());
        let script = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/claude-reply-hook.sh");
        let run = |action: &str, payload: Value| {
            let mut child = std::process::Command::new("bash")
                .arg(script)
                .arg(action)
                .env("CLAUDE_CONFIG_DIR", temp.path())
                .env("HOME", temp.path())
                .stdin(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(payload.to_string().as_bytes())
                .unwrap();
            assert!(child.wait().unwrap().success());
        };
        let claims = || read_permission_claims(&config, "hook-session", 0);
        let scopes = || claims().into_iter().map(|c| c.scope).collect::<Vec<_>>();
        let marker = temp.path().join("abtop-reply-state").join("hook-session");
        let request = |tool: &str| {
            serde_json::json!({"session_id":"hook-session",
                "hook_event_name":"PermissionRequest", "tool_name":tool})
        };
        let notification = serde_json::json!({"session_id":"hook-session",
            "hook_event_name":"Notification", "notification_type":"permission_prompt"});
        let completed = |tool: &str, id: &str| {
            serde_json::json!({"session_id":"hook-session", "hook_event_name":"PostToolUse",
                "tool_name":tool, "tool_use_id":id})
        };

        run("mark", request("Bash"));
        assert_eq!(scopes(), vec![named("Bash")]);
        // The empty Notification keeps the identified marker and adds its own claim.
        run("mark", notification.clone());
        assert_eq!(scopes(), vec![named("Bash"), ClaimScope::Any]);
        // Another tool finishing does not clear the Bash prompt.
        run("clear", completed("Read", "toolu_r"));
        assert_eq!(scopes(), vec![named("Bash"), ClaimScope::Any]);
        run("clear", completed("Bash", "toolu_b"));
        assert!(claims().is_empty());
        assert!(!marker.exists());

        // Identity-less marker: any completed tool clears it.
        run("mark", notification);
        assert_eq!(scopes(), vec![ClaimScope::Any]);
        run("clear", completed("Edit", "toolu_e"));
        assert!(!marker.exists());

        // A payload with a call id is recorded; a sibling with another id
        // does not clear it, the same id does.
        run(
            "mark",
            serde_json::json!({"session_id":"hook-session",
                "hook_event_name":"PermissionRequest", "tool_name":"Bash",
                "tool_use_id":"toolu_a"}),
        );
        assert_eq!(scopes(), vec![ClaimScope::Id("toolu_a".into())]);
        run("clear", completed("Bash", "toolu_other"));
        assert_eq!(scopes(), vec![ClaimScope::Id("toolu_a".into())]);
        run("clear", completed("Bash", "toolu_a"));
        assert!(!marker.exists());

        // Longest possible marker stays within the 128-byte reader limit and
        // the name is cut at the shared cap.
        let long = "t".repeat(PERMISSION_MARKER_TOOL_CAP + 10);
        run(
            "mark",
            serde_json::json!({"session_id":"hook-session",
                "hook_event_name":"Notification", "tool_name":long,
                "tool_use_id":"i".repeat(40)}),
        );
        run(
            "mark",
            serde_json::json!({"session_id":"hook-session",
            "hook_event_name":"Notification"}),
        );
        assert!(fs::metadata(&marker).unwrap().len() <= MAX_PERMISSION_MARKER_BYTES);
        assert_eq!(claims().len(), 2);

        run(
            "clear_all",
            serde_json::json!({"session_id":"hook-session"}),
        );
        assert!(!marker.exists());
        // Unsafe session ids never create files.
        run(
            "mark",
            serde_json::json!({"session_id":"../escape", "tool_name":"Bash"}),
        );
        assert!(!temp.path().join("escape").exists());
    }
    #[test]
    fn test_load_session_working_async_subagent_is_executing() {
        // Regression for #156: an async Agent tool returns immediately, so the
        // parent has no pending tool while its subagent keeps working. The
        // working subagent must keep the parent Executing instead of Waiting.
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".claude");
        let sessions_dir = profile.join("sessions");
        let projects = profile.join("projects");
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();

        let pid = 9104;
        let sid = "async-subagent";
        let session_path = sessions_dir.join(format!("{}.json", pid));
        write_session_file(&session_path, pid, sid, &cwd);

        let project_dir = projects.join(encode_cwd_path(cwd.to_str().unwrap()));
        std::fs::create_dir_all(&project_dir).unwrap();
        let transcript = project_dir.join(format!("{}.jsonl", sid));
        std::fs::write(
            &transcript,
            r#"{"type":"user","timestamp":"2026-03-28T15:00:00Z","message":{"role":"user","content":"research this"}}
{"type":"assistant","timestamp":"2026-03-28T15:00:05Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"tool_use","name":"Agent","id":"agent-tool-1","input":{"prompt":"research"}}]}}
{"type":"user","timestamp":"2026-03-28T15:00:06Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"agent-tool-1","content":"Async agent launched successfully.\nagentId: child-1\nrunning in background"}]}}
{"type":"assistant","timestamp":"2026-03-28T15:00:07Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"Waiting for the research result."}]}}
"#,
        )
        .unwrap();

        let subagents_dir = project_dir.join(sid).join("subagents");
        std::fs::create_dir_all(&subagents_dir).unwrap();
        std::fs::write(
            subagents_dir.join("agent-child-1.meta.json"),
            r#"{"agentType":"general-purpose","description":"research"}"#,
        )
        .unwrap();
        let subagent_transcript = subagents_dir.join("agent-child-1.jsonl");
        std::fs::write(
            &subagent_transcript,
            r#"{"type":"user","timestamp":"2026-03-28T15:00:06Z","message":{"role":"user","content":"research"}}
"#,
        )
        .unwrap();
        set_mtime(&subagent_transcript, 0);

        let config = ConfigDir::new(profile.clone());
        let process_info = make_proc_info(pid, "claude");
        let mut collector = ClaudeCollector::new();
        collector.config_dirs = vec![config.clone()];

        let session_paths = vec![(session_path, config)];
        let ctx = build_discovery_context(&session_paths, &process_info, 0);
        let sessions = collector.load_session_paths(
            &session_paths,
            &process_info,
            &HashMap::new(),
            &HashMap::new(),
            &ctx,
        );

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].subagents.len(), 1);
        assert_eq!(sessions[0].subagents[0].status, "working");
        assert_eq!(
            sessions[0].status,
            SessionStatus::Executing,
            "a working async subagent must keep its parent Executing",
        );
    }

    #[test]
    fn test_load_session_overrides_sid_after_clear() {
        // Reproduces issue #68: session file still points at the PID's initial
        // sessionId, but a newer transcript (from /clear) exists in the same
        // project dir. The loaded session must reflect the new sid + its
        // counters, not the stale one.
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".claude");
        let sessions = profile.join("sessions");
        let projects = profile.join("projects");
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();

        let pid = 7777;
        let old_sid = "old-sid-before-clear";
        let new_sid = "new-sid-after-clear";
        let session_path = sessions.join(format!("{}.json", pid));
        write_session_file(&session_path, pid, old_sid, &cwd);

        let old_transcript = write_transcript(&projects, &cwd, old_sid, "first");
        let new_transcript = write_transcript(&projects, &cwd, new_sid, "after clear");
        set_mtime(&old_transcript, -30);
        set_mtime(&new_transcript, 0);

        let config = ConfigDir::new(profile.clone());
        let process_info = make_proc_info(pid, "claude");
        let mut collector = ClaudeCollector::new();
        collector.config_dirs = vec![config.clone()];

        let session_paths = vec![(session_path, config)];
        let ctx = build_discovery_context(&session_paths, &process_info, 0);
        let sessions = collector.load_session_paths(
            &session_paths,
            &process_info,
            &HashMap::new(),
            &HashMap::new(),
            &ctx,
        );

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, new_sid);
        // Token counters must reflect the NEW transcript's contents, not
        // a stale/empty state. `write_transcript` produces one assistant
        // turn with input_tokens=12 → verify that flowed through.
        assert_eq!(sessions[0].total_input_tokens, 12);
    }

    #[test]
    fn test_load_session_overrides_sid_in_worktree_project_dir() {
        // Worktree parity: when the transcript dir doesn't match
        // encode_cwd_path(cwd), the live-sid lookup must still find the
        // new jsonl next to the old one in the actual worktree dir.
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".claude");
        let sessions = profile.join("sessions");
        let projects = profile.join("projects");
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();

        let pid = 8888;
        let old_sid = "worktree-old";
        let new_sid = "worktree-new";

        // Simulate a worktree session: transcripts in a dir that does NOT
        // match encode_cwd_path(cwd).
        let worktree_dir = projects.join("worktree-branch");
        std::fs::create_dir_all(&worktree_dir).unwrap();
        let old_transcript = worktree_dir.join(format!("{}.jsonl", old_sid));
        let new_transcript = worktree_dir.join(format!("{}.jsonl", new_sid));
        let turn_line = r#"{"type":"user","timestamp":"2026-03-28T15:00:00Z","message":{"role":"user","content":"x"}}
{"type":"assistant","timestamp":"2026-03-28T15:00:05Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":42,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"ok"}]}}
"#;
        std::fs::write(&old_transcript, turn_line).unwrap();
        std::fs::write(&new_transcript, turn_line).unwrap();
        set_mtime(&old_transcript, -30);
        set_mtime(&new_transcript, 0);

        let session_path = sessions.join(format!("{}.json", pid));
        write_session_file(&session_path, pid, old_sid, &cwd);

        let config = ConfigDir::new(profile.clone());
        let process_info = make_proc_info(pid, "claude");
        let mut collector = ClaudeCollector::new();
        collector.config_dirs = vec![config.clone()];

        let session_paths = vec![(session_path, config)];
        let ctx = build_discovery_context(&session_paths, &process_info, 0);
        let sessions = collector.load_session_paths(
            &session_paths,
            &process_info,
            &HashMap::new(),
            &HashMap::new(),
            &ctx,
        );

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, new_sid);
        assert_eq!(sessions[0].total_input_tokens, 42);
    }

    #[test]
    fn test_load_session_skips_override_when_multiple_pids_share_cwd() {
        // Cross-PID hijack guard: two claude PIDs in the same cwd must keep
        // their original session_ids even if newer jsonls exist, since we
        // can't tell from mtime alone which one owns each file.
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".claude");
        let sessions = profile.join("sessions");
        let projects = profile.join("projects");
        let cwd = temp.path().join("shared-repo");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();

        let pid_a = 9001;
        let pid_b = 9002;
        let sid_a = "sid-a";
        let sid_b = "sid-b";
        let path_a = sessions.join(format!("{}.json", pid_a));
        let path_b = sessions.join(format!("{}.json", pid_b));
        write_session_file(&path_a, pid_a, sid_a, &cwd);
        write_session_file(&path_b, pid_b, sid_b, &cwd);

        // Both PIDs have their jsonls + a "mystery" newer one that neither
        // session file claims. Without the guard, both PIDs would race to
        // adopt it.
        write_transcript(&projects, &cwd, sid_a, "a");
        write_transcript(&projects, &cwd, sid_b, "b");
        let mystery = write_transcript(&projects, &cwd, "newer-jsonl-someone-cleared", "mystery");
        set_mtime(&mystery, 0);

        let config = ConfigDir::new(profile.clone());
        let mut process_info = make_proc_info(pid_a, "claude");
        process_info.insert(
            pid_b,
            ProcInfo {
                pid: pid_b,
                ppid: 1,
                rss_kb: 2048,
                cpu_pct: 0.0,
                command: "claude".to_string(),
            },
        );
        let mut collector = ClaudeCollector::new();
        collector.config_dirs = vec![config.clone()];

        let session_paths = vec![(path_a, config.clone()), (path_b, config)];
        let ctx = build_discovery_context(&session_paths, &process_info, 0);
        let sessions = collector.load_session_paths(
            &session_paths,
            &process_info,
            &HashMap::new(),
            &HashMap::new(),
            &ctx,
        );

        let sids: std::collections::HashSet<&str> =
            sessions.iter().map(|s| s.session_id.as_str()).collect();
        assert!(sids.contains(sid_a), "expected sid-a, got {:?}", sids);
        assert!(sids.contains(sid_b), "expected sid-b, got {:?}", sids);
        assert!(
            !sids.contains("newer-jsonl-someone-cleared"),
            "the mystery sid must not hijack either PID: {:?}",
            sids
        );
    }

    #[test]
    fn test_load_session_overrides_sid_despite_print_sibling() {
        // Regression guard: abtop spawns `claude --print` for summary
        // generation. Its `sessions/{PID}.json` lands in the same cwd as
        // the real session. If `build_discovery_context` counted those
        // spawns, `pids_per_cwd` would flip to 2 and the cross-PID guard
        // would silently suppress the /clear override on the real
        // session — re-introducing issue #68 on every machine running
        // abtop. Filter them out instead.
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".claude");
        let sessions_dir = profile.join("sessions");
        let projects = profile.join("projects");
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();

        let abtop_pid = 9999u32;
        let real_pid = 7070;
        let print_pid = 7071;
        let real_old = "real-old";
        let real_new = "real-new";
        let print_sid = "print-spawn";

        let real_path = sessions_dir.join(format!("{}.json", real_pid));
        let print_path = sessions_dir.join(format!("{}.json", print_pid));
        write_session_file(&real_path, real_pid, real_old, &cwd);
        write_session_file(&print_path, print_pid, print_sid, &cwd);

        let old_transcript = write_transcript(&projects, &cwd, real_old, "first");
        let new_transcript = write_transcript(&projects, &cwd, real_new, "after clear");
        set_mtime(&old_transcript, -30);
        set_mtime(&new_transcript, 0);

        let config = ConfigDir::new(profile.clone());
        let mut process_info = make_proc_info(real_pid, "claude");
        process_info.insert(
            abtop_pid,
            ProcInfo {
                pid: abtop_pid,
                ppid: 1,
                rss_kb: 1024,
                cpu_pct: 0.0,
                command: "abtop".to_string(),
            },
        );
        process_info.insert(
            print_pid,
            ProcInfo {
                pid: print_pid,
                ppid: abtop_pid,
                rss_kb: 512,
                cpu_pct: 0.0,
                command: "claude --print -".to_string(),
            },
        );

        let mut collector = ClaudeCollector::new();
        collector.config_dirs = vec![config.clone()];
        let session_paths = vec![(real_path, config.clone()), (print_path, config)];
        let ctx = build_discovery_context(&session_paths, &process_info, abtop_pid);

        // The --print PID must not appear in the discovery context, so
        // `pids_per_cwd` stays at 1 and the override fires.
        assert_eq!(
            ctx.pids_per_cwd.get(cwd.to_str().unwrap()).copied(),
            Some(1),
            "--print sibling must not inflate pids_per_cwd",
        );
        assert!(
            !ctx.claimed_sids_by_pid.contains_key(&print_pid),
            "--print PID must not claim a sid",
        );

        let sessions = collector.load_session_paths(
            &session_paths,
            &process_info,
            &HashMap::new(),
            &HashMap::new(),
            &ctx,
        );

        // Only the real session survives (--print is dropped in
        // load_session); its sid must be the post-/clear one.
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, real_new);
    }

    #[test]
    fn test_load_session_keeps_user_spawned_print() {
        // Counterpart to the regression above: a `claude --print` session
        // launched by the *user* (not abtop) must be surfaced. Before the
        // self-PID descendant check, every `--print` invocation was filtered
        // unconditionally — this guards against that regressing.
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".claude");
        let sessions_dir = profile.join("sessions");
        let projects = profile.join("projects");
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();

        let abtop_pid = 9999u32;
        let user_print_pid = 4242u32;
        let user_print_sid = "user-print-sid";
        let session_path = sessions_dir.join(format!("{}.json", user_print_pid));
        write_session_file(&session_path, user_print_pid, user_print_sid, &cwd);
        write_transcript(&projects, &cwd, user_print_sid, "user prompt");

        let config = ConfigDir::new(profile.clone());

        let mut process_info = HashMap::new();
        process_info.insert(
            abtop_pid,
            ProcInfo {
                pid: abtop_pid,
                ppid: 1,
                rss_kb: 1024,
                cpu_pct: 0.0,
                command: "abtop".to_string(),
            },
        );
        process_info.insert(
            user_print_pid,
            ProcInfo {
                pid: user_print_pid,
                ppid: 1, // launched by the user, not abtop
                rss_kb: 512,
                cpu_pct: 0.5,
                command: "claude --print user-script".to_string(),
            },
        );

        let mut collector = ClaudeCollector::new();
        collector.config_dirs = vec![config.clone()];
        let session_paths = vec![(session_path, config)];
        let ctx = build_discovery_context(&session_paths, &process_info, abtop_pid);
        let sessions = collector.load_session_paths(
            &session_paths,
            &process_info,
            &HashMap::new(),
            &HashMap::new(),
            &ctx,
        );

        assert_eq!(
            sessions.len(),
            1,
            "user-launched `claude --print` must be surfaced",
        );
        assert_eq!(sessions[0].session_id, user_print_sid);
    }

    #[test]
    fn test_transcript_cache_evicts_old_sid_after_clear() {
        // Two-poll regression: on the first tick abtop parses the old
        // transcript and caches its counters under `old_sid`. After
        // `/clear`, a new `new_sid.jsonl` appears while `old_sid.jsonl`
        // lingers on disk. The second tick must (1) switch the session
        // to `new_sid`, (2) report ONLY the new transcript's counters —
        // not the sum of both — and (3) drop the stale `old_sid` entry
        // from `transcript_cache` so it can't leak forward.
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".claude");
        let sessions_dir = profile.join("sessions");
        let projects = profile.join("projects");
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();

        let pid = 6565;
        let old_sid = "pre-clear-sid";
        let new_sid = "post-clear-sid";
        let session_path = sessions_dir.join(format!("{}.json", pid));
        write_session_file(&session_path, pid, old_sid, &cwd);

        let old_transcript = write_transcript(&projects, &cwd, old_sid, "first chat");

        let config = ConfigDir::new(profile.clone());
        let process_info = make_proc_info(pid, "claude");
        let mut collector = ClaudeCollector::new();
        collector.config_dirs = vec![config.clone()];

        // Poll 1 — only old_sid exists.
        let session_paths = vec![(session_path.clone(), config.clone())];
        let ctx = build_discovery_context(&session_paths, &process_info, 0);
        let first = collector.load_session_paths(
            &session_paths,
            &process_info,
            &HashMap::new(),
            &HashMap::new(),
            &ctx,
        );
        collector.evict_stale_cache(&first);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].session_id, old_sid);
        assert_eq!(first[0].total_input_tokens, 12);
        assert!(
            collector.transcript_cache.contains_key(old_sid),
            "poll 1 should have cached the old sid",
        );

        // Simulate /clear: old jsonl is now older than the new one,
        // which appears with the same 12-token turn. If cache eviction
        // fails and counters leak, total_input_tokens would become 24.
        set_mtime(&old_transcript, -30);
        let new_transcript = {
            let dir = projects.join(encode_cwd_path(cwd.to_str().unwrap()));
            let p = dir.join(format!("{}.jsonl", new_sid));
            std::fs::write(
                &p,
                r#"{"type":"user","timestamp":"2026-03-28T15:10:00Z","message":{"role":"user","content":"second chat"}}
{"type":"assistant","timestamp":"2026-03-28T15:10:05Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":12,"output_tokens":6,"cache_read_input_tokens":3,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"done"}]}}
"#,
            )
            .unwrap();
            p
        };
        set_mtime(&new_transcript, 0);

        // Poll 2 — override must fire and old cache entry must drop.
        let ctx2 = build_discovery_context(&session_paths, &process_info, 0);
        let second = collector.load_session_paths(
            &session_paths,
            &process_info,
            &HashMap::new(),
            &HashMap::new(),
            &ctx2,
        );
        collector.evict_stale_cache(&second);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].session_id, new_sid);
        assert_eq!(
            second[0].total_input_tokens, 12,
            "counters must reflect only the new transcript, not old+new",
        );
        assert!(
            !collector.transcript_cache.contains_key(old_sid),
            "stale old sid must be evicted after /clear",
        );
        assert!(
            collector.transcript_cache.contains_key(new_sid),
            "new sid must be present in the cache after poll 2",
        );
    }

    fn chat_line(kind: &str, ts: &str, content: Value) -> String {
        serde_json::json!({"type":kind, "timestamp":ts,
            "message":{"role":kind, "content":content,
                "usage":{"input_tokens":1,"output_tokens":1}}})
        .to_string()
    }

    fn chat_tool_use(id: &str, name: &str, input: Value) -> String {
        chat_line(
            "assistant",
            "2026-03-28T15:00:05Z",
            serde_json::json!([{"type":"tool_use","id":id,"name":name,"input":input}]),
        )
    }

    fn chat_tool_result(id: &str, content: Value, is_error: bool) -> String {
        chat_line(
            "user",
            "2026-03-28T15:00:06Z",
            serde_json::json!([{"type":"tool_result","tool_use_id":id,
                "content":content,"is_error":is_error}]),
        )
    }

    fn chat_texts(result: &TranscriptResult) -> Vec<(ChatRole, String)> {
        result
            .chat_messages
            .iter()
            .map(|m| (m.role.clone(), m.text.clone()))
            .collect()
    }

    #[test]
    fn chat_lists_tool_calls_and_results_in_transcript_order() {
        let output = (1..=20)
            .map(|i| format!("test line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                &chat_line(
                    "user",
                    "2026-03-28T15:00:00Z",
                    serde_json::json!("run the tests"),
                ),
                &chat_line(
                    "assistant",
                    "2026-03-28T15:00:01Z",
                    serde_json::json!([
                        {"type":"text","text":"Running them."},
                        {"type":"tool_use","id":"b1","name":"Bash",
                         "input":{"command":"cargo test\nsecond line","description":"d"}}
                    ]),
                ),
                &chat_tool_result("b1", serde_json::json!(output), false),
                &chat_tool_use("b2", "Bash", serde_json::json!({"command":"false"})),
                &chat_tool_result("b2", serde_json::json!("Exit code 1\nboom"), true),
                &chat_line(
                    "assistant",
                    "2026-03-28T15:00:09Z",
                    serde_json::json!([{"type":"text","text":"Two failures."}]),
                ),
            ],
        );
        let result = parse_transcript(file.path(), 0);
        let texts = chat_texts(&result);
        let roles: Vec<_> = texts.iter().map(|(role, _)| role.clone()).collect();
        assert_eq!(
            roles,
            vec![
                ChatRole::User,
                ChatRole::Assistant,
                ChatRole::ToolCall,
                ChatRole::ToolResult,
                ChatRole::ToolCall,
                ChatRole::ToolError,
                ChatRole::Assistant,
            ]
        );
        assert_eq!(texts[2].1, "Bash cargo test");
        // Command output keeps its first 8 lines and counts the rest.
        let shown: Vec<&str> = texts[3].1.lines().collect();
        assert_eq!(shown.len(), 9);
        assert_eq!(shown[0], "test line 1");
        assert_eq!(shown[7], "test line 8");
        assert_eq!(shown[8], "… 12 more lines");
        assert_eq!(texts[5].1, "Exit code 1\nboom");
    }

    #[test]
    fn chat_never_shows_file_contents_prompts_or_thinking() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                &chat_line(
                    "assistant",
                    "2026-03-28T15:00:01Z",
                    serde_json::json!([
                        {"type":"thinking","thinking":"THINKSECRET plan","signature":"sig"},
                        {"type":"tool_use","id":"r1","name":"Read","input":{"file_path":"/w/src/lib.rs"}}
                    ]),
                ),
                &chat_tool_result(
                    "r1",
                    serde_json::json!("READSECRET one\nREADSECRET two"),
                    false,
                ),
                &chat_tool_use(
                    "e1",
                    "Edit",
                    serde_json::json!({"file_path":"/w/src/lib.rs",
                        "old_string":"OLDSECRET","new_string":"NEWSECRET"}),
                ),
                &chat_tool_result(
                    "e1",
                    serde_json::json!("String to replace not found: OLDSECRET"),
                    true,
                ),
                &chat_tool_use(
                    "w1",
                    "Write",
                    serde_json::json!({"file_path":"/w/new.txt","content":"WRITESECRET"}),
                ),
                &chat_tool_result("w1", serde_json::json!("File created"), false),
                &chat_tool_use(
                    "g1",
                    "Grep",
                    serde_json::json!({"pattern":"needle","path":"/w"}),
                ),
                &chat_tool_result(
                    "g1",
                    serde_json::json!([{"type":"text","text":"GREPSECRET /w/a.rs:3: needle"}]),
                    false,
                ),
                &chat_tool_use(
                    "u1",
                    "WebFetch",
                    serde_json::json!({"url":"https://x.test/a?tok=URLSECRET"}),
                ),
                &chat_tool_result("u1", serde_json::json!("FETCHSECRET page body"), false),
                &chat_tool_use(
                    "t1",
                    "Task",
                    serde_json::json!({"description":"explore","prompt":"PROMPTSECRET"}),
                ),
                &chat_tool_result("t1", serde_json::json!("SUBSECRET findings"), false),
                // A result whose call was never seen is only summarized.
                &chat_tool_result("lost", serde_json::json!("LOSTSECRET"), false),
            ],
        );
        let result = parse_transcript(file.path(), 0);
        let texts = chat_texts(&result);
        for (_, text) in &texts {
            for secret in [
                "THINKSECRET",
                "READSECRET",
                "OLDSECRET",
                "NEWSECRET",
                "WRITESECRET",
                "GREPSECRET",
                "URLSECRET",
                "FETCHSECRET",
                "PROMPTSECRET",
                "SUBSECRET",
                "LOSTSECRET",
            ] {
                assert!(!text.contains(secret), "{secret} leaked in {text:?}");
            }
        }
        let call_lines: Vec<&str> = texts
            .iter()
            .filter(|(role, _)| *role == ChatRole::ToolCall)
            .map(|(_, text)| text.as_str())
            .collect();
        assert_eq!(
            call_lines,
            vec![
                "Read /w/src/lib.rs",
                "Edit /w/src/lib.rs",
                "Write /w/new.txt",
                "Grep needle",
                "WebFetch https://x.test/a",
                "Task explore",
            ]
        );
        assert_eq!(
            texts[1].1, "2 lines, 29 B · content hidden",
            "Read results are a size summary"
        );
        // The Edit failure repeats file text, so it is a summary flagged as an error.
        assert_eq!(texts[3].0, ChatRole::ToolError);
        assert!(texts[3].1.starts_with("error · 1 line"), "{:?}", texts[3]);
        assert_eq!(texts.last().unwrap().1, "1 line, 10 B · content hidden");
    }

    #[test]
    fn chat_tool_lines_are_sanitized_and_redacted() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                &chat_tool_use(
                    "b1",
                    "Bash",
                    serde_json::json!({"command":"export K=sk-ant-abc123 \u{1b}[31m\u{202e}x"}),
                ),
                &chat_tool_result(
                    "b1",
                    serde_json::json!("key ghp_secrettoken\n\u{1b}[2Jcleared \u{202e}rtl"),
                    false,
                ),
            ],
        );
        let result = parse_transcript(file.path(), 0);
        for message in &result.chat_messages {
            assert!(!message.text.contains("sk-ant-abc123"), "{}", message.text);
            assert!(
                !message.text.contains("ghp_secrettoken"),
                "{}",
                message.text
            );
            assert!(!message.text.contains('\u{1b}'));
            assert!(!message.text.contains('\u{202e}'));
            assert!(message.text.contains("[REDACTED]") || message.role == ChatRole::ToolResult);
        }
        assert!(result.chat_messages[0].text.contains("[REDACTED]"));
        assert!(result.chat_messages[1].text.contains("[REDACTED]"));
    }

    #[test]
    fn chat_results_find_their_tool_across_incremental_parses_and_partial_lines() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                &chat_tool_use("b1", "Bash", serde_json::json!({"command":"ls"})),
                &chat_tool_use("r1", "Read", serde_json::json!({"file_path":"/w/a.rs"})),
            ],
        );
        let first = parse_transcript(file.path(), 0);
        assert_eq!(first.chat_messages.len(), 2);
        assert_eq!(first.chat_tools.len(), 2);

        // Results arrive in a later read; the second line is cut mid-JSON.
        let bash = chat_tool_result("b1", serde_json::json!("a.txt\nb.txt"), false);
        let read = chat_tool_result("r1", serde_json::json!("READSECRET"), false);
        let (head, tail) = read.split_at(read.len() / 2);
        writeln!(file, "{bash}").unwrap();
        write!(file, "{head}").unwrap();
        file.flush().unwrap();
        let delta = parse_transcript_with_state(
            file.path(),
            first.new_offset,
            0,
            0,
            None,
            Some(&first.chat_tools),
        );
        assert_eq!(
            chat_texts(&delta),
            vec![(ChatRole::ToolResult, "a.txt\nb.txt".to_string())]
        );
        // The partial line stays unread until it is complete.
        assert_eq!(delta.new_offset, first.new_offset + bash.len() as u64 + 1);

        writeln!(file, "{tail}").unwrap();
        file.flush().unwrap();
        let second = parse_transcript_with_state(
            file.path(),
            delta.new_offset,
            0,
            0,
            None,
            Some(&delta.chat_tools),
        );
        assert_eq!(
            chat_texts(&second),
            vec![(
                ChatRole::ToolResult,
                "1 line, 10 B · content hidden".to_string()
            )]
        );

        // Without the carried ids a result can only be summarized.
        let blind = parse_transcript_with_state(file.path(), first.new_offset, 0, 0, None, None);
        assert_eq!(blind.chat_messages.len(), 2);
        assert!(blind
            .chat_messages
            .iter()
            .all(|m| m.text.ends_with("content hidden")));

        // The incremental pieces add up to the full parse.
        let full = parse_transcript(file.path(), 0);
        let mut merged = first.chat_messages.clone();
        merged.extend(delta.chat_messages);
        merged.extend(second.chat_messages);
        crate::model::trim_chat_window(&mut merged);
        assert_eq!(merged, full.chat_messages);
    }

    #[test]
    fn chat_window_stays_bounded_with_tool_heavy_transcripts() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        for i in 0..200u32 {
            let id = format!("id{i}");
            let mut lines = vec![chat_tool_use(
                &id,
                "Bash",
                serde_json::json!({"command":format!("echo {i}")}),
            )];
            lines.push(chat_tool_result(
                &id,
                serde_json::json!(format!("out {i}")),
                false,
            ));
            if i.is_multiple_of(20) {
                lines.push(chat_line(
                    "assistant",
                    "2026-03-28T15:00:09Z",
                    serde_json::json!([{"type":"text","text":format!("note {i}")}]),
                ));
            }
            let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
            write_lines(&mut file, &refs);
        }
        let result = parse_transcript(file.path(), 0);
        assert!(result.chat_messages.len() <= crate::model::MAX_CHAT_ITEMS);
        assert_eq!(result.chat_messages.last().unwrap().text, "out 199");
        assert!(result.chat_tools.len() <= 128);
        let dialogue = result
            .chat_messages
            .iter()
            .filter(|m| m.role.is_dialogue())
            .count();
        assert!(dialogue <= crate::model::MAX_CHAT_MESSAGES);
        for message in &result.chat_messages {
            assert!(message.text.chars().count() <= 20_000);
        }
    }

    #[test]
    fn collector_keeps_tool_results_matched_after_caching_and_resets_on_truncation() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".claude");
        let config = ConfigDir::new(profile);
        let cwd = temp.path().join("repo");
        fs::create_dir_all(&config.sessions_dir).unwrap();
        fs::create_dir_all(&cwd).unwrap();
        let pid = 9300;
        let sid = "chat-tools";
        let session_path = config.sessions_dir.join("9300.json");
        write_session_file(&session_path, pid, sid, &cwd);
        let project = config
            .projects_dir
            .join(encode_cwd_path(cwd.to_str().unwrap()));
        fs::create_dir_all(&project).unwrap();
        let transcript = project.join(format!("{sid}.jsonl"));
        let paths = vec![(session_path, config)];
        let procs = make_proc_info(pid, "claude");
        let ctx = build_discovery_context(&paths, &procs, 0);
        let mut collector = ClaudeCollector::new();
        let load = |collector: &mut ClaudeCollector| {
            collector
                .load_session_paths(&paths, &procs, &HashMap::new(), &HashMap::new(), &ctx)
                .remove(0)
        };

        let call = chat_tool_use("b1", "Bash", serde_json::json!({"command":"ls"}));
        fs::write(&transcript, format!("{call}\n")).unwrap();
        let session = load(&mut collector);
        assert_eq!(session.chat_messages.len(), 1);
        assert_eq!(session.chat_messages[0].text, "Bash ls");

        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .unwrap();
        writeln!(
            file,
            "{}",
            chat_tool_result("b1", serde_json::json!("one\ntwo"), false)
        )
        .unwrap();
        file.flush().unwrap();
        let session = load(&mut collector);
        let texts: Vec<_> = session
            .chat_messages
            .iter()
            .map(|m| m.text.as_str())
            .collect();
        assert_eq!(texts, vec!["Bash ls", "one\ntwo"]);
        assert_eq!(session.chat_messages[1].role, ChatRole::ToolResult);

        // A no-op tick keeps the retained items as they are.
        assert_eq!(load(&mut collector).chat_messages.len(), 2);

        // A shorter file is a fresh transcript: items and tool ids start over,
        // so a result without its call is only summarized.
        fs::write(
            &transcript,
            format!(
                "{}\n",
                chat_tool_result("b1", serde_json::json!("x"), false)
            ),
        )
        .unwrap();
        let session = load(&mut collector);
        assert_eq!(session.chat_messages.len(), 1);
        assert_eq!(
            session.chat_messages[0].text,
            "1 line, 1 B · content hidden"
        );
    }
}
