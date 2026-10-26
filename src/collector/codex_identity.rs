//! Associate daemon-backed Codex terminal clients with their current thread.
//! Directory names cannot identify clients when several work in one project.

use super::process::{self, ProcInfo};
use std::collections::{HashMap, HashSet};
use std::process::Command;
use unicode_segmentation::UnicodeSegmentation;

#[cfg(target_os = "linux")]
#[derive(Debug)]
pub(super) struct StartedSession {
    pub id: String,
    pub cwd: std::path::PathBuf,
    pub started_ms: u64,
}

/// A newly created thread may not have a name yet. Require unique client and
/// thread startup times as well as cwd, rather than pairing by directory alone.
#[cfg(target_os = "linux")]
pub(super) fn discover_startups(
    clients: &[u32],
    sessions: &[StartedSession],
    matched: &mut HashMap<u32, String>,
) {
    let Ok(stat) = std::fs::read_to_string("/proc/stat") else {
        return;
    };
    let Some(boot_secs) = stat
        .lines()
        .find_map(|line| line.strip_prefix("btime ")?.parse::<u64>().ok())
    else {
        return;
    };
    let ticks_per_sec = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks_per_sec <= 0 {
        return;
    }
    let clients: Vec<_> = clients
        .iter()
        .filter(|pid| !matched.contains_key(pid))
        .filter_map(|&pid| {
            let argv = process_args(pid, &HashMap::new());
            if argv
                .iter()
                .any(|arg| matches!(arg.as_str(), "resume" | "fork" | "agents"))
            {
                return None;
            }
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            let ticks = stat
                .rsplit_once(')')?
                .1
                .split_whitespace()
                .nth(19)?
                .parse::<u64>()
                .ok()?;
            let started_ms = boot_secs
                .checked_mul(1000)?
                .checked_add(ticks.checked_mul(1000)? / ticks_per_sec as u64)?;
            let cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).ok()?;
            Some((pid, started_ms, cwd))
        })
        .collect();
    let sessions: Vec<_> = sessions
        .iter()
        .filter(|session| !matched.values().any(|id| id == &session.id))
        .collect();
    matched.extend(unique_startups(&clients, &sessions));
}

#[cfg(target_os = "linux")]
fn unique_startups(
    clients: &[(u32, u64, std::path::PathBuf)],
    sessions: &[&StartedSession],
) -> HashMap<u32, String> {
    let pairs: Vec<_> = clients
        .iter()
        .flat_map(|(pid, started_ms, cwd)| {
            sessions.iter().filter_map(move |session| {
                (*cwd == session.cwd
                    && session.started_ms >= *started_ms
                    && session.started_ms.saturating_sub(*started_ms) <= 5000)
                    .then_some((*pid, session.id.as_str()))
            })
        })
        .collect();
    pairs
        .iter()
        .filter(|(pid, id)| {
            pairs.iter().filter(|(other, _)| other == pid).count() == 1
                && pairs.iter().filter(|(_, other)| other == id).count() == 1
        })
        .map(|(pid, id)| (*pid, id.to_string()))
        .collect()
}

