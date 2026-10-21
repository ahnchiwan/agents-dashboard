//! Bounded, on-demand history discovery. Never participates in live accounting.
use crate::collector::{redact_secrets, sanitize_terminal_text};
use serde_json::Value;
use std::collections::{BTreeSet, HashSet};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

const SAMPLE_BYTES: usize = 64 * 1024;
const MAX_FILES: usize = 20_000;
const MAX_ENTRIES: usize = 100_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Agent {
    Claude,
    Codex,
}
impl Agent {
    pub fn cli(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Codex => "Codex",
        }
    }
    pub fn root_env(self) -> &'static str {
        match self {
            Self::Claude => "CLAUDE_CONFIG_DIR",
            Self::Codex => "CODEX_HOME",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ArchivedSession {
    pub agent: Agent,
    pub id: String,
    pub title: String,
    pub cwd: PathBuf,
    pub root: PathBuf,
    pub modified: u64,
}

#[derive(Default)]
pub struct ScanResult {
    pub sessions: Vec<ArchivedSession>,
    pub limited: bool,
}

pub fn default_root(agent: Agent) -> PathBuf {
    std::env::var_os(agent.root_env())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_default()
                .join(format!(".{}", agent.cli()))
        })
}

pub fn claude_roots(configured: &[PathBuf]) -> Vec<PathBuf> {
    let mut roots: BTreeSet<PathBuf> = configured.iter().cloned().collect();
    roots.insert(default_root(Agent::Claude));
    if let Some(home) = dirs::home_dir() {
        roots.insert(home.join(".claude"));
        if let Ok(entries) = fs::read_dir(home) {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().starts_with(".claude-")
                    && entry.path().join("projects").is_dir()
                {
                    roots.insert(entry.path());
                }
            }
        }
    }
    roots.into_iter().collect()
}

/// Read only complete JSONL records from bounded head/tail samples. A giant or
/// partially-written record cannot cause unbounded allocation or hide the tail.
pub(crate) fn sampled_records(path: &Path) -> Vec<Value> {
    let Ok(mut file) = File::open(path) else {
        return vec![];
    };
    let Ok(meta) = file.metadata() else {
        return vec![];
    };
    if !meta.is_file() {
        return vec![];
    }
    let mut head = Vec::new();
    if Read::by_ref(&mut file)
        .take(SAMPLE_BYTES as u64)
        .read_to_end(&mut head)
        .is_err()
    {
        return vec![];
    }
    let mut chunks = vec![head];
    if meta.len() > SAMPLE_BYTES as u64 {
        // For overlapping samples, start exactly after the head and discard the
        // first fragment; otherwise an oversized record might be parsed twice.
        let start = (meta.len().saturating_sub(SAMPLE_BYTES as u64)).max(SAMPLE_BYTES as u64);
        if file.seek(SeekFrom::Start(start)).is_ok() {
            let mut tail = Vec::new();
            let _ = file.take(SAMPLE_BYTES as u64).read_to_end(&mut tail);
            if let Some(pos) = tail.iter().position(|b| *b == b'\n') {
                chunks.push(tail[pos + 1..].to_vec());
            }
        }
    }
    chunks
        .into_iter()
        .flat_map(|bytes| {
            let mut records = Vec::new();
            for line in bytes.split_inclusive(|b| *b == b'\n') {
                if line.last() != Some(&b'\n') {
                    continue;
                }
                if let Ok(v) = serde_json::from_slice(line) {
                    records.push(v);
                }
            }
            records
        })
        .collect()
}

pub(crate) fn display(text: &str) -> String {
    redact_secrets(&sanitize_terminal_text(text))
        .chars()
        .take(200)
        .collect()
}

/// Extract text blocks before skipping leading transcript-only wrappers. Unlike
/// the live collector's short prompt summary, history needs the full first prompt
/// to distinguish helper sessions even when a custom title is already present.
fn claude_user_prompt(record: &Value) -> String {
    if record["isMeta"] == true {
        return String::new();
    }
    let raw = match &record["message"]["content"] {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| block["type"] == "text")
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return String::new(),
    };
    let mut prompt = raw.trim();
    loop {
        let Some(tag) = [
            "system-reminder",
            "local-command-caveat",
            "local-command-stdout",
            "local-command-stderr",
            "command-name",
            "command-message",
            "command-args",
            "bash-input",
            "bash-stdout",
            "bash-stderr",
        ]
        .into_iter()
        .find(|tag| prompt.starts_with(&format!("<{tag}>"))) else {
            return prompt.to_owned();
        };
        let closing = format!("</{tag}>");
        let Some(end) = prompt.find(&closing) else {
            return String::new();
        };
        prompt = prompt[end + closing.len()..].trim_start();
    }
}

