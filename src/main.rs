mod acs;
mod app;
mod appmanager;
mod colors;
mod config;
mod db;
mod input;
mod progresslog;
mod restore;
mod snapshot;
mod ssh;
mod terminal;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use app::App;

/// One step of a `--script` test program. Steps are separated by `;;`.
///   sleep:MS              wait
///   attach:HOST/SESSION   open a session
///   keys:TEXT             send bytes to the pty (\n \r \t \e \xNN escapes)
///   shot:PATH             save a PNG screenshot of the window
///   select:R1,C1,R2,C2    set the selection (cell coordinates)
///   copy                  copy selection to the system clipboard
///   paste                 paste system clipboard into the pty
///   print-selection       print the selected text to stdout
///   print-clipboard       print the system clipboard to stdout
///   newmodal:HOST         open the new-session dialog for a host
///   modal-accept          accept the open dialog
///   snapshot-now          poll all hosts immediately
///   restore:HOST/NAME     restore a cached session and attach
///   restoremodal:HOST/NAME open the restore dialog for a cached session
///   restore-all:HOST      restore every cached session on a host
///   dump-live:HOST        print the live session list to stdout
///   dump-closed:HOST      print the cached/closed session list to stdout
///   hide:HOST/SESSION     hide a live session (client-side view preference)
///   unhide:HOST/SESSION   unhide a session
///   dump-hidden:HOST      print the live-and-hidden session list to stdout
///   quit                  exit the app
#[derive(Debug, Clone)]
enum Step {
    Sleep(u64),
    Attach(String, String),
    Keys(Vec<u8>),
    Shot(String),
    Select(usize, usize, usize, usize),
    Copy,
    Paste,
    PrintSelection,
    PrintClipboard,
    NewModal(String),
    ModalAccept,
    Font(f32),
    DumpLog,
    HasLog,
    SnapshotNow,
    Restore(String, String),
    RestoreModal(String, String),
    RestoreAll(String),
    DumpLive(String),
    DumpClosed(String),
    ExpandClosed(String),
    Hide(String, String),
    Unhide(String, String),
    DumpHidden(String),
    Quit,
}

fn parse_script(s: &str) -> Vec<Step> {
    let mut steps = Vec::new();
    for raw in s.split(";;") {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let (cmd, arg) = match raw.split_once(':') {
            Some((c, a)) => (c.trim(), a),
            None => (raw, ""),
        };
        let step = match cmd {
            "sleep" => arg.trim().parse().ok().map(Step::Sleep),
            "attach" => arg
                .trim()
                .split_once('/')
                .map(|(h, s)| Step::Attach(h.to_string(), s.to_string())),
            "keys" => Some(Step::Keys(input::unescape_keys(arg))),
            "shot" => Some(Step::Shot(arg.trim().to_string())),
            "select" => {
                let nums: Vec<usize> = arg
                    .split(',')
                    .filter_map(|n| n.trim().parse().ok())
                    .collect();
                if nums.len() == 4 {
                    Some(Step::Select(nums[0], nums[1], nums[2], nums[3]))
                } else {
                    None
                }
            }
            "copy" => Some(Step::Copy),
            "paste" => Some(Step::Paste),
            "newmodal" => Some(Step::NewModal(arg.trim().to_string())),
            "modal-accept" => Some(Step::ModalAccept),
            "font" => arg.trim().parse().ok().map(Step::Font),
            "dump-log" => Some(Step::DumpLog),
            "has-log" => Some(Step::HasLog),
            "snapshot-now" => Some(Step::SnapshotNow),
            "restore" => arg
                .trim()
                .split_once('/')
                .map(|(h, s)| Step::Restore(h.to_string(), s.to_string())),
            "restoremodal" => arg
                .trim()
                .split_once('/')
                .map(|(h, s)| Step::RestoreModal(h.to_string(), s.to_string())),
            "restore-all" => Some(Step::RestoreAll(arg.trim().to_string())),
            "dump-live" => Some(Step::DumpLive(arg.trim().to_string())),
            "dump-closed" => Some(Step::DumpClosed(arg.trim().to_string())),
            "expand-closed" => Some(Step::ExpandClosed(arg.trim().to_string())),
            "hide" => arg
                .trim()
                .split_once('/')
                .map(|(h, s)| Step::Hide(h.to_string(), s.to_string())),
            "unhide" => arg
                .trim()
                .split_once('/')
                .map(|(h, s)| Step::Unhide(h.to_string(), s.to_string())),
            "dump-hidden" => Some(Step::DumpHidden(arg.trim().to_string())),
            "print-selection" => Some(Step::PrintSelection),
            "print-clipboard" => Some(Step::PrintClipboard),
            "quit" => Some(Step::Quit),
            _ => None,
        };
        match step {
            Some(st) => steps.push(st),
            None => eprintln!("script: ignoring bad step: {raw}"),
        }
    }
    steps
}

