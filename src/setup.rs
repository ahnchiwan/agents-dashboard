use serde_json::Value;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::path::PathBuf;

const REPLY_SCRIPT: &str = include_str!("../scripts/claude-reply-hook.sh");

/// Install observation-only permission hooks, retaining all existing settings.
pub fn run_reply_setup() -> io::Result<()> {
    for program in ["bash", "jq"] {
        let output = std::process::Command::new(program)
            .arg("--version")
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "{program} is required for the Claude Reply hook"
            )));
        }
    }
    let dir = claude_dir();
    let backup = install_reply_hook(&dir)?;
    println!(
        "Installed Claude permission Reply hooks in {}",
        dir.display()
    );
    if let Some(path) = backup {
        println!("Previous settings saved to {}", path.display());
    }
    println!("Restart Claude Code to load the hooks. Existing StatusLine and permission rules are preserved.");
    println!("Codex permission prompts are detected from the Action Required title in tmux; no hook is needed.");
    Ok(())
}

fn install_reply_hook(dir: &Path) -> io::Result<Option<PathBuf>> {
    fs::create_dir_all(dir)?;
    let path = dir.join("settings.json");
    let original = match fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let mut settings: Value = match &original {
        Some(bytes) => serde_json::from_slice(bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
        None => serde_json::json!({}),
    };
    let before = settings.clone();
    let script = dir.join("abtop-reply-hook.sh");
    merge_reply_hooks(&mut settings, &script)?;
    let mut hook_file = tempfile::NamedTempFile::new_in(dir)?;
    hook_file.write_all(REPLY_SCRIPT.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        hook_file
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o700))?;
    }
    hook_file.persist(&script).map_err(|error| error.error)?;
    if settings == before {
        return Ok(None);
    }

    let backup = if let Some(bytes) = original {
        let mut backup = tempfile::Builder::new()
            .prefix("settings.json.abtop-reply-backup-")
            .suffix(".json")
            .tempfile_in(dir)?;
        backup.write_all(&bytes)?;
        Some(backup.keep().map_err(|error| error.error)?.1)
    } else {
        None
    };
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    serde_json::to_writer_pretty(file.as_file_mut(), &settings)?;
    file.write_all(b"\n")?;
    file.persist(&path).map_err(|error| error.error)?;
    Ok(backup)
}

fn merge_reply_hooks(settings: &mut Value, script: &Path) -> io::Result<()> {
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Claude settings and hooks must be JSON objects, with arrays for hook events",
        )
    };
    let object = settings.as_object_mut().ok_or_else(invalid)?;
    if object.get("disableAllHooks").and_then(Value::as_bool) == Some(true) {
        return Err(io::Error::other("Claude hooks are disabled by disableAllHooks; permission Reply detection requires hooks to be enabled"));
    }
    let hooks = object
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));
    let hooks = hooks.as_object_mut().ok_or_else(invalid)?;
    // Hook commands are shell strings, so quote the path independently of JSON.
    let script = format!("'{}'", script.to_string_lossy().replace('\'', "'\\''"));
    for (event, action, matcher) in [
        ("PermissionRequest", "mark", None),
        ("Notification", "mark", Some("permission_prompt")),
        ("PostToolUse", "clear", None),
        ("PostToolUseFailure", "clear", None),
        ("PermissionDenied", "clear", None),
        ("SessionEnd", "clear_all", None),
    ] {
        let mut group = serde_json::json!({"hooks":[{"type":"command","command":format!("bash {script} {action}")}]});
        if let Some(matcher) = matcher {
            group["matcher"] = matcher.into();
        }
        let entries = hooks.entry(event).or_insert_with(|| serde_json::json!([]));
        let entries = entries.as_array_mut().ok_or_else(invalid)?;
        if !entries.contains(&group) {
            entries.push(group);
        }
    }
    Ok(())
}

const STATUSLINE_SCRIPT: &str = r#"#!/bin/bash
# abtop StatusLine hook — writes rate limit data for abtop to read.
# Installed by: abtop --setup
# Reads JSON from stdin with a 5s timeout, pipes it to python via stdin
# to avoid ARG_MAX limits on large payloads.
INPUT=""
while IFS= read -r -t 5 line || [ -n "$line" ]; do
    INPUT="${INPUT}${line}
"
done
[ -z "$INPUT" ] && exit 0
printf '%s' "$INPUT" | python3 -c "
import sys, json, time, os
data = json.load(sys.stdin)
rl = data.get('rate_limits')
if not rl:
    sys.exit(0)
out = {'source': 'claude', 'updated_at': int(time.time())}
fh = rl.get('five_hour')
if fh:
    out['five_hour'] = {'used_percentage': fh.get('used_percentage', 0), 'resets_at': fh.get('resets_at', 0)}
sd = rl.get('seven_day')
if sd:
    out['seven_day'] = {'used_percentage': sd.get('used_percentage', 0), 'resets_at': sd.get('resets_at', 0)}
config_dir = os.environ.get('CLAUDE_CONFIG_DIR', os.path.join(os.path.expanduser('~'), '.claude'))
with open(os.path.join(config_dir, 'abtop-rate-limits.json'), 'w') as f:
    json.dump(out, f)
" 2>/dev/null
"#;

fn claude_dir() -> PathBuf {
    std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".claude"))
}

fn script_path() -> PathBuf {
    claude_dir().join("abtop-statusline.sh")
}

fn settings_path() -> PathBuf {
    claude_dir().join("settings.json")
}