fn parse(path: &Path, root: &Path, agent: Agent) -> Option<ArchivedSession> {
    let records = sampled_records(path);
    if records.is_empty() {
        return None;
    }
    let mut id = if agent == Agent::Claude {
        path.file_stem()?.to_str()?.to_owned()
    } else {
        String::new()
    };
    let mut cwd = PathBuf::new();
    let mut title = String::new();
    let mut first_user_seen = false;
    for record in &records {
        if record.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            return None;
        }
        if agent == Agent::Codex && record["type"] == "session_meta" {
            let p = &record["payload"];
            let source = &p["source"];
            if source.get("subagent").is_some()
                || source.as_str().is_some_and(|s| s.contains("subagent"))
                || p.get("parent_thread_id").is_some_and(|v| !v.is_null())
                || p["thread_source"] == "guardian_review"
            {
                return None;
            }
            id = p["id"].as_str().unwrap_or_default().to_owned();
            cwd = p["cwd"].as_str().unwrap_or_default().into();
            title = p["thread_name"].as_str().unwrap_or_default().to_owned();
        }
        if agent == Agent::Claude {
            if let Some(raw) = record["cwd"].as_str() {
                cwd = raw.into();
            }
            if let Some(name) = record["customTitle"]
                .as_str()
                .or(record["summary"].as_str())
            {
                title = name.to_owned();
            }
            if !first_user_seen && record["type"] == "user" {
                let prompt = claude_user_prompt(record);
                if !prompt.is_empty() {
                    if prompt
                        .trim_start_matches(|c: char| c == '-' || c.is_whitespace())
                        .starts_with("You are a conversation title generator.")
                    {
                        return None;
                    }
                    first_user_seen = true;
                    if title.is_empty() {
                        title = prompt;
                    }
                }
            }
        }
        if agent == Agent::Codex && title.is_empty() && record["payload"]["type"] == "user_message"
        {
            title = record["payload"]["message"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
        }
    }
    if id.is_empty()
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return None;
    }
    let modified = fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(ArchivedSession {
        agent,
        id,
        title: display(&title),
        cwd,
        root: root.into(),
        modified,
    })
}