struct MainApp {
    inner: App,
    script: Vec<Step>,
    script_idx: usize,
    script_wait_until: Instant,
    pending_shot: bool,
    /// Last frame with terminal activity — drives activity-based repaint pacing.
    last_active: Instant,
}

#[allow(deprecated)]
impl eframe::App for MainApp {
    fn ui(&mut self, _ui: &mut egui::Ui, _frame: &mut eframe::Frame) {}

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.inner.check_results();
        let had_output = self.inner.read_all_panes();

        // Activity-based pacing: the terminal only needs a fast repaint when
        // something actually changed — PTY output, input, or a drag. Otherwise
        // we'd repaint the whole terminal at 60fps forever (~50% CPU on idle).
        // Any activity opens a short "lively" window; when it lapses we drop to
        // a slow heartbeat that still catches new output within ~250ms.
        let had_input = ctx.input(|i| !i.events.is_empty() || i.pointer.any_down());
        if had_output || had_input {
            self.last_active = Instant::now();
        }

        // Save any screenshots delivered by the backend.
        let shots: Vec<(egui::UserData, std::sync::Arc<egui::ColorImage>)> = ctx.input(|i| {
            i.events
                .iter()
                .filter_map(|e| {
                    if let egui::Event::Screenshot {
                        user_data, image, ..
                    } = e
                    {
                        Some((user_data.clone(), image.clone()))
                    } else {
                        None
                    }
                })
                .collect()
        });
        for (user_data, image) in shots {
            if let Some(path) = user_data
                .data
                .as_ref()
                .and_then(|d| d.downcast_ref::<String>())
            {
                save_png(path, &image);
                println!("SCREENSHOT_SAVED:{path}");
                self.pending_shot = false;
            }
        }

        self.run_script(ctx);

        if self.inner.handle_events(ctx) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        egui::TopBottomPanel::bottom("status")
            .exact_height(24.0)
            .frame(egui::Frame::new().fill(egui::Color32::from_rgb(32, 34, 38)))
            .show(ctx, |ui| {
                self.inner.render_status_bar(ui);
            });

        if self.inner.show_sidebar {
            egui::SidePanel::left("sidebar")
                .resizable(true)
                .default_width(240.0)
                .width_range(160.0..=420.0)
                .frame(egui::Frame::new().fill(egui::Color32::from_rgb(24, 26, 30)))
                .show(ctx, |ui| {
                    self.inner.render_sidebar(ui);
                });
        }

        // Right-hand progress pane — only present when the active session has
        // a non-empty log, so it costs no screen room otherwise.
        if self.inner.has_log() {
            egui::SidePanel::right("progress")
                .resizable(true)
                .default_width(380.0)
                .width_range(220.0..=640.0)
                .frame(egui::Frame::new().fill(egui::Color32::from_rgb(20, 21, 25)))
                .show(ctx, |ui| {
                    self.inner.render_progress(ui);
                });
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(colors::DEFAULT_BG))
            .show(ctx, |ui| {
                self.inner.render_terminal(ui);
            });

        self.inner.render_modal(ctx);

        // Fast repaints only while lively (recent output/input) or a script is
        // driving; otherwise a slow heartbeat that still polls the PTY and
        // drains snapshot/log/app-sync channels within ~250ms.
        let scripting = self.script_idx < self.script.len() || self.pending_shot;
        let lively = scripting || self.last_active.elapsed() < Duration::from_millis(400);
        ctx.request_repaint_after(Duration::from_millis(if lively { 16 } else { 500 }));
    }
}

