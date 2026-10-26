//! Top-level session browser and creation form, independent of live metrics.
use crate::archive::{self, Agent, ArchivedSession, ScanResult};
use crate::launcher::{self, LaunchRequest};
use crate::model::{AgentSession, SessionStatus};
use crossterm::event::KeyCode;
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MainTab {
    #[default]
    Dashboard,
    Old,
    New,
}
impl MainTab {
    pub const ALL: [Self; 3] = [Self::Dashboard, Self::Old, Self::New];
    pub fn label(self) -> &'static str {
        match self {
            Self::Dashboard => "Agents Dashboard",
            Self::Old => "Session Archive",
            Self::New => "New Session",
        }
    }
}

pub struct NewSession {
    pub agent: Agent,
    pub models: Vec<String>,
    pub model: usize,
    pub model_source: String,
    pub projects: Vec<PathBuf>,
    pub project: usize,
    pub path: String,
    pub editing: bool,
    original_path: String,
    pub field: usize,
}
impl Default for NewSession {
    fn default() -> Self {
        Self {
            agent: Agent::Claude,
            models: vec!["Default".into()],
            model: 0,
            model_source: "Configured models only; catalog not loaded".into(),
            projects: Vec::new(),
            project: 0,
            // Start where the dashboard was launched; history never replaces it.
            path: std::env::current_dir()
                .ok()
                .or_else(dirs::home_dir)
                .map(|dir| dir.to_string_lossy().into_owned())
                .unwrap_or_default(),
            editing: false,
            original_path: String::new(),
            field: 0,
        }
    }
}

pub(crate) struct ArchiveResume {
    pub request: LaunchRequest,
    pub path: String,
}

#[derive(Default)]
pub struct Workspace {
    pub tab: MainTab,
    pub archive: Vec<ArchivedSession>,
    pub selected: usize,
    pub query: String,
    pub searching: bool,
    pub agent_filter: usize,
    pub loading: bool,
    pub loaded: bool,
    pub limited: bool,
    pub message: String,
    pub new: NewSession,
    pub claude_dirs: Vec<PathBuf>,
    pub(crate) resume: Option<ArchiveResume>,
    receiver: Option<Receiver<ScanResult>>,
}

