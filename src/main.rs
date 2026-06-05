mod acs;
mod app;
mod colors;
mod config;
mod input;
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
///   modal-accept          accept the dialog (create + attach)
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
}

#[allow(deprecated)]
impl eframe::App for MainApp {
    fn ui(&mut self, _ui: &mut egui::Ui, _frame: &mut eframe::Frame) {}

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.inner.check_listing_results();
        self.inner.read_all_panes();

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

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(colors::DEFAULT_BG))
            .show(ctx, |ui| {
                self.inner.render_terminal(ui);
            });

        self.inner.render_modal(ctx);

        // Steady repaint while a terminal is attached or a script is driving.
        let scripting = self.script_idx < self.script.len() || self.pending_shot;
        if self.inner.focus == app::Focus::Terminal || scripting {
            ctx.request_repaint_after(Duration::from_millis(16));
        } else {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
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

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--script" => {
                if let Some(s) = it.next() {
                    script = s.clone();
                }
            }
            "--list" => list_and_exit = true,
            "--help" | "-h" => {
                println!(
                    "usage: tmuxmux [hosts.toml] [--list] [--script 'step;;step;;...']\n\
                     script steps: sleep:MS attach:HOST/SESSION keys:TEXT shot:PATH\n\
                     select:R1,C1,R2,C2 copy paste print-selection print-clipboard quit"
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
        ..Default::default()
    };

    if let Err(e) = eframe::run_native(
        "tmuxmux",
        native_options,
        Box::new(move |cc| {
            cc.egui_ctx.style_mut(|s| s.visuals = egui::Visuals::dark());
            Ok(Box::new(MainApp {
                inner: App::new(cfg),
                script: steps,
                script_idx: 0,
                script_wait_until: Instant::now(),
                pending_shot: false,
            }))
        }),
    ) {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}