/// Fixed-depth traversal avoids symlink recursion and never enters Claude's
/// nested subagents directories. Directory and transcript budgets bound work.
pub fn scan(claude: &[PathBuf], codex: &Path) -> ScanResult {
    let mut result = ScanResult::default();
    let mut files = Vec::new();
    let mut entries_left = MAX_ENTRIES;
    for (root, agent, depth) in claude
        .iter()
        .map(|p| (p.as_path(), Agent::Claude, 1))
        .chain(std::iter::once((codex, Agent::Codex, 3)))
    {
        let base = root.join(if agent == Agent::Claude {
            "projects"
        } else {
            "sessions"
        });
        let mut stack = vec![(base, depth)];
        while let Some((dir, remaining)) = stack.pop() {
            let Ok(entries) = fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                if entries_left == 0 {
                    result.limited = true;
                    break;
                }
                entries_left -= 1;
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                if kind.is_dir() && remaining > 0 {
                    stack.push((entry.path(), remaining - 1));
                } else if kind.is_file()
                    && remaining == 0
                    && entry.path().extension().is_some_and(|e| e == "jsonl")
                {
                    if agent == Agent::Codex
                        && !entry.file_name().to_string_lossy().starts_with("rollout-")
                    {
                        continue;
                    }
                    if agent == Agent::Claude
                        && entry.file_name().to_string_lossy().starts_with("agent-")
                    {
                        continue;
                    }
                    let modified = entry
                        .metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .unwrap_or(UNIX_EPOCH);
                    files.push((entry.path(), root.to_path_buf(), agent, modified));
                }
            }
            if result.limited {
                break;
            }
        }
        if result.limited {
            break;
        }
    }
    files.sort_by(|a, b| b.3.cmp(&a.3).then_with(|| a.0.cmp(&b.0)));
    if files.len() > MAX_FILES {
        result.limited = true;
        files.truncate(MAX_FILES);
    }
    let titles: std::collections::HashMap<String, String> =
        sampled_records(&codex.join("session_index.jsonl"))
            .into_iter()
            .filter_map(|v| {
                Some((
                    v["id"].as_str()?.to_owned(),
                    display(v["thread_name"].as_str()?),
                ))
            })
            .collect();
    let mut seen = HashSet::new();
    for (path, root, agent, _) in files {
        if let Some(mut session) = parse(&path, &root, agent) {
            if agent == Agent::Codex {
                if let Some(title) = titles.get(&session.id) {
                    session.title = title.clone();
                }
            }
            if seen.insert((agent, root, session.id.clone())) {
                result.sessions.push(session);
            }
        }
    }
    result
        .sessions
        .sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.id.cmp(&b.id)));
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn history_is_bounded_tolerant_and_excludes_children() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("projects/encoded");
        fs::create_dir_all(project.join("main/subagents")).unwrap();
        fs::write(project.join("main.jsonl"), b"broken\n{\"type\":\"user\",\"cwd\":\"/raw path\",\"message\":{\"content\":\"hello\"}}\n{\"partial\":").unwrap();
        fs::write(project.join("side.jsonl"), b"{\"isSidechain\":true}\n").unwrap();
        fs::write(project.join("main/subagents/child.jsonl"), b"{}\n").unwrap();
        let result = scan(&[temp.path().into()], &temp.path().join("missing"));
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].cwd, Path::new("/raw path"));
        assert_eq!(result.sessions[0].root, temp.path());
        assert_eq!(result.sessions[0].title, "hello");
        let huge = project.join("huge.jsonl");
        fs::write(&huge, vec![b'x'; SAMPLE_BYTES * 5]).unwrap();
        assert!(sampled_records(&huge).is_empty());
    }
    #[test]
    fn huge_record_does_not_hide_later_complete_records() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bounded.jsonl");
        let mut bytes = b"{\"type\":\"user\",\"cwd\":\"/original\"}\n".to_vec();
        bytes.extend(vec![b'x'; SAMPLE_BYTES * 5]);
        bytes
            .extend(b"\n{\"type\":\"custom-title\",\"customTitle\":\"Tail title\"}\n{\"partial\":");
        fs::write(&path, bytes).unwrap();
        let parsed = parse(&path, temp.path(), Agent::Claude).unwrap();
        assert_eq!(parsed.cwd, Path::new("/original"));
        assert_eq!(parsed.title, "Tail title");
        assert_eq!(sampled_records(&path).len(), 2);
    }
    fn parse_claude_records(records: &[Value]) -> Option<ArchivedSession> {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("main.jsonl");
        let jsonl = records
            .iter()
            .map(|record| format!("{record}\n"))
            .collect::<String>();
        fs::write(&path, jsonl).unwrap();
        parse(&path, temp.path(), Agent::Claude)
    }

    fn user(content: Value) -> Value {
        serde_json::json!({"type": "user", "cwd": "/project", "message": {"content": content}})
    }

    #[test]
    fn title_helpers_are_excluded_after_wrappers_in_both_content_shapes() {
        let helper = "You are a conversation title generator. Generate a title.";
        let caveat = "<local-command-caveat>The command below was run directly in Claude Code,</local-command-caveat>";
        for content in [
            serde_json::json!(helper),
            serde_json::json!(format!("-\n{helper}")),
            serde_json::json!(format!("  - \n\t{helper}")),
            serde_json::json!(format!("{caveat}\n-\n{helper}")),
            serde_json::json!([
                {"type": "text", "text": "-"},
                {"type": "text", "text": helper}
            ]),
            serde_json::json!(format!("{caveat}\n{helper}")),
            serde_json::json!([
                {"type": "text", "text": "<system-reminder>Context</system-reminder>"},
                {"type": "text", "text": caveat},
                {"type": "text", "text": helper}
            ]),
        ] {
            assert!(parse_claude_records(&[
                serde_json::json!({"type": "custom-title", "customTitle": "Helper title"}),
                user(serde_json::json!(caveat)),
                user(content),
            ])
            .is_none());
        }
    }

    #[test]
    fn local_commands_and_system_wrappers_do_not_hide_real_sessions() {
        let caveat = "<local-command-caveat>The command below was run directly in Claude Code,</local-command-caveat>";
        for content in [
            serde_json::json!("Implement archive filtering"),
            serde_json::json!(format!(
                "{caveat}<system-reminder>Context</system-reminder>\nImplement archive filtering"
            )),
            serde_json::json!([
                {"type": "tool_result", "content": "Ignore tool output"},
                {"type": "text", "text": "<system-reminder>Context</system-reminder>"},
                {"type": "text", "text": caveat},
                {"type": "text", "text": "Implement archive filtering"}
            ]),
            serde_json::json!([
                {"type": "text", "text": "<system-reminder>Context"},
                {"type": "text", "text": "</system-reminder>Implement archive filtering"}
            ]),
        ] {
            let parsed = parse_claude_records(&[
                user(serde_json::json!(caveat)),
                user(serde_json::json!("<command-name>/clear</command-name><command-message>clear</command-message><command-args></command-args>")),
                user(serde_json::json!([{"type": "text", "text": "<local-command-stdout>Output</local-command-stdout>"}])),
                user(serde_json::json!([{"type": "tool_result", "content": "Output"}])),
                serde_json::json!({"type": "user", "isMeta": true, "message": {"content": "Metadata"}}),
                user(content),
                user(serde_json::json!("You are a conversation title generator. Quoted in a later prompt.")),
            ])
            .unwrap();
            assert_eq!(parsed.title, "Implement archive filtering");
            assert_eq!(parsed.cwd, Path::new("/project"));
        }
    }

    #[test]
    fn custom_titles_still_override_prompts_and_helper_mentions_are_not_prefixes() {
        let prompt = "Investigate You are a conversation title generator. sessions";
        let prefixed_request = "- Investigate You are a conversation title generator. sessions";
        assert_eq!(
            parse_claude_records(&[user(serde_json::json!(prefixed_request))])
                .unwrap()
                .title,
            prefixed_request
        );
        for before in [true, false] {
            let custom = serde_json::json!({"type": "custom-title", "customTitle": "Chosen title"});
            let prompt = user(serde_json::json!(prompt));
            let records = if before {
                vec![custom, prompt]
            } else {
                vec![prompt, custom]
            };
            assert_eq!(
                parse_claude_records(&records).unwrap().title,
                "Chosen title"
            );
        }
        assert_eq!(
            parse_claude_records(&[user(serde_json::json!(prompt))])
                .unwrap()
                .title,
            prompt
        );
    }

    #[test]
    fn incomplete_wrappers_are_skipped_and_prompt_display_stays_safe_and_bounded() {
        let parsed = parse_claude_records(&[
            user(serde_json::json!("<system-reminder>Incomplete context")),
            user(serde_json::json!(format!(
                "Fix\u{1b} sk_test_secret {}",
                "x".repeat(400)
            ))),
        ])
        .unwrap();
        assert_eq!(parsed.title.chars().count(), 200);
        assert!(parsed.title.starts_with("Fix [REDACTED] "));
        assert!(!parsed.title.contains('\u{1b}'));
    }

    #[test]
    fn codex_metadata_and_subagent_source() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("sessions/2026/01/01");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("rollout-main.jsonl"), "{\"type\":\"session_meta\",\"payload\":{\"id\":\"main\",\"cwd\":\"/deleted\",\"source\":\"cli\"}}\n").unwrap();
        fs::write(dir.join("rollout-child.jsonl"), "{\"type\":\"session_meta\",\"payload\":{\"id\":\"child\",\"source\":{\"subagent\":{}}}}\n").unwrap();
        fs::write(dir.join("rollout-guardian.jsonl"), "{\"type\":\"session_meta\",\"payload\":{\"id\":\"guardian\",\"thread_source\":\"guardian_review\"}}\n").unwrap();
        fs::write(temp.path().join("session_index.jsonl"), "{\"id\":\"main\",\"thread_name\":\"Old title\"}\n{\"id\":\"main\",\"thread_name\":\"Current title\"}\n").unwrap();
        let result = scan(&[], temp.path());
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].title, "Current title");
        assert_eq!(result.sessions[0].id, "main");
        assert_eq!(result.sessions[0].cwd, Path::new("/deleted"));
    }
}
