//! Rebuild a cached session on its host: windows in original order, splits,
//! layouts, working directories, and the commands that were running typed
//! back into their panes.
//!
//! The generated script captures tmux-assigned pane ids (`-P -F '#{pane_id}'`)
//! into shell variables and targets those, so it is immune to non-default
//! `base-index` / `pane-base-index` settings on the remote server.

use std::collections::BTreeMap;

use crate::snapshot::{PaneSnap, SessionSnap};

pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

pub fn build_restore_script(session: &SessionSnap) -> String {
    // Group panes by window, both ordered.
    let mut windows: BTreeMap<i64, Vec<&PaneSnap>> = BTreeMap::new();
    for p in &session.panes {
        windows.entry(p.window_index).or_default().push(p);
    }
    for panes in windows.values_mut() {
        panes.sort_by_key(|p| p.pane_index);
    }

    let qname = sh_quote(&session.name);
    let mut structure: Vec<String> = Vec::new();
    let mut keys: Vec<String> = Vec::new();
    let mut var = 0usize;

    for (wpos, (_, panes)) in windows.iter().enumerate() {
        let first = panes[0];
        let wname = if first.window_name.is_empty() {
            String::new()
        } else {
            format!(" -n {}", sh_quote(&first.window_name))
        };
        // A cached cwd may have vanished (reboot cleaned /tmp, repo moved);
        // tmux refuses to create a pane with a bad -c and that would abort
        // the whole && chain, so fall back to $HOME at restore time.
        let cwd = |p: &PaneSnap| {
            if p.cwd.is_empty() {
                String::new()
            } else {
                let q = sh_quote(&p.cwd);
                format!(" -c \"$(test -d {q} && echo {q} || echo \"$HOME\")\"")
            }
        };

        // First pane of the window: new-session for the first window,
        // new-window after that. -P -F prints the new pane's id.
        let first_var = var;
        if wpos == 0 {
            structure.push(format!(
                "P{first_var}=$(tmux new-session -d -P -F '#{{pane_id}}' -s {qname}{wname}{} -x 220 -y 50)",
                cwd(first)
            ));
        } else {
            structure.push(format!(
                "P{first_var}=$(tmux new-window -d -P -F '#{{pane_id}}' -t {qname}:{wname}{})",
                cwd(first)
            ));
        }
        var += 1;

        // Remaining panes: split inside the window, layout fixed afterwards.
        for p in &panes[1..] {
            structure.push(format!(
                "P{var}=$(tmux split-window -d -P -F '#{{pane_id}}' -t \"$P{first_var}\"{})",
                cwd(p)
            ));
            var += 1;
        }
        if panes.len() > 1 && !first.window_layout.is_empty() {
            structure.push(format!(
                "tmux select-layout -t \"$P{first_var}\" {}",
                sh_quote(&first.window_layout)
            ));
        }

        // Relaunch commands once the structure exists. Joined with ';' so a
        // failed relaunch doesn't abort the rest.
        for (i, p) in panes.iter().enumerate() {
            if let Some(cmdline) = &p.cmdline {
                let v = first_var + i;
                keys.push(format!(
                    "tmux send-keys -t \"$P{v}\" -l -- {}",
                    sh_quote(cmdline)
                ));
                keys.push(format!("tmux send-keys -t \"$P{v}\" Enter"));
            }
        }
    }

    let mut script = structure.join(" && ");
    if !keys.is_empty() {
        script.push_str("; ");
        script.push_str(&keys.join("; "));
    }
    script
}

#[derive(Debug)]
pub struct RestoreResult {
    pub host_name: String,
    pub session_name: String,
    pub attach: bool,
    pub error: Option<String>,
}
