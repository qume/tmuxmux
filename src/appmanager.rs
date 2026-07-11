//! Pull apps from geocam apps-manager instances and materialise them as
//! auto-managed hosts. Each `[[app_managers]]` entry (domain + user/pass) is
//! logged into (`POST /api/auth/login` → JWT), then `GET /api/apps/connect-list`
//! returns the apps grouped mine/shared/public with a ready-to-run ssh command
//! per app. Discovered apps are reconciled into hosts.toml under a marker:
//! new ones added, existing kept, vanished ones marked closed (not deleted).

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use crate::config::{AppManager, Config, Host};

const MARKER: &str =
    "# >>> tmuxmux app-manager sync — auto-generated below; edits here are overwritten >>>";

#[derive(Debug, Clone)]
pub struct DiscoveredApp {
    pub name: String,
    pub status: String,
    pub ssh_command: String,
    pub owner: String,
    pub host: Option<String>,
    /// "mine" | "shared" | "public"
    pub category: String,
}

#[derive(Debug)]
pub struct FetchResult {
    /// The manager's configured domain (reconciliation key).
    pub domain: String,
    /// Friendly label: config name → API instance name → domain.
    pub instance_name: String,
    pub apps: Vec<DiscoveredApp>,
    /// Set on any failure; the manager's existing hosts are then left as-is.
    pub error: Option<String>,
}

fn base_url(domain: &str) -> String {
    if domain.starts_with("http://") || domain.starts_with("https://") {
        domain.trim_end_matches('/').to_string()
    } else {
        format!("https://{}", domain.trim_end_matches('/'))
    }
}

/// Log in and fetch the connect-list for one instance. Blocking; run in a thread.
pub fn fetch(m: &AppManager) -> FetchResult {
    let mut result = FetchResult {
        domain: m.domain.clone(),
        instance_name: m.name.clone().unwrap_or_else(|| m.domain.clone()),
        apps: Vec::new(),
        error: None,
    };
    let base = base_url(&m.domain);
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout(Duration::from_secs(25))
        .build();

    // 1. login → JWT
    let token = match agent
        .post(&format!("{base}/api/auth/login"))
        .send_json(ureq::json!({ "email": m.username, "password": m.password }))
    {
        Ok(resp) => match resp.into_json::<serde_json::Value>() {
            Ok(v) => v
                .get("access_token")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string()),
            Err(e) => {
                result.error = Some(format!("login: bad response: {e}"));
                return result;
            }
        },
        Err(ureq::Error::Status(code, _)) => {
            result.error = Some(if code == 401 {
                "login failed (check username/password)".into()
            } else {
                format!("login: HTTP {code}")
            });
            return result;
        }
        Err(e) => {
            result.error = Some(format!("login: {e}"));
            return result;
        }
    };
    let Some(token) = token else {
        result.error = Some("login: no access_token in response".into());
        return result;
    };

    // 2. connect-list
    let data = match agent
        .get(&format!("{base}/api/apps/connect-list"))
        .set("Authorization", &format!("Bearer {token}"))
        .call()
    {
        Ok(resp) => match resp.into_json::<serde_json::Value>() {
            Ok(v) => v,
            Err(e) => {
                result.error = Some(format!("connect-list: bad json: {e}"));
                return result;
            }
        },
        Err(ureq::Error::Status(code, _)) => {
            result.error = Some(format!("connect-list: HTTP {code}"));
            return result;
        }
        Err(e) => {
            result.error = Some(format!("connect-list: {e}"));
            return result;
        }
    };

    if let Some(n) = data
        .get("instance")
        .and_then(|i| i.get("name"))
        .and_then(|n| n.as_str())
    {
        if m.name.is_none() && !n.is_empty() {
            result.instance_name = n.to_string();
        }
    }

    for cat in ["mine", "shared", "public"] {
        let Some(arr) = data
            .get("categories")
            .and_then(|c| c.get(cat))
            .and_then(|a| a.as_array())
        else {
            continue;
        };
        for a in arr {
            let name = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let ssh = a.get("ssh_command").and_then(|v| v.as_str()).unwrap_or("");
            if name.is_empty() || ssh.is_empty() {
                continue;
            }
            result.apps.push(DiscoveredApp {
                name: name.to_string(),
                status: a
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
                ssh_command: ssh.to_string(),
                owner: a
                    .get("owner")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                host: a
                    .get("host")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                category: cat.to_string(),
            });
        }
    }
    result
}

