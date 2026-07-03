use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use egui::{Color32, FontId, Pos2, Rect, Sense, Ui, Vec2};

use crate::colors::{convert_bg, convert_fg, DEFAULT_BG, DEFAULT_FG, SELECTION_BG};
use crate::config::{Config, Host};
use crate::db::{self, Db};
use crate::input::key_event_to_bytes;
use crate::progresslog::{spawn_fetch, LogResult};
use crate::restore::{build_restore_script, RestoreResult};
use crate::snapshot::{spawn_snapshots, PaneSnap, SessionSnap, SnapshotResult};
use crate::ssh::{build_attach_command, build_new_session_command, build_shell_command};
use crate::terminal::TerminalPane;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Focus {
    Tree,
    Terminal,
}

/// A live session as shown in the sidebar.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub name: String,
    /// Short name of the command running in it ("claude", "vim"), if any.
    pub hint: Option<String>,
    /// Working directory of the focused pane — where we look for the log.
    pub cwd: Option<String>,
}

pub struct HostEntry {
    pub host: Host,
    pub sessions: Vec<SessionInfo>,
    pub closed: Vec<db::ClosedSession>,
    pub closed_expanded: bool,
    pub loaded: bool,
    pub error: Option<String>,
    pub expanded: bool,
}

/// Flattened sidebar row, rebuilt every frame from `hosts`.
#[derive(Clone)]
enum Row {
    Host(usize),
    Session(usize, usize),
    /// The "+ new" row at the end of a host's session list.
    NewSession(usize),
    /// "⟲ closed (N)" toggle for the cached-session group.
    ClosedToggle(usize),
    /// One restorable cached session.
    Closed(usize, usize),
    /// "⟲ restore all" bulk action.
    RestoreAll(usize),
}

/// Whichever dialog is open.
pub enum AppModal {
    NewSession {
        host_idx: usize,
        name: String,
        just_opened: bool,
    },
    Restore {
        host_idx: usize,
        name: String,
        detail: SessionSnap,
    },
    RestoreAll {
        host_idx: usize,
        names: Vec<String>,
    },
}

/// Pool of conflict-free default session names.
const NAME_POOL: &[&str] = &[
    "red", "orange", "yellow", "green", "blue", "indigo", "violet", "cyan",
    "magenta", "teal", "coral", "amber", "olive", "maroon", "navy", "plum",
    "salmon", "sienna", "khaki", "crimson", "turquoise", "lavender", "mint",
    "jade", "ruby", "slate", "ochre", "pearl", "cobalt", "saffron",
];

/// Selection endpoints in cell coordinates (row, col), in stream order.
#[derive(Clone, Copy, Debug)]
pub struct Selection {
    pub anchor: (usize, usize),
    pub head: (usize, usize),
}

impl Selection {
    fn ordered(&self) -> ((usize, usize), (usize, usize)) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
    fn contains(&self, row: usize, col: usize) -> bool {
        let (start, end) = self.ordered();
        (row, col) >= start && (row, col) <= end
    }
}

struct TermLayout {
    origin: Pos2,
    glyph_w: f32,
    line_h: f32,
    cols: usize,
    rows: usize,
}

pub struct App {
    pub config: Config,
    pub hosts: Vec<HostEntry>,
    pub focus: Focus,
    pub show_sidebar: bool,

    panes: HashMap<String, TerminalPane>,
    active_key: Option<String>,

    snapshot_tx: mpsc::Sender<SnapshotResult>,
    snapshot_rx: mpsc::Receiver<SnapshotResult>,
    restore_tx: mpsc::Sender<RestoreResult>,
    restore_rx: mpsc::Receiver<RestoreResult>,
    /// Hosts with a snapshot currently in flight — never double-poll a slow host.
    in_flight: HashSet<String>,
    db: Option<Db>,
    poll_interval: Duration,
    last_poll: Instant,

    // Narrative progress log (right-hand pane).
    log_tx: mpsc::Sender<LogResult>,
    log_rx: mpsc::Receiver<LogResult>,
    log_enabled: bool,
    log_filename: String,
    log_content: Option<String>,
    log_path: Option<String>,
    log_mtime: Option<i64>,
    /// (host, session) the current log content belongs to.
    log_for: Option<(String, String)>,
    log_in_flight: bool,
    log_hidden: bool,
    last_log_fetch: Instant,

    tree_cursor: usize,

    pub selection: Option<Selection>,
    selecting: bool,
    clipboard: Option<arboard::Clipboard>,
    pub modal: Option<AppModal>,

    layout: Option<TermLayout>,
    status: String,
    font_size: f32,
}

impl App {
    pub fn new(
        config: Config,
        db_path_override: Option<PathBuf>,
        interval_override: Option<u64>,
    ) -> Self {
        let cache_cfg = config.cache.clone().unwrap_or_default();
        let interval_secs = interval_override
            .or(cache_cfg.interval_secs)
            .unwrap_or(60);
        let retention_days = cache_cfg.retention_days.unwrap_or(30);
        let db_path = db_path_override
            .or_else(|| cache_cfg.path.as_ref().map(PathBuf::from))
            .unwrap_or_else(db::default_db_path);

        let db = match Db::open(&db_path) {
            Ok(d) => {
                log::info!("session cache: {}", db_path.display());
                d.prune(retention_days, db::now_epoch());
                Some(d)
            }
            Err(e) => {
                log::error!("cannot open session cache {}: {e}", db_path.display());
                None
            }
        };

        let hosts: Vec<HostEntry> = config
            .hosts
            .iter()
            .map(|h| HostEntry {
                host: h.clone(),
                sessions: Vec::new(),
                closed: db
                    .as_ref()
                    .map(|d| d.closed_sessions(&h.name))
                    .unwrap_or_default(),
                closed_expanded: false,
                loaded: false,
                error: None,
                expanded: true,
            })
            .collect();

        let clipboard = match arboard::Clipboard::new() {
            Ok(c) => Some(c),
            Err(e) => {
                log::warn!("clipboard unavailable: {e}");
                None
            }
        };

        let (snapshot_tx, snapshot_rx) = mpsc::channel();
        let (restore_tx, restore_rx) = mpsc::channel();
        let (log_tx, log_rx) = mpsc::channel();

        let log_cfg = config.log.clone().unwrap_or_default();

        let mut app = App {
            config,
            hosts,
            focus: Focus::Tree,
            show_sidebar: true,
            panes: HashMap::new(),
            active_key: None,
            snapshot_tx,
            snapshot_rx,
            restore_tx,
            restore_rx,
            in_flight: HashSet::new(),
            db,
            poll_interval: Duration::from_secs(interval_secs),
            last_poll: Instant::now(),
            log_tx,
            log_rx,
            log_enabled: log_cfg.enabled(),
            log_filename: log_cfg.filename(),
            log_content: None,
            log_path: None,
            log_mtime: None,
            log_for: None,
            log_in_flight: false,
            log_hidden: false,
            last_log_fetch: Instant::now(),
            tree_cursor: 0,
            selection: None,
            selecting: false,
            clipboard,
            modal: None,
            layout: None,
            status: "loading sessions...".into(),
            font_size: 14.0,
        };
        app.poll_now();
        app
    }

    // ---------- snapshot polling ----------

    /// Snapshot every host that isn't already being polled.
    pub fn poll_now(&mut self) {
        let due: Vec<Host> = self
            .config
            .hosts
            .iter()
            .filter(|h| !self.in_flight.contains(&h.name))
            .cloned()
            .collect();
        for h in &due {
            self.in_flight.insert(h.name.clone());
        }
        self.last_poll = Instant::now();
        spawn_snapshots(due, self.snapshot_tx.clone());
    }

    /// Called every frame: kick scheduled polls, absorb results.
    pub fn check_results(&mut self) {
        if !self.poll_interval.is_zero() && self.last_poll.elapsed() >= self.poll_interval {
            self.poll_now();
        }

        while let Ok(result) = self.snapshot_rx.try_recv() {
            self.apply_snapshot_result(result);
        }

        while let Ok(result) = self.restore_rx.try_recv() {
            self.apply_restore_result(result);
        }

        while let Ok(result) = self.log_rx.try_recv() {
            self.apply_log_result(result);
        }
        self.maybe_fetch_log();
    }

    // ---------- progress log ----------

