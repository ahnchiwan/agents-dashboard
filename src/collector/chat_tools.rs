//! Display-only chat lines for tool calls and tool results.
//!
//! The chat view shows what the agent ran, not what it read. Every string
//! produced here is terminal-sanitized and secret-redacted, and the rules are
//! deliberately allow-list based:
//!
//! - A call line is the tool name plus at most one argument taken from a fixed
//!   set of keys (path, command, pattern, url, query, ...). Keys that carry
//!   file bodies (`content`, `old_string`, `new_string`, patches, prompts) are
//!   never read, so they cannot reach the screen.
//! - Command output (`ResultKind::Output`) is shown as a bounded head only.
//!   The output of every other tool (file reads, searches, web fetches, agent
//!   replies, unknown tools, and all unmatched results) is reduced to a line and
//!   byte count (`ResultKind::Summary`).

use crate::collector::{redact_secrets, sanitize_terminal_text};
use serde_json::Value;

/// Longest call line, tool name included.
pub(crate) const MAX_CALL_CHARS: usize = 160;
/// Most output lines shown for one command result.
pub(crate) const MAX_RESULT_LINES: usize = 8;
/// Most output bytes shown for one command result.
pub(crate) const MAX_RESULT_BYTES: usize = 1_200;
/// Longest single output line shown.
pub(crate) const MAX_RESULT_LINE_CHARS: usize = 200;
/// Only this much of a result is scanned, so a huge output stays cheap.
const MAX_RESULT_SCAN_BYTES: usize = 16 * 1024;
const MAX_TOOL_NAME_CHARS: usize = 40;
/// Tool calls remembered so a later result can find its tool.
const MAX_TRACKED_TOOLS: usize = 128;
const MAX_TOOL_ID_BYTES: usize = 128;

/// How much of a tool result may be shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResultKind {
    /// Command output: a bounded head of the text.
    Output,
    /// Anything that may carry file or document content: counts only.
    Summary,
}

/// Claude tools whose output is command output.
pub(crate) fn claude_result_kind(tool: &str) -> ResultKind {
    match tool {
        "Bash" | "BashOutput" => ResultKind::Output,
        _ => ResultKind::Summary,
    }
}

/// Codex tools whose output is command output.
pub(crate) fn codex_result_kind(tool: &str) -> ResultKind {
    match tool {
        "exec_command" | "shell" | "shell_command" | "local_shell" | "container.exec"
        | "write_stdin" => ResultKind::Output,
        _ => ResultKind::Summary,
    }
}

/// Tool call id to result kind, newest last. Carried between incremental
/// parses because a result can arrive in a later read than its call.
pub(crate) type ChatTools = Vec<(String, ResultKind)>;

pub(crate) fn remember_tool(tools: &mut ChatTools, id: &str, kind: ResultKind) {
    if id.is_empty() || id.len() > MAX_TOOL_ID_BYTES {
        return;
    }
    tools.push((id.to_string(), kind));
    let len = tools.len();
    if len > MAX_TRACKED_TOOLS {
        tools.drain(..len - MAX_TRACKED_TOOLS);
    }
}

/// Kind for a result id. A result whose call is unknown is a summary.
pub(crate) fn tool_kind(tools: &ChatTools, id: &str) -> ResultKind {
    tools
        .iter()
        .rfind(|(known, _)| known == id)
        .map_or(ResultKind::Summary, |(_, kind)| *kind)
}

fn truncate_head(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

fn truncate_tail(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        s.to_string()
    } else {
        let tail: String = s.chars().skip(count - max.saturating_sub(1)).collect();
        format!("…{tail}")
    }
}

fn clean(s: &str) -> String {
    redact_secrets(&sanitize_terminal_text(s))
}

/// The string, or the shell script of a `["bash", "-lc", script]` argv.
fn string_or_argv(value: &Value) -> Option<String> {
    if let Some(s) = value.as_str() {
        return Some(s.to_string());
    }
    let parts: Vec<&str> = value.as_array()?.iter().filter_map(Value::as_str).collect();
    match parts.as_slice() {
        [] => None,
        [_, flag, script, ..] if flag.starts_with('-') => Some((*script).to_string()),
        _ => Some(parts.join(" ")),
    }
}