impl Workspace {
    pub fn switch(&mut self, backwards: bool, live: &[AgentSession]) {
        let index = MainTab::ALL
            .iter()
            .position(|t| *t == self.tab)
            .unwrap_or(0);
        self.set_tab(
            MainTab::ALL[(index + if backwards { 2 } else { 1 }) % 3],
            live,
        );
    }
    pub fn set_tab(&mut self, tab: MainTab, live: &[AgentSession]) {
        self.tab = tab;
        self.searching = false;
        if self.resume.take().is_some() {
            self.message.clear();
        }
        if self.new.editing {
            self.new.path = self.new.original_path.clone();
        }
        self.new.editing = false;
        if self.tab != MainTab::Dashboard && !self.loaded {
            self.refresh();
        }
        if self.tab == MainTab::New {
            self.update_projects(live);
            self.new.load_models();
        }
    }
    pub fn refresh(&mut self) {
        if self.loading {
            return;
        }
        let roots = archive::claude_roots(&self.claude_dirs);
        let codex = archive::default_root(Agent::Codex);
        let (tx, rx) = mpsc::channel();
        self.receiver = Some(rx);
        self.loading = true;
        std::thread::spawn(move || {
            let _ = tx.send(archive::scan(&roots, &codex));
        });
    }
    pub fn poll(&mut self, live: &[AgentSession]) {
        if let Some(rx) = &self.receiver {
            match rx.try_recv() {
                Ok(result) => {
                    self.archive = result.sessions;
                    self.limited = result.limited;
                    self.loaded = true;
                    self.loading = false;
                    self.receiver = None;
                    self.update_projects(live);
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.loading = false;
                    self.receiver = None;
                    self.message = "History scan failed; press r to retry.".into();
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        self.selected = self
            .selected
            .min(self.visible(live).len().saturating_sub(1));
    }
    pub fn visible(&self, live: &[AgentSession]) -> Vec<&ArchivedSession> {
        let query = self.query.to_lowercase();
        self.archive
            .iter()
            .filter(|s| {
                !live.iter().any(|l| {
                    l.agent_cli == s.agent.cli()
                        && l.session_id == s.id
                        && l.status != SessionStatus::Done
                }) && (self.agent_filter == 0
                    || (self.agent_filter == 1 && s.agent == Agent::Claude)
                    || (self.agent_filter == 2 && s.agent == Agent::Codex))
                    && (query.is_empty()
                        || format!(
                            "{} {} {} {}",
                            s.id,
                            s.title,
                            s.cwd.display(),
                            s.root.display()
                        )
                        .to_lowercase()
                        .contains(&query))
            })
            .collect()
    }
    fn update_projects(&mut self, live: &[AgentSession]) {
        let mut seen = BTreeSet::new();
        let projects = live
            .iter()
            .map(|s| PathBuf::from(&s.cwd))
            .chain(self.archive.iter().map(|s| s.cwd.clone()))
            .filter(|p| p.is_absolute() && p.is_dir() && seen.insert(p.clone()))
            .take(100)
            .collect();
        self.new.projects = projects;
        // The path is authoritative; history refreshes only update the chooser.
        self.new.project = self
            .new
            .projects
            .iter()
            .position(|p| p.to_string_lossy() == self.new.path)
            .unwrap_or(self.new.projects.len());
    }
    pub fn handle_key(&mut self, code: KeyCode, live: &[AgentSession], demo: bool) {
        self.handle_key_with(code, live, demo, launcher::launch);
    }
    fn handle_key_with(
        &mut self,
        code: KeyCode,
        live: &[AgentSession],
        demo: bool,
        mut launch: impl FnMut(&LaunchRequest) -> Result<(), String>,
    ) {
        if self.tab == MainTab::Old {
            if let Some(resume) = &mut self.resume {
                match code {
                    KeyCode::Esc => {
                        self.resume = None;
                        self.message.clear();
                    }
                    KeyCode::Enter if !demo => {
                        let cwd = expand_home(&resume.path);
                        if !cwd.is_absolute() || !cwd.is_dir() {
                            self.message =
                                "Choose an existing absolute directory (or ~/...).".into();
                            return;
                        }
                        let mut request = resume.request.clone();
                        request.cwd = cwd;
                        self.message = match launch(&request) {
                            Ok(()) => {
                                self.resume = None;
                                format!("Opened {} window.", request.agent.label())
                            }
                            Err(error) => error,
                        };
                    }
                    KeyCode::Backspace => {
                        resume.path.pop();
                    }
                    KeyCode::Char(c) if !c.is_control() && resume.path.len() < 4096 => {
                        resume.path.push(c);
                    }
                    _ => {}
                }
                return;
            }
            if self.searching {
                match code {
                    KeyCode::Esc => {
                        self.searching = false;
                        self.query.clear();
                        self.selected = 0;
                    }
                    KeyCode::Enter => self.searching = false,
                    KeyCode::Backspace => {
                        self.query.pop();
                        self.selected = 0;
                    }
                    KeyCode::Char(c) if !c.is_control() && self.query.len() < 1000 => {
                        self.query.push(c);
                        self.selected = 0;
                    }
                    _ => {}
                }
                return;
            }
            match code {
                KeyCode::Char('/') => self.searching = true,
                KeyCode::Esc => {
                    self.query.clear();
                    self.selected = 0;
                }
                KeyCode::Left => {
                    self.agent_filter = (self.agent_filter + 2) % 3;
                    self.selected = 0;
                }
                KeyCode::Right => {
                    self.agent_filter = (self.agent_filter + 1) % 3;
                    self.selected = 0;
                }
                KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => {
                    self.selected =
                        (self.selected + 1).min(self.visible(live).len().saturating_sub(1))
                }
                KeyCode::Char('r') if !demo => self.refresh(),
                KeyCode::Enter if !demo => {
                    let request = self
                        .visible(live)
                        .get(self.selected)
                        .map(|s| LaunchRequest::from(*s));
                    if let Some(request) = request {
                        if !request.cwd.is_absolute() || !request.cwd.is_dir() {
                            self.resume = Some(ArchiveResume {
                                path: request.cwd.to_string_lossy().into_owned(),
                                request,
                            });
                            self.message = "Original working directory is unavailable; enter its new location.".into();
                        } else {
                            self.launch(request, &mut launch);
                        }
                    }
                }
                _ => {}
            }
        } else if self.tab == MainTab::New {
            if self.new.editing {
                match code {
                    KeyCode::Esc => {
                        self.new.path = self.new.original_path.clone();
                        self.new.editing = false;
                    }
                    KeyCode::Enter => {
                        self.new.editing = false;
                        self.new.load_models();
                    }
                    KeyCode::Backspace => {
                        self.new.path.pop();
                    }
                    KeyCode::Char(c) if !c.is_control() && self.new.path.len() < 4096 => {
                        self.new.path.push(c);
                    }
                    _ => {}
                }
                return;
            }
            match code {
                KeyCode::Esc => {
                    self.new = NewSession::default();
                    self.tab = MainTab::Dashboard;
                    self.message.clear();
                }
                KeyCode::Up => self.new.field = (self.new.field + 3) % 4,
                KeyCode::Down => self.new.field = (self.new.field + 1) % 4,
                KeyCode::Left | KeyCode::Right => self.new.choose(code == KeyCode::Left),
                KeyCode::Enter if self.new.field == 2 => {
                    self.new.original_path = self.new.path.clone();
                    self.new.editing = true;
                }
                KeyCode::Enter if self.new.field == 3 && !demo => {
                    let cwd = expand_home(&self.new.path);
                    self.launch(
                        LaunchRequest {
                            agent: self.new.agent,
                            cwd,
                            root: archive::default_root(self.new.agent),
                            resume: None,
                            model: (self.new.model > 0)
                                .then(|| self.new.models[self.new.model].clone()),
                        },
                        &mut launch,
                    );
                }
                _ => {}
            }
        }
    }
    fn launch(
        &mut self,
        request: LaunchRequest,
        mut launch: impl FnMut(&LaunchRequest) -> Result<(), String>,
    ) {
        self.message = match launch(&request) {
            Ok(()) => format!("Opened {} window.", request.agent.label()),
            Err(error) => error,
        };
    }
}

fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        dirs::home_dir().unwrap_or_default().join(rest)
    } else {
        path.into()
    }
}

fn read_small(path: &Path) -> Option<String> {
    let file = File::open(path).ok()?;
    if file.metadata().ok()?.len() > 2 * 1024 * 1024 {
        return None;
    }
    let mut text = String::new();
    file.take(2 * 1024 * 1024).read_to_string(&mut text).ok()?;
    Some(text)
}
fn valid_model(model: &str) -> bool {
    !model.is_empty()
        && model.len() < 200
        && !model.starts_with('-')
        && !model.chars().any(char::is_control)
}

/// Only offer models explicitly present in local CLI settings/catalogs. No
/// guesses about provider availability and no direct model text entry.
const MODEL_ENV_KEYS: [&str; 4] = [
    "ANTHROPIC_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
];

/// These are forwarded as literal env argv by the launcher, so tmux's server
/// environment cannot silently select a different provider from this form.
pub(crate) const CLAUDE_CONTEXT_ENV_KEYS: [&str; 8] = [
    "ANTHROPIC_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_BASE_URL",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
];

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&read_small(path)?).ok()
}