    /// Fetch the active session's log on a timer (and whenever the active
    /// session changes). Cheap no-op when there's no active session.
    fn maybe_fetch_log(&mut self) {
        if !self.log_enabled || self.log_in_flight {
            return;
        }
        let Some(key) = self.active_key.clone() else {
            return;
        };
        let Some((host_name, session_name)) = key.split_once('/') else {
            return;
        };
        // Only re-poll every few seconds once we already have this session's log.
        let same = self
            .log_for
            .as_ref()
            .map(|(h, s)| h == host_name && s == session_name)
            .unwrap_or(false);
        if same && self.last_log_fetch.elapsed() < Duration::from_secs(4) {
            return;
        }
        let Some(entry) = self.hosts.iter().find(|e| e.host.name == host_name) else {
            return;
        };
        let cwd = entry
            .sessions
            .iter()
            .find(|s| s.name == session_name)
            .and_then(|s| s.cwd.clone());
        let Some(cwd) = cwd else {
            return; // no cwd known yet; wait for the next snapshot
        };
        self.log_in_flight = true;
        self.last_log_fetch = Instant::now();
        spawn_fetch(
            entry.host.clone(),
            session_name.to_string(),
            cwd,
            self.log_filename.clone(),
            self.log_tx.clone(),
        );
    }

    fn apply_log_result(&mut self, result: LogResult) {
        self.log_in_flight = false;
        // Ignore results for a session we've since navigated away from.
        let matches_active = self
            .active_key
            .as_deref()
            .and_then(|k| k.split_once('/'))
            .map(|(h, s)| h == result.host_name && s == result.session_name)
            .unwrap_or(false);
        if !matches_active {
            return;
        }
        if !result.resolved {
            return; // connection blip — keep whatever we were showing
        }
        self.log_for = Some((result.host_name, result.session_name));
        self.log_content = result.content;
        self.log_path = result.path;
        self.log_mtime = result.mtime;
    }

    /// The progress pane is shown only when we have non-empty content for the
    /// active session and the user hasn't hidden it.
    pub fn has_log(&self) -> bool {
        self.log_enabled
            && !self.log_hidden
            && self
                .log_content
                .as_ref()
                .map(|c| !c.trim().is_empty())
                .unwrap_or(false)
    }

    fn apply_snapshot_result(&mut self, result: SnapshotResult) {
        self.in_flight.remove(&result.host_name);
        let now = db::now_epoch();
        let Some(entry) = self
            .hosts
            .iter_mut()
            .find(|e| e.host.name == result.host_name)
        else {
            return;
        };
        entry.loaded = true;
        if let Some(err) = result.error {
            // Unreachable host: keep showing the last known sessions.
            entry.error = Some(err);
        } else {
            entry.error = None;
            entry.sessions = result
                .sessions
                .iter()
                .map(|s| SessionInfo {
                    name: s.name.clone(),
                    hint: session_hint(s),
                    cwd: active_cwd(s),
                })
                .collect();
            if let Some(db) = self.db.as_mut() {
                if let Err(e) = db.apply_snapshot(&result.host_name, &result.sessions, now) {
                    log::error!("cache write failed for {}: {e}", result.host_name);
                }
                entry.closed = db.closed_sessions(&result.host_name);
            }
        }
        if self.hosts.iter().all(|e| e.loaded) && self.status == "loading sessions..." {
            self.status = "ready".into();
        }
    }

    fn apply_restore_result(&mut self, result: RestoreResult) {
        if let Some(err) = result.error {
            self.status = format!("restore {} failed: {}", result.session_name, err);
            return;
        }
        let now = db::now_epoch();
        if let Some(db) = self.db.as_ref() {
            db.mark_alive(&result.host_name, &result.session_name, now);
        }
        if let Some(entry) = self
            .hosts
            .iter_mut()
            .find(|e| e.host.name == result.host_name)
        {
            if !entry.sessions.iter().any(|s| s.name == result.session_name) {
                entry.sessions.push(SessionInfo {
                    name: result.session_name.clone(),
                    hint: None,
                    cwd: None,
                });
            }
            entry.closed.retain(|c| c.name != result.session_name);
        }
        self.status = format!("restored {}/{}", result.host_name, result.session_name);
        if result.attach {
            let (h, s) = (result.host_name, result.session_name);
            self.activate_session(&h, &s);
        }
    }

    pub fn refresh_all(&mut self) {
        self.status = "refreshing...".into();
        self.poll_now();
    }

    pub fn host_index(&self, name: &str) -> Option<usize> {
        self.hosts.iter().position(|e| e.host.name == name)
    }

    fn adjust_font(&mut self, delta: f32) {
        self.set_font(self.font_size + delta);
    }

    /// Test hook (mirrors the Ctrl+=/Ctrl+- keybinding path).
    pub fn set_font_for_test(&mut self, size: f32) {
        self.set_font(size);
    }

    pub fn font_size(&self) -> f32 {
        self.font_size
    }

    fn set_font(&mut self, size: f32) {
        let clamped = size.clamp(7.0, 40.0);
        if clamped != self.font_size {
            self.font_size = clamped;
            self.status = format!("font size {}", clamped as i32);
        }
    }

    // ---------- pane management ----------

    pub fn read_all_panes(&mut self) -> bool {
        // Pump every pane, not just the active one, so background sessions
        // never stall on a full PTY buffer.
        let active = self.active_key.clone();
        let mut active_output = false;
        for (key, pane) in self.panes.iter_mut() {
            let any = pane.try_read();
            if any && Some(key.as_str()) == active.as_deref() {
                active_output = true;
            }
        }
        active_output
    }

    pub fn activate_session(&mut self, host_name: &str, session_name: &str) {
        let host = match self.config.hosts.iter().find(|h| h.name == host_name) {
            Some(h) => h.clone(),
            None => {
                self.status = format!("unknown host: {host_name}");
                return;
            }
        };
        let cmd = build_attach_command(&host, session_name);
        self.open_pane(&host, session_name, cmd);
    }

    fn open_pane(&mut self, host: &Host, session_name: &str, cmd: Vec<String>) {
        let key = format!("{}/{}", host.name, session_name);
        if let Some(pane) = self.panes.get(&key) {
            if !pane.alive {
                self.panes.remove(&key);
            }
        }
        if !self.panes.contains_key(&key) {
            let env = merge_env(&self.config.env, &host.env);
            log::info!("spawning: {:?}", cmd);
            let (cols, rows) = self
                .layout
                .as_ref()
                .map(|l| (l.cols, l.rows))
                .unwrap_or((80, 24));
            self.panes
                .insert(key.clone(), TerminalPane::new(cmd, cols.max(2), rows.max(2), env));
        }
        // New session on screen: drop the old log so the pane doesn't flash
        // stale content, and force an immediate refetch.
        if self.log_for.as_ref() != Some(&(host.name.clone(), session_name.to_string())) {
            self.log_content = None;
            self.log_path = None;
            self.log_mtime = None;
            self.log_for = None;
            self.last_log_fetch = Instant::now() - Duration::from_secs(60);
        }
        self.active_key = Some(key.clone());
        self.focus = Focus::Terminal;
        self.selection = None;
        self.selecting = false;
        self.status = key;
    }

    // ---------- modals ----------

    /// First name from the pool not used by a live *or* cached session on
    /// this host (a clash with a cached one would overwrite its history);
    /// falls back to numbered variants.
    fn default_session_name(&self, host_idx: usize) -> String {
        let entry = &self.hosts[host_idx];
        let taken = |candidate: &str| {
            entry.sessions.iter().any(|s| s.name == candidate)
                || entry.closed.iter().any(|c| c.name == candidate)
        };
        for name in NAME_POOL {
            if !taken(name) {
                return (*name).to_string();
            }
        }
        for n in 2.. {
            for name in NAME_POOL {
                let candidate = format!("{name}-{n}");
                if !taken(&candidate) {
                    return candidate;
                }
            }
        }
        unreachable!()
    }

    pub fn open_new_session_modal(&mut self, host_idx: usize) {
        let name = self.default_session_name(host_idx);
        self.modal = Some(AppModal::NewSession {
            host_idx,
            name,
            just_opened: true,
        });
    }

    pub fn open_new_session_modal_by_host(&mut self, host_name: &str) {
        if let Some(idx) = self.hosts.iter().position(|e| e.host.name == host_name) {
            self.open_new_session_modal(idx);
        } else {
            self.status = format!("unknown host: {host_name}");
        }
    }

