//! Seamless SSH key setup. If the user already has a key we use it; if not we
//! generate an ed25519 one via `ssh-keygen` (which ships with the same OpenSSH
//! that provides `ssh`, on Linux/Mac/Windows). The public key is then handed to
//! each app-manager so containers can authorize it — letting connections drop
//! the password entirely.

use std::path::PathBuf;
use std::process::Command;

fn ssh_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|h| PathBuf::from(h).join(".ssh"))
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| {
            Command::new("hostname")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "tmuxmux".into())
}

fn read_pub(path: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Ensure a usable SSH public key exists, returning `(public_key_line,
/// newly_generated)`. Prefers an existing key; only generates when the user
/// has none. Returns None if we can't locate a home dir or `ssh-keygen` fails
/// (callers then just fall back to password auth).
pub fn ensure_public_key() -> Option<(String, bool)> {
    let dir = ssh_dir()?;

    // Use an existing public key if there is one (preference order).
    for name in ["id_ed25519.pub", "id_ecdsa.pub", "id_rsa.pub"] {
        if let Some(pk) = read_pub(&dir.join(name)) {
            return Some((pk, false));
        }
    }

    let priv_path = dir.join("id_ed25519");
    // A private key with no .pub beside it: derive the public key rather than
    // regenerate (which would prompt to overwrite and hang).
    if priv_path.exists() {
        if let Ok(out) = Command::new("ssh-keygen")
            .arg("-y")
            .arg("-f")
            .arg(&priv_path)
            .output()
        {
            if out.status.success() {
                if let Ok(s) = String::from_utf8(out.stdout) {
                    let s = s.trim().to_string();
                    if !s.is_empty() {
                        return Some((s, false));
                    }
                }
            }
        }
        return None; // can't derive (passphrase-protected?) — leave it alone
    }

    // No key at all — generate an ed25519 keypair, no passphrase.
    std::fs::create_dir_all(&dir).ok()?;
    let comment = format!("tmuxmux@{}", hostname());
    let status = Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-q", "-C", &comment, "-f"])
        .arg(&priv_path)
        .status()
        .ok()?;
    if !status.success() {
        log::error!("ssh-keygen failed");
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&priv_path, std::fs::Permissions::from_mode(0o600));
    }
    log::info!("generated a new ed25519 SSH key at {}", priv_path.display());
    read_pub(&dir.join("id_ed25519.pub")).map(|pk| (pk, true))
}
