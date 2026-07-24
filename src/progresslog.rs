//! Fetches the narrative progress log for the session currently on screen.
//!
//! The log is a human-facing journal (see the `progress-log` skill) kept at
//! the git root of the working directory. Because most sessions run on remote
//! hosts, the file usually lives on the far side of an ssh connection, so we
//! resolve the git root and read the file with a single shell command that
//! runs locally or remotely via the host's connection recipe.

use std::sync::mpsc;
use std::thread;

use crate::config::Host;
use crate::restore::sh_quote;
use crate::ssh::build_shell_command;

/// Only ever transfer the tail of a log — journals can grow without bound and
/// we only render the most recent stretch anyway.
const MAX_BYTES: usize = 256 * 1024;

const MARK: &str = "__TMUXMUX_LOG__";

#[derive(Debug, Clone)]
pub struct LogResult {
    pub host_name: String,
    pub session_name: String,
    /// The working directory this was resolved against (dedupes stale fetches).
    pub cwd: String,
    /// None = no log file present (pane should hide).
    pub content: Option<String>,
    /// Resolved absolute path of the file, for display/debugging.
    pub path: Option<String>,
    /// Modification time (epoch secs) — lets us skip re-render when unchanged.
    pub mtime: Option<i64>,
    /// True when the host answered definitively (file present or absent);
    /// false on a connection failure, so the caller keeps the last content.
    pub resolved: bool,
}

fn fetch_script(cwd: &str, filename: &str) -> String {
    let qcwd = sh_quote(cwd);
    let qfile = sh_quote(filename);
    // git root of the pane's cwd, falling back to the cwd itself; then look
    // for the log there. Emit a marker + mtime, then the (tail of the) file.
    format!(
        "cwd={qcwd}; \
         root=$(git -C \"$cwd\" rev-parse --show-toplevel 2>/dev/null); \
         [ -z \"$root\" ] && root=\"$cwd\"; \
         f=\"$root/\"{qfile}; \
         if [ -f \"$f\" ]; then \
           echo {MARK}$(stat -c %Y \"$f\" 2>/dev/null || stat -f %m \"$f\" 2>/dev/null); \
           echo \"$f\"; \
           tail -c {MAX_BYTES} \"$f\"; \
         else echo {MARK}NONE; fi"
    )
}

pub fn fetch_log(
    host: Host,
    session_name: String,
    cwd: String,
    filename: String,
) -> LogResult {
    let mut result = LogResult {
        host_name: host.name.clone(),
        session_name,
        cwd: cwd.clone(),
        content: None,
        path: None,
        mtime: None,
        resolved: false,
    };
    if cwd.is_empty() {
        return result;
    }
    let argv = build_shell_command(&host, &fetch_script(&cwd, &filename));
    let stdout = match crate::terminal::run_argv(argv) {
        Ok(s) => s,
        Err(e) => {
            log::warn!("log fetch failed for {}: {e}", host.name);
            return result;
        }
    };
    let Some(idx) = stdout.find(MARK) else {
        // No marker => the command didn't complete (connection issue); leave
        // the previous content in place by signalling "no update".
        return result;
    };
    // Marker present => the host answered.
    result.resolved = true;
    let after = &stdout[idx + MARK.len()..];
    let mut lines = after.lines();
    let first = lines.next().unwrap_or("").trim();
    if first == "NONE" {
        // Definitively absent — content stays None so the pane hides.
        return result;
    }
    result.mtime = first.parse::<i64>().ok();
    result.path = lines.next().map(|s| s.to_string());
    let body: String = {
        // Everything after the mtime and path lines.
        let consumed = MARK.len()
            + after
                .match_indices('\n')
                .nth(1)
                .map(|(i, _)| i + 1)
                .unwrap_or(after.len());
        stdout[idx + consumed..].to_string()
    };
    result.content = Some(body);
    result
}

pub fn spawn_fetch(
    host: Host,
    session_name: String,
    cwd: String,
    filename: String,
    tx: mpsc::Sender<LogResult>,
) {
    thread::spawn(move || {
        let _ = tx.send(fetch_log(host, session_name, cwd, filename));
    });
}
