use crate::config::Host;
use std::process::Command;

const SSH_OPTS: &[&str] = &[
    "-o", "BatchMode=yes",
    "-o", "ConnectTimeout=5",
    "-o", "ServerAliveInterval=30",
];

pub fn ssh_target(host: &Host) -> String {
    match &host.username {
        Some(u) => format!("{}@{}", u, host.name),
        None => host.name.clone(),
    }
}

pub fn build_list_command(host: &Host) -> Command {
    if host.local {
        let mut cmd = Command::new("tmux");
        cmd.arg("ls");
        cmd
    } else if let Some(ref raw) = host.command {
        let parts = shlex_like_split(raw);
        let mut cmd = Command::new(&parts[0]);
        for arg in &parts[1..] {
            cmd.arg(arg);
        }
        cmd.arg("tmux");
        cmd.arg("ls");
        cmd
    } else {
        let mut cmd = Command::new("ssh");
        cmd.args(SSH_OPTS);
        cmd.arg(ssh_target(host));
        cmd.arg("tmux");
        cmd.arg("ls");
        cmd
    }
}

pub fn build_attach_command(host: &Host, session: &str) -> Vec<String> {
    // `-u` forces tmux to treat this client as UTF-8. ssh doesn't forward the
    // locale, so without it the remote tmux assumes a non-UTF-8 client and
    // downgrades box-drawing to the DEC ACS charset ("qqqq" horizontal lines).
    if host.local {
        vec!["tmux".into(), "-u".into(), "attach".into(), "-t".into(), session.to_string()]
    } else if let Some(ref raw) = host.command {
        let mut parts = shlex_like_split(raw);
        parts.push("tmux".into());
        parts.push("-u".into());
        parts.push("attach".into());
        parts.push("-t".into());
        parts.push(session.to_string());
        parts
    } else {
        let mut parts = vec!["ssh".to_string()];
        parts.extend(SSH_OPTS.iter().map(|s| s.to_string()));
        parts.push("-t".to_string());
        parts.push(ssh_target(host));
        parts.push("tmux".into());
        parts.push("-u".into());
        parts.push("attach".into());
        parts.push("-t".into());
        parts.push(session.to_string());
        parts
    }
}

#[derive(Debug, Clone)]
pub struct SessionListResult {
    pub host_name: String,
    pub sessions: Vec<String>,
    pub error: Option<String>,
}

pub fn list_sessions(host: Host) -> SessionListResult {
    let name = host.name.clone();
    let mut cmd = build_list_command(&host);

    let output = match cmd.output() {
        Ok(o) => o,
        Err(e) => {
            return SessionListResult {
                host_name: name,
                sessions: vec![],
                error: Some(format!("{}", e)),
            };
        }
    };

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let sessions: Vec<String> = stdout
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                if line.is_empty() || !line.contains(':') {
                    return None;
                }
                Some(line.split(':').next().unwrap().to_string())
            })
            .collect();
        return SessionListResult {
            host_name: name,
            sessions,
            error: None,
        };
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();

    if stderr.contains("no server running") || stderr.contains("no sessions") {
        return SessionListResult {
            host_name: name,
            sessions: vec![],
            error: None,
        };
    }

    let err_msg = stderr.lines().last().unwrap_or("unknown error").to_string();
    SessionListResult {
        host_name: name,
        sessions: vec![],
        error: Some(err_msg),
    }
}

pub fn shlex_like_split(input: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\'' if !in_double => {
                in_single = !in_single;
            }
            '"' if !in_single => {
                in_double = !in_double;
            }
            '\\' if !in_single => {
                if let Some(next) = chars.peek() {
                    current.push(*next);
                    chars.next();
                }
            }
            ' ' | '\t' if !in_single && !in_double => {
                if !current.is_empty() {
                    parts.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(c),
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}
