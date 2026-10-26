//! Local process/terminal integration. No real agent, credentials, or API calls.
#![cfg(target_os = "linux")]

use abtop::app::{App, JumpOutcome};
use abtop::archive::{Agent, ArchivedSession};
use abtop::config::PanelVisibility;
use abtop::model::SessionStatus;
use abtop::theme::Theme;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

struct Lab {
    root: tempfile::TempDir,
    old_home: Option<OsString>,
    old_tmux: Option<OsString>,
}

impl Lab {
    fn socket(&self) -> PathBuf {
        self.root.path().join("tmux.sock")
    }

    fn tmux(&self, args: &[&str]) -> String {
        let output = Command::new("tmux")
            .arg("-S")
            .arg(self.socket())
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .arg("-S")
            .arg(self.socket())
            .arg("kill-server")
            .output();
        for (key, old) in [("CODEX_HOME", &self.old_home), ("TMUX", &self.old_tmux)] {
            match old {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

#[test]
#[ignore = "requires a local tmux executable; run with cargo test --test codex_tmux -- --ignored"]
fn same_directory_sessions_jump_and_kill_their_own_client() {
    let lab = Lab {
        root: tempfile::tempdir().unwrap(),
        old_home: std::env::var_os("CODEX_HOME"),
        old_tmux: std::env::var_os("TMUX"),
    };
    let project = lab.root.path().join("project");
    let config = lab.root.path().join("codex-home");
    let day = config.join("sessions/2020/01/01");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&day).unwrap();
    let client = lab.root.path().join("codex");
    // A shell with a Codex executable name and a Codex-style terminal title.
    // The shell's read builtin waits without spawning an orphanable child.
    fs::write(
        &client,
        r#"#!/bin/sh
printf '\033]0;%s\007' "$1"
while IFS= read -r line; do :; done
"#,
    )
    .unwrap();
    fs::set_permissions(&client, fs::Permissions::from_mode(0o700)).unwrap();
    let ids = [
        "019abcde-1111-7111-8111-111111111111",
        "019abcde-2222-7222-8222-222222222222",
    ];
    let names = ["Fix checkout", "Fix search"];
    let mut index = String::new();
    for (id, name) in ids.iter().zip(names) {
        let meta = serde_json::json!({"type":"session_meta","payload":{
            "id":id,"cwd":project,"originator":"codex-tui","timestamp":"2020-01-01T00:00:00Z"
        }});
        fs::write(
            day.join(format!("rollout-old-{id}.jsonl")),
            format!("{meta}\n"),
        )
        .unwrap();
        index.push_str(&format!(
            "{}\n",
            serde_json::json!({"id":id,"thread_name":name})
        ));
    }
    fs::write(config.join("session_index.jsonl"), index).unwrap();
    let cwd = project.to_str().unwrap();
    let binary = client.to_str().unwrap();
    let first_pid: u32 = lab
        .tmux(&[
            "new-session",
            "-d",
            "-s",
            "check",
            "-n",
            "first",
            "-c",
            cwd,
            "-P",
            "-F",
            "#{pane_pid}",
            binary,
            "Fix checkout | project",
        ])
        .parse()
        .unwrap();
    let second_pid: u32 = lab
        .tmux(&[
            "new-window",
            "-d",
            "-t",
            "check",
            "-n",
            "second",
            "-c",
            cwd,
            "-P",
            "-F",
            "#{pane_pid}",
            binary,
            "Fix search | project",
        ])
        .parse()
        .unwrap();
    let server_pid = lab.tmux(&["display-message", "-p", "#{pid}"]);
    // This integration test runs in its own test binary, with one test, so
    // these temporary environment changes do not affect parallel unit tests.
    std::env::set_var("TMUX", format!("{},{server_pid},0", lab.socket().display()));
    std::env::set_var("CODEX_HOME", &config);
    let mut app = App::new_with_config_and_claude_dirs(
        Theme::default(),
        &["claude".into(), "opencode".into()],
        PanelVisibility::default(),
        &[],
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        app.tick_no_summaries();
        if app.sessions.len() == 2 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "both terminal clients must be discovered"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    for (id, pid) in ids.iter().zip([first_pid, second_pid]) {
        let session = app.sessions.iter().find(|s| s.session_id == *id).unwrap();
        assert_eq!(session.pid, pid);
        assert_eq!(session.jump_pid, Some(pid));
        assert_eq!(session.status, SessionStatus::Waiting);
        app.workspace.archive.push(ArchivedSession {
            agent: Agent::Codex,
            id: id.to_string(),
            title: "Archived".into(),
            cwd: project.clone(),
            root: config.clone(),
            modified: 1,
        });
    }
    assert!(app.workspace.visible(&app.sessions).is_empty());

    // Permission prompts live in the TUI rather than the rollout. Both blink
    // phases must remain Reply, and an approval/denial must clear it without a
    // transcript write. Another session in the same cwd remains unaffected.
    for prefix in ["[ ! ] Action Required", "[ . ] Action Required"] {
        lab.tmux(&[
            "select-pane",
            "-t",
            "check:first.0",
            "-T",
            &format!("{prefix} Fix checkout | project"),
        ]);
        app.tick_no_summaries();
        assert_eq!(
            app.sessions
                .iter()
                .find(|s| s.session_id == ids[0])
                .unwrap()
                .status,
            SessionStatus::AwaitingInput
        );
        assert_eq!(
            app.sessions
                .iter()
                .find(|s| s.session_id == ids[1])
                .unwrap()
                .status,
            SessionStatus::Waiting
        );
    }
    lab.tmux(&[
        "select-pane",
        "-t",
        "check:first.0",
        "-T",
        "Fix checkout | project",
    ]);
    app.tick_no_summaries();
    assert!(app
        .sessions
        .iter()
        .all(|s| s.status == SessionStatus::Waiting));
    for (id, window) in ids.iter().zip(["first", "second"]) {
        app.selected = app
            .sessions
            .iter()
            .position(|s| s.session_id == *id)
            .unwrap();
        assert!(matches!(app.jump_to_session(), JumpOutcome::Jumped));
        assert_eq!(
            lab.tmux(&["display-message", "-p", "#{window_name}"]),
            window
        );
    }

    // A pending x confirmation must not follow an index to another session
    // when the session list changes between key presses.
    app.selected = app
        .sessions
        .iter()
        .position(|s| s.session_id == ids[0])
        .unwrap();
    app.kill_selected();
    app.sessions.swap(0, 1);
    app.kill_selected();
    assert!(std::path::Path::new(&format!("/proc/{first_pid}")).exists());
    assert!(std::path::Path::new(&format!("/proc/{second_pid}")).exists());
    app.selected = app
        .sessions
        .iter()
        .position(|s| s.session_id == ids[1])
        .unwrap();
    app.kill_selected();
    app.kill_selected();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        app.tick_no_summaries();
        if app.sessions.len() == 1 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the killed session must disappear"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(app.sessions[0].session_id, ids[0]);
    assert_eq!(app.sessions[0].pid, first_pid);
    let archived = app.workspace.visible(&app.sessions);
    assert_eq!(archived.len(), 1);
    assert_eq!(archived[0].id, ids[1]);
}