    pub fn open_restore_modal(&mut self, host_idx: usize, name: &str) {
        let host_name = self.hosts[host_idx].host.name.clone();
        let detail = self
            .db
            .as_ref()
            .and_then(|d| d.session_detail(&host_name, name));
        match detail {
            Some(detail) => {
                self.modal = Some(AppModal::Restore {
                    host_idx,
                    name: name.to_string(),
                    detail,
                });
            }
            None => self.status = format!("no cached detail for {host_name}/{name}"),
        }
    }

    pub fn open_restore_all_modal(&mut self, host_idx: usize) {
        let names: Vec<String> = self.hosts[host_idx]
            .closed
            .iter()
            .map(|c| c.name.clone())
            .collect();
        if names.is_empty() {
            return;
        }
        self.modal = Some(AppModal::RestoreAll { host_idx, names });
    }

    /// Confirm whichever dialog is open.
    pub fn accept_modal(&mut self) {
        let Some(modal) = self.modal.take() else {
            return;
        };
        match modal {
            AppModal::NewSession {
                host_idx, name, ..
            } => {
                // tmux session names may not contain ':' or '.'.
                let name: String = name
                    .trim()
                    .replace([':', '.'], "-")
                    .replace(char::is_whitespace, "-");
                if name.is_empty() {
                    self.status = "session name empty — cancelled".into();
                    return;
                }
                let host = self.config.hosts[host_idx].clone();
                let cmd = build_new_session_command(&host, &name);
                self.open_pane(&host, &name, cmd);
                // Show it in the sidebar immediately; the next poll confirms it.
                let entry = &mut self.hosts[host_idx];
                if !entry.sessions.iter().any(|s| s.name == name) {
                    entry.sessions.push(SessionInfo {
                        name,
                        hint: None,
                        cwd: None,
                    });
                }
            }
            AppModal::Restore {
                host_idx, name, ..
            } => {
                self.restore_sessions(host_idx, vec![name], true);
            }
            AppModal::RestoreAll { host_idx, names } => {
                self.restore_sessions(host_idx, names, false);
            }
        }
    }

    /// Recreate cached sessions on their host in a background thread.
    /// Attaches to the (single) session when `attach` is set.
    pub fn restore_sessions(&mut self, host_idx: usize, names: Vec<String>, attach: bool) {
        let host = self.config.hosts[host_idx].clone();
        let Some(db) = self.db.as_ref() else {
            self.status = "session cache unavailable".into();
            return;
        };
        let mut jobs: Vec<(String, Vec<String>)> = Vec::new();
        for name in names {
            match db.session_detail(&host.name, &name) {
                Some(detail) => {
                    let script = build_restore_script(&detail);
                    jobs.push((name, build_shell_command(&host, &script)));
                }
                None => log::warn!("no cached detail for {}/{name}", host.name),
            }
        }
        if jobs.is_empty() {
            self.status = "nothing to restore".into();
            return;
        }
        self.status = format!("restoring {} session(s) on {}...", jobs.len(), host.name);
        let tx = self.restore_tx.clone();
        let host_name = host.name.clone();
        thread::spawn(move || {
            for (name, argv) in jobs {
                let mut cmd = std::process::Command::new(&argv[0]);
                cmd.args(&argv[1..]);
                let error = match cmd.output() {
                    Ok(out) if out.status.success() => None,
                    Ok(out) => {
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        let stdout = String::from_utf8_lossy(&out.stdout);
                        Some(
                            stderr
                                .lines()
                                .chain(stdout.lines())
                                .last()
                                .unwrap_or("failed")
                                .to_string(),
                        )
                    }
                    Err(e) => Some(e.to_string()),
                };
                let _ = tx.send(RestoreResult {
                    host_name: host_name.clone(),
                    session_name: name,
                    attach,
                    error,
                });
            }
        });
    }

    pub fn delete_cached_session(&mut self, host_idx: usize, name: &str) {
        let host_name = self.hosts[host_idx].host.name.clone();
        if let Some(db) = self.db.as_ref() {
            db.delete_session(&host_name, name);
        }
        self.hosts[host_idx].closed.retain(|c| c.name != name);
        self.status = format!("forgot {host_name}/{name}");
    }

    pub fn active_pane_mut(&mut self) -> Option<&mut TerminalPane> {
        let key = self.active_key.clone()?;
        self.panes.get_mut(&key)
    }

    fn write_pty(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        if let Some(pane) = self.active_pane_mut() {
            pane.write_input(data);
        }
    }

    pub fn cycle_session(&mut self, dir: i32) {
        let ordered: Vec<(String, String)> = self
            .hosts
            .iter()
            .flat_map(|e| {
                e.sessions
                    .iter()
                    .map(|s| (e.host.name.clone(), s.name.clone()))
                    .collect::<Vec<_>>()
            })
            .collect();
        if ordered.is_empty() {
            return;
        }
        let current = self.active_key.clone().unwrap_or_default();
        let pos = ordered
            .iter()
            .position(|(h, s)| format!("{}/{}", h, s) == current);
        let idx = match pos {
            Some(p) => (p as i32 + dir).rem_euclid(ordered.len() as i32) as usize,
            None => 0,
        };
        let (h, s) = ordered[idx].clone();
        self.activate_session(&h, &s);
    }

    // ---------- clipboard / selection ----------

    pub fn selection_text(&self) -> Option<String> {
        let sel = self.selection?;
        let key = self.active_key.as_ref()?;
        let pane = self.panes.get(key)?;
        let screen = pane.parser.screen();
        let (rows, cols) = screen.size();
        let ((sr, sc), (er, ec)) = sel.ordered();
        let mut lines: Vec<String> = Vec::new();
        for row in sr..=er.min(rows as usize - 1) {
            let c0 = if row == sr { sc } else { 0 };
            let c1 = if row == er { ec } else { cols as usize - 1 };
            let mut line = String::new();
            for col in c0..=c1.min(cols as usize - 1) {
                if let Some(cell) = screen.cell(row as u16, col as u16) {
                    if cell.is_wide_continuation() {
                        continue;
                    }
                    let s = cell.contents();
                    if s.is_empty() {
                        line.push(' ');
                    } else {
                        line.push_str(s);
                    }
                }
            }
            lines.push(line.trim_end().to_string());
        }
        Some(lines.join("\n"))
    }

    pub fn copy_selection(&mut self) {
        let text = match self.selection_text() {
            Some(t) if !t.is_empty() => t,
            _ => {
                self.status = "nothing selected".into();
                return;
            }
        };
        let n = text.chars().count();
        if let Some(cb) = self.clipboard.as_mut() {
            match cb.set_text(text) {
                Ok(()) => self.status = format!("copied {n} chars"),
                Err(e) => self.status = format!("copy failed: {e}"),
            }
        } else {
            self.status = "clipboard unavailable".into();
        }
    }

    pub fn paste_clipboard(&mut self) {
        let text = match self.clipboard.as_mut().and_then(|c| c.get_text().ok()) {
            Some(t) if !t.is_empty() => t,
            _ => {
                self.status = "clipboard empty".into();
                return;
            }
        };
        self.paste_text(&text);
    }

    pub fn paste_text(&mut self, text: &str) {
        // Terminals send CR for newline.
        let normalized = text.replace("\r\n", "\r").replace('\n', "\r");
        let bracketed = self
            .active_pane_mut()
            .map(|p| p.parser.screen().bracketed_paste())
            .unwrap_or(false);
        let mut data = Vec::new();
        if bracketed {
            data.extend_from_slice(b"\x1b[200~");
            data.extend_from_slice(normalized.as_bytes());
            data.extend_from_slice(b"\x1b[201~");
        } else {
            data.extend_from_slice(normalized.as_bytes());
        }
        self.write_pty(&data);
    }

    pub fn set_selection_cells(&mut self, r1: usize, c1: usize, r2: usize, c2: usize) {
        self.selection = Some(Selection {
            anchor: (r1, c1),
            head: (r2, c2),
        });
    }

    pub fn send_keys(&mut self, data: &[u8]) {
        self.write_pty(data);
    }

    // ---------- keyboard ----------

