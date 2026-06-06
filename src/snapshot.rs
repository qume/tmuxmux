//! Periodic capture of what's running on each host: every tmux session's
//! windows, panes, layouts, working directories and — by walking the host's
//! process tree — the full command line running in each pane. One shell
//! round-trip per host. Results feed both the sidebar and the sqlite cache,
//! so vanished sessions can be restored later (see restore.rs).

use std::collections::HashMap;
use std::sync::mpsc;
use std::thread;

use crate::config::Host;
use crate::ssh::build_shell_command;

/// Field separator for the tmux format string. It must be printable ASCII:
/// tmux octal-escapes control characters on output (0x1f arrives as literal
/// "\037"), and on hosts without a UTF-8 locale (ssh forwards none) it
/// flattens non-ASCII to underscores. This string surviving in a session
/// name, path or layout is about as likely as a cosmic ray.
const SEP: &str = "<#~#>";
const PS_MARKER: &str = "__TMUXMUX_PS__";

#[derive(Debug, Clone)]
pub struct PaneSnap {
    pub window_index: i64,
    pub window_name: String,
    pub window_layout: String,
    pub pane_index: i64,
    /// Short process name of whatever is in the pane ("claude", "vim", "bash").
    pub command: String,
    /// Full command line of the foreground program, when it isn't a bare shell.
    pub cmdline: Option<String>,
    pub cwd: String,
}

#[derive(Debug, Clone)]
pub struct SessionSnap {
    pub name: String,
    pub created_at: Option<i64>,
    pub panes: Vec<PaneSnap>,
}

#[derive(Debug)]
pub struct SnapshotResult {
    pub host_name: String,
    pub sessions: Vec<SessionSnap>,
    pub error: Option<String>,
}

/// The remote shell command. tmux errors are folded into stdout so a host
/// without a tmux server parses as "no sessions" rather than a failure.
fn snapshot_command() -> String {
    format!(
        "tmux list-panes -a -F '#{{session_name}}{s}#{{session_created}}{s}#{{window_index}}{s}#{{window_name}}{s}#{{window_layout}}{s}#{{pane_index}}{s}#{{pane_pid}}{s}#{{pane_current_command}}{s}#{{pane_current_path}}' 2>&1; echo {m}; ps -eo ppid=,pid=,args= 2>/dev/null",
        s = SEP,
        m = PS_MARKER
    )
}