fn configured_context(root: &Path, cwd: &Path, inherited: &Value) -> (Value, Option<String>) {
    let mut env = inherited.as_object().cloned().unwrap_or_default();
    let mut model = None;
    // Bounded, explicit settings layers only. Managed policy, login and remote
    // feature flags cannot be inferred here; the UI must not promise parity.
    for path in [
        root.join("settings.json"),
        cwd.join(".claude/settings.json"),
        cwd.join(".claude/settings.local.json"),
    ] {
        if let Some(settings) = read_json(&path) {
            if let Some(value) = settings["model"].as_str() {
                model = Some(value.to_owned());
            }
            if let Some(values) = settings["env"].as_object() {
                for key in CLAUDE_CONTEXT_ENV_KEYS {
                    if let Some(value) = values.get(key).filter(|v| v.is_string()) {
                        env.insert(key.to_owned(), value.clone());
                    }
                }
            }
        }
    }
    (Value::Object(env), model)
}

fn timestamp(cache: &Value, now: i64) -> Option<i64> {
    cache["fetchedAt"].as_i64().filter(|f| *f >= 0 && *f <= now)
}

/// Parse only model rows and the documented alias target slots, never arbitrary
/// nested `id` fields (effort levels, toggles, and vocabulary are not models).
fn cc_models(cache: &Value, now: i64) -> Option<(i64, bool, BTreeSet<String>)> {
    if cache["version"] != 2 || cache["catalog"]["surface"] != "cc" {
        return None;
    }
    let fetched = timestamp(cache, now)?;
    let stale_at = cache["staleAt"].as_i64().filter(|s| *s >= fetched)?;
    let config = &cache["catalog"]["config"];
    let rows = config["models"].as_array()?;
    let mut models: BTreeSet<String> = rows
        .iter()
        .take(1000)
        .filter_map(|row| row["id"].as_str())
        .filter(|id| valid_model(id))
        .map(str::to_owned)
        .collect();
    if let Some(aliases) = config["provider_alias_targets"].as_object() {
        for (alias, target) in aliases.iter().take(100) {
            if valid_model(alias) && target["default"].as_str().is_some_and(valid_model) {
                models.insert(alias.clone());
                models.insert(target["default"].as_str()?.to_owned());
            }
        }
    }
    Some((fetched, now >= stale_at, models))
}

/// Claude sessions launch through `ocx claude`, which points them at the
/// OpenCodex proxy. Only its port is used; the rest of the file is ignored.
fn ocx_proxy_base() -> Option<String> {
    let path = dirs::home_dir()?.join(".opencodex/runtime-port.json");
    let port = u16::try_from(read_json(&path)?["port"].as_u64()?).ok()?;
    Some(format!("http://127.0.0.1:{port}"))
}

/// Merge the gateway cache rows when its provider URL matches `base`; returns
/// the freshness label, or `None` when the cache is missing/invalid/mismatched.
fn gateway_models(
    root: &Path,
    base: &str,
    now: i64,
    models: &mut BTreeSet<String>,
) -> Option<&'static str> {
    let cache = read_json(&root.join("cache/gateway-models.json"))?;
    let fetched = timestamp(&cache, now)?;
    if cache["baseUrl"].as_str()?.trim_end_matches('/') != base {
        return None;
    }
    models.extend(
        cache["models"]
            .as_array()?
            .iter()
            .take(1000)
            .filter_map(|row| row["id"].as_str())
            .map(str::to_owned),
    );
    Some(if now - fetched > 24 * 60 * 60 * 1000 {
        "stale >24h"
    } else {
        "recent <=24h"
    })
}

fn claude_catalog(
    root: &Path,
    env: &Value,
    ocx_base: Option<&str>,
    models: &mut BTreeSet<String>,
) -> String {
    let now = chrono::Utc::now().timestamp_millis();
    if [
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
        "CLAUDE_CODE_USE_FOUNDRY",
    ]
    .iter()
    .any(|key| env[*key].as_str().is_some_and(|v| v == "1" || v == "true"))
    {
        return "Configured only (cloud provider; no scoped catalog)".into();
    }
    let base = env["ANTHROPIC_BASE_URL"]
        .as_str()
        .unwrap_or("")
        .trim_end_matches('/');
    if !base.is_empty() && base != "https://api.anthropic.com" {
        return match gateway_models(root, base, now, models) {
            Some(age) => format!("Gateway cache ({age}; last-known, not verified)"),
            None => "Configured only (gateway cache missing/invalid/provider mismatch)".into(),
        };
    }
    // Without an explicit provider the session still runs through ocx, so its
    // proxy cache is the catalog; otherwise fall through to the stock catalog.
    if base.is_empty() {
        if let Some(age) = ocx_base.and_then(|ocx| gateway_models(root, ocx, now, models)) {
            return format!("Gateway cache via ocx ({age}; last-known, not verified)");
        }
    }
    // The cc cache is shared across CLI contexts. Choose one newest valid
    // snapshot, not a union of organizations or gateway/stock catalogs.
    let mut newest = None;
    if let Ok(entries) = std::fs::read_dir(root.join("cache/model-catalog")) {
        for entry in entries.take(128).flatten() {
            if !entry.file_name().to_string_lossy().ends_with("-cc.json") {
                continue;
            }
            if let Some(parsed) = read_json(&entry.path()).and_then(|c| cc_models(&c, now)) {
                if newest
                    .as_ref()
                    .is_none_or(|(fetched, _, _)| parsed.0 > *fetched)
                {
                    newest = Some(parsed);
                }
            }
        }
    }
    if let Some((_, stale, rows)) = newest {
        models.extend(rows);
        format!(
            "CLI cc cache ({}; account/flags unverified)",
            if stale { "stale" } else { "fresh" }
        )
    } else {
        "Configured only (CLI cc cache missing/invalid)".into()
    }
}