pub fn run_setup() {
    println!("abtop --setup: configuring Claude Code StatusLine hook\n");

    // Ensure ~/.claude directory exists
    let dir = claude_dir();
    if let Err(e) = fs::create_dir_all(&dir) {
        eprintln!("  ✗ failed to create {}: {}", dir.display(), e);
        std::process::exit(1);
    }

    // Step 1: Write the statusline script
    let script = script_path();
    match fs::write(&script, STATUSLINE_SCRIPT) {
        Ok(_) => {
            // chmod +x
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&script, fs::Permissions::from_mode(0o700));
            }
            println!("  ✓ wrote {}", script.display());
        }
        Err(e) => {
            eprintln!("  ✗ failed to write {}: {}", script.display(), e);
            std::process::exit(1);
        }
    }

    // Step 2: Update settings.json
    let settings_file = settings_path();
    let mut settings: Value = if settings_file.exists() {
        let content = match fs::read_to_string(&settings_file) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("  ✗ cannot read {}: {}", settings_file.display(), e);
                std::process::exit(1);
            }
        };
        match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(e) => {
                eprintln!(
                    "  ✗ {} contains invalid JSON: {}",
                    settings_file.display(),
                    e
                );
                eprintln!("    fix the file manually before running --setup");
                std::process::exit(1);
            }
        }
    } else {
        Value::Object(Default::default())
    };

    let obj = settings.as_object_mut().unwrap();

    // Check if statusLine is already configured
    let expected_cmd = script.display().to_string();
    if let Some(existing) = obj.get("statusLine") {
        if let Some(existing_obj) = existing.as_object() {
            if let Some(cmd) = existing_obj.get("command") {
                let cmd_str = cmd.as_str().unwrap_or("");
                if cmd_str != expected_cmd && !cmd_str.is_empty() {
                    eprintln!("  ⚠ statusLine already configured: {}", cmd_str);
                    eprintln!("    to override, remove the existing statusLine key from:");
                    eprintln!("    {}", settings_file.display());
                    std::process::exit(1);
                }
            }
        }
    }

    // Set statusLine config
    obj.insert(
        "statusLine".to_string(),
        serde_json::json!({
            "type": "command",
            "command": script.display().to_string()
        }),
    );

    match fs::write(
        &settings_file,
        serde_json::to_string_pretty(&settings).unwrap_or_default(),
    ) {
        Ok(_) => println!("  ✓ updated {}", settings_file.display()),
        Err(e) => {
            eprintln!("  ✗ failed to update {}: {}", settings_file.display(), e);
            std::process::exit(1);
        }
    }

    println!("\n  done! rate limit data will appear in abtop after the next Claude response.");
    println!("  restart any running Claude Code sessions to activate.");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_setup_preserves_existing_settings_and_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("profile with 'quotes'");
        fs::create_dir(&dir).unwrap();
        let path = dir.join("settings.json");
        let original = serde_json::json!({
            "statusLine":{"type":"command","command":"existing-statusline"},
            "permissions":{"deny":["Bash(rm *)"]},
            "hooks":{"PermissionRequest":[{"matcher":"Bash","hooks":[{"type":"command","command":"existing-hook"}]}]},
            "model":"configured-model"
        });
        let bytes = serde_json::to_vec_pretty(&original).unwrap();
        fs::write(&path, &bytes).unwrap();
        let backup = install_reply_hook(&dir).unwrap().unwrap();
        assert_eq!(fs::read(backup).unwrap(), bytes);
        assert_eq!(
            fs::read_to_string(dir.join("abtop-reply-hook.sh")).unwrap(),
            REPLY_SCRIPT
        );
        let settings: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        for key in ["statusLine", "permissions", "model"] {
            assert_eq!(settings[key], original[key]);
        }
        assert_eq!(
            settings["hooks"]["PermissionRequest"][0],
            original["hooks"]["PermissionRequest"][0]
        );
        assert_eq!(
            settings["hooks"]["PermissionRequest"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            settings["hooks"]["Notification"][0]["matcher"],
            "permission_prompt"
        );
        assert!(install_reply_hook(&dir).unwrap().is_none());
        let again: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(again, settings);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(dir.join("abtop-reply-hook.sh"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        // Check that shell quoting survives a profile path containing spaces
        // and apostrophes, and that installed hooks only create/clear markers.
        if !std::process::Command::new("jq")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
        {
            return;
        }
        let run = |event: &str, payload: Value| {
            use std::process::{Command, Stdio};
            let command = settings["hooks"][event].as_array().unwrap().last().unwrap()["hooks"][0]
                ["command"]
                .as_str()
                .unwrap();
            let mut child = Command::new("bash")
                .args(["-c", command])
                .env("CLAUDE_CONFIG_DIR", &dir)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            serde_json::to_writer(child.stdin.take().unwrap(), &payload).unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success());
            assert!(
                output.stdout.is_empty(),
                "observation hooks must not grant permissions"
            );
        };
        run(
            "PermissionRequest",
            serde_json::json!({"session_id":"setup-test","tool_name":"Bash","hook_event_name":"PermissionRequest"}),
        );
        let marker = dir.join("abtop-reply-state/setup-test");
        assert!(marker.is_file());
        run(
            "PermissionDenied",
            serde_json::json!({"session_id":"setup-test","tool_name":"Bash","hook_event_name":"PermissionDenied"}),
        );
        assert!(!marker.exists());
    }

    #[test]
    fn reply_setup_rejects_invalid_settings_without_overwriting_them() {
        for content in [
            "not json",
            "[]",
            r#"{"hooks":null}"#,
            r#"{"hooks":{"Notification":{}}}"#,
            r#"{"disableAllHooks":true}"#,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("settings.json");
            fs::write(&path, content).unwrap();
            assert!(install_reply_hook(dir.path()).is_err());
            assert_eq!(fs::read_to_string(path).unwrap(), content);
            assert!(!dir.path().join("abtop-reply-hook.sh").exists());
        }
    }
}