    /// Process raw egui events. Returns true if the app should quit.
    pub fn handle_events(&mut self, ctx: &egui::Context) -> bool {
        // While the new-session dialog is open, leave all input to egui so
        // its TextEdit receives keystrokes; the dialog handles Enter/Esc.
        if self.modal.is_some() {
            return false;
        }
        let (events, modifiers) = ctx.input(|i| (i.events.clone(), i.modifiers));
        let mut quit = false;

        for event in &events {
            match event {
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers: mods,
                    ..
                } => {
                    // Global shortcuts (never forwarded).
                    if mods.ctrl && mods.shift && *key == egui::Key::Q {
                        quit = true;
                        continue;
                    }
                    if *key == egui::Key::F2 {
                        self.show_sidebar = !self.show_sidebar;
                        continue;
                    }
                    if *key == egui::Key::F5 {
                        self.refresh_all();
                        continue;
                    }
                    if mods.ctrl && !mods.shift && *key == egui::Key::CloseBracket {
                        self.cycle_session(1);
                        continue;
                    }
                    if mods.ctrl && !mods.shift && *key == egui::Key::Backslash {
                        self.cycle_session(-1);
                        continue;
                    }
                    if mods.ctrl && mods.shift && *key == egui::Key::E {
                        self.focus = Focus::Tree;
                        self.show_sidebar = true;
                        continue;
                    }
                    if mods.ctrl && mods.shift && *key == egui::Key::L {
                        self.log_hidden = !self.log_hidden;
                        self.status = if self.log_hidden {
                            "progress log hidden".into()
                        } else {
                            "progress log shown".into()
                        };
                        continue;
                    }
                    // Font size: Ctrl+= / Ctrl++ bigger, Ctrl+- smaller,
                    // Ctrl+0 reset. (Shift makes '=' into '+', so accept both.)
                    if mods.ctrl
                        && (*key == egui::Key::Equals || *key == egui::Key::Plus)
                    {
                        self.adjust_font(1.0);
                        continue;
                    }
                    if mods.ctrl && *key == egui::Key::Minus {
                        self.adjust_font(-1.0);
                        continue;
                    }
                    if mods.ctrl && *key == egui::Key::Num0 {
                        self.set_font(14.0);
                        continue;
                    }
                    match self.focus {
                        Focus::Terminal => {
                            // Enter on a dead pane reconnects (matches the banner).
                            if *key == egui::Key::Enter
                                && self.active_pane_mut().map(|p| !p.alive).unwrap_or(false)
                            {
                                if let Some((h, s)) = self
                                    .active_key
                                    .clone()
                                    .as_deref()
                                    .and_then(|k| k.split_once('/'))
                                {
                                    let (h, s) = (h.to_string(), s.to_string());
                                    self.activate_session(&h, &s);
                                }
                                continue;
                            }
                            let app_cursor = self
                                .active_pane_mut()
                                .map(|p| p.parser.screen().application_cursor())
                                .unwrap_or(false);
                            let bytes = key_event_to_bytes(*key, *mods, app_cursor);
                            self.write_pty(&bytes);
                        }
                        Focus::Tree => {
                            self.handle_tree_key(*key);
                        }
                    }
                }
                egui::Event::Text(t) => {
                    match self.focus {
                        Focus::Terminal => {
                            let mut data = Vec::new();
                            // Alt+letter sends ESC prefix (Text events carry no
                            // modifiers, so consult the live modifier state).
                            if modifiers.alt && !modifiers.ctrl {
                                data.push(0x1b);
                            }
                            data.extend_from_slice(t.as_bytes());
                            self.write_pty(&data);
                        }
                        Focus::Tree => {
                            match t.as_str() {
                                "j" => self.handle_tree_key(egui::Key::ArrowDown),
                                "k" => self.handle_tree_key(egui::Key::ArrowUp),
                                _ => {}
                            }
                        }
                    }
                }
                // egui-winit swallows Ctrl+C/X/V (with or without shift) and
                // emits these instead. Shift distinguishes copy/paste from the
                // raw control bytes a terminal needs (Ctrl+C must stay SIGINT).
                egui::Event::Copy => {
                    if modifiers.shift {
                        self.copy_selection();
                    } else if self.focus == Focus::Terminal {
                        self.write_pty(&[0x03]);
                    }
                }
                egui::Event::Cut => {
                    if self.focus == Focus::Terminal && !modifiers.shift {
                        self.write_pty(&[0x18]);
                    }
                }
                egui::Event::Paste(s) => {
                    if modifiers.shift {
                        let s = s.clone();
                        self.paste_text(&s);
                    } else if self.focus == Focus::Terminal {
                        self.write_pty(&[0x16]);
                    }
                }
                _ => {}
            }
        }

        // Keep egui's own widgets from also reacting to keyboard input
        // (tab-focus navigation, space activating a focused row, ...).
        // Pointer state has already been digested at begin_pass, so sidebar
        // clicks and scrolling keep working.
        ctx.input_mut(|i| {
            i.events.retain(|e| {
                !matches!(
                    e,
                    egui::Event::Key { .. }
                        | egui::Event::Text(_)
                        | egui::Event::Copy
                        | egui::Event::Cut
                        | egui::Event::Paste(_)
                )
            })
        });