/// Merge fetch results with the previously-known auto hosts into the new
/// auto-host list. Hand-written hosts (manager == None) are not touched here.
///
/// - discovered app → upsert (command/status/category refreshed, closed=false)
/// - previously-auto host of a *successfully* polled manager that's gone →
///   kept with closed=true
/// - hosts of a *failed* manager → carried over unchanged (we don't know)
pub fn reconcile(prev_hosts: &[Host], results: &[FetchResult]) -> Vec<Host> {
    let mut out: Vec<Host> = Vec::new();
    for r in results {
        let prev_for: Vec<&Host> = prev_hosts
            .iter()
            .filter(|h| h.manager.as_deref() == Some(r.domain.as_str()))
            .collect();

        if r.error.is_some() {
            out.extend(prev_for.into_iter().cloned());
            continue;
        }

        let discovered: HashSet<&str> = r.apps.iter().map(|a| a.name.as_str()).collect();
        for a in &r.apps {
            out.push(Host {
                name: a.name.clone(),
                username: None,
                command: Some(a.ssh_command.clone()),
                local: false,
                env: None,
                manager: Some(r.domain.clone()),
                category: Some(a.category.clone()),
                status: Some(a.status.clone()),
                closed: false,
            });
        }
        for h in prev_for {
            if !discovered.contains(h.name.as_str()) {
                let mut c = h.clone();
                c.closed = true;
                out.push(c);
            }
        }
    }
    out
}

fn toml_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

fn render_host(h: &Host) -> String {
    let mut s = String::from("[[hosts]]\n");
    s += &format!("name = {}\n", toml_str(&h.name));
    if let Some(m) = &h.manager {
        s += &format!("manager = {}\n", toml_str(m));
    }
    if let Some(c) = &h.category {
        s += &format!("category = {}\n", toml_str(c));
    }
    if let Some(st) = &h.status {
        s += &format!("status = {}\n", toml_str(st));
    }
    s += &format!("closed = {}\n", h.closed);
    if let Some(cmd) = &h.command {
        s += &format!("command = {}\n", toml_str(cmd));
    }
    s.push('\n');
    s
}

/// Rewrite hosts.toml: keep everything above the marker verbatim (hand-written
/// hosts, app_managers, comments), regenerate the auto section below it.
pub fn write_back(path: &Path, auto_hosts: &[Host]) -> std::io::Result<()> {
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    let prefix = match raw.find(MARKER) {
        Some(i) => raw[..i].trim_end(),
        None => raw.trim_end(),
    };
    let mut out = String::new();
    out.push_str(prefix);
    out.push_str("\n\n");
    out.push_str(MARKER);
    out.push('\n');

    // Group by manager then category for a readable, diff-stable file.
    let managers: Vec<&String> = {
        let mut seen: Vec<&String> = Vec::new();
        for h in auto_hosts {
            if let Some(m) = &h.manager {
                if !seen.contains(&m) {
                    seen.push(m);
                }
            }
        }
        seen
    };
    for m in managers {
        out.push_str(&format!("\n# --- {m} ---\n"));
        for cat in ["mine", "shared", "public"] {
            for h in auto_hosts
                .iter()
                .filter(|h| h.manager.as_deref() == Some(m.as_str()))
                .filter(|h| h.category.as_deref() == Some(cat))
            {
                out.push_str(&render_host(h));
            }
        }
    }
    std::fs::write(path, out)
}

pub struct SyncSummary {
    pub lines: Vec<String>,
    pub auto_hosts: Vec<Host>,
}

/// Blocking full sync: fetch every manager, reconcile, write the file. Used by
/// the `--sync-apps` CLI and reusable for a background refresh.
pub fn sync_blocking(path: &Path, cfg: &Config) -> SyncSummary {
    let results: Vec<FetchResult> = cfg.app_managers.iter().map(fetch).collect();
    let auto = reconcile(&cfg.hosts, &results);
    let mut lines = Vec::new();
    for r in &results {
        match &r.error {
            Some(e) => lines.push(format!("{}: ERROR {}", r.domain, e)),
            None => {
                let (mut mine, mut shared, mut public) = (0, 0, 0);
                for a in &r.apps {
                    match a.category.as_str() {
                        "mine" => mine += 1,
                        "shared" => shared += 1,
                        _ => public += 1,
                    }
                }
                lines.push(format!(
                    "{}: {} apps (mine {}, shared {}, public {})",
                    r.instance_name,
                    r.apps.len(),
                    mine,
                    shared,
                    public
                ));
            }
        }
    }
    let closed = auto.iter().filter(|h| h.closed).count();
    if closed > 0 {
        lines.push(format!("{closed} app(s) no longer visible — marked closed"));
    }
    if let Err(e) = write_back(path, &auto) {
        lines.push(format!("write failed: {e}"));
    }
    SyncSummary {
        lines,
        auto_hosts: auto,
    }
}
