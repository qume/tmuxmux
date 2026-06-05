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
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub hosts: Vec<Host>,
    #[serde(default)]
    pub env: Option<HashMap<String, String>>,
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