        quit
    }

    fn visible_rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        for (hi, entry) in self.hosts.iter().enumerate() {
            rows.push(Row::Host(hi));
            if entry.expanded {
                for si in 0..entry.sessions.len() {
                    rows.push(Row::Session(hi, si));
                }
                if entry.loaded && entry.error.is_none() {
                    rows.push(Row::NewSession(hi));
                }
                if !entry.closed.is_empty() {
                    rows.push(Row::ClosedToggle(hi));
                    if entry.closed_expanded {
                        for ci in 0..entry.closed.len() {
                            rows.push(Row::Closed(hi, ci));
                        }
                        if entry.closed.len() > 1 {
                            rows.push(Row::RestoreAll(hi));
                        }
                    }
                }
            }
        }
        rows
    }

    fn handle_tree_key(&mut self, key: egui::Key) {
        let rows = self.visible_rows();
        if rows.is_empty() {
            return;
        }
        self.tree_cursor = self.tree_cursor.min(rows.len() - 1);
        match key {
            egui::Key::ArrowUp => {
                self.tree_cursor = self.tree_cursor.saturating_sub(1);
            }
            egui::Key::ArrowDown => {
                self.tree_cursor = (self.tree_cursor + 1).min(rows.len() - 1);
            }
            egui::Key::ArrowLeft => match rows[self.tree_cursor] {
                Row::Host(hi) | Row::NewSession(hi) => {
                    self.hosts[hi].expanded = false;
                }
                Row::ClosedToggle(hi) | Row::Closed(hi, _) | Row::RestoreAll(hi) => {
                    self.hosts[hi].closed_expanded = false;
                }
                Row::Session(..) => {}
            },
            egui::Key::ArrowRight => match rows[self.tree_cursor] {
                Row::Host(hi) => self.hosts[hi].expanded = true,
                Row::ClosedToggle(hi) => self.hosts[hi].closed_expanded = true,
                _ => {}
            },
            egui::Key::Enter => match rows[self.tree_cursor] {
                Row::Host(hi) => {
                    self.hosts[hi].expanded = !self.hosts[hi].expanded;
                }
                Row::Session(hi, si) => {
                    let h = self.hosts[hi].host.name.clone();
                    let s = self.hosts[hi].sessions[si].name.clone();
                    self.activate_session(&h, &s);
                }
                Row::NewSession(hi) => {
                    self.open_new_session_modal(hi);
                }
                Row::ClosedToggle(hi) => {
                    self.hosts[hi].closed_expanded = !self.hosts[hi].closed_expanded;
                }
                Row::Closed(hi, ci) => {
                    let name = self.hosts[hi].closed[ci].name.clone();
                    self.open_restore_modal(hi, &name);
                }
                Row::RestoreAll(hi) => {
                    self.open_restore_all_modal(hi);
                }
            },
            _ => {}
        }
    }

    // ---------- sidebar ----------

    pub fn render_sidebar(&mut self, ui: &mut Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.add_space(6.0);
            ui.colored_label(Color32::from_gray(200), egui::RichText::new("sessions").strong());
        });
        ui.separator();

        let rows = self.visible_rows();
        let mut pending: Option<(String, String)> = None;
        let mut toggle: Option<usize> = None;
        let mut toggle_closed: Option<usize> = None;
        let mut new_modal: Option<usize> = None;
        let mut restore_one: Option<(usize, String)> = None;
        let mut restore_all: Option<usize> = None;

        egui::ScrollArea::vertical().show(ui, |ui| {
            for (i, row) in rows.iter().enumerate() {
                let is_cursor = self.focus == Focus::Tree && self.tree_cursor == i;
                let (label, hint, indent, color, is_active) = match row {
                    Row::Host(hi) => {
                        let e = &self.hosts[hi.to_owned()];
                        let arrow = if e.expanded { "▾" } else { "▸" };
                        let suffix = if !e.loaded {
                            " …".to_string()
                        } else if let Some(err) = &e.error {
                            format!(" ✗ {}", truncate(err, 28))
                        } else {
                            format!(" ({})", e.sessions.len())
                        };
                        let color = if e.error.is_some() {
                            Color32::from_rgb(235, 90, 90)
                        } else {
                            Color32::from_rgb(90, 200, 250)
                        };
                        (
                            format!("{arrow} {}{suffix}", e.host.name),
                            None,
                            6.0,
                            color,
                            false,
                        )
                    }
                    Row::Session(hi, si) => {
                        let e = &self.hosts[*hi];
                        let s = &e.sessions[*si];
                        let key = format!("{}/{}", e.host.name, s.name);
                        let active = self.active_key.as_deref() == Some(key.as_str());
                        let color = if active {
                            Color32::from_rgb(140, 235, 140)
                        } else {
                            Color32::from_gray(220)
                        };
                        (s.name.clone(), s.hint.clone(), 24.0, color, active)
                    }
                    Row::NewSession(_) => (
                        "+ new".to_string(),
                        None,
                        24.0,
                        Color32::from_gray(130),
                        false,
                    ),
                    Row::ClosedToggle(hi) => {
                        let e = &self.hosts[*hi];
                        let arrow = if e.closed_expanded { "▾" } else { "▸" };
                        (
                            format!("{arrow} ⟲ closed ({})", e.closed.len()),
                            None,
                            24.0,
                            Color32::from_rgb(150, 130, 90),
                            false,
                        )
                    }
                    Row::Closed(hi, ci) => {
                        let c = &self.hosts[*hi].closed[ci.to_owned()];
                        let age = c
                            .closed_at
                            .map(|t| friendly_age(db::now_epoch() - t))
                            .unwrap_or_default();
                        let hint = match &c.hint {
                            Some(h) => format!("{h} · {age}"),
                            None => age,
                        };
                        (
                            format!("⟲ {}", c.name),
                            Some(hint),
                            36.0,
                            Color32::from_gray(150),
                            false,
                        )
                    }
                    Row::RestoreAll(hi) => {
                        let n = self.hosts[*hi].closed.len();
                        (
                            format!("⟲ restore all ({n})"),
                            None,
                            36.0,
                            Color32::from_rgb(150, 130, 90),
                            false,
                        )
                    }
                };

                let desired = Vec2::new(ui.available_width(), 20.0);
                let (rect, response) = ui.allocate_exact_size(desired, Sense::click());
                if is_cursor {
                    ui.painter()
                        .rect_filled(rect, 3.0, Color32::from_rgb(55, 65, 85));
                } else if is_active {
                    ui.painter()
                        .rect_filled(rect, 3.0, Color32::from_rgb(40, 55, 40));
                } else if response.hovered() {
                    ui.painter().rect_filled(rect, 3.0, Color32::from_gray(45));
                }
                let text_rect = ui.painter().text(
                    rect.left_center() + Vec2::new(indent, 0.0),
                    egui::Align2::LEFT_CENTER,
                    &label,
                    FontId::monospace(13.0),
                    color,
                );
                if let Some(hint) = hint {
                    ui.painter().text(
                        egui::pos2(text_rect.right() + 10.0, rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        truncate(&hint, 14),
                        FontId::monospace(11.0),
                        Color32::from_gray(110),
                    );
                }

                if response.clicked() {
                    self.tree_cursor = i;
                    match row {
                        Row::Host(hi) => toggle = Some(*hi),
                        Row::Session(hi, si) => {
                            let e = &self.hosts[*hi];
                            pending =
                                Some((e.host.name.clone(), e.sessions[*si].name.clone()));
                        }
                        Row::NewSession(hi) => new_modal = Some(*hi),
                        Row::ClosedToggle(hi) => toggle_closed = Some(*hi),
                        Row::Closed(hi, ci) => {
                            restore_one =
                                Some((*hi, self.hosts[*hi].closed[*ci].name.clone()));
                        }
                        Row::RestoreAll(hi) => restore_all = Some(*hi),
                    }
                }
            }
        });

        if let Some(hi) = toggle {
            self.hosts[hi].expanded = !self.hosts[hi].expanded;
        }
        if let Some(hi) = toggle_closed {
            self.hosts[hi].closed_expanded = !self.hosts[hi].closed_expanded;
        }
        if let Some((h, s)) = pending {
            self.activate_session(&h, &s);
        }
        if let Some(hi) = new_modal {
            self.open_new_session_modal(hi);
        }
        if let Some((hi, name)) = restore_one {
            self.open_restore_modal(hi, &name);
        }
        if let Some(hi) = restore_all {
            self.open_restore_all_modal(hi);
        }
    }

    /// Whichever dialog is open. Rendered last so it sits on top.
    pub fn render_modal(&mut self, ctx: &egui::Context) {
        let Some(modal) = self.modal.as_mut() else {
            return;
        };
        let host_idx = match modal {
            AppModal::NewSession { host_idx, .. }
            | AppModal::Restore { host_idx, .. }
            | AppModal::RestoreAll { host_idx, .. } => *host_idx,
        };
        let host_name = self.hosts[host_idx].host.name.clone();
        let mut accept = false;
        let mut cancel = false;
        let mut forget: Option<String> = None;

        let response = egui::Modal::new(egui::Id::new("tmuxmux_modal")).show(ctx, |ui| {
            ui.set_width(380.0);
            match modal {
                AppModal::NewSession {
                    name, just_opened, ..
                } => {
                    ui.heading(format!("new session on {host_name}"));
                    ui.add_space(8.0);
                    let edit = ui.add(
                        egui::TextEdit::singleline(name)
                            .hint_text("session name")
                            .desired_width(f32::INFINITY),
                    );
                    if *just_opened {
                        *just_opened = false;
                        edit.request_focus();
                    }
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui.button("create").clicked() {
                            accept = true;
                        }
                        if ui.button("cancel").clicked() {
                            cancel = true;
                        }
                    });
                }
                AppModal::Restore { name, detail, .. } => {
                    ui.heading(format!("restore '{name}' on {host_name}"));
                    ui.add_space(6.0);
                    ui.colored_label(
                        Color32::from_gray(150),
                        "recreates windows, panes and working directories,\nand re-runs the commands that were running:",
                    );
                    ui.add_space(6.0);
                    for p in &detail.panes {
                        let cmd = p
                            .cmdline
                            .as_deref()
                            .map(|c| format!("$ {}", truncate(c, 40)))
                            .unwrap_or_else(|| "(shell)".to_string());
                        ui.label(
                            egui::RichText::new(format!(
                                "  {}:{}  {}  {}",
                                p.window_index,
                                p.pane_index,
                                truncate(&p.cwd, 26),
                                cmd
                            ))
                            .monospace()
                            .size(12.0)
                            .color(Color32::from_gray(190)),
                        );
                    }
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui.button("restore & attach").clicked() {
                            accept = true;
                        }
                        if ui.button("forget").clicked() {
                            forget = Some(name.clone());
                        }
                        if ui.button("cancel").clicked() {
                            cancel = true;
                        }
                    });
                }
                AppModal::RestoreAll { names, .. } => {
                    ui.heading(format!(
                        "restore {} sessions on {host_name}",
                        names.len()
                    ));
                    ui.add_space(6.0);
                    for name in names.iter().take(12) {
                        ui.label(
                            egui::RichText::new(format!("  ⟲ {name}"))
                                .monospace()
                                .size(12.0)
                                .color(Color32::from_gray(190)),
                        );
                    }
                    if names.len() > 12 {
                        ui.colored_label(
                            Color32::from_gray(140),
                            format!("  … and {} more", names.len() - 12),
                        );
                    }
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui.button(format!("restore all {}", names.len())).clicked() {
                            accept = true;
                        }
                        if ui.button("cancel").clicked() {
                            cancel = true;
                        }
                    });
                }
            }
            if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                accept = true;
            }
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                cancel = true;
            }
        });
        if response.backdrop_response.clicked() {
            cancel = true;
        }

        if accept {
            self.accept_modal();
        } else if let Some(name) = forget {
            self.modal = None;
            self.delete_cached_session(host_idx, &name);
        } else if cancel {
            self.modal = None;
        }
    }

    // ---------- terminal ----------

    pub fn render_terminal(&mut self, ui: &mut Ui) {
        let avail = ui.available_rect_before_wrap();

        let font_id = FontId::monospace(self.font_size);
        let (glyph_w, line_h) = ui.ctx().fonts_mut(|f| {
            (
                f.glyph_width(&font_id, 'M'),
                f.row_height(&font_id),
            )
        });

        let pad = 4.0;
        let inner = avail.shrink(pad);
        let cols = ((inner.width() / glyph_w).floor() as usize).clamp(2, 500);
        let rows = ((inner.height() / line_h).floor() as usize).clamp(2, 200);

        self.layout = Some(TermLayout {
            origin: inner.min,
            glyph_w,
            line_h,
            cols,
            rows,
        });

        // Base fill: the whole panel is always the default background.
        ui.painter().rect_filled(avail, 0.0, DEFAULT_BG);

        let key = match self.active_key.clone() {
            Some(k) => k,
            None => {
                self.draw_placeholder(ui, avail);
                return;
            }
        };

        // Resize pane to fit.
        let alive = {
            let pane = match self.panes.get_mut(&key) {
                Some(p) => p,
                None => {
                    self.draw_placeholder(ui, avail);
                    return;
                }
            };
            if pane.alive {
                pane.resize(cols, rows);
            }
            pane.alive
        };

        // Mouse interaction (selection or forwarding) before painting so a
        // drag this frame highlights this frame.
        let response = ui.allocate_rect(avail, Sense::click_and_drag());
        if alive {
            self.handle_terminal_mouse(ui, &response, inner.min, glyph_w, line_h, cols, rows);
        }

        let pane = match self.panes.get(&key) {
            Some(p) => p,
            None => return,
        };
        let screen = pane.parser.screen();
        let (srows, scols) = screen.size();
        let draw_rows = (srows as usize).min(rows);
        let draw_cols = (scols as usize).min(cols);

        let painter = ui.painter_at(avail);
        let origin = inner.min;
        let selection = self.selection;

        // Pass 1: background rects, merged into runs.
        for row in 0..draw_rows {
            let y = origin.y + row as f32 * line_h;
            let mut col = 0;
            while col < draw_cols {
                let bg = cell_bg(screen, row, col, &selection);
                let mut run = col + 1;
                while run < draw_cols && cell_bg(screen, row, run, &selection) == bg {
                    run += 1;
                }
                if bg != DEFAULT_BG {
                    let x = origin.x + col as f32 * glyph_w;
                    let w = (run - col) as f32 * glyph_w;
                    painter.rect_filled(
                        Rect::from_min_size(Pos2::new(x, y), Vec2::new(w, line_h)),
                        0.0,
                        bg,
                    );
                }
                col = run;
            }
        }

        // Pass 2: text runs grouped by style.
        for row in 0..draw_rows {
            let y = origin.y + row as f32 * line_h;
            let mut col = 0;
            while col < draw_cols {
                let cell = match screen.cell(row as u16, col as u16) {
                    Some(c) => c,
                    None => {
                        col += 1;
                        continue;
                    }
                };
                if cell.is_wide_continuation() || !cell.has_contents() {
                    col += 1;
                    continue;
                }
                let style = cell_style(cell);
                if cell.is_wide() {
                    // Wide glyphs painted alone so the grid stays aligned.
                    let x = origin.x + col as f32 * glyph_w;
                    painter.text(
                        Pos2::new(x, y),
                        egui::Align2::LEFT_TOP,
                        cell.contents(),
                        font_id.clone(),
                        style.0,
                    );
                    col += 2;
                    continue;
                }
                let mut text = String::from(cell.contents());
                let mut run = col + 1;
                while run < draw_cols {
                    let next = match screen.cell(row as u16, run as u16) {
                        Some(c) => c,
                        None => break,
                    };
                    if next.is_wide() || next.is_wide_continuation() {
                        break;
                    }
                    if cell_style(next) != style {
                        break;
                    }
                    if next.has_contents() {
                        text.push_str(next.contents());
                    } else {
                        text.push(' ');
                    }
                    run += 1;
                }
                let x = origin.x + col as f32 * glyph_w;
                if !text.chars().all(|c| c == ' ') {
                    painter.text(
                        Pos2::new(x, y),
                        egui::Align2::LEFT_TOP,
                        text.trim_end(),
                        font_id.clone(),
                        style.0,
                    );
                }
                if style.1 {
                    // underline
                    let w = (run - col) as f32 * glyph_w;
                    painter.line_segment(
                        [
                            Pos2::new(x, y + line_h - 1.5),
                            Pos2::new(x + w, y + line_h - 1.5),
                        ],
                        (1.0, style.0),
                    );
                }
                col = run;
            }
        }

        // Cursor (block, inverted) — hidden when the pane is dead.
        if alive && !screen.hide_cursor() {
            let (crow, ccol) = screen.cursor_position();
            if (crow as usize) < draw_rows && (ccol as usize) < draw_cols {
                let x = origin.x + ccol as f32 * glyph_w;
                let y = origin.y + crow as f32 * line_h;
                let cur_rect = Rect::from_min_size(Pos2::new(x, y), Vec2::new(glyph_w, line_h));
                if self.focus == Focus::Terminal {
                    painter.rect_filled(cur_rect, 0.0, DEFAULT_FG);
                    if let Some(cell) = screen.cell(crow, ccol) {
                        if cell.has_contents() {
                            painter.text(
                                Pos2::new(x, y),
                                egui::Align2::LEFT_TOP,
                                cell.contents(),
                                font_id.clone(),
                                DEFAULT_BG,
                            );
                        }
                    }
                } else {
                    painter.rect_stroke(
                        cur_rect,
                        0.0,
                        (1.0, DEFAULT_FG),
                        egui::StrokeKind::Inside,
                    );
                }
            }
        }

        if !alive {
            let msg = "[ session ended — press Enter to reconnect, or pick another session ]";
            painter.rect_filled(
                Rect::from_min_size(origin, Vec2::new(avail.width() - 2.0 * pad, line_h)),
                0.0,
                Color32::from_rgb(80, 30, 30),
            );
            painter.text(
                origin,
                egui::Align2::LEFT_TOP,
                msg,
                font_id,
                Color32::from_rgb(255, 200, 200),
            );
        }

        if response.clicked() && self.focus != Focus::Terminal {
            self.focus = Focus::Terminal;
        }
    }

    fn draw_placeholder(&self, ui: &mut Ui, avail: Rect) {
        let painter = ui.painter_at(avail);
        let center = avail.center();
        painter.text(
            center - Vec2::new(0.0, 30.0),
            egui::Align2::CENTER_CENTER,
            "tmuxmux",
            FontId::proportional(28.0),
            Color32::from_gray(200),
        );
        painter.text(
            center + Vec2::new(0.0, 8.0),
            egui::Align2::CENTER_CENTER,
            "select a session from the sidebar",
            FontId::proportional(15.0),
            Color32::from_gray(140),
        );
        painter.text(
            center + Vec2::new(0.0, 34.0),
            egui::Align2::CENTER_CENTER,
            "↑↓ navigate · Enter connect · F2 sidebar · F5 refresh · Ctrl+]/\\ cycle",
            FontId::monospace(13.0), // the proportional font lacks arrow glyphs
            Color32::from_gray(110),
        );
    }

    fn handle_terminal_mouse(
        &mut self,
        ui: &Ui,
        response: &egui::Response,
        origin: Pos2,
        glyph_w: f32,
        line_h: f32,
        cols: usize,
        rows: usize,
    ) {
        let to_cell = |pos: Pos2| -> (usize, usize) {
            let col = (((pos.x - origin.x) / glyph_w).floor() as i64).clamp(0, cols as i64 - 1);
            let row = (((pos.y - origin.y) / line_h).floor() as i64).clamp(0, rows as i64 - 1);
            (row as usize, col as usize)
        };

        let (primary_pressed, primary_down, primary_released, pointer_pos, shift) =
            ui.input(|i| {
                (
                    i.pointer.primary_pressed(),
                    i.pointer.primary_down(),
                    i.pointer.primary_released(),
                    i.pointer.interact_pos(),
                    i.modifiers.shift,
                )
            });

        let (mouse_mode, mouse_sgr) = {
            let pane = match self.active_pane_mut() {
                Some(p) => p,
                None => return,
            };
            let screen = pane.parser.screen();
            (
                screen.mouse_protocol_mode(),
                matches!(
                    screen.mouse_protocol_encoding(),
                    vt100_ctt::MouseProtocolEncoding::Sgr
                ),
            )
        };
        let forwarding = mouse_mode != vt100_ctt::MouseProtocolMode::None && !shift;

        // Wheel
        if response.hovered() {
            let scroll = ui.input(|i| {
                i.events
                    .iter()
                    .filter_map(|e| {
                        if let egui::Event::MouseWheel { delta, .. } = e {
                            Some(delta.y)
                        } else {
                            None
                        }
                    })
                    .sum::<f32>()
            });
            if scroll.abs() > 0.0 {
                let up = scroll > 0.0;
                if forwarding {
                    if let Some(pos) = pointer_pos {
                        let (row, col) = to_cell(pos);
                        let btn = if up { 64 } else { 65 };
                        let seq = encode_mouse(btn, col, row, true, mouse_sgr);
                        self.write_pty(&seq);
                    }
                } else {
                    // Alternate-scroll: wheel sends arrows (tmux copy-mode &
                    // shells understand these).
                    let alt_screen = self
                        .active_pane_mut()
                        .map(|p| p.parser.screen().alternate_screen())
                        .unwrap_or(false);
                    if alt_screen {
                        let seq: &[u8] = if up { b"\x1bOA\x1bOA\x1bOA" } else { b"\x1bOB\x1bOB\x1bOB" };
                        self.write_pty(seq);
                    }
                }
            }
        }

        if forwarding {
            // Forward presses/releases/drags with *cell* coordinates (the old
            // version sent pixel coordinates — another of its bugs).
            if let Some(pos) = pointer_pos {
                let (row, col) = to_cell(pos);
                if primary_pressed && response.hovered() {
                    let seq = encode_mouse(0, col, row, true, mouse_sgr);
                    self.write_pty(&seq);
                }
                if primary_released {
                    let seq = encode_mouse(0, col, row, false, mouse_sgr);
                    self.write_pty(&seq);
                }
                let motion = matches!(
                    mouse_mode,
                    vt100_ctt::MouseProtocolMode::ButtonMotion
                        | vt100_ctt::MouseProtocolMode::AnyMotion
                );
                if motion && primary_down && !primary_pressed && response.hovered() {
                    let seq = encode_mouse(32, col, row, true, mouse_sgr);
                    self.write_pty(&seq);
                }
            }
            self.selecting = false;
            return;
        }

        // Local selection.
        if primary_pressed && response.hovered() {
            if let Some(pos) = pointer_pos {
                let cell = to_cell(pos);
                self.selection = Some(Selection {
                    anchor: cell,
                    head: cell,
                });
                self.selecting = true;
            }
        }
        if self.selecting && primary_down {
            if let Some(pos) = pointer_pos {
                if let Some(sel) = self.selection.as_mut() {
                    sel.head = to_cell(pos);
                }
            }
        }
        if primary_released && self.selecting {
            self.selecting = false;
            if let Some(sel) = self.selection {
                if sel.anchor == sel.head {
                    // Plain click: clear selection; Ctrl+click opens URLs.
                    self.selection = None;
                    let ctrl = ui.input(|i| i.modifiers.ctrl);
                    if ctrl {
                        if let Some(pos) = pointer_pos {
                            let (row, col) = to_cell(pos);
                            self.try_open_url(row, col);
                        }
                    }
                }
            }
        }
    }

    fn try_open_url(&mut self, row: usize, col: usize) {
        let key = match self.active_key.as_ref() {
            Some(k) => k,
            None => return,
        };
        let pane = match self.panes.get(key) {
            Some(p) => p,
            None => return,
        };
        let screen = pane.parser.screen();
        let (_, cols) = screen.size();
        let line: String = (0..cols)
            .map(|c| {
                screen
                    .cell(row as u16, c)
                    .map(|cell| {
                        if cell.is_wide_continuation() {
                            String::new()
                        } else if cell.has_contents() {
                            cell.contents().to_string()
                        } else {
                            " ".to_string()
                        }
                    })
                    .unwrap_or_else(|| " ".to_string())
            })
            .collect();
        for prefix in ["https://", "http://"] {
            let mut start = 0;
            while let Some(idx) = line[start..].find(prefix) {
                let s = start + idx;
                let end = line[s..]
                    .find(|c: char| c.is_whitespace() || c == '"' || c == '\'')
                    .map(|e| s + e)
                    .unwrap_or(line.len());
                if col >= s && col < end {
                    let url = line[s..end].trim_end_matches([')', ']', '.', ',']);
                    self.status = format!("opening {url}");
                    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
                    return;
                }
                start = end.max(s + 1);
            }
        }
    }

    pub fn render_status_bar(&self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.add_space(6.0);
            let session = self.active_key.as_deref().unwrap_or("—");
            ui.colored_label(Color32::from_rgb(140, 235, 140), session);
            ui.separator();
            ui.colored_label(Color32::from_gray(150), &self.status);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(6.0);
                ui.colored_label(
                    Color32::from_gray(120),
                    "drag:select  C-S-c:copy  C-S-v:paste  C-]/\\:cycle  C-S-e:tree  C-+/-:font  C-S-l:log  F2:sidebar  F5:refresh  C-S-q:quit",
                );
            });
        });
    }

    /// The narrative progress pane. Only called when `has_log()` is true.
    pub fn render_progress(&self, ui: &mut Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            ui.colored_label(
                Color32::from_rgb(210, 180, 120),
                egui::RichText::new("progress").strong(),
            );
            if let Some(path) = &self.log_path {
                let name = path.rsplit('/').next().unwrap_or(path);
                ui.colored_label(Color32::from_gray(110), name);
            }
        });
        ui.separator();
        let content = self.log_content.clone().unwrap_or_default();
        // Default scroll position is the top — where the "Right now" block and
        // newest entry live, so a returning reader is oriented without scrolling.
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(4.0);
                render_markdown(ui, &content);
                ui.add_space(8.0);
            });
    }

    /// Test accessor: the log content currently held for the active session.
    pub fn log_content_for_test(&self) -> Option<String> {
        self.log_content.clone()
    }
}

