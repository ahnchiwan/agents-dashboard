//! Launch interactive CLIs in a new window of the dashboard's tmux session.
use crate::archive::{Agent, ArchivedSession};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone)]
pub struct LaunchRequest {
    pub agent: Agent,
    pub cwd: PathBuf,
    pub root: PathBuf,
    pub resume: Option<String>,
    pub model: Option<String>,
}
impl From<&ArchivedSession> for LaunchRequest {
    fn from(s: &ArchivedSession) -> Self {
        Self {
            agent: s.agent,
            cwd: s.cwd.clone(),
            root: s.root.clone(),
            resume: Some(s.id.clone()),
            model: None,
        }
    }
}

fn executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn find_executable(name: &str) -> Option<PathBuf> {
    // Resolve an executable, never a shell function or alias.
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|dir| dir.join(name))
        .find(|path| executable(path))
        .and_then(|path| {
            if path.is_absolute() {
                Some(path)
            } else {
                std::env::current_dir().ok().map(|cwd| cwd.join(path))
            }
        })
}

impl LaunchRequest {
    fn validate(&self) -> Result<(), String> {
        if self.cwd.as_os_str().is_empty() || !self.cwd.is_absolute() || !self.cwd.is_dir() {
            return Err(
                "Working directory is missing or deleted; select an existing absolute path.".into(),
            );
        }
        if self.root.as_os_str().is_empty() || !self.root.is_absolute() || !self.root.is_dir() {
            return Err(
                "Original CLI config root is missing; refusing to use another profile.".into(),
            );
        }
        if self.resume.as_ref().is_some_and(|id| {
            id.is_empty()
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        }) {
            return Err("Invalid session ID.".into());
        }
        Ok(())
    }

    fn cli_args(&self, binary: &Path) -> Vec<OsString> {
        // tmux executes multi-argument commands directly, without sh -c. env
        // carries the exact original root even when tmux's server env differs.
        let mut root = OsString::from(format!("{}=", self.agent.root_env()));
        root.push(self.root.as_os_str());
        let mut args = vec![
            OsString::from("env"),
            OsString::from("-u"),
            OsString::from("CLAUDECODE"),
        ];
        if self.agent == Agent::Claude {
            let context =
                crate::workspace::CLAUDE_CONTEXT_ENV_KEYS.map(|key| (key, std::env::var_os(key)));
            append_claude_context(&mut args, &context);
        }
        args.push(root);
        args.push(binary.as_os_str().into());
        if self.agent == Agent::Claude && binary.file_name() == Some(OsStr::new("ocx")) {
            // `ocx claude` wires the OpenCodex proxy routing.
            args.push("claude".into());
        }
        if let Some(id) = &self.resume {
            args.push(
                if self.agent == Agent::Claude {
                    "--resume"
                } else {
                    "resume"
                }
                .into(),
            );
            args.push(id.into());
        }
        if self.agent == Agent::Codex {
            args.push("-C".into());
            args.push(self.cwd.as_os_str().into());
        }
        if let Some(model) = &self.model {
            args.push(
                if self.agent == Agent::Claude {
                    "--model"
                } else {
                    "-m"
                }
                .into(),
            );
            args.push(model.into());
        }
        args
    }
}

fn append_claude_context(args: &mut Vec<OsString>, context: &[(&str, Option<OsString>)]) {
    // Unset even absent dashboard keys: a long-lived tmux server may retain them.
    for (key, _) in context {
        args.extend([OsString::from("-u"), OsString::from(key)]);
    }
    for (key, value) in context {
        if let Some(value) = value {
            let mut assignment = OsString::from(format!("{key}="));
            assignment.push(value);
            args.push(assignment);
        }
    }
}

/// tmux's own argv parser treats a trailing semicolon as a command separator,
/// even with direct process arguments. Escape that transport layer too.
fn escape_tmux_arg(value: &OsStr, formats: bool) -> OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let bytes = value.as_bytes();
        let mut escaped = Vec::with_capacity(bytes.len());
        for (index, byte) in bytes.iter().enumerate() {
            if formats && *byte == b'#' {
                escaped.push(b'#');
            }
            if !formats && index + 1 == bytes.len() && *byte == b';' {
                escaped.push(b'\\');
            }
            escaped.push(*byte);
        }
        OsString::from_vec(escaped)
    }
    #[cfg(not(unix))]
    {
        let text = value.to_string_lossy();
        let mut escaped = if formats {
            text.replace('#', "##")
        } else {
            text.into_owned()
        };
        if !formats && escaped.ends_with(';') {
            escaped.insert(escaped.len() - 1, '\\');
        }
        escaped.into()
    }
}