impl MainApp {
    fn run_script(&mut self, ctx: &egui::Context) {
        // A pending screenshot blocks the script so `shot` is synchronous.
        while !self.pending_shot && self.script_idx < self.script.len() {
            if Instant::now() < self.script_wait_until {
                break;
            }
            let step = self.script[self.script_idx].clone();
            self.script_idx += 1;
            log::info!("script step: {:?}", step);
            match step {
                Step::Sleep(ms) => {
                    self.script_wait_until = Instant::now() + Duration::from_millis(ms);
                }
                Step::Attach(h, s) => self.inner.activate_session(&h, &s),
                Step::Keys(data) => self.inner.send_keys(&data),
                Step::Shot(path) => {
                    self.pending_shot = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(
                        egui::UserData::new(path),
                    ));
                }
                Step::Select(r1, c1, r2, c2) => self.inner.set_selection_cells(r1, c1, r2, c2),
                Step::Copy => self.inner.copy_selection(),
                Step::Paste => self.inner.paste_clipboard(),
                Step::PrintSelection => {
                    println!(
                        "SELECTION>>>{}<<<",
                        self.inner.selection_text().unwrap_or_default()
                    );
                }
                Step::NewModal(h) => self.inner.open_new_session_modal_by_host(&h),
                Step::ModalAccept => self.inner.accept_modal(),
                Step::Font(n) => self.inner.set_font_for_test(n),
                Step::DumpLog => {
                    println!(
                        "LOG>>>{}<<<",
                        self.inner.log_content_for_test().unwrap_or_default()
                    );
                }
                Step::HasLog => println!("HASLOG:{}", self.inner.has_log()),
                Step::SnapshotNow => self.inner.poll_now(),
                Step::Restore(h, s) => match self.inner.host_index(&h) {
                    Some(hi) => self.inner.restore_sessions(hi, vec![s], true),
                    None => eprintln!("script: unknown host {h}"),
                },
                Step::RestoreModal(h, s) => match self.inner.host_index(&h) {
                    Some(hi) => self.inner.open_restore_modal(hi, &s),
                    None => eprintln!("script: unknown host {h}"),
                },
                Step::RestoreAll(h) => match self.inner.host_index(&h) {
                    Some(hi) => {
                        let names: Vec<String> = self.inner.hosts[hi]
                            .closed
                            .iter()
                            .map(|c| c.name.clone())
                            .collect();
                        self.inner.restore_sessions(hi, names, false);
                    }
                    None => eprintln!("script: unknown host {h}"),
                },
                Step::DumpLive(h) => {
                    let line = self
                        .inner
                        .host_index(&h)
                        .map(|hi| {
                            self.inner.hosts[hi]
                                .sessions
                                .iter()
                                .map(|s| {
                                    format!(
                                        "{}[{}]",
                                        s.name,
                                        s.hint.as_deref().unwrap_or("")
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join(",")
                        })
                        .unwrap_or_else(|| "?unknown-host".into());
                    println!("LIVE:{h}>>>{line}<<<");
                }
                Step::ExpandClosed(h) => {
                    if let Some(hi) = self.inner.host_index(&h) {
                        self.inner.hosts[hi].closed_expanded = true;
                    }
                }
                Step::DumpClosed(h) => {
                    let line = self
                        .inner
                        .host_index(&h)
                        .map(|hi| {
                            self.inner.hosts[hi]
                                .closed
                                .iter()
                                .map(|c| {
                                    format!(
                                        "{}[{}]",
                                        c.name,
                                        c.hint.as_deref().unwrap_or("")
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join(",")
                        })
                        .unwrap_or_else(|| "?unknown-host".into());
                    println!("CLOSED:{h}>>>{line}<<<");
                }
                Step::Hide(h, s) => match self.inner.host_index(&h) {
                    Some(hi) => self.inner.hide_session(hi, &s),
                    None => eprintln!("script: unknown host {h}"),
                },
                Step::Unhide(h, s) => match self.inner.host_index(&h) {
                    Some(hi) => self.inner.unhide_session(hi, &s),
                    None => eprintln!("script: unknown host {h}"),
                },
                Step::DumpHidden(h) => {
                    let line = self
                        .inner
                        .host_index(&h)
                        .map(|hi| self.inner.hosts[hi].hidden.join(","))
                        .unwrap_or_else(|| "?unknown-host".into());
                    println!("HIDDEN:{h}>>>{line}<<<");
                }
                Step::PrintClipboard => {
                    let text = arboard::Clipboard::new()
                        .and_then(|mut c| c.get_text())
                        .unwrap_or_default();
                    println!("CLIPBOARD>>>{text}<<<");
                }
                Step::Quit => {
                    // Flush stdout before dying.
                    use std::io::Write;
                    let _ = std::io::stdout().flush();
                    std::process::exit(0);
                }
            }
        }
    }
}

fn save_png(path: &str, image: &egui::ColorImage) {
    let [w, h] = image.size;
    let mut bytes = Vec::with_capacity(w * h * 4);
    for px in &image.pixels {
        bytes.extend_from_slice(&px.to_array());
    }
    match image::RgbaImage::from_raw(w as u32, h as u32, bytes) {
        Some(img) => {
            if let Err(e) = img.save(path) {
                eprintln!("failed to save screenshot {path}: {e}");
            }
        }
        None => eprintln!("failed to build image buffer for {path}"),
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut config_path: Option<PathBuf> = None;
    let mut script = String::new();
    let mut list_and_exit = false;
    let mut sync_apps = false;
    let mut dump_cache = false;
    let mut db_path: Option<PathBuf> = None;
    let mut cache_interval: Option<u64> = None;
    let mut snap_host: Option<String> = None;
    let mut fetch_log_arg: Option<String> = None;
    let mut hide_arg: Option<String> = None;
    let mut unhide_arg: Option<String> = None;
    let mut dump_hidden_host: Option<String> = None;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--script" => {
                if let Some(s) = it.next() {
                    script = s.clone();
                }
            }
            "--list" => list_and_exit = true,
            "--sync-apps" => sync_apps = true,
            "--dump-cache" => dump_cache = true,
            "--snap" => {
                // Debug: take one snapshot of a host and print it.
                snap_host = it.next().cloned();
            }
            "--fetch-log" => {
                // Debug: fetch the progress log for HOST:CWD and print it.
                fetch_log_arg = it.next().cloned();
            }
            "--hide" => {
                // Debug: hide a session in the cache (HOST/NAME) and exit.
                hide_arg = it.next().cloned();
            }
            "--unhide" => {
                // Debug: unhide a session in the cache (HOST/NAME) and exit.
                unhide_arg = it.next().cloned();
            }
            "--dump-hidden" => {
                // Debug: print the persisted hidden set for HOST and exit.
                dump_hidden_host = it.next().cloned();
            }
            "--db" => db_path = it.next().map(PathBuf::from),
            "--cache-interval" => cache_interval = it.next().and_then(|s| s.parse().ok()),
            "--help" | "-h" => {
                println!(
                    "usage: tmuxmux [hosts.toml] [--list] [--dump-cache] [--db PATH]\n\
                     [--cache-interval SECS] [--hide HOST/NAME] [--unhide HOST/NAME]\n\
                     [--dump-hidden HOST] [--script 'step;;step;;...']\n\
                     script steps: sleep:MS attach:HOST/SESSION keys:TEXT shot:PATH\n\
                     select:R1,C1,R2,C2 copy paste print-selection print-clipboard\n\
                     newmodal:HOST modal-accept snapshot-now restore:HOST/NAME\n\
                     restoremodal:HOST/NAME restore-all:HOST dump-live:HOST\n\
                     dump-closed:HOST hide:HOST/NAME unhide:HOST/NAME\n\
                     dump-hidden:HOST quit"
                );
                return;
            }
            other if !other.starts_with("--") => config_path = Some(PathBuf::from(other)),
            other => eprintln!("unknown flag: {other}"),
        }
    }

    let path = match config::find_config_path(config_path) {
        Some(p) => p,
        None => {
            eprintln!("No hosts.toml found (cwd, beside binary, or ~/.config/tmuxmux/).");
            std::process::exit(1);
        }
    };
    let cfg = match config::load_config(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Config error: {e}");
            std::process::exit(1);
        }
    };
    log::info!("loaded {} hosts from {}", cfg.hosts.len(), path.display());

    if let Some(name) = snap_host {
        match cfg.hosts.iter().find(|h| h.name == name) {
            Some(host) => {
                let lc = cfg.log.clone().unwrap_or_default();
                let logf = if lc.enabled() { Some(lc.filename()) } else { None };
                let r = snapshot::take_snapshot(host.clone(), logf.as_deref());
                println!("error: {:?}", r.error);
                for s in r.sessions {
                    println!(
                        "session {} created={:?} has_log={}",
                        s.name, s.created_at, s.has_log
                    );
                    for p in s.panes {
                        println!(
                            "  {}:{} ({}) cwd={} cmd={} cmdline={:?} layout={}",
                            p.window_index,
                            p.pane_index,
                            p.window_name,
                            p.cwd,
                            p.command,
                            p.cmdline,
                            p.window_layout
                        );
                    }
                }
            }
            None => eprintln!("unknown host {name}"),
        }
        return;
    }

    if let Some(spec) = fetch_log_arg {
        // spec = HOST:CWD
        let (hname, cwd) = spec.split_once(':').unwrap_or(("localhost", spec.as_str()));
        let host = cfg
            .hosts
            .iter()
            .find(|h| h.name == hname)
            .cloned()
            .unwrap_or(config::Host {
                name: "localhost".into(),
                username: None,
                command: None,
                local: true,
                env: None,
                manager: None,
                category: None,
                status: None,
                closed: false,
            });
        let filename = cfg.log.as_ref().map(|l| l.filename()).unwrap_or_else(|| "PROGRESS.md".into());
        let r = progresslog::fetch_log(host, "debug".into(), cwd.to_string(), filename);
        println!("resolved={} path={:?} mtime={:?}", r.resolved, r.path, r.mtime);
        println!("--- content ---");
        print!("{}", r.content.unwrap_or_default());
        return;
    }

    if sync_apps {
        if cfg.app_managers.is_empty() {
            println!("no [[app_managers]] configured in {}", path.display());
        } else {
            let summary = appmanager::sync_blocking(&path, &cfg);
            for l in &summary.lines {
                println!("{l}");
            }
            println!(
                "wrote {} auto-managed host(s) to {}",
                summary.auto_hosts.len(),
                path.display()
            );
        }
        return;
    }

    if dump_cache {
        let path = db_path
            .or_else(|| {
                cfg.cache
                    .as_ref()
                    .and_then(|c| c.path.as_ref())
                    .map(PathBuf::from)
            })
            .unwrap_or_else(db::default_db_path);
        match db::Db::open(&path) {
            Ok(d) => print!("{}", d.dump()),
            Err(e) => eprintln!("cannot open {}: {e}", path.display()),
        }
        return;
    }

    // Headless hidden-session ops against the cache (no GUI, no display).
    if hide_arg.is_some() || unhide_arg.is_some() || dump_hidden_host.is_some() {
        let path = db_path
            .clone()
            .or_else(|| {
                cfg.cache
                    .as_ref()
                    .and_then(|c| c.path.as_ref())
                    .map(PathBuf::from)
            })
            .unwrap_or_else(db::default_db_path);
        match db::Db::open(&path) {
            Ok(d) => {
                if let Some(spec) = hide_arg.as_deref().and_then(|s| s.split_once('/')) {
                    d.hide_session(spec.0, spec.1);
                    println!("hid {}/{}", spec.0, spec.1);
                }
                if let Some(spec) = unhide_arg.as_deref().and_then(|s| s.split_once('/')) {
                    d.unhide_session(spec.0, spec.1);
                    println!("unhid {}/{}", spec.0, spec.1);
                }
                if let Some(h) = dump_hidden_host {
                    println!("HIDDEN:{h}>>>{}<<<", d.hidden_for_host(&h).join(","));
                }
            }
            Err(e) => eprintln!("cannot open {}: {e}", path.display()),
        }
        return;
    }

    if list_and_exit {
        let mut handles = Vec::new();
        for host in cfg.hosts.clone() {
            handles.push(std::thread::spawn(move || ssh::list_sessions(host)));
        }
        for h in handles {
            if let Ok(r) = h.join() {
                match r.error {
                    Some(e) => println!("{}: ERROR {}", r.host_name, e),
                    None => println!("{}: {}", r.host_name, r.sessions.join(", ")),
                }
            }
        }
        return;
    }

    let steps = parse_script(&script);

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 760.0])
            .with_title(format!("tmuxmux v{}", env!("CARGO_PKG_VERSION"))),
        renderer: eframe::Renderer::Glow,
        // vsync makes eglSwapBuffers block on a compositor frame callback —
        // which never arrives while the screen is locked/blanked or the
        // window is fully occluded, freezing the whole app (PTYs included).
        // We pace ourselves with request_repaint_after instead.
        vsync: false,
        ..Default::default()
    };

    if let Err(e) = eframe::run_native(
        "tmuxmux",
        native_options,
        Box::new(move |cc| {
            cc.egui_ctx.style_mut(|s| s.visuals = egui::Visuals::dark());
            Ok(Box::new(MainApp {
                inner: App::new(cfg, path.clone(), db_path, cache_interval),
                script: steps,
                script_idx: 0,
                script_wait_until: Instant::now(),
                pending_shot: false,
                last_active: Instant::now(),
            }))
        }),
    ) {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}
