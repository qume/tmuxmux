use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};

use crate::acs::AcsFilter;

pub struct TerminalPane {
    pub parser: vt100_ctt::Parser,
    acs: AcsFilter,
    master: Option<Box<dyn MasterPty + Send>>,
    reader: Option<Box<dyn Read + Send>>,
    writer: Option<Box<dyn Write + Send>>,
    pub alive: bool,
    pub cols: usize,
    pub rows: usize,
    kill_pid: Option<u32>,
}

impl TerminalPane {
    pub fn new(
        cmd: Vec<String>,
        cols: usize,
        rows: usize,
        env: Option<std::collections::HashMap<String, String>>,
    ) -> Self {
        let mut pane = TerminalPane {
            parser: vt100_ctt::Parser::new(rows as u16, cols as u16, 0),
            acs: AcsFilter::new(),
            master: None,
            reader: None,
            writer: None,
            alive: false,
            cols,
            rows,
            kill_pid: None,
        };
        pane.spawn(cmd, env);
        pane
    }

    fn spawn(
        &mut self,
        cmd: Vec<String>,
        env: Option<std::collections::HashMap<String, String>>,
    ) {
        if cmd.is_empty() {
            return;
        }

        let pty_system = native_pty_system();
        let pair = match pty_system.openpty(PtySize {
            rows: self.rows as u16,
            cols: self.cols as u16,
            pixel_width: 0,
            pixel_height: 0,
        }) {
            Ok(p) => p,
            Err(e) => {
                log::error!("Failed to open PTY: {e}");
                return;
            }
        };

        #[cfg(unix)]
        {
            if let Some(fd) = pair.master.as_raw_fd() {
                unsafe {
                    let flags = libc::fcntl(fd, libc::F_GETFL, 0);
                    libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
                }
            }
        }

        let reader = match pair.master.try_clone_reader() {
            Ok(r) => r,
            Err(e) => {
                log::error!("Failed to clone reader: {e}");
                return;
            }
        };

        let writer = match pair.master.take_writer() {
            Ok(w) => w,
            Err(e) => {
                log::error!("Failed to take writer: {e}");
                return;
            }
        };

        let program = cmd[0].clone();
        let args: Vec<String> = cmd[1..].to_vec();

        let mut cmd_builder = CommandBuilder::new(&program);
        cmd_builder.args(&args);
        cmd_builder.env("TERM", "xterm-256color");
        // Allow running tmuxmux from inside a tmux session.
        cmd_builder.env_remove("TMUX");
        cmd_builder.env_remove("TMUX_PANE");

        if let Some(ref env) = env {
            for (key, value) in env {
                if value.is_empty() {
                    cmd_builder.env_remove(key);
                } else {
                    cmd_builder.env(key, value);
                }
            }
        }

        let child = match pair.slave.spawn_command(cmd_builder) {
            Ok(c) => c,
            Err(e) => {
                log::error!("Failed to spawn: {e}");
                return;
            }
        };

        let pid = child.process_id();
        drop(pair.slave);
        drop(child);

        self.master = Some(pair.master);
        self.reader = Some(reader);
        self.writer = Some(writer);
        self.alive = true;
        self.kill_pid = pid;
    }

    pub fn try_read(&mut self) -> bool {
        let reader = match self.reader.as_mut() {
            Some(r) => r,
            None => return false,
        };

        let mut buf = [0u8; 65536];
        let mut filtered: Vec<u8> = Vec::new();
        let mut any = false;
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    self.alive = false;
                    break;
                }
                Ok(n) => {
                    // Translate DEC special graphics (ACS) to Unicode before
                    // parsing — vt100-ctt ignores charset escapes.
                    filtered.clear();
                    self.acs.feed(&buf[..n], &mut filtered);
                    self.parser.process(&filtered);
                    any = true;
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    self.alive = false;
                    break;
                }
            }
        }
        any
    }

    pub fn write_input(&mut self, data: &[u8]) {
        if let Some(ref mut writer) = self.writer {
            let _ = writer.write_all(data);
            let _ = writer.flush();
        }
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        if cols == self.cols && rows == self.rows {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        // Resize the existing screen in place — recreating the parser would
        // drop everything drawn before tmux's redraw arrives (visible flash).
        self.parser.screen_mut().set_size(rows as u16, cols as u16);
        if let Some(ref master) = self.master {
            let _ = master.resize(PtySize {
                rows: rows as u16,
                cols: cols as u16,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
    }
}

impl Drop for TerminalPane {
    fn drop(&mut self) {
        if let Some(pid) = self.kill_pid {
            #[cfg(unix)]
            unsafe {
                libc::kill(pid as i32, libc::SIGHUP);
            }
            #[cfg(windows)]
            {
                let _ = std::process::Command::new("taskkill")
                    .args(["/PID", &pid.to_string(), "/F"])
                    .output();
            }
        }
    }
}
