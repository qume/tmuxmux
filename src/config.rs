use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct Host {
    pub name: String,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub local: bool,
    #[serde(default)]
    pub env: Option<HashMap<String, String>>,

    // --- fields for hosts auto-managed from an app-manager instance ---
    /// Domain of the app-manager this host came from (None = hand-written).
    #[serde(default)]
    pub manager: Option<String>,
    /// Visibility bucket on that instance: "mine" | "shared" | "public".
    #[serde(default)]
    pub category: Option<String>,
    /// Last-known app lifecycle status (running, stopped, …).
    #[serde(default)]
    pub status: Option<String>,
    /// True when the app is no longer returned by its manager (destroyed or
    /// unshared) — kept in the file, shown dimmed, rather than deleted.
    #[serde(default)]
    pub closed: bool,
}

/// An app-manager instance to pull apps from (geocam apps-manager). Each one
/// expands, on launch, into many auto-managed [[hosts]] grouped by category.
#[derive(Debug, Clone, Deserialize)]
pub struct AppManager {
    /// API base, e.g. "apps-manager-app.geocam.io" (https:// optional).
    pub domain: String,
    pub username: String,
    pub password: String,
    /// Optional friendly label; defaults to the instance name from the API,
    /// falling back to the domain.
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct CacheConfig {
    /// Seconds between background snapshots of every host's sessions.
    /// 0 disables periodic polling (startup + F5 still snapshot).
    #[serde(default)]
    pub interval_secs: Option<u64>,
    /// Closed sessions are forgotten after this many days.
    #[serde(default)]
    pub retention_days: Option<i64>,
    /// Override the sqlite file location.
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct LogConfig {
    /// Narrative-log filename looked for at the git root of the active
    /// session's working directory. Default "PROGRESS.md".
    #[serde(default)]
    pub filename: Option<String>,
    /// Set false to disable the progress pane entirely.
    #[serde(default)]
    pub enabled: Option<bool>,
}

impl LogConfig {
    pub fn filename(&self) -> String {
        self.filename
            .clone()
            .unwrap_or_else(|| "PROGRESS.md".to_string())
    }
    pub fn enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub hosts: Vec<Host>,
    #[serde(default)]
    pub env: Option<HashMap<String, String>>,
    #[serde(default)]
    pub cache: Option<CacheConfig>,
    #[serde(default)]
    pub log: Option<LogConfig>,
    #[serde(default)]
    pub app_managers: Vec<AppManager>,
}

pub fn find_config_path(explicit: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        if p.exists() {
            return Some(p);
        }
        eprintln!("Config file not found: {}", p.display());
        return None;
    }
    // Look next to the executable first so a packaged binary finds its config,
    // then the cwd, then the user config dir.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("hosts.toml");
            if p.exists() {
                return Some(p);
            }
        }
    }
    let cwd = PathBuf::from("hosts.toml");
    if cwd.exists() {
        return Some(cwd);
    }
    if let Ok(home) = std::env::var("HOME") {
        let p = Path::new(&home).join(".config/tmuxmux/hosts.toml");
        if p.exists() {
            return Some(p);
        }
    }
    if let Ok(home) = std::env::var("USERPROFILE") {
        let p = Path::new(&home).join(".config/tmuxmux/hosts.toml");
        if p.exists() {
            return Some(p);
        }
    }
    None
}

pub fn load_config(path: &Path) -> Result<Config, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Cannot read {}: {}", path.display(), e))?;
    toml::from_str(&content).map_err(|e| format!("Parse error in {}: {}", path.display(), e))
}