pub fn take_snapshot(host: Host) -> SnapshotResult {
    let argv = build_shell_command(&host, &snapshot_command());
    let mut cmd = std::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    let output = match cmd.output() {
        Ok(o) => o,
        Err(e) => {
            return SnapshotResult {
                host_name: host.name,
                sessions: vec![],
                error: Some(e.to_string()),
            }
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout).replace('\r', "");
    if !stdout.contains(PS_MARKER) {
        // The shell never ran on the far side — connection-level failure.
        let stderr = String::from_utf8_lossy(&output.stderr);
        let msg = stderr
            .lines()
            .last()
            .or_else(|| stdout.lines().last())
            .unwrap_or("connection failed")
            .to_string();
        return SnapshotResult {
            host_name: host.name,
            sessions: vec![],
            error: Some(msg),
        };
    }

    let (pane_part, ps_part) = stdout.split_once(PS_MARKER).unwrap();

    // tmux not running (or zero sessions) is a normal empty result.
    let benign = ["no server running", "no sessions", "error connecting to"];
    let mut error = None;
    let has_panes = pane_part.lines().any(|l| l.contains(SEP));
    if !has_panes {
        let noise = pane_part.trim();
        if !noise.is_empty() && !benign.iter().any(|b| noise.contains(b)) {
            error = Some(noise.lines().last().unwrap_or("").to_string());
        }
    }

    let sessions = parse_snapshot(pane_part, ps_part);
    SnapshotResult {
        host_name: host.name,
        sessions,
        error,
    }
}

fn parse_snapshot(pane_part: &str, ps_part: &str) -> Vec<SessionSnap> {
    // Process table: pid → args, ppid → [(pid, args)]
    let mut procs: HashMap<i64, String> = HashMap::new();
    let mut children: HashMap<i64, Vec<(i64, String)>> = HashMap::new();
    for line in ps_part.lines() {
        if let Some((ppid, pid, args)) = parse_ps_line(line) {
            procs.insert(pid, args.clone());
            children.entry(ppid).or_default().push((pid, args));
        }
    }
    for kids in children.values_mut() {
        kids.sort_by_key(|(pid, _)| *pid);
    }

    let mut sessions: Vec<SessionSnap> = Vec::new();
    for line in pane_part.lines() {
        let fields: Vec<&str> = line.split(SEP).collect();
        if fields.len() != 9 {
            continue;
        }
        let name = fields[0].to_string();
        let created_at = fields[1].parse::<i64>().ok();
        let pane_pid = fields[6].parse::<i64>().unwrap_or(0);
        let pane = PaneSnap {
            window_index: fields[2].parse().unwrap_or(0),
            window_name: fields[3].to_string(),
            window_layout: fields[4].to_string(),
            pane_index: fields[5].parse().unwrap_or(0),
            command: fields[7].to_string(),
            cmdline: pane_cmdline(pane_pid, fields[7], &procs, &children),
            cwd: fields[8].to_string(),
        };
        match sessions.iter_mut().find(|s| s.name == name) {
            Some(s) => s.panes.push(pane),
            None => sessions.push(SessionSnap {
                name,
                created_at,
                panes: vec![pane],
            }),
        }
    }
    sessions
}

/// "  123  456 some command args" → (123, 456, "some command args"),
/// preserving the args text verbatim.
fn parse_ps_line(line: &str) -> Option<(i64, i64, String)> {
    let rest = line.trim_start();
    let (ppid_str, rest) = rest.split_once(char::is_whitespace)?;
    let rest = rest.trim_start();
    let (pid_str, args) = rest.split_once(char::is_whitespace)?;
    let ppid = ppid_str.parse().ok()?;
    let pid = pid_str.parse().ok()?;
    Some((ppid, pid, args.trim_start().to_string()))
}

const SHELLS: &[&str] = &["bash", "zsh", "fish", "sh", "dash", "ksh", "tcsh", "csh"];

/// A process that is just an interactive shell sitting at a prompt —
/// not worth recording or re-running.
fn is_bare_shell(args: &str) -> bool {
    let mut tokens = args.split_whitespace();
    let first = match tokens.next() {
        Some(t) => t,
        None => return true,
    };
    if tokens.next().is_some() {
        return false; // shell running a script/command — keep it
    }
    let base = first.rsplit('/').next().unwrap_or(first);
    let base = base.trim_start_matches('-'); // login shells show as "-bash"
    SHELLS.contains(&base)
}

/// Best guess at the command running in a pane: the oldest non-shell child
/// of the pane's process; or the pane process itself when tmux started it
/// directly (no shell wrapper).
fn pane_cmdline(
    pane_pid: i64,
    pane_command: &str,
    procs: &HashMap<i64, String>,
    children: &HashMap<i64, Vec<(i64, String)>>,
) -> Option<String> {
    if let Some(kids) = children.get(&pane_pid) {
        for (_, args) in kids {
            if !is_bare_shell(args) {
                return Some(args.clone());
            }
        }
    }
    if !SHELLS.contains(&pane_command) {
        if let Some(args) = procs.get(&pane_pid) {
            if !is_bare_shell(args) {
                return Some(args.clone());
            }
        }
    }
    None
}

/// Poll the given hosts concurrently, one thread each, reporting on `tx`.
pub fn spawn_snapshots(hosts: Vec<Host>, tx: mpsc::Sender<SnapshotResult>) {
    for host in hosts {
        let tx = tx.clone();
        thread::spawn(move || {
            let _ = tx.send(take_snapshot(host));
        });
    }
}