/// Minimal markdown renderer — enough for a narrative journal: ATX headings,
/// bullet/numbered lists, `---` rules, fenced code, blockquote callouts,
/// paragraphs (consecutive text lines soft-wrap into one), and inline
/// **bold** / *italic* / `code`.
fn render_markdown(ui: &mut Ui, text: &str) {
    let mut para: Vec<&str> = Vec::new();
    let mut quote: Vec<&str> = Vec::new();
    let mut code: Vec<&str> = Vec::new();
    let mut in_fence = false;

    // Buffers must flush (in order) before any block-level element or a change
    // of kind, so paragraphs and callouts group their lines correctly.
    for raw in text.lines() {
        let line = raw.trim_end();
        let trimmed = line.trim_start();

        if trimmed.starts_with("```") {
            flush_para(ui, &mut para);
            flush_quote(ui, &mut quote);
            if in_fence {
                flush_code(ui, &mut code);
                in_fence = false;
            } else {
                in_fence = true;
            }
            continue;
        }
        if in_fence {
            code.push(raw);
            continue;
        }

        if trimmed.is_empty() {
            flush_para(ui, &mut para);
            flush_quote(ui, &mut quote);
            ui.add_space(6.0);
        } else if let Some(q) = trimmed.strip_prefix("> ").or_else(|| trimmed.strip_prefix(">")) {
            flush_para(ui, &mut para);
            quote.push(q);
        } else if let Some(h) = trimmed.strip_prefix("### ") {
            flush_para(ui, &mut para);
            flush_quote(ui, &mut quote);
            heading(ui, h, 15.0, Color32::from_rgb(150, 200, 235));
        } else if let Some(h) = trimmed.strip_prefix("## ") {
            flush_para(ui, &mut para);
            flush_quote(ui, &mut quote);
            ui.add_space(4.0);
            heading(ui, h, 17.0, Color32::from_rgb(210, 180, 120));
        } else if let Some(h) = trimmed.strip_prefix("# ") {
            flush_para(ui, &mut para);
            flush_quote(ui, &mut quote);
            ui.add_space(4.0);
            heading(ui, h, 20.0, Color32::from_rgb(230, 200, 140));
        } else if trimmed == "---" || trimmed == "***" || trimmed == "___" {
            flush_para(ui, &mut para);
            flush_quote(ui, &mut quote);
            ui.separator();
        } else if let Some(item) = strip_bullet(trimmed) {
            flush_para(ui, &mut para);
            flush_quote(ui, &mut quote);
            ui.horizontal_wrapped(|ui| {
                ui.add_space(10.0);
                ui.colored_label(Color32::from_gray(150), "•");
                ui.add_space(4.0);
                inline(ui, item, Color32::from_gray(205));
            });
        } else {
            flush_quote(ui, &mut quote);
            para.push(trimmed);
        }
    }
    flush_para(ui, &mut para);
    flush_quote(ui, &mut quote);
    if in_fence {
        flush_code(ui, &mut code);
    }
}