/// The single most useful argument of a tool input, from an allow-list of
/// keys. Returns an empty string when none is present.
fn key_arg(input: &Value, budget: usize) -> String {
    for key in ["file_path", "notebook_path"] {
        if let Some(path) = input.get(key).and_then(Value::as_str) {
            return truncate_tail(&clean(path), budget);
        }
    }
    for key in ["command", "cmd"] {
        if let Some(command) = input.get(key).and_then(string_or_argv) {
            let first = command.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
            return truncate_head(&clean(first.trim()), budget);
        }
    }
    if let Some(pattern) = input.get("pattern").and_then(Value::as_str) {
        return truncate_head(&clean(pattern.trim()), budget);
    }
    if let Some(path) = input.get("path").and_then(Value::as_str) {
        return truncate_tail(&clean(path), budget);
    }
    for key in ["query", "description", "skill"] {
        if let Some(text) = input.get(key).and_then(Value::as_str) {
            return truncate_head(&clean(text.trim()), budget);
        }
    }
    if let Some(url) = input.get("url").and_then(Value::as_str) {
        // Query strings and fragments often carry tokens.
        let base = url.split(['?', '#']).next().unwrap_or("");
        return truncate_head(&clean(base.trim()), budget);
    }
    String::new()
}

/// One-line call text: `<tool name> <key argument>`.
pub(crate) fn call_text(name: &str, input: &Value) -> String {
    let name = truncate_head(clean(name).trim(), MAX_TOOL_NAME_CHARS);
    let name = if name.is_empty() { "tool".into() } else { name };
    let budget = MAX_CALL_CHARS
        .saturating_sub(name.chars().count() + 1)
        .max(8);
    let arg = key_arg(input, budget);
    if arg.is_empty() {
        name
    } else {
        format!("{name} {arg}")
    }
}

/// Call text from a name and an already-sanitized argument, for rollout
/// schemas that only expose a pre-reduced path.
pub(crate) fn call_text_with_arg(name: &str, arg: &str) -> String {
    let name = truncate_head(clean(name).trim(), MAX_TOOL_NAME_CHARS);
    let budget = MAX_CALL_CHARS
        .saturating_sub(name.chars().count() + 1)
        .max(8);
    let arg = truncate_tail(&clean(arg), budget);
    if arg.is_empty() {
        name
    } else {
        format!("{name} {arg}")
    }
}

fn plural(n: usize, unit: &str) -> String {
    if n == 1 {
        format!("1 {unit}")
    } else {
        format!("{n} {unit}s")
    }
}

