//! sqlite-backed cache of every session ever seen on every host, with the
//! per-pane layout/cwd/command detail needed to recreate them. Sessions that
//! disappear from a host are marked closed (not deleted), which is what makes
//! "restore that thing I closed last week" and "repopulate this VM after the
//! server rebooted" possible.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, params_from_iter, Connection};

use crate::snapshot::{PaneSnap, SessionSnap};

#[derive(Debug, Clone)]
pub struct ClosedSession {
    pub name: String,
    pub closed_at: Option<i64>,
    pub pane_count: i64,
    /// Short name of the most interesting command that was running.
    pub hint: Option<String>,
}

pub struct Db {
    conn: Connection,
}

pub fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// ~/.local/share/tmuxmux/sessions.db (or XDG_DATA_HOME / USERPROFILE).
pub fn default_db_path() -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .ok()
        .or_else(|| {
            std::env::var("HOME")
                .map(|h| Path::new(&h).join(".local/share"))
                .ok()
        })
        .or_else(|| {
            std::env::var("USERPROFILE")
                .map(|h| Path::new(&h).join(".tmuxmux"))
                .ok()
        })
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("tmuxmux").join("sessions.db")
}

impl Db {
    pub fn open(path: &Path) -> rusqlite::Result<Db> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (
                id INTEGER PRIMARY KEY,
                host TEXT NOT NULL,
                name TEXT NOT NULL,
                created_at INTEGER,
                first_seen INTEGER NOT NULL,
                last_seen INTEGER NOT NULL,
                closed_at INTEGER,
                alive INTEGER NOT NULL DEFAULT 1,
                UNIQUE(host, name)
            );
            CREATE TABLE IF NOT EXISTS panes (
                id INTEGER PRIMARY KEY,
                session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                window_index INTEGER NOT NULL,
                window_name TEXT,
                window_layout TEXT,
                pane_index INTEGER NOT NULL,
                command TEXT,
                cmdline TEXT,
                cwd TEXT,
                UNIQUE(session_id, window_index, pane_index)
            );",
        )?;
        Ok(Db { conn })
    }

    /// Record one successful poll of `host`: refresh the live sessions and
    /// mark everything no longer present as closed. Only call on success —
    /// an unreachable host tells us nothing about its sessions.
    pub fn apply_snapshot(
        &mut self,
        host: &str,
        sessions: &[SessionSnap],
        now: i64,
    ) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        for s in sessions {
            tx.execute(
                "INSERT INTO sessions (host, name, created_at, first_seen, last_seen, alive)
                 VALUES (?1, ?2, ?3, ?4, ?4, 1)
                 ON CONFLICT(host, name) DO UPDATE SET
                   last_seen = excluded.last_seen,
                   created_at = excluded.created_at,
                   alive = 1,
                   closed_at = NULL",
                params![host, s.name, s.created_at, now],
            )?;
            let sid: i64 = tx.query_row(
                "SELECT id FROM sessions WHERE host = ?1 AND name = ?2",
                params![host, s.name],
                |r| r.get(0),
            )?;
            tx.execute("DELETE FROM panes WHERE session_id = ?1", params![sid])?;
            for p in &s.panes {
                tx.execute(
                    "INSERT INTO panes (session_id, window_index, window_name, window_layout,
                                        pane_index, command, cmdline, cwd)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                     ON CONFLICT(session_id, window_index, pane_index) DO NOTHING",
                    params![
                        sid,
                        p.window_index,
                        p.window_name,
                        p.window_layout,
                        p.pane_index,
                        p.command,
                        p.cmdline,
                        p.cwd
                    ],
                )?;
            }
        }
        // Anything alive in the cache but absent from this poll just closed.
        let names: Vec<&str> = sessions.iter().map(|s| s.name.as_str()).collect();
        let placeholders = vec!["?"; names.len()].join(",");
        let sql = format!(
            "UPDATE sessions SET alive = 0, closed_at = ?1
             WHERE host = ?2 AND alive = 1 AND name NOT IN ({placeholders})"
        );
        let mut args: Vec<rusqlite::types::Value> = vec![now.into(), host.to_string().into()];
        args.extend(names.iter().map(|n| n.to_string().into()));
        tx.execute(&sql, params_from_iter(args))?;
        tx.commit()
    }

    pub fn closed_sessions(&self, host: &str) -> Vec<ClosedSession> {
        let mut stmt = match self.conn.prepare(
            "SELECT s.name, s.closed_at,
                    (SELECT COUNT(*) FROM panes p WHERE p.session_id = s.id),
                    (SELECT p.command FROM panes p
                      WHERE p.session_id = s.id AND p.cmdline IS NOT NULL
                      ORDER BY p.window_index, p.pane_index LIMIT 1)
             FROM sessions s
             WHERE s.host = ?1 AND s.alive = 0
             ORDER BY s.closed_at DESC, s.name",
        ) {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        stmt.query_map(params![host], |r| {
            Ok(ClosedSession {
                name: r.get(0)?,
                closed_at: r.get(1)?,
                pane_count: r.get(2)?,
                hint: r.get(3)?,
            })
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }

    /// Live-session hints for the sidebar: name → short command.
    pub fn session_detail(&self, host: &str, name: &str) -> Option<SessionSnap> {
        let sid: i64 = self
            .conn
            .query_row(
                "SELECT id FROM sessions WHERE host = ?1 AND name = ?2",
                params![host, name],
                |r| r.get(0),
            )
            .ok()?;
        let mut stmt = self
            .conn
            .prepare(
                "SELECT window_index, window_name, window_layout, pane_index, command, cmdline, cwd
                 FROM panes WHERE session_id = ?1
                 ORDER BY window_index, pane_index",
            )
            .ok()?;
        let panes: Vec<PaneSnap> = stmt
            .query_map(params![sid], |r| {
                Ok(PaneSnap {
                    window_index: r.get(0)?,
                    window_name: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    window_layout: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    pane_index: r.get(3)?,
                    command: r.get::<_, Option<String>>(4)?.unwrap_or_default(),
                    cmdline: r.get(5)?,
                    cwd: r.get::<_, Option<String>>(6)?.unwrap_or_default(),
                })
            })
            .ok()?
            .filter_map(|r| r.ok())
            .collect();
        if panes.is_empty() {
            return None;
        }
        Some(SessionSnap {
            name: name.to_string(),
            created_at: None,
            panes,
        })
    }

    pub fn delete_session(&self, host: &str, name: &str) {
        let _ = self.conn.execute(
            "DELETE FROM sessions WHERE host = ?1 AND name = ?2",
            params![host, name],
        );
    }

    /// Called right after a successful restore so the closed entry doesn't
    /// linger until the next poll confirms it.
    pub fn mark_alive(&self, host: &str, name: &str, now: i64) {
        let _ = self.conn.execute(
            "UPDATE sessions SET alive = 1, closed_at = NULL, last_seen = ?3
             WHERE host = ?1 AND name = ?2",
            params![host, name, now],
        );
    }

    /// Drop closed sessions not seen for `days`.
    pub fn prune(&self, days: i64, now: i64) {
        let cutoff = now - days * 86_400;
        let _ = self.conn.execute(
            "DELETE FROM sessions WHERE alive = 0 AND COALESCE(closed_at, last_seen) < ?1",
            params![cutoff],
        );
    }

    /// Human-readable dump for --dump-cache and the test harness.
    pub fn dump(&self) -> String {
        let mut out = String::new();
        let mut stmt = match self.conn.prepare(
            "SELECT s.host, s.name, s.alive, s.closed_at, s.last_seen, s.id
             FROM sessions s ORDER BY s.host, s.alive DESC, s.name",
        ) {
            Ok(s) => s,
            Err(e) => return format!("error: {e}"),
        };
        let rows: Vec<(String, String, i64, Option<i64>, i64, i64)> = stmt
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })
            .map(|it| it.filter_map(|r| r.ok()).collect())
            .unwrap_or_default();
        for (host, name, alive, closed_at, last_seen, sid) in rows {
            let state = if alive == 1 {
                "live".to_string()
            } else {
                format!("closed@{}", closed_at.unwrap_or(0))
            };
            out.push_str(&format!("{host}/{name} [{state}] last_seen={last_seen}\n"));
            let mut pstmt = match self.conn.prepare(
                "SELECT window_index, window_name, pane_index, command, cmdline, cwd
                 FROM panes WHERE session_id = ?1 ORDER BY window_index, pane_index",
            ) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let panes: Vec<(i64, Option<String>, i64, Option<String>, Option<String>, Option<String>)> =
                pstmt
                    .query_map(params![sid], |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                        ))
                    })
                    .map(|it| it.filter_map(|r| r.ok()).collect())
                    .unwrap_or_default();
            for (wi, wname, pi, command, cmdline, cwd) in panes {
                out.push_str(&format!(
                    "  {wi}:{pi} ({}) cwd={} cmd={} cmdline={}\n",
                    wname.unwrap_or_default(),
                    cwd.unwrap_or_default(),
                    command.unwrap_or_default(),
                    cmdline.unwrap_or_default(),
                ));
            }
        }
        out
    }
}
