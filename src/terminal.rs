use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::time::{Duration, Instant};

use crate::acs::AcsFilter;

/// Run a shell-command argv and return combined stdout+stderr. Commands that
/// start with `sshpass` go through the pty-capture path (feeds the password,
/// never invokes sshpass — Windows-safe); everything else uses a plain
/// subprocess, which is cheaper and unchanged.
pub fn run_argv(argv: Vec<String>) -> Result<String, String> {
    if argv.first().map(|s| s == "sshpass").unwrap_or(false) {
        let (out, _ok) = run_capture(argv, None, Duration::from_secs(25));
        Ok(out)
    } else {
        let output = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .output()
            .map_err(|e| e.to_string())?;
        let mut s = String::from_utf8_lossy(&output.stdout).to_string();
        s.push_str(&String::from_utf8_lossy(&output.stderr));
        Ok(s)
    }
}

/// Run a command to completion capturing its output, feeding an
/// `sshpass -p …` password on the prompt via a pty — so `sshpass` itself is
/// never invoked (it doesn't exist on Windows). Used for the non-interactive
/// ssh calls (snapshot, log fetch). Returns (combined stdout+stderr, success).
pub fn run_capture(
    argv: Vec<String>,
    env: Option<HashMap<String, String>>,
    timeout: Duration,
) -> (String, bool) {
    let (argv, password) = strip_sshpass(argv);
    if argv.is_empty() {
        return (String::new(), false);
    }
    let pair = match native_pty_system().openpty(PtySize {
        rows: 50,
        cols: 220,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(p) => p,
        Err(_) => return (String::new(), false),
    };
    #[cfg(unix)]
    {
        if let Some(fd) = pair.master.as_raw_fd() {
            unsafe {
                let f = libc::fcntl(fd, libc::F_GETFL, 0);
                libc::fcntl(fd, libc::F_SETFL, f | libc::O_NONBLOCK);
            }
        }
    }
    let mut reader = match pair.master.try_clone_reader() {
        Ok(r) => r,
        Err(_) => return (String::new(), false),
    };
    let mut writer = match pair.master.take_writer() {
        Ok(w) => w,
        Err(_) => return (String::new(), false),
    };
    let mut builder = CommandBuilder::new(&argv[0]);
    builder.args(&argv[1..]);
    builder.env("TERM", "dumb");
    if let Some(env) = &env {
        for (k, v) in env {
            if v.is_empty() {
                builder.env_remove(k);
            } else {
                builder.env(k, v);
            }
        }
    }
    let mut child = match pair.slave.spawn_command(builder) {
        Ok(c) => c,
        Err(_) => return (String::new(), false),
    };
    drop(pair.slave);

    let mut out: Vec<u8> = Vec::new();
    let mut sent = false;
    let deadline = Instant::now() + timeout;
    let mut buf = [0u8; 16384];
    loop {
        if Instant::now() > deadline {
            let _ = child.kill();
            break;
        }
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                if !sent {
                    if let Some(pw) = &password {
                        if out.windows(7).any(|w| w == b"assword") {
                            let _ = writer.write_all(pw.as_bytes());
                            let _ = writer.write_all(b"\r");
                            let _ = writer.flush();
                            sent = true;
                        }
                    }
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    // Child exited — drain a little more, then finish.
                    std::thread::sleep(Duration::from_millis(20));
                    if let Ok(n) = reader.read(&mut buf) {
                        if n > 0 {
                            out.extend_from_slice(&buf[..n]);
                        }
                    }
                    break;
                }
                std::thread::sleep(Duration::from_millis(15));
            }
            Err(_) => break,
        }
    }
    let success = child.wait().map(|s| s.success()).unwrap_or(false);
    (String::from_utf8_lossy(&out).to_string(), success)
}

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
    /// Password stripped from an `sshpass -p …` wrapper. We type it into the
    /// pty ourselves on the password prompt, so `sshpass` never has to exist —
    /// which makes the same command work on Windows (no sshpass there).
    password: Option<String>,
    password_sent: bool,
}

/// Pull the password out of an `sshpass -p PW <cmd…>` prefix, returning the
/// bare command plus the password. Leaves non-sshpass commands untouched.
fn strip_sshpass(cmd: Vec<String>) -> (Vec<String>, Option<String>) {
    if cmd.first().map(|s| s == "sshpass").unwrap_or(false) && cmd.len() >= 2 {
        // `-p PW <cmd>`
        if cmd[1] == "-p" && cmd.len() >= 4 {
            return (cmd[3..].to_vec(), Some(cmd[2].clone()));
        }
        // `-pPW <cmd>` (glued)
        if cmd[1].starts_with("-p") && cmd[1].len() > 2 && cmd.len() >= 3 {
            return (cmd[2..].to_vec(), Some(cmd[1][2..].to_string()));
        }
    }
    (cmd, None)
}

impl TerminalPane {
    pub fn new(
        cmd: Vec<String>,
        cols: usize,
        rows: usize,
        env: Option<std::collections::HashMap<String, String>>,
    ) -> Self {
        let (cmd, password) = strip_sshpass(cmd);
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
            password,
            password_sent: false,
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
        // Auto-type the sshpass password once, when the ssh password prompt
        // shows up. Only runs pre-auth (password_sent gates it), and matches
        // "assword" to cover "Password:" / "password:".
        if any && !self.password_sent && self.password.is_some() {
            let prompting = self.parser.screen().contents().contains("assword");
            if prompting {
                let pw = self.password.clone().unwrap();
                self.write_input(pw.as_bytes());
                self.write_input(b"\r");
                self.password_sent = true;
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
