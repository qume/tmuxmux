use std::collections::HashMap;
use std::sync::mpsc;
use std::thread;

use egui::{Color32, FontId, Pos2, Rect, Sense, Ui, Vec2};

use crate::colors::{convert_bg, convert_fg, DEFAULT_BG, DEFAULT_FG, SELECTION_BG};
use crate::config::{Config, Host};
use crate::input::key_event_to_bytes;
use crate::ssh::{build_attach_command, list_sessions, SessionListResult};
use crate::terminal::TerminalPane;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Focus {
    Tree,
    Terminal,
}

pub struct HostEntry {
    pub host: Host,
    pub sessions: Vec<String>,
    pub loaded: bool,
    pub error: Option<String>,
    pub expanded: bool,
}

/// Flattened sidebar row, rebuilt every frame from `hosts`.
#[derive(Clone)]
enum Row {
    Host(usize),
    Session(usize, usize),
}

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
    listing_rx: mpsc::Receiver<SessionListResult>,

    tree_cursor: usize,

    pub selection: Option<Selection>,
    selecting: bool,
    clipboard: Option<arboard::Clipboard>,

    layout: Option<TermLayout>,
    status: String,
    font_size: f32,
}

impl App {
    pub fn new(config: Config) -> Self {
        let hosts: Vec<HostEntry> = config
            .hosts
            .iter()
            .map(|h| HostEntry {
                host: h.clone(),
                sessions: Vec::new(),
                loaded: false,
                error: None,
                expanded: true,
            })
            .collect();

        let listing_rx = spawn_listing(&config.hosts);

        let clipboard = match arboard::Clipboard::new() {
            Ok(c) => Some(c),
            Err(e) => {
                log::warn!("clipboard unavailable: {e}");
                None
            }
        };

        App {
            config,
            hosts,
            focus: Focus::Tree,
            show_sidebar: true,
            panes: HashMap::new(),
            active_key: None,
            listing_rx,
            tree_cursor: 0,
            selection: None,
            selecting: false,
            clipboard,
            layout: None,
            status: "loading sessions...".into(),
            font_size: 14.0,
        }
    }

    // ---------- session listing ----------

    pub fn check_listing_results(&mut self) {
        while let Ok(result) = self.listing_rx.try_recv() {
            if let Some(entry) = self.hosts.iter_mut().find(|e| e.host.name == result.host_name) {
                entry.sessions = result.sessions;
                entry.error = result.error;
                entry.loaded = true;
            }
            if self.hosts.iter().all(|e| e.loaded) {
                self.status = "ready".into();
            }
        }
    }

    pub fn refresh_all(&mut self) {
        for entry in &mut self.hosts {
            entry.loaded = false;
        }
        self.status = "refreshing...".into();
        self.listing_rx = spawn_listing(&self.config.hosts);
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
        let key = format!("{}/{}", host_name, session_name);
        if let Some(pane) = self.panes.get(&key) {
            if !pane.alive {
                self.panes.remove(&key);
            }
        }
        if !self.panes.contains_key(&key) {
            let host = match self.config.hosts.iter().find(|h| h.name == host_name) {
                Some(h) => h.clone(),
                None => {
                    self.status = format!("unknown host: {host_name}");
                    return;
                }
            };
            let env = merge_env(&self.config.env, &host.env);
            let cmd = build_attach_command(&host, session_name);
            log::info!("spawning: {:?}", cmd);
            let (cols, rows) = self
                .layout
                .as_ref()
                .map(|l| (l.cols, l.rows))
                .unwrap_or((80, 24));
            self.panes
                .insert(key.clone(), TerminalPane::new(cmd, cols.max(2), rows.max(2), env));
        }
        self.active_key = Some(key.clone());
        self.focus = Focus::Terminal;
        self.selection = None;
        self.selecting = false;
        self.status = key;
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
                    .map(|s| (e.host.name.clone(), s.clone()))
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
            egui::Key::ArrowLeft => {
                if let Row::Host(hi) = rows[self.tree_cursor] {
                    self.hosts[hi].expanded = false;
                }
            }
            egui::Key::ArrowRight => {
                if let Row::Host(hi) = rows[self.tree_cursor] {
                    self.hosts[hi].expanded = true;
                }
            }
            egui::Key::Enter => match rows[self.tree_cursor] {
                Row::Host(hi) => {
                    self.hosts[hi].expanded = !self.hosts[hi].expanded;
                }
                Row::Session(hi, si) => {
                    let h = self.hosts[hi].host.name.clone();
                    let s = self.hosts[hi].sessions[si].clone();
                    self.activate_session(&h, &s);
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

        egui::ScrollArea::vertical().show(ui, |ui| {
            for (i, row) in rows.iter().enumerate() {
                let is_cursor = self.focus == Focus::Tree && self.tree_cursor == i;
                let (label, indent, color, is_active) = match row {
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
                        (format!("{arrow} {}{suffix}", e.host.name), 6.0, color, false)
                    }
                    Row::Session(hi, si) => {
                        let e = &self.hosts[*hi];
                        let key = format!("{}/{}", e.host.name, e.sessions[*si]);
                        let active = self.active_key.as_deref() == Some(key.as_str());
                        let color = if active {
                            Color32::from_rgb(140, 235, 140)
                        } else {
                            Color32::from_gray(220)
                        };
                        (e.sessions[*si].clone(), 24.0, color, active)
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
                ui.painter().text(
                    rect.left_center() + Vec2::new(indent, 0.0),
                    egui::Align2::LEFT_CENTER,
                    &label,
                    FontId::monospace(13.0),
                    color,
                );

                if response.clicked() {
                    self.tree_cursor = i;
                    match row {
                        Row::Host(hi) => toggle = Some(*hi),
                        Row::Session(hi, si) => {
                            let e = &self.hosts[*hi];
                            pending = Some((e.host.name.clone(), e.sessions[*si].clone()));
                        }
                    }
                }
            }
        });

        if let Some(hi) = toggle {
            self.hosts[hi].expanded = !self.hosts[hi].expanded;
        }
        if let Some((h, s)) = pending {
            self.activate_session(&h, &s);
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
            FontId::proportional(13.0),
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
                    "drag:select  C-S-c:copy  C-S-v:paste  C-]/\\:cycle  C-S-e:tree  F2:sidebar  F5:refresh  C-S-q:quit",
                );
            });
        });
    }
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

fn spawn_listing(hosts: &[Host]) -> mpsc::Receiver<SessionListResult> {
    let (tx, rx) = mpsc::channel();
    // One thread per host so a slow tunnel doesn't serialize the others.
    for host in hosts.iter().cloned() {
        let tx = tx.clone();
        thread::spawn(move || {
            let _ = tx.send(list_sessions(host));
        });
    }
    rx
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

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n).collect();
        format!("{t}…")
    }
}