fn flush_para(ui: &mut Ui, para: &mut Vec<&str>) {
    if para.is_empty() {
        return;
    }
    let text = para.join(" ");
    para.clear();
    ui.horizontal_wrapped(|ui| {
        ui.add_space(4.0);
        inline(ui, &text, Color32::from_gray(205));
    });
}

/// The `> …` block is the hero element (the "Now / Health / Watch out"
/// callout), so render it as a highlighted panel with an amber left edge —
/// each source line on its own row, bright text.
fn flush_quote(ui: &mut Ui, quote: &mut Vec<&str>) {
    if quote.is_empty() {
        return;
    }
    let lines: Vec<String> = quote.iter().map(|s| s.to_string()).collect();
    quote.clear();
    ui.add_space(2.0);
    egui::Frame::new()
        .fill(Color32::from_rgb(34, 31, 22))
        .inner_margin(egui::Margin {
            left: 10,
            right: 8,
            top: 6,
            bottom: 6,
        })
        .corner_radius(4.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            for (i, line) in lines.iter().enumerate() {
                if i > 0 {
                    ui.add_space(3.0);
                }
                ui.horizontal_wrapped(|ui| {
                    inline(ui, line, Color32::from_rgb(225, 225, 220));
                });
            }
        });
    ui.add_space(2.0);
}