fn run(program: &OsStr, args: &[OsString]) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(crate::archive::display(&String::from_utf8_lossy(
            &output.stderr,
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

pub fn launch(request: &LaunchRequest) -> Result<(), String> {
    request.validate()?;
    if std::env::var_os("TMUX").is_none() {
        return Err("Session launching requires running Agents Dashboard inside tmux.".into());
    }
    let pane = std::env::var_os("TMUX_PANE").ok_or("Cannot identify the dashboard tmux pane.")?;
    let tmux = find_executable("tmux").ok_or("tmux executable was not found in PATH.")?;
    let binary = resolve_binary(request.agent, find_executable)?;
    launch_with(request, &binary, |args| run(tmux.as_os_str(), args), &pane)
}

/// Claude prefers `ocx` (OpenCodex proxy) when it is on PATH and otherwise
/// falls back to plain `claude`; Codex always runs `codex`.
fn resolve_binary(agent: Agent, find: impl Fn(&str) -> Option<PathBuf>) -> Result<PathBuf, String> {
    match agent {
        Agent::Claude => find("ocx")
            .or_else(|| find("claude"))
            .ok_or_else(|| "Neither ocx nor claude executable was found in PATH.".into()),
        Agent::Codex => find(agent.cli())
            .ok_or_else(|| format!("{} executable was not found in PATH.", agent.label())),
    }
}

fn launch_with(
    request: &LaunchRequest,
    binary: &Path,
    mut command: impl FnMut(&[OsString]) -> Result<String, String>,
    pane: &OsStr,
) -> Result<(), String> {
    let mut tmux = |args: &[OsString]| {
        let escaped: Vec<_> = args.iter().map(|arg| escape_tmux_arg(arg, false)).collect();
        command(&escaped)
    };
    let session = tmux(&[
        "display-message".into(),
        "-p".into(),
        "-t".into(),
        pane.into(),
        "#{session_id}".into(),
    ])?;
    if !session.starts_with('$')
        || !session[1..].bytes().all(|b| b.is_ascii_digit())
        || session.len() < 2
    {
        return Err("Cannot identify the current tmux session.".into());
    }
    let mut args: Vec<OsString> = vec![
        "new-window".into(),
        "-d".into(),
        "-P".into(),
        "-F".into(),
        "#{window_id}".into(),
        "-t".into(),
        format!("{session}:").into(),
        "-n".into(),
        request.agent.label().into(),
        "-c".into(),
        escape_tmux_arg(request.cwd.as_os_str(), true),
    ];
    args.extend(request.cli_args(binary));
    let window = tmux(&args)?;
    if !window.starts_with('@')
        || !window[1..].bytes().all(|b| b.is_ascii_digit())
        || window.len() < 2
    {
        return Err("tmux created a window but returned an invalid window ID.".into());
    }
    for option in ["automatic-rename", "allow-rename"] {
        tmux(&[
            "set-option".into(),
            "-w".into(),
            "-t".into(),
            window.clone().into(),
            option.into(),
            "off".into(),
        ])
        .map_err(|e| format!("Window {window} created, but name protection failed: {e}"))?;
    }
    tmux(&[
        "rename-window".into(),
        "-t".into(),
        window.clone().into(),
        request.agent.label().into(),
    ])?;
    tmux(&["select-window".into(), "-t".into(), window.into()])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn routing_environment_is_literal_and_absent_values_clear_tmux_server_state() {
        let mut args = vec!["env".into()];
        append_claude_context(
            &mut args,
            &[
                (
                    "ANTHROPIC_BASE_URL",
                    Some("https://provider.invalid/;$(literal)".into()),
                ),
                ("CLAUDE_CODE_USE_VERTEX", None),
            ],
        );
        assert_eq!(
            args,
            [
                "env",
                "-u",
                "ANTHROPIC_BASE_URL",
                "-u",
                "CLAUDE_CODE_USE_VERTEX",
                "ANTHROPIC_BASE_URL=https://provider.invalid/;$(literal)"
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn claude_runs_through_ocx_claude_for_new_and_resumed_sessions() {
        let mut request = LaunchRequest {
            agent: Agent::Claude,
            cwd: "/cwd".into(),
            root: "/root".into(),
            resume: Some("id".into()),
            model: Some("m".into()),
        };
        let tail = |request: &LaunchRequest| {
            let args = request.cli_args(Path::new("/bin/ocx"));
            let start = args.iter().position(|a| a == "/bin/ocx").unwrap();
            args[start..].to_vec()
        };
        assert_eq!(
            tail(&request),
            ["/bin/ocx", "claude", "--resume", "id", "--model", "m"].map(OsString::from)
        );
        request.resume = None;
        request.model = None;
        assert_eq!(tail(&request), ["/bin/ocx", "claude"].map(OsString::from));
        // Plain claude gets no subcommand.
        request.resume = Some("id".into());
        let args = request.cli_args(Path::new("/bin/claude"));
        let start = args.iter().position(|a| a == "/bin/claude").unwrap();
        assert_eq!(
            args[start..],
            ["/bin/claude", "--resume", "id"].map(OsString::from)
        );
        request.agent = Agent::Codex;
        assert!(!request
            .cli_args(Path::new("/bin/codex"))
            .contains(&"claude".into()));
    }

    #[test]
    fn claude_prefers_ocx_and_falls_back_to_plain_claude() {
        let on_path = |names: &'static [&'static str]| {
            move |name: &str| {
                names
                    .contains(&name)
                    .then(|| PathBuf::from("/bin").join(name))
            }
        };
        let both = on_path(&["ocx", "claude", "codex"]);
        assert_eq!(
            resolve_binary(Agent::Claude, both).unwrap(),
            Path::new("/bin/ocx")
        );
        assert_eq!(
            resolve_binary(Agent::Codex, both).unwrap(),
            Path::new("/bin/codex")
        );
        let plain = on_path(&["claude"]);
        assert_eq!(
            resolve_binary(Agent::Claude, plain).unwrap(),
            Path::new("/bin/claude")
        );
        assert!(resolve_binary(Agent::Codex, plain)
            .unwrap_err()
            .contains("Codex"));
        let none = on_path(&[]);
        assert!(resolve_binary(Agent::Claude, none)
            .unwrap_err()
            .contains("Neither ocx nor claude"));
    }
    #[test]
    fn launcher_uses_new_window_exact_names_and_literal_args() {
        for agent in [Agent::Claude, Agent::Codex] {
            let request = LaunchRequest {
                agent,
                cwd: "/project with spaces/$(touch bad)".into(),
                root: "/profile ' ;$HOME".into(),
                resume: Some("session-id".into()),
                model: Some("model;$(bad)".into()),
            };
            let mut calls = Vec::new();
            launch_with(
                &request,
                Path::new("/bin/agent"),
                |args| {
                    calls.push(args.to_vec());
                    Ok(match args[0].to_str().unwrap() {
                        "display-message" => "$7",
                        "new-window" => "@12",
                        _ => "",
                    }
                    .into())
                },
                OsStr::new("%9"),
            )
            .unwrap();
            assert_eq!(calls.len(), 6);
            assert!(calls[1].contains(&OsString::from("$7:")));
            assert!(calls[1].contains(&request.cwd.as_os_str().to_owned()));
            assert!(calls[1].contains(&OsString::from("model;$(bad)")));
            assert!(!calls
                .iter()
                .flatten()
                .any(|s| s == "split-window" || s == "sh"));
            assert_eq!(calls[4].last().unwrap(), agent.label());
            assert_eq!(calls[2].last().unwrap(), "off");
            assert_eq!(calls[3].last().unwrap(), "off");
        }
    }
    #[test]
    fn tmux_transport_escapes_separators_and_cwd_formats() {
        assert_eq!(escape_tmux_arg(OsStr::new("literal;"), false), "literal\\;");
        assert_eq!(escape_tmux_arg(OsStr::new(";"), false), "\\;");
        assert_eq!(
            escape_tmux_arg(OsStr::new("/raw #{session_name} #(command)"), true),
            "/raw ##{session_name} ##(command)"
        );
        #[cfg(unix)]
        {
            use std::os::unix::ffi::{OsStrExt, OsStringExt};
            let raw = OsString::from_vec(vec![b'/', 0xff, b'#', b';']);
            let formatted = escape_tmux_arg(&raw, true);
            let escaped = escape_tmux_arg(&formatted, false);
            assert_eq!(escaped.as_bytes(), &[b'/', 0xff, b'#', b'#', b'\\', b';']);
        }
    }
    #[test]
    fn tmux_failures_do_not_continue_or_select_an_unprotected_window() {
        let request = LaunchRequest {
            agent: Agent::Codex,
            cwd: "/cwd".into(),
            root: "/root".into(),
            resume: Some("id".into()),
            model: None,
        };
        let mut calls = 0;
        let error = launch_with(
            &request,
            Path::new("/bin/codex"),
            |args| {
                calls += 1;
                match args[0].to_str().unwrap() {
                    "display-message" => Ok("$1".into()),
                    "new-window" => Ok("@2".into()),
                    "set-option" => Err("mock failure".into()),
                    _ => panic!("Must not continue after name protection failure"),
                }
            },
            OsStr::new("%1"),
        )
        .unwrap_err();
        assert_eq!(calls, 3);
        assert!(error.contains("Window @2 created"));
        assert!(!request
            .cli_args(Path::new("/bin/codex"))
            .contains(&"-m".into()));
    }
    #[test]
    fn default_model_omits_option_and_deleted_paths_fail() {
        let temp = tempfile::tempdir().unwrap();
        let mut request = LaunchRequest {
            agent: Agent::Claude,
            cwd: temp.path().into(),
            root: temp.path().into(),
            resume: None,
            model: None,
        };
        assert!(request.validate().is_ok());
        assert!(!request
            .cli_args(Path::new("/bin/claude"))
            .contains(&"--model".into()));
        request.cwd = temp.path().join("deleted");
        assert!(request.validate().unwrap_err().contains("directory"));
        request.cwd = temp.path().into();
        request.root = temp.path().join("deleted");
        assert!(request.validate().unwrap_err().contains("root"));
    }
}