pub(super) fn discover(
    processes: &HashMap<u32, ProcInfo>,
    clients: &[u32],
    names: &HashMap<String, String>,
) -> HashMap<u32, String> {
    if clients.is_empty() {
        return HashMap::new();
    }
    // Read pane titles only. Do not capture conversation text or send input.
    let panes = Command::new("tmux")
        .args(["list-panes", "-a", "-F", "#{pane_pid}\t#{pane_title}"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default();
    let args: HashMap<u32, Vec<String>> = clients
        .iter()
        .map(|&pid| (pid, process_args(pid, processes)))
        .collect();
    let mut catalog = names.clone();
    for argv in args.values() {
        if let Some(id) = resumed_session(argv, names) {
            catalog.entry(id).or_default();
        }
    }
    match_clients(processes, clients, &catalog, &panes, |pid| {
        args[&pid].clone()
    })
}

fn match_clients(
    processes: &HashMap<u32, ProcInfo>,
    clients: &[u32],
    names: &HashMap<String, String>,
    panes: &str,
    args: impl Fn(u32) -> Vec<String>,
) -> HashMap<u32, String> {
    let mut titles: HashMap<u32, HashSet<String>> = HashMap::new();
    for line in panes.lines() {
        let Some((pid, title)) = line.split_once('\t') else {
            continue;
        };
        let Ok(pane_pid) = pid.parse() else {
            continue;
        };
        let pane_clients: Vec<u32> = clients
            .iter()
            .copied()
            .filter(|pid| *pid == pane_pid || process::is_descendant_of(*pid, pane_pid, processes))
            .collect();
        let [client] = pane_clients.as_slice() else {
            continue;
        };
        titles.entry(*client).or_default().insert(title.to_string());
    }

    let mut matched = HashMap::new();
    for &pid in clients {
        let pane_titles = titles.get(&pid);
        // A current title takes precedence over the original `resume` argument:
        // /new and thread switching can change the thread without changing argv.
        let title_matches: HashSet<String> = pane_titles
            .into_iter()
            .flatten()
            .filter_map(|title| session_from_title(title, names))
            .collect();
        let id = if title_matches.len() == 1 {
            title_matches.into_iter().next()
        } else if pane_titles.is_none() {
            resumed_session(&args(pid), names)
        } else {
            // Even an unrecognized pane title can mean the client switched to a
            // new, unnamed thread. Do not reuse a stale resume argument then.
            None
        };
        if let Some(id) = id {
            matched.insert(pid, id);
        }
    }
    // Two terminals can view the same thread. Neither is a unique kill target.
    let mut counts = HashMap::new();
    for id in matched.values() {
        *counts.entry(id.clone()).or_insert(0) += 1;
    }
    matched.retain(|_, id| counts[id] == 1);
    matched
}

fn session_from_title(title: &str, names: &HashMap<String, String>) -> Option<String> {
    let title = title
        .trim_start_matches(|c: char| c.is_whitespace() || c == '●')
        .trim_start_matches("[ ! ] Action Required")
        .trim_start_matches("[ . ] Action Required")
        .trim_start_matches(|c: char| {
            c.is_whitespace() || matches!(c, '\u{2800}'..='\u{28ff}' | '●')
        });
    let bounded_title = format!(" | {title} | ");
    // A session-ID segment resolves duplicate names. Codex truncates this
    // segment to 32 graphemes; reject collisions in that prefix too.
    let ids: Vec<_> = names
        .keys()
        .filter(|id| {
            is_session_id(id)
                && (bounded_title.contains(&format!(" | {id} | "))
                    || bounded_title.contains(&format!(" | {} | ", title_part(id, 32))))
        })
        .collect();
    if !ids.is_empty() {
        return (ids.len() == 1).then(|| ids[0].clone());
    }
    let mut matches = names.iter().filter_map(|(id, name)| {
        // Compare entire rendered name segments, never arbitrary substrings.
        let name_matches = !name.trim().is_empty()
            && title.contains(" | ")
            && bounded_title.contains(&format!(" | {} | ", title_part(name, 48)));
        name_matches.then(|| id.clone())
    });
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

fn title_part(value: &str, limit: usize) -> String {
    let graphemes: Vec<&str> = value.graphemes(true).collect();
    if graphemes.len() <= limit {
        value.to_string()
    } else {
        format!("{}...", graphemes[..limit - 3].concat())
    }
}

fn resumed_session(args: &[String], names: &HashMap<String, String>) -> Option<String> {
    let mut args = args.iter().skip(1);
    while let Some(arg) = args.next() {
        if arg == "resume" {
            let value = args.next()?;
            if names.contains_key(value) || is_session_id(value) {
                return Some(value.clone());
            }
            let mut ids = names.iter().filter(|(_, name)| *name == value);
            let (id, _) = ids.next()?;
            return ids.next().is_none().then(|| id.clone());
        }
        if matches!(
            arg.as_str(),
            "-c" | "--config"
                | "-m"
                | "--model"
                | "-p"
                | "--profile"
                | "-s"
                | "--sandbox"
                | "-a"
                | "--ask-for-approval"
                | "-C"
                | "--cd"
                | "--add-dir"
                | "--remote"
                | "--remote-auth-token-env"
                | "-i"
                | "--image"
                | "--enable"
                | "--disable"
        ) {
            args.next()?;
        } else if !arg.starts_with('-') || arg == "--" {
            // A positional prompt can itself contain "resume <id>".
            return None;
        }
    }
    None
}

pub(super) fn is_session_id(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

#[cfg(target_os = "linux")]
fn process_args(pid: u32, _processes: &HashMap<u32, ProcInfo>) -> Vec<String> {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .unwrap_or_default()
        .split(|byte| *byte == 0)
        .filter(|arg| !arg.is_empty())
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect()
}

#[cfg(not(target_os = "linux"))]
fn process_args(pid: u32, processes: &HashMap<u32, ProcInfo>) -> Vec<String> {
    processes
        .get(&pid)
        .map(|info| {
            info.command
                .split_whitespace()
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn processes() -> HashMap<u32, ProcInfo> {
        [
            (10, 1, "bash"),
            (11, 10, "codex"),
            (20, 1, "codex"),
            (30, 1, "codex app-server"),
        ]
        .into_iter()
        .map(|(pid, ppid, command)| {
            (
                pid,
                ProcInfo {
                    pid,
                    ppid,
                    command: command.into(),
                    rss_kb: 0,
                    cpu_pct: 0.0,
                },
            )
        })
        .collect()
    }

    fn names() -> HashMap<String, String> {
        HashMap::from([
            (
                "019abcde-1111-7111-8111-111111111111".into(),
                "Fix checkout".into(),
            ),
            (
                "019abcde-2222-7222-8222-222222222222".into(),
                "Fix search".into(),
            ),
        ])
    }

    #[test]
    fn identifies_two_clients_in_one_directory_by_current_pane_title() {
        let matches = match_clients(
            &processes(),
            &[11, 20],
            &names(),
            "10\t⠹ Fix checkout | project\n20\tFix search | project\n",
            |_| vec![],
        );
        assert_eq!(
            matches.get(&11).map(String::as_str),
            Some("019abcde-1111-7111-8111-111111111111")
        );
        assert_eq!(
            matches.get(&20).map(String::as_str),
            Some("019abcde-2222-7222-8222-222222222222")
        );
    }

    #[test]
    fn current_title_overrides_resumed_thread_after_switching() {
        let matches = match_clients(
            &processes(),
            &[11],
            &names(),
            "10\tFix search | project\n",
            |_| {
                vec![
                    "codex".into(),
                    "resume".into(),
                    "019abcde-1111-7111-8111-111111111111".into(),
                ]
            },
        );
        assert_eq!(
            matches.get(&11).map(String::as_str),
            Some("019abcde-2222-7222-8222-222222222222")
        );
    }

    #[test]
    fn duplicate_and_truncated_names_do_not_guess() {
        let mut names = names();
        names.insert("other".into(), "Fix checkout".into());
        assert_eq!(session_from_title("Fix checkout | project", &names), None);
        names.insert("long-one".into(), format!("{} one", "x".repeat(50)));
        names.insert("long-two".into(), format!("{} two", "x".repeat(50)));
        assert_eq!(
            session_from_title(&format!("{}... | project", "x".repeat(45)), &names),
            None
        );
        assert_eq!(
            session_from_title("Fix checkout extra | project", &names),
            None
        );
    }

    #[test]
    fn session_id_title_segment_distinguishes_duplicate_names() {
        let mut names = names();
        let first = "019abcde-1111-7111-8111-111111111111";
        let second = "019abcde-2222-7222-8222-222222222222";
        names.insert(second.into(), "Fix checkout".into());
        assert_eq!(
            session_from_title(
                &format!("{} | Fix checkout | project", title_part(first, 32)),
                &names
            ),
            Some(first.into())
        );
        names.insert(
            "019abcde-1111-7111-8111-111111111199".into(),
            "Other".into(),
        );
        assert_eq!(
            session_from_title(&format!("{} | project", title_part(first, 32)), &names),
            None
        );
    }

    #[test]
    fn recognizes_action_required_and_grapheme_truncation() {
        let name = format!("{} completed", "👩‍💻".repeat(49));
        let names = HashMap::from([("one".into(), name)]);
        assert_eq!(
            session_from_title(
                &format!("[ ! ] Action Required {}... | project", "👩‍💻".repeat(45)),
                &names
            ),
            Some("one".into())
        );
    }

    #[test]
    fn multiple_clients_in_a_pane_or_on_one_thread_are_not_unique() {
        let mut procs = processes();
        procs.get_mut(&20).unwrap().ppid = 10;
        assert!(match_clients(
            &procs,
            &[11, 20],
            &names(),
            "10\tFix checkout | project\n",
            |_| vec![]
        )
        .is_empty());
        assert!(match_clients(
            &processes(),
            &[11, 20],
            &names(),
            "10\tFix checkout | project\n20\tFix checkout | project\n",
            |_| vec![]
        )
        .is_empty());
    }

    #[test]
    fn resume_arguments_support_same_directory_without_tmux() {
        let names = names();
        let matches = match_clients(&processes(), &[11, 20], &names, "", |pid| {
            vec![
                "codex".into(),
                "-C".into(),
                "/same/project".into(),
                "resume".into(),
                if pid == 11 {
                    "Fix checkout".into()
                } else {
                    "019abcde-2222-7222-8222-222222222222".into()
                },
            ]
        });
        assert_eq!(matches.len(), 2);
        assert_eq!(
            resumed_session(&["codex".into(), "please resume Fix search".into()], &names),
            None
        );
        assert_eq!(
            resumed_session(
                &["codex".into(), "fork".into(), "Fix search".into()],
                &names
            ),
            None
        );
    }

    #[test]
    fn unnamed_title_does_not_reuse_an_old_resume_argument() {
        let matches = match_clients(&processes(), &[11], &names(), "10\tproject\n", |_| {
            vec!["codex".into(), "resume".into(), "Fix checkout".into()]
        });
        assert!(matches.is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unnamed_startups_in_one_directory_need_unique_creation_intervals() {
        let cwd = std::path::PathBuf::from("/same/project");
        let clients = [(11, 1000, cwd.clone()), (20, 11000, cwd.clone())];
        let first = StartedSession {
            id: "first".into(),
            cwd: cwd.clone(),
            started_ms: 2000,
        };
        let second = StartedSession {
            id: "second".into(),
            cwd: cwd.clone(),
            started_ms: 12000,
        };
        let sessions = [&first, &second];
        let matched = unique_startups(&clients, &sessions);
        assert_eq!(
            matched,
            HashMap::from([(11, "first".into()), (20, "second".into())])
        );

        // A newly opened empty client must not claim an old daemon transcript.
        assert!(unique_startups(&[(30, 20000, cwd.clone())], &sessions).is_empty());
        // Closely concurrent clients need a title/session ID to disambiguate.
        assert!(unique_startups(
            &[(11, 1000, cwd.clone()), (20, 1100, cwd.clone())],
            &[&first]
        )
        .is_empty());
        assert!(unique_startups(
            &[(11, 1000, cwd.clone())],
            &[
                &first,
                &StartedSession {
                    id: "another".into(),
                    cwd: cwd.clone(),
                    started_ms: 2500,
                }
            ]
        )
        .is_empty());
        assert!(unique_startups(&[(11, 1000, "/other/project".into())], &[&first]).is_empty());
    }
}