fn models_for_context(
    agent: Agent,
    root: &Path,
    cwd: &Path,
    inherited: &Value,
    ocx_base: Option<&str>,
) -> (Vec<String>, String) {
    let mut models = BTreeSet::new();
    let source;
    if agent == Agent::Claude {
        let (env, configured) = configured_context(root, cwd, inherited);
        models.extend(configured);
        for key in MODEL_ENV_KEYS {
            if let Some(model) = env[key].as_str() {
                models.insert(model.to_owned());
            }
        }
        source = claude_catalog(root, &env, ocx_base, &mut models);
    } else {
        source = "Codex local config/cache (not verified)".into();
        if let Some(config) = read_small(&root.join("config.toml")) {
            for line in config.lines() {
                if let Some((key, value)) = line.split_once('=') {
                    if key.trim() == "model" {
                        // Accept only an entire quoted literal, not TOML expressions.
                        let raw = value.trim();
                        if let Ok(model) = serde_json::from_str::<String>(raw) {
                            models.insert(model);
                        } else if raw.starts_with('\'') && raw.ends_with('\'') && raw.len() > 2 {
                            models.insert(raw[1..raw.len() - 1].into());
                        }
                    }
                }
            }
        }
        if let Some(cache) = read_small(&root.join("models_cache.json"))
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        {
            if let Some(entries) = cache["models"].as_array() {
                for entry in entries {
                    if entry["visibility"] == "hide" {
                        continue;
                    }
                    if let Some(slug) = entry["slug"].as_str() {
                        models.insert(slug.to_owned());
                    }
                }
            }
        }
    }
    (
        std::iter::once("Default".into())
            .chain(
                models
                    .into_iter()
                    .filter(|m| m != "Default" && valid_model(m)),
            )
            .collect(),
        source,
    )
}
#[cfg(test)]
fn models_from_root(agent: Agent, root: &Path, inherited: &Value) -> Vec<String> {
    models_for_context(agent, root, root, inherited, None).0
}
impl NewSession {
    fn load_models(&mut self) {
        let inherited = CLAUDE_CONTEXT_ENV_KEYS
            .into_iter()
            .filter_map(|key| {
                std::env::var(key)
                    .ok()
                    .map(|value| (key.to_owned(), Value::String(value)))
            })
            .collect::<serde_json::Map<String, Value>>();
        let selected = self.models.get(self.model).cloned();
        let ocx_base = (self.agent == Agent::Claude).then(ocx_proxy_base).flatten();
        (self.models, self.model_source) = models_for_context(
            self.agent,
            &archive::default_root(self.agent),
            &expand_home(&self.path),
            &Value::Object(inherited),
            ocx_base.as_deref(),
        );
        self.model = selected
            .and_then(|model| self.models.iter().position(|item| *item == model))
            .unwrap_or(0);
    }
    fn choose(&mut self, backwards: bool) {
        let cycle =
            |current: usize, len: usize| (current + if backwards { len - 1 } else { 1 }) % len;
        match self.field {
            0 => {
                self.agent = if self.agent == Agent::Claude {
                    Agent::Codex
                } else {
                    Agent::Claude
                };
                self.load_models();
            }
            1 => self.model = cycle(self.model, self.models.len()),
            2 => {
                self.project = cycle(self.project, self.projects.len() + 1);
                if let Some(path) = self.projects.get(self.project) {
                    self.path = path.to_string_lossy().into_owned();
                }
                self.load_models();
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive_workspace(agent: Agent, cwd: &Path, root: &Path) -> Workspace {
        Workspace {
            tab: MainTab::Old,
            loaded: true,
            archive: vec![ArchivedSession {
                agent,
                id: "archived-session".into(),
                title: "Archived work".into(),
                cwd: cwd.into(),
                root: root.into(),
                modified: 0,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn archive_existing_directory_resumes_without_path_prompt() {
        let temp = tempfile::tempdir().unwrap();
        for agent in [Agent::Claude, Agent::Codex] {
            let mut workspace = archive_workspace(agent, temp.path(), temp.path());
            let mut calls = 0;
            workspace.handle_key_with(KeyCode::Enter, &[], false, |request| {
                calls += 1;
                assert_eq!(request.agent, agent);
                assert_eq!(request.cwd, temp.path());
                assert_eq!(request.root, temp.path());
                assert_eq!(request.resume.as_deref(), Some("archived-session"));
                assert!(request.model.is_none());
                Ok(())
            });
            assert_eq!(calls, 1);
            assert!(workspace.resume.is_none());
        }
    }

    #[test]
    fn archive_missing_directory_prompts_and_resumes_frozen_session_at_new_path() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("moved");
        let replacement = temp.path().join("new project");
        std::fs::create_dir(&replacement).unwrap();
        for agent in [Agent::Claude, Agent::Codex] {
            let mut workspace = archive_workspace(agent, &missing, temp.path());
            workspace.handle_key_with(KeyCode::Enter, &[], false, |_| panic!("Must prompt first"));
            assert_eq!(
                workspace.resume.as_ref().unwrap().path,
                missing.to_string_lossy()
            );
            for _ in missing.to_string_lossy().chars() {
                workspace.handle_key(KeyCode::Backspace, &[], false);
            }
            for c in replacement.to_string_lossy().chars() {
                workspace.handle_key(KeyCode::Char(c), &[], false);
            }
            // A background scan may replace the list while the prompt is open.
            workspace.archive.clear();
            let mut calls = 0;
            workspace.handle_key_with(KeyCode::Enter, &[], false, |request| {
                calls += 1;
                assert_eq!(request.agent, agent);
                assert_eq!(request.cwd, replacement);
                assert_eq!(request.root, temp.path());
                assert_eq!(request.resume.as_deref(), Some("archived-session"));
                assert!(request.model.is_none());
                Ok(())
            });
            assert_eq!(calls, 1);
            assert!(workspace.resume.is_none());
            assert_eq!(
                workspace.message,
                format!("Opened {} window.", agent.label())
            );
        }
    }

    #[test]
    fn archive_replacement_rejects_invalid_paths_and_keeps_original_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("moved");
        let file = temp.path().join("file");
        std::fs::write(&file, "not a directory").unwrap();
        let root = temp.path().join("missing-config");
        let mut workspace = archive_workspace(Agent::Claude, &missing, &root);
        workspace.handle_key(KeyCode::Enter, &[], false);
        for path in [
            String::new(),
            "relative".into(),
            missing.to_string_lossy().into_owned(),
            file.to_string_lossy().into_owned(),
        ] {
            workspace.resume.as_mut().unwrap().path = path;
            workspace.handle_key_with(KeyCode::Enter, &[], false, |_| panic!("Invalid path"));
            assert!(workspace.message.contains("existing absolute directory"));
            assert!(workspace.resume.is_some());
        }
        workspace.resume.as_mut().unwrap().path = temp.path().to_string_lossy().into_owned();
        workspace.handle_key(KeyCode::Enter, &[], false);
        assert!(workspace.message.contains("config root"));
        assert_eq!(workspace.resume.as_ref().unwrap().request.root, root);
        assert_eq!(workspace.resume.as_ref().unwrap().request.cwd, missing);
        assert_eq!(workspace.archive[0].cwd, missing);
    }

    #[test]
    fn archive_replacement_accepts_home_paths() {
        let temp = tempfile::tempdir().unwrap();
        let mut workspace =
            archive_workspace(Agent::Claude, &temp.path().join("moved"), temp.path());
        workspace.handle_key(KeyCode::Enter, &[], false);
        workspace.resume.as_mut().unwrap().path = "~/".into();
        let mut calls = 0;
        workspace.handle_key_with(KeyCode::Enter, &[], false, |request| {
            calls += 1;
            assert_eq!(request.cwd, dirs::home_dir().unwrap());
            Ok(())
        });
        assert_eq!(calls, 1);
        assert!(workspace.resume.is_none());
    }

    #[test]
    fn archive_path_prompt_cancels_without_launching_or_changing_archive() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("moved");
        let mut workspace = archive_workspace(Agent::Claude, &missing, temp.path());
        workspace.handle_key_with(KeyCode::Enter, &[], true, |_| {
            panic!("Demo must not launch")
        });
        assert!(workspace.resume.is_none());
        workspace.handle_key(KeyCode::Enter, &[], false);
        for code in [
            KeyCode::Char('q'),
            KeyCode::Char('/'),
            KeyCode::Char('r'),
            KeyCode::Down,
        ] {
            workspace.handle_key(code, &[], false);
        }
        assert!(!workspace.searching);
        assert!(!workspace.loading);
        assert_eq!(workspace.selected, 0);
        workspace.handle_key_with(KeyCode::Esc, &[], false, |_| panic!("Cancelled"));
        assert!(workspace.resume.is_none());
        assert!(workspace.message.is_empty());
        assert_eq!(workspace.archive[0].cwd, missing);
        workspace.handle_key(KeyCode::Enter, &[], false);
        workspace.set_tab(MainTab::Dashboard, &[]);
        assert!(workspace.resume.is_none());
        assert!(workspace.message.is_empty());
    }

    #[test]
    fn form_navigation_requires_create_button_and_escape_restores_edit() {
        let mut workspace = Workspace {
            tab: MainTab::New,
            ..Default::default()
        };
        workspace.new.path = "/original".into();
        workspace.handle_key(KeyCode::Down, &[], true);
        assert_eq!(workspace.new.field, 1);
        workspace.handle_key(KeyCode::Down, &[], true);
        workspace.handle_key(KeyCode::Enter, &[], true);
        assert!(workspace.new.editing);
        workspace.handle_key(KeyCode::Char('x'), &[], true);
        workspace.handle_key(KeyCode::Esc, &[], true);
        assert_eq!(workspace.new.path, "/original");
        assert!(!workspace.new.editing);
        assert!(workspace.message.is_empty());
        workspace.handle_key(KeyCode::Down, &[], true);
        assert_eq!(workspace.new.field, 3);
    }
    #[test]
    fn keyboard_agent_change_reloads_source_and_model_keys_remain_selection_only() {
        let mut workspace = Workspace {
            tab: MainTab::New,
            ..Default::default()
        };
        workspace.new.models = vec!["Default".into(), "not-a-codex-model".into()];
        workspace.new.model = 1;
        workspace.handle_key(KeyCode::Right, &[], true);
        assert_eq!(workspace.new.agent, Agent::Codex);
        assert!(workspace.new.model_source.starts_with("Codex local"));
        assert_eq!(workspace.new.model, 0);
        assert!(!workspace
            .new
            .models
            .iter()
            .any(|m| m == "not-a-codex-model"));
        workspace.new.field = 1;
        let source = workspace.new.model_source.clone();
        workspace.handle_key(KeyCode::Right, &[], true);
        assert_eq!(workspace.new.model_source, source);
        assert!(workspace.message.is_empty());
    }

    #[test]
    fn models_are_local_and_default_is_first() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("settings.json"),
            r#"{"model":"configured-model","env":{"ANTHROPIC_DEFAULT_OPUS_MODEL":"custom/model"}}"#,
        )
        .unwrap();
        assert_eq!(
            models_from_root(Agent::Claude, temp.path(), &Value::Null),
            vec!["Default", "configured-model", "custom/model"]
        );
        std::fs::write(temp.path().join("models_cache.json"), r#"{"models":[{"slug":"visible","visibility":"list"},{"slug":"hidden","visibility":"hide"}]}"#).unwrap();
        assert_eq!(
            models_from_root(Agent::Codex, temp.path(), &Value::Null),
            vec!["Default", "visible"]
        );
    }
    #[test]
    fn gateway_models_deduplicate_and_reject_wrong_provider_or_invalid_timestamps() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("cache")).unwrap();
        std::fs::write(temp.path().join("settings.json"), r#"{"model":"configured","modelSettings":{"not-a-model":{}},"env":{"ANTHROPIC_DEFAULT_OPUS_MODEL":"opus-route"}}"#).unwrap();
        let inherited = serde_json::json!({"ANTHROPIC_BASE_URL":"http://provider.invalid/", "ANTHROPIC_MODEL":"env-route"});
        let now = chrono::Utc::now().timestamp_millis();
        let catalog = |url: &str, fetched: i64| {
            serde_json::json!({
                "baseUrl":url, "fetchedAt":fetched,
                "models":[{"id":"configured"}, {"id":"gateway-route"}, {"id":"gateway-route"},
                          {"id":"--invalid"}, {"id":"bad\nmodel"}, {"display_name":"not-an-id"}]
            })
        };
        let cache_path = temp.path().join("cache/gateway-models.json");
        std::fs::write(
            &cache_path,
            catalog("http://provider.invalid", now).to_string(),
        )
        .unwrap();
        assert_eq!(
            models_from_root(Agent::Claude, temp.path(), &inherited),
            [
                "Default",
                "configured",
                "env-route",
                "gateway-route",
                "opus-route"
            ]
        );
        for (url, fetched) in [
            ("http://other.invalid", now),
            ("http://provider.invalid", now + 60_000),
            ("http://provider.invalid", i64::MIN),
        ] {
            std::fs::write(&cache_path, catalog(url, fetched).to_string()).unwrap();
            assert_eq!(
                models_from_root(Agent::Claude, temp.path(), &inherited),
                ["Default", "configured", "env-route", "opus-route"]
            );
        }
        std::fs::write(&cache_path, "not json").unwrap();
        assert_eq!(
            models_from_root(Agent::Claude, temp.path(), &inherited).len(),
            4
        );
        std::fs::write(
            &cache_path,
            catalog("http://provider.invalid", now).to_string(),
        )
        .unwrap();
        assert_eq!(
            models_from_root(Agent::Claude, temp.path(), &Value::Null),
            ["Default", "configured", "opus-route"]
        );
        // A CLI settings provider overrides the inherited provider for cache scope.
        std::fs::write(
            temp.path().join("settings.json"),
            r#"{"env":{"ANTHROPIC_BASE_URL":"http://other.invalid"}}"#,
        )
        .unwrap();
        assert_eq!(
            models_from_root(Agent::Claude, temp.path(), &inherited),
            ["Default", "env-route"]
        );
    }

    #[test]
    fn source_gate_keeps_old_gateway_rows_without_merging_cc_and_respects_settings() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir_all(root.join("cache/model-catalog")).unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        std::fs::write(
            root.join("cache/gateway-models.json"),
            serde_json::json!({
                "baseUrl":"https://gateway.invalid", "fetchedAt":0,
                "models":[{"id":"gateway-only"}]
            })
            .to_string(),
        )
        .unwrap();
        let cc = serde_json::json!({"version":2,"fetchedAt":now-1000,"staleAt":now-500,
        "catalog":{"surface":"cc","config":{
            "models":[{"id":"stock-only","thinking":{"effort_options":[{"id":"max"}]}}],
            "provider_alias_targets":{"sonnet":{"default":"stock-only","per_provider":{"gateway":"not-first-party"}}}
        }}});
        std::fs::write(
            root.join("cache/model-catalog/test-cc.json"),
            cc.to_string(),
        )
        .unwrap();
        let gateway = serde_json::json!({"ANTHROPIC_BASE_URL":"https://gateway.invalid/"});
        let (models, source) = models_for_context(Agent::Claude, root, root, &gateway, None);
        assert_eq!(models, ["Default", "gateway-only"]);
        assert!(source.contains("stale >24h"));
        let (models, source) = models_for_context(Agent::Claude, root, root, &Value::Null, None);
        assert_eq!(models, ["Default", "sonnet", "stock-only"]);
        assert!(source.contains("CLI cc cache (stale"));
        for key in [
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_USE_FOUNDRY",
        ] {
            let env = serde_json::json!({key:"1"});
            assert_eq!(
                models_for_context(Agent::Claude, root, root, &env, None).0,
                ["Default"]
            );
        }
        std::fs::create_dir(root.join(".claude")).unwrap();
        std::fs::write(root.join("settings.json"), r#"{"env":{"ANTHROPIC_BASE_URL":"https://gateway.invalid","ANTHROPIC_MODEL":"user-model"}}"#).unwrap();
        std::fs::write(
            root.join(".claude/settings.json"),
            r#"{"env":{"ANTHROPIC_BASE_URL":""}}"#,
        )
        .unwrap();
        std::fs::write(
            root.join(".claude/settings.local.json"),
            r#"{"env":{"ANTHROPIC_MODEL":"project-local-model"}}"#,
        )
        .unwrap();
        let (models, source) = models_for_context(Agent::Claude, root, root, &gateway, None);
        assert_eq!(
            models,
            ["Default", "project-local-model", "sonnet", "stock-only"]
        );
        assert!(source.contains("CLI cc cache"));
    }

    #[test]
    fn ocx_proxy_cache_supplies_claude_models_only_when_no_provider_is_set() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir_all(root.join("cache/model-catalog")).unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        std::fs::write(
            root.join("cache/gateway-models.json"),
            serde_json::json!({
                "baseUrl":"http://127.0.0.1:10100", "fetchedAt":now,
                "models":[{"id":"ocx-claude-native--gpt-6-luna"}, {"id":"ocx-claude-claude-cli--claude-opus-5-5"}]
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            root.join("cache/model-catalog/test-cc.json"),
            serde_json::json!({"version":2,"fetchedAt":now-1000,"staleAt":now+1000,
            "catalog":{"surface":"cc","config":{"models":[{"id":"stock-only"}]}}})
            .to_string(),
        )
        .unwrap();
        let ocx = Some("http://127.0.0.1:10100");

        // No provider configured: ocx rows replace the stock catalog.
        let (models, source) = models_for_context(Agent::Claude, root, root, &Value::Null, ocx);
        assert_eq!(
            models,
            [
                "Default",
                "ocx-claude-claude-cli--claude-opus-5-5",
                "ocx-claude-native--gpt-6-luna"
            ]
        );
        assert!(source.contains("via ocx"), "{source}");
        // Empty base URL counts as no provider.
        let empty = serde_json::json!({"ANTHROPIC_BASE_URL":""});
        assert_eq!(
            models_for_context(Agent::Claude, root, root, &empty, ocx)
                .0
                .len(),
            3
        );
        // Proxy URL mismatch, unknown proxy, or Codex keep the previous catalogs.
        for ocx in [Some("http://127.0.0.1:10101"), None] {
            let (models, source) = models_for_context(Agent::Claude, root, root, &Value::Null, ocx);
            assert_eq!(models, ["Default", "stock-only"]);
            assert!(source.contains("CLI cc cache"), "{source}");
        }
        // An explicit provider or cloud mode is never replaced by the ocx proxy.
        let other = serde_json::json!({"ANTHROPIC_BASE_URL":"http://other.invalid"});
        let (models, source) = models_for_context(Agent::Claude, root, root, &other, ocx);
        assert_eq!(models, ["Default"]);
        assert!(source.contains("provider mismatch"), "{source}");
        let stock = serde_json::json!({"ANTHROPIC_BASE_URL":"https://api.anthropic.com"});
        assert_eq!(
            models_for_context(Agent::Claude, root, root, &stock, ocx).0,
            ["Default", "stock-only"]
        );
        let bedrock = serde_json::json!({"CLAUDE_CODE_USE_BEDROCK":"1"});
        assert_eq!(
            models_for_context(Agent::Claude, root, root, &bedrock, ocx).0,
            ["Default"]
        );
        assert_eq!(
            models_for_context(Agent::Codex, root, root, &Value::Null, ocx).0,
            ["Default"]
        );
    }

    #[test]
    fn cc_parser_validates_schema_timestamps_and_only_reads_model_slots() {
        let now = chrono::Utc::now().timestamp_millis();
        let good = serde_json::json!({"version":2,"fetchedAt":now-100,"staleAt":now+100,
            "catalog":{"surface":"cc","config":{"models":[{"id":"model-a"},{"id":"--invalid"},{"id":"bad\nmodel"}],
                "settings_vocabulary":{"id":"not-model"},
                "provider_alias_targets":{"opus":{"default":"model-a"},"bad":{"default":{"id":"not-a-target"}}}}}});
        let (_, stale, models) = cc_models(&good, now).unwrap();
        assert!(!stale);
        assert_eq!(models.into_iter().collect::<Vec<_>>(), ["model-a", "opus"]);
        for (pointer, value) in [
            ("/version", serde_json::json!(1)),
            ("/fetchedAt", serde_json::json!(now + 1)),
            ("/fetchedAt", serde_json::json!(-1)),
            ("/staleAt", Value::Null),
            ("/staleAt", serde_json::json!(now - 101)),
            ("/catalog/surface", serde_json::json!("ccd")),
            (
                "/catalog/config/models",
                serde_json::json!({"id":"wrong-shape"}),
            ),
        ] {
            let mut invalid = good.clone();
            *invalid.pointer_mut(pointer).unwrap() = value;
            assert!(cc_models(&invalid, now).is_none(), "{pointer}");
        }
    }

    #[test]
    fn newest_valid_cc_snapshot_wins_and_malformed_files_are_ignored() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("cache/model-catalog");
        std::fs::create_dir_all(&dir).unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        for (name, fetched, id) in [
            ("old-cc.json", now - 100, "old"),
            ("new-cc.json", now - 10, "new"),
        ] {
            std::fs::write(
                dir.join(name),
                serde_json::json!({"version":2,"fetchedAt":fetched,"staleAt":now+100,
                "catalog":{"surface":"cc","config":{"models":[{"id":id}]}}})
                .to_string(),
            )
            .unwrap();
        }
        std::fs::write(dir.join("broken-cc.json"), "{broken").unwrap();
        assert_eq!(
            models_from_root(Agent::Claude, temp.path(), &Value::Null),
            ["Default", "new"]
        );
        std::fs::write(dir.join("new-cc.json"), "null").unwrap();
        assert_eq!(
            models_from_root(Agent::Claude, temp.path(), &Value::Null),
            ["Default", "old"]
        );
    }

    #[test]
    fn history_refresh_preserves_default_accepted_and_recent_path_edits() {
        let temp = tempfile::tempdir().unwrap();
        let mut workspace = Workspace {
            tab: MainTab::New,
            ..Default::default()
        };
        let default_path = workspace.new.path.clone();
        workspace.archive.push(ArchivedSession {
            agent: Agent::Claude,
            id: "recent".into(),
            title: String::new(),
            cwd: temp.path().into(),
            root: temp.path().into(),
            modified: 0,
        });
        workspace.update_projects(&[]);
        assert_eq!(workspace.new.path, default_path);
        workspace.new.field = 2;
        workspace.handle_key(KeyCode::Right, &[], true);
        assert_eq!(workspace.new.path, temp.path().to_string_lossy());
        workspace.handle_key(KeyCode::Enter, &[], true);
        assert!(workspace.new.editing);
        workspace.handle_key(KeyCode::Char('/'), &[], true);
        workspace.handle_key(KeyCode::Char('x'), &[], true);
        workspace.handle_key(KeyCode::Enter, &[], true);
        let edited = workspace.new.path.clone();
        workspace.update_projects(&[]);
        assert_eq!(workspace.new.path, edited);
        assert_eq!(workspace.new.project, workspace.new.projects.len());
        // The recent chooser index cannot override the accepted path at launch.
        workspace.new.project = 0;
        workspace.new.field = 3;
        workspace.handle_key(KeyCode::Enter, &[], false);
        assert!(workspace.message.contains("directory"));
    }

    #[test]
    fn live_duplicates_are_excluded_without_changing_snapshot_accounting() {
        let mut app = crate::app::App::new_with_config(
            crate::theme::Theme::default(),
            &[],
            crate::config::PanelVisibility::default(),
        );
        crate::demo::populate_demo(&mut app);
        let mut live = app
            .sessions
            .iter()
            .find(|s| s.agent_cli == "claude")
            .unwrap()
            .clone();
        live.pid = 123;
        live.status = SessionStatus::Waiting;
        let stable_snapshot = |app: &crate::app::App| {
            let mut value = serde_json::to_value(app.to_snapshot(2000)).unwrap();
            value.as_object_mut().unwrap().remove("generated_at_ms");
            for session in value["sessions"].as_array_mut().unwrap() {
                session.as_object_mut().unwrap().remove("elapsed_secs");
            }
            value
        };
        let before = stable_snapshot(&app);
        app.workspace.archive.push(ArchivedSession {
            agent: Agent::Claude,
            id: live.session_id.clone(),
            title: "Archived".into(),
            cwd: "/raw".into(),
            root: "/root".into(),
            modified: 1,
        });
        assert!(app.workspace.visible(&[live.clone()]).is_empty());
        // Filesystem-first Codex discovery can temporarily have no owner PID.
        // A session already on the dashboard still must not be offered to resume.
        live.pid = 0;
        live.status = SessionStatus::Unknown;
        assert!(app.workspace.visible(&[live.clone()]).is_empty());
        live.status = SessionStatus::Done;
        assert_eq!(app.workspace.visible(&[live]).len(), 1);
        assert_eq!(stable_snapshot(&app), before);
    }
    #[test]
    fn scan_completion_does_not_replace_an_active_custom_path_edit() {
        let temp = tempfile::tempdir().unwrap();
        let mut workspace = Workspace {
            tab: MainTab::New,
            ..Default::default()
        };
        let default_path = workspace.new.path.clone();
        workspace.new.field = 2;
        workspace.handle_key(KeyCode::Enter, &[], true);
        assert!(workspace.new.editing);
        let (tx, rx) = mpsc::channel();
        workspace.receiver = Some(rx);
        tx.send(ScanResult {
            sessions: vec![ArchivedSession {
                agent: Agent::Claude,
                id: "recent".into(),
                title: String::new(),
                cwd: temp.path().into(),
                root: temp.path().into(),
                modified: 1,
            }],
            limited: false,
        })
        .unwrap();
        workspace.poll(&[]);
        assert_eq!(workspace.new.path, default_path);
        assert_eq!(workspace.new.project, workspace.new.projects.len());
        workspace.handle_key(KeyCode::Char('/'), &[], true);
        assert_eq!(workspace.new.path, format!("{default_path}/"));
    }
    #[test]
    fn create_requires_button_focus_and_model_is_selection_only() {
        let mut workspace = Workspace {
            tab: MainTab::New,
            ..Default::default()
        };
        for code in [KeyCode::Enter, KeyCode::Char('c'), KeyCode::Char('z')] {
            workspace.handle_key(code, &[], false);
        }
        assert!(workspace.message.is_empty());
        workspace.new.field = 1;
        workspace.new.models = vec!["Default".into(), "local/model".into()];
        workspace.handle_key(KeyCode::Char('x'), &[], false);
        assert_eq!(workspace.new.model, 0);
        workspace.handle_key(KeyCode::Right, &[], false);
        assert_eq!(workspace.new.model, 1);
        workspace.handle_key(KeyCode::Left, &[], false);
        assert_eq!(workspace.new.model, 0);
        workspace.new.field = 3;
        workspace.new.path = "/nonexistent-abtop-test-directory".into();
        workspace.handle_key(KeyCode::Enter, &[], false);
        assert!(workspace.message.contains("directory"));
    }
    #[test]
    fn archive_filter_is_separate_from_live_data() {
        let mut workspace = Workspace::default();
        workspace.archive.push(ArchivedSession {
            agent: Agent::Claude,
            id: "one".into(),
            title: "Example".into(),
            cwd: "/raw".into(),
            root: "/root".into(),
            modified: 1,
        });
        assert_eq!(workspace.visible(&[]).len(), 1);
        workspace.agent_filter = 2;
        assert!(workspace.visible(&[]).is_empty());
        workspace.agent_filter = 0;
        workspace.query = "example".into();
        assert_eq!(workspace.visible(&[]).len(), 1);
        workspace.query = "missing".into();
        assert!(workspace.visible(&[]).is_empty());
    }
}