fn flush_code(ui: &mut Ui, code: &mut Vec<&str>) {
    if code.is_empty() {
        return;
    }
    let text = code.join("\n");
    code.clear();
    egui::Frame::new()
        .fill(Color32::from_gray(24))
        .inner_margin(6.0)
        .corner_radius(4.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                egui::RichText::new(text)
                    .monospace()
                    .size(12.0)
                    .color(Color32::from_gray(200)),
            );
        });
}

fn heading(ui: &mut Ui, text: &str, size: f32, color: Color32) {
    ui.horizontal_wrapped(|ui| {
        ui.add_space(4.0);
        ui.label(egui::RichText::new(text).size(size).strong().color(color));
    });
}

fn strip_bullet(s: &str) -> Option<&str> {
    for p in ["- ", "* ", "+ "] {
        if let Some(rest) = s.strip_prefix(p) {
            return Some(rest);
        }
    }
    // "1. " / "12) " style ordered lists.
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !digits.is_empty() {
        let after = &s[digits.len()..];
        if let Some(rest) = after.strip_prefix(". ").or_else(|| after.strip_prefix(") ")) {
            return Some(rest);
        }
    }
    None
}

/// Render a line with inline **bold**, *italic* and `code` spans, wrapping.
fn inline(ui: &mut Ui, text: &str, base: Color32) {
    ui.spacing_mut().item_spacing.x = 0.0;
    let mut chars = text.char_indices().peekable();
    let mut plain = String::new();
    let flush = |ui: &mut Ui, plain: &mut String| {
        if !plain.is_empty() {
            ui.label(egui::RichText::new(std::mem::take(plain)).color(base));
        }
    };
    while let Some((i, c)) = chars.next() {
        match c {
            '`' => {
                if let Some(end) = text[i + 1..].find('`') {
                    flush(ui, &mut plain);
                    let code = &text[i + 1..i + 1 + end];
                    ui.label(
                        egui::RichText::new(code)
                            .monospace()
                            .size(12.5)
                            .background_color(Color32::from_gray(30))
                            .color(Color32::from_rgb(220, 200, 150)),
                    );
                    for _ in 0..end + 1 {
                        chars.next();
                    }
                } else {
                    plain.push(c);
                }
            }
            '*' => {
                let bold = chars.peek().map(|&(_, n)| n == '*').unwrap_or(false);
                let marker = if bold { "**" } else { "*" };
                let start = i + marker.len();
                if let Some(rel) = text[start..].find(marker) {
                    flush(ui, &mut plain);
                    let span = &text[start..start + rel];
                    let rt = egui::RichText::new(span).color(base);
                    ui.label(if bold { rt.strong() } else { rt.italics() });
                    // advance past span + closing marker (+ the extra '*' if bold)
                    for _ in 0..(rel + marker.len() + (marker.len() - 1)) {
                        chars.next();
                    }
                } else {
                    plain.push(c);
                }
            }
            _ => plain.push(c),
        }
    }
    flush(ui, &mut plain);
}

fn cell_bg(
    screen: &vt100_ctt::Screen,
    row: usize,
    col: usize,
    selection: &Option<Selection>,
) -> Color32 {
    if let Some(sel) = selection {
        if sel.contains(row, col) {
            return SELECTION_BG;
        }
    }
    match screen.cell(row as u16, col as u16) {
        Some(cell) => {
            let (fg, bg) = resolve_colors(cell);
            let _ = fg;
            bg
        }
        None => DEFAULT_BG,
    }
}

/// Resolve a cell's effective fg/bg, handling Default, bold and inverse.
fn resolve_colors(cell: &vt100_ctt::Cell) -> (Color32, Color32) {
    let mut fg = convert_fg(cell.fgcolor(), cell.bold());
    let mut bg = convert_bg(cell.bgcolor());
    if cell.inverse() {
        std::mem::swap(&mut fg, &mut bg);
        // Inverse of "default fg on default bg" must be readable: an unset
        // bg that just became fg would be black-on-white-ish — fine; but an
        // unset fg that became bg needs the *foreground* default.
        if cell.fgcolor() == vt100_ctt::Color::Default {
            bg = DEFAULT_FG;
        }
        if cell.bgcolor() == vt100_ctt::Color::Default {
            fg = DEFAULT_BG;
        }
    }
    if cell.dim() {
        let [r, g, b, _] = fg.to_array();
        fg = Color32::from_rgb(
            (r as f32 * 0.6) as u8,
            (g as f32 * 0.6) as u8,
            (b as f32 * 0.6) as u8,
        );
    }
    (fg, bg)
}

/// (fg color, underline) — the run-grouping key for text painting.
fn cell_style(cell: &vt100_ctt::Cell) -> (Color32, bool) {
    let (fg, _) = resolve_colors(cell);
    (fg, cell.underline())
}

fn encode_mouse(button: u8, col: usize, row: usize, pressed: bool, sgr: bool) -> Vec<u8> {
    if sgr {
        format!(
            "\x1b[<{};{};{}{}",
            button,
            col + 1,
            row + 1,
            if pressed { 'M' } else { 'm' }
        )
        .into_bytes()
    } else {
        // Legacy X10 encoding.
        let b = if pressed { button } else { 3 };
        vec![
            0x1b,
            b'[',
            b'M',
            32 + b,
            32 + (col as u8).saturating_add(1).min(223),
            32 + (row as u8).saturating_add(1).min(223),
        ]
    }
}

/// Working directory to resolve the progress log against: the focused pane's
/// cwd, falling back to the first pane.
fn active_cwd(s: &SessionSnap) -> Option<String> {
    s.panes
        .iter()
        .find(|p: &&PaneSnap| p.active)
        .or_else(|| s.panes.first())
        .map(|p| p.cwd.clone())
        .filter(|c| !c.is_empty())
}

/// Sidebar hint for a live session: the short name of the first pane that's
/// running something other than a bare shell.
fn session_hint(s: &SessionSnap) -> Option<String> {
    s.panes
        .iter()
        .find(|p| p.cmdline.is_some())
        .map(|p| p.command.clone())
        .filter(|c| !c.is_empty())
}

fn merge_env(
    global: &Option<HashMap<String, String>>,
    host: &Option<HashMap<String, String>>,
) -> Option<HashMap<String, String>> {
    match (global, host) {
        (None, None) => None,
        (Some(g), None) => Some(g.clone()),
        (None, Some(h)) => Some(h.clone()),
        (Some(g), Some(h)) => {
            let mut merged = g.clone();
            for (k, v) in h {
                merged.insert(k.clone(), v.clone());
            }
            Some(merged)
        }
    }
}

fn friendly_age(secs: i64) -> String {
    match secs {
        i64::MIN..=59 => "now".into(),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n).collect();
        format!("{t}…")
    }
}