fn human_bytes(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// The result text of one tool: a bounded head for command output, a size
/// summary for everything else. `output` is the raw result text.
pub(crate) fn result_text(output: &str, kind: ResultKind, is_error: bool) -> String {
    let prefix = if is_error { "error · " } else { "" };
    let body = output.trim_start_matches(['\n', '\r']).trim_end();
    if body.is_empty() {
        return format!("{prefix}no output");
    }
    let total_lines = body.lines().count();
    if kind == ResultKind::Summary {
        return format!(
            "{prefix}{}, {} · content hidden",
            plural(total_lines, "line"),
            human_bytes(output.len())
        );
    }

    let mut end = body.len().min(MAX_RESULT_SCAN_BYTES);
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    let mut shown: Vec<String> = Vec::new();
    let mut bytes = 0;
    for line in body[..end].lines() {
        if shown.len() == MAX_RESULT_LINES {
            break;
        }
        let line = truncate_head(clean(line).trim_end(), MAX_RESULT_LINE_CHARS);
        if bytes + line.len() > MAX_RESULT_BYTES && !shown.is_empty() {
            break;
        }
        bytes += line.len() + 1;
        shown.push(line);
    }
    let hidden = total_lines.saturating_sub(shown.len());
    if hidden > 0 {
        shown.push(format!(
            "… {hidden} more {}",
            if hidden == 1 { "line" } else { "lines" }
        ));
    }
    shown.join("\n")
}

/// Split a Codex shell result into its exit code and the command output.
///
/// Current rollouts print `Process exited with code N` metadata above an
/// `Output:` line; older ones wrap the text as `{"output": ..., "metadata":
/// {"exit_code": N}}`. Anything else is returned unchanged with no exit code.
pub(crate) fn split_codex_output(raw: &str) -> (Option<i64>, String) {
    if raw.trim_start().starts_with('{') {
        if let Ok(value) = serde_json::from_str::<Value>(raw) {
            if let Some(output) = value.get("output").and_then(Value::as_str) {
                let code = value.pointer("/metadata/exit_code").and_then(Value::as_i64);
                return (code, output.to_string());
            }
        }
    }
    let marker = raw
        .match_indices("Output:\n")
        .find(|(i, _)| *i == 0 || raw.as_bytes()[i - 1] == b'\n');
    let Some((at, _)) = marker else {
        return (None, raw.to_string());
    };
    let code = raw[..at].lines().find_map(|line| {
        line.trim()
            .strip_prefix("Process exited with code ")
            .and_then(|rest| rest.trim().parse::<i64>().ok())
    });
    (code, raw[at + "Output:\n".len()..].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn call_text_uses_only_allow_listed_arguments() {
        assert_eq!(
            call_text("Read", &json!({"file_path": "/work/src/main.rs"})),
            "Read /work/src/main.rs"
        );
        assert_eq!(
            call_text("Bash", &json!({"command": "cargo test\nexit 1"})),
            "Bash cargo test"
        );
        assert_eq!(
            call_text("Grep", &json!({"pattern": "fn main", "path": "src"})),
            "Grep fn main"
        );
        assert_eq!(
            call_text("LS", &json!({"path": "/work/src"})),
            "LS /work/src"
        );
        assert_eq!(
            call_text(
                "WebFetch",
                &json!({"url": "https://a.test/p?token=abc#frag"})
            ),
            "WebFetch https://a.test/p"
        );
        // File bodies and unknown keys never appear, even without any key arg.
        let write = json!({"content": "SECRET BODY", "old_string": "OLD", "new_string": "NEW"});
        assert_eq!(call_text("Write", &write), "Write");
        let write = json!({"file_path": "/a/b.rs", "content": "SECRET BODY"});
        assert_eq!(call_text("Write", &write), "Write /a/b.rs");
        assert_eq!(
            call_text("mcp__x__tool", &json!({"text": "SECRET"})),
            "mcp__x__tool"
        );
    }

    #[test]
    fn call_text_sanitizes_redacts_and_bounds_the_line() {
        let text = call_text(
            "Bash",
            &json!({"command": "curl -H 'Bearer abc' \u{1b}[31mx"}),
        );
        assert!(text.contains("[REDACTED]"), "{text}");
        assert!(!text.contains('\u{1b}'));
        let long = call_text("Bash", &json!({"command": "x".repeat(500)}));
        assert!(long.chars().count() <= MAX_CALL_CHARS);
        assert!(long.ends_with('…'));
        let path = call_text(
            "Read",
            &json!({"file_path": format!("/{}/tail.rs", "d".repeat(400))}),
        );
        assert!(path.chars().count() <= MAX_CALL_CHARS);
        assert!(path.ends_with("tail.rs"));
        assert!(call_text("\u{202e}\n", &json!({})).starts_with("tool"));
    }

    #[test]
    fn call_text_reads_codex_argv_commands() {
        let input = json!({"command": ["bash", "-lc", "ls -la\nrm x"]});
        assert_eq!(call_text("shell", &input), "shell ls -la");
        let input = json!({"command": ["git", "status"]});
        assert_eq!(call_text("shell", &input), "shell git status");
    }

    #[test]
    fn summary_results_report_counts_and_hide_content() {
        let text = result_text("alpha\nbeta\ngamma\n", ResultKind::Summary, false);
        assert_eq!(text, "3 lines, 17 B · content hidden");
        assert!(!text.contains("alpha"));
        let big = "x\n".repeat(2048);
        assert_eq!(
            result_text(&big, ResultKind::Summary, false),
            "2048 lines, 4.0 KB · content hidden"
        );
        let error = result_text("String to replace: SECRET", ResultKind::Summary, true);
        assert_eq!(error, "error · 1 line, 25 B · content hidden");
        assert_eq!(result_text("  \n", ResultKind::Summary, false), "no output");
        assert_eq!(
            result_text("", ResultKind::Output, true),
            "error · no output"
        );
    }

    #[test]
    fn command_output_is_bounded_with_a_more_lines_marker() {
        let output = (1..=20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let text = result_text(&output, ResultKind::Output, false);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), MAX_RESULT_LINES + 1);
        assert_eq!(lines[0], "line 1");
        assert_eq!(lines[MAX_RESULT_LINES - 1], "line 8");
        assert_eq!(lines[MAX_RESULT_LINES], "… 12 more lines");
        assert_eq!(
            result_text("a\nb\nc\nd\ne\nf\ng\nh\ni", ResultKind::Output, false)
                .lines()
                .last(),
            Some("… 1 more line")
        );
        // A short output is returned untouched.
        assert_eq!(result_text("ok\n", ResultKind::Output, false), "ok");
    }

    #[test]
    fn command_output_respects_the_byte_and_line_width_caps() {
        let wide = (0..8)
            .map(|_| "y".repeat(190))
            .collect::<Vec<_>>()
            .join("\n");
        let text = result_text(&wide, ResultKind::Output, false);
        let shown = text.lines().filter(|l| l.starts_with('y')).count();
        assert!(shown < 8, "byte cap should stop before 8 wide lines");
        assert!(text.len() <= MAX_RESULT_BYTES + 32);
        assert!(text.lines().last().unwrap().starts_with("… "));

        let one = result_text(&"z".repeat(5000), ResultKind::Output, false);
        assert_eq!(one.chars().count(), MAX_RESULT_LINE_CHARS);
        assert!(one.ends_with('…'));

        // Huge outputs are only scanned in part but still counted by line.
        let huge = "row\n".repeat(100_000);
        let text = result_text(&huge, ResultKind::Output, false);
        assert!(text.ends_with("… 99992 more lines"), "{text}");
    }

    #[test]
    fn command_output_is_sanitized_and_redacted() {
        let text = result_text(
            "token ghp_abcdef123\n\u{1b}[31mred\u{1b}[0m \u{202e}rtl\nAuthorization: Bearer sk-ant-xyz",
            ResultKind::Output,
            false,
        );
        assert!(!text.contains("ghp_abcdef123"));
        assert!(!text.contains("sk-ant-xyz"));
        assert!(!text.contains('\u{1b}'));
        assert!(!text.contains('\u{202e}'));
        assert!(text.contains("[REDACTED]"));
        assert!(text.contains("[31mred"));
    }

    #[test]
    fn codex_output_is_split_into_exit_code_and_body() {
        let raw = "Chunk ID: abc\nWall time: 0.1 seconds\nProcess exited with code 2\nOriginal token count: 4\nOutput:\nboom\nbang\n";
        assert_eq!(
            split_codex_output(raw),
            (Some(2), "boom\nbang\n".to_string())
        );
        let ok = "Process exited with code 0\nOutput:\nfine";
        assert_eq!(split_codex_output(ok), (Some(0), "fine".to_string()));
        let legacy = r#"{"output":"hello\n","metadata":{"exit_code":1,"duration_seconds":0.1}}"#;
        assert_eq!(split_codex_output(legacy), (Some(1), "hello\n".to_string()));
        assert_eq!(split_codex_output("plain"), (None, "plain".to_string()));
        // A mid-line "Output:" is not a marker.
        assert_eq!(
            split_codex_output("see Output:\nnot a marker"),
            (None, "see Output:\nnot a marker".to_string())
        );
    }

    #[test]
    fn tool_tracking_is_bounded_and_defaults_to_summary() {
        let mut tools = ChatTools::new();
        remember_tool(&mut tools, "a", ResultKind::Output);
        remember_tool(&mut tools, "", ResultKind::Output);
        remember_tool(&mut tools, &"x".repeat(200), ResultKind::Output);
        assert_eq!(tools.len(), 1);
        assert_eq!(tool_kind(&tools, "a"), ResultKind::Output);
        assert_eq!(tool_kind(&tools, "missing"), ResultKind::Summary);
        for i in 0..300 {
            remember_tool(&mut tools, &format!("id{i}"), ResultKind::Output);
        }
        assert_eq!(tools.len(), MAX_TRACKED_TOOLS);
        assert_eq!(tool_kind(&tools, "a"), ResultKind::Summary);
        assert_eq!(tool_kind(&tools, "id299"), ResultKind::Output);
    }

    #[test]
    fn result_kinds_follow_the_tool_name() {
        assert_eq!(claude_result_kind("Bash"), ResultKind::Output);
        for tool in [
            "Read",
            "Grep",
            "Glob",
            "WebFetch",
            "Edit",
            "Task",
            "mcp__x__y",
        ] {
            assert_eq!(claude_result_kind(tool), ResultKind::Summary, "{tool}");
        }
        assert_eq!(codex_result_kind("exec_command"), ResultKind::Output);
        assert_eq!(codex_result_kind("apply_patch"), ResultKind::Summary);
        assert_eq!(codex_result_kind("view_image"), ResultKind::Summary);
    }
}
