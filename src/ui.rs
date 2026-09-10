use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use anyhow::Result;
use eframe::egui::{self, Color32, RichText, Ui};
use tokio::runtime::Handle;
use tokio::sync::oneshot;

use crate::catalog::Catalog;
use crate::config::Opts;
use crate::deno::DenoRuntime;
use crate::install::{self, sidecar_path, DenoSource, DetectedDeno, InstallProgress};
use crate::listen::{self, HttpReady};
use crate::mcp_config::{self, StdioLaunch};
use crate::server::{ScriptMcp, ToolCallOutcome, ToolCallRecord};

pub fn run(opts: Opts) -> Result<()> {
    let handle = Handle::current();
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([640.0, 980.0])
            .with_min_inner_size([520.0, 700.0])
            .with_title("ScriptMCP"),
        ..Default::default()
    };
    eframe::run_native(
        "ScriptMCP",
        native_options,
        Box::new(move |_cc| Ok(Box::new(ScriptMcpApp::new(opts, handle)))),
    )
    .map_err(|error| anyhow::anyhow!("{error}"))
}

struct ScriptMcpApp {
    opts: Opts,
    rt: Handle,
    scripts: String,
    deno: Option<DetectedDeno>,
    sidecar: String,
    tools: Vec<ToolRow>,
    tools_status: String,
    install: InstallUi,
    http: HttpState,
    recent_calls: Vec<ToolCallRecord>,
    status: String,
}

struct ToolRow {
    name: String,
    path: String,
    description: String,
}

enum InstallUi {
    Idle,
    Running {
        message: String,
        done: u64,
        total: Option<u64>,
        rx: Receiver<InstallEvent>,
    },
    Failed(String),
}

enum InstallEvent {
    Progress(InstallProgress),
    Finished(Result<DetectedDeno, String>),
}

enum HttpState {
    Stopped,
    Starting {
        rx: oneshot::Receiver<Result<HttpReady, String>>,
    },
    Listening {
        addr: SocketAddr,
        server: ScriptMcp,
        generation: u64,
    },
    Failed(String),
}

impl ScriptMcpApp {
    fn new(opts: Opts, rt: Handle) -> Self {
        let scripts = opts.scripts.display().to_string();
        let sidecar = sidecar_path()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| "(cannot locate executable directory)".into());
        let deno = install::detect(&opts.deno);
        let mut app = Self {
            opts,
            rt,
            scripts,
            deno,
            sidecar,
            tools: Vec::new(),
            tools_status: String::new(),
            install: InstallUi::Idle,
            http: HttpState::Stopped,
            recent_calls: Vec::new(),
            status: String::new(),
        };
        app.refresh_tools();
        if app.deno.is_some() {
            app.start_http();
        }
        app
    }

    fn refresh_detect(&mut self) {
        self.opts.scripts = PathBuf::from(self.scripts.trim());
        self.deno = install::detect(&self.opts.deno);
        self.refresh_tools();
    }

    fn refresh_tools(&mut self) {
        self.opts.scripts = PathBuf::from(self.scripts.trim());
        if self.deno.is_none() {
            self.tools.clear();
            self.tools_status = "Install Deno to scan scripts.".into();
            return;
        }
        self.tools_status = "Scanning scripts…".into();
        let handle = self.rt.clone();
        let result = if let HttpState::Listening { server, .. } = &self.http {
            let server = server.clone();
            let dir = self.opts.scripts.clone();
            thread::spawn(move || handle.block_on(async { server.set_scripts_dir(dir).await }))
                .join()
                .unwrap_or_else(|_| Err(anyhow::anyhow!("script scan thread panicked")))
        } else {
            let opts = self.opts.clone();
            thread::spawn(move || {
                handle.block_on(async {
                    let runtime = DenoRuntime::new(&opts).await?;
                    Catalog::load(&opts.scripts, &runtime).await
                })
            })
            .join()
            .unwrap_or_else(|_| Err(anyhow::anyhow!("script scan thread panicked")))
        };

        match result {
            Ok(catalog) => {
                self.apply_catalog(&catalog);
                if let HttpState::Listening {
                    server, generation, ..
                } = &mut self.http
                {
                    *generation = server.generation();
                }
            }
            Err(error) => {
                self.tools.clear();
                self.tools_status = error.to_string();
            }
        }
    }

    fn apply_catalog(&mut self, catalog: &Catalog) {
        self.tools = catalog
            .tools()
            .iter()
            .map(|tool| ToolRow {
                name: tool.meta.name.clone(),
                path: tool.path.display().to_string(),
                description: tool.meta.description.clone(),
            })
            .collect();
        self.tools_status = if self.tools.is_empty() {
            format!("No scripts found in {}", self.opts.scripts.display())
        } else {
            format!("{} tool(s)", self.tools.len())
        };
    }

    fn start_install(&mut self) {
        if matches!(self.install, InstallUi::Running { .. }) {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.install = InstallUi::Running {
            message: "Starting Deno install…".into(),
            done: 0,
            total: None,
            rx,
        };
        thread::spawn(move || install_worker(tx));
    }

    fn start_http(&mut self) {
        if matches!(
            self.http,
            HttpState::Starting { .. } | HttpState::Listening { .. }
        ) {
            return;
        }
        if self.deno.is_none() {
            self.http = HttpState::Failed("Install Deno before starting the HTTP server.".into());
            return;
        }
        let (tx, rx) = oneshot::channel();
        let opts = self.opts.clone();
        self.rt.spawn(async move {
            let _ = listen::serve_http(&opts, Some(tx)).await;
        });
        self.http = HttpState::Starting { rx };
    }

    fn poll_install(&mut self, ctx: &egui::Context) {
        let InstallUi::Running { rx, .. } = &self.install else {
            return;
        };
        ctx.request_repaint();

        let mut message = None;
        let mut download = None;
        let mut finished = None;
        while let Ok(event) = rx.try_recv() {
            match event {
                InstallEvent::Progress(InstallProgress::Message(text)) => message = Some(text),
                InstallEvent::Progress(InstallProgress::Download { done, total }) => {
                    download = Some((done, total));
                }
                InstallEvent::Finished(result) => finished = Some(result),
            }
        }

        if let InstallUi::Running {
            message: current,
            done,
            total,
            ..
        } = &mut self.install
        {
            if let Some(text) = message {
                *current = text;
            }
            if let Some((d, t)) = download {
                *done = d;
                *total = t;
            }
        }

        if let Some(result) = finished {
            match result {
                Ok(detected) => {
                    self.status = format!(
                        "Installed {} ({})",
                        detected.path.display(),
                        detected.version
                    );
                    self.deno = Some(detected);
                    self.install = InstallUi::Idle;
                    self.refresh_tools();
                    self.start_http();
                }
                Err(error) => {
                    self.install = InstallUi::Failed(error);
                }
            }
        }
    }

    fn poll_http(&mut self, ctx: &egui::Context) {
        let HttpState::Starting { rx } = &mut self.http else {
            return;
        };
        ctx.request_repaint();
        match rx.try_recv() {
            Ok(Ok(ready)) => {
                self.apply_catalog(&ready.server.snapshot());
                self.recent_calls = ready.server.recent_calls();
                self.status = format!("MCP HTTP listening on {}", mcp_config::http_url(ready.addr));
                self.http = HttpState::Listening {
                    addr: ready.addr,
                    generation: ready.server.generation(),
                    server: ready.server,
                };
            }
            Ok(Err(error)) => {
                self.http = HttpState::Failed(error);
            }
            Err(oneshot::error::TryRecvError::Empty) => {}
            Err(oneshot::error::TryRecvError::Closed) => {
                self.http = HttpState::Failed("HTTP server exited before binding.".into());
            }
        }
    }

    fn poll_catalog(&mut self, ctx: &egui::Context) {
        let (current, calls, catalog) = {
            let HttpState::Listening {
                server, generation, ..
            } = &self.http
            else {
                return;
            };
            ctx.request_repaint_after(std::time::Duration::from_millis(300));
            let current = server.generation();
            let calls = server.recent_calls();
            let catalog = (current != *generation).then(|| server.snapshot());
            (current, calls, catalog)
        };
        self.recent_calls = calls;
        if let Some(catalog) = catalog {
            self.apply_catalog(&catalog);
            if let HttpState::Listening { generation, .. } = &mut self.http {
                *generation = current;
            }
        }
    }
}

fn install_worker(tx: Sender<InstallEvent>) {
    let result = install::install_sidecar(|progress| {
        let _ = tx.send(InstallEvent::Progress(progress));
    });
    let _ = tx.send(InstallEvent::Finished(
        result.map_err(|error| error.to_string()),
    ));
}

impl eframe::App for ScriptMcpApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_install(ctx);
        self.poll_http(ctx);
        self.poll_catalog(ctx);

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(8.0);
            ui.heading("ScriptMCP");
            ui.label(
                RichText::new("Deno scripts, exposed as MCP tools")
                    .color(ui.visuals().weak_text_color()),
            );
            ui.add_space(12.0);
            deno_panel(self, ui);
            ui.add_space(12.0);
            scripts_panel(self, ui);
            ui.add_space(12.0);
            tools_panel(self, ui);
            ui.add_space(12.0);
            calls_panel(self, ui);
            ui.add_space(12.0);
            mcp_panel(self, ui);
            if !self.status.is_empty() {
                ui.add_space(8.0);
                ui.label(RichText::new(&self.status).color(Color32::from_rgb(80, 170, 110)));
            }
        });
    }
}

fn deno_panel(app: &mut ScriptMcpApp, ui: &mut Ui) {
    ui.push_id("deno_panel", |ui| {
        ui.group(|ui| {
            ui.label(RichText::new("Deno runtime").strong());
            ui.add_space(4.0);
            match &app.deno {
                Some(deno) => {
                    ui.horizontal(|ui| {
                        ui.colored_label(Color32::from_rgb(80, 170, 110), "Installed");
                        ui.label(deno.version.clone());
                    });
                    ui.label(format!("{} · {}", deno.path.display(), deno.source.label()));
                }
                None => {
                    ui.colored_label(Color32::from_rgb(210, 90, 80), "Deno is not installed");
                    ui.label("ScriptMCP can download the official Deno binary into:");
                    ui.monospace(&app.sidecar);
                }
            }
            ui.add_space(6.0);

            let reinstall = app
                .deno
                .as_ref()
                .is_some_and(|deno| deno.source == DenoSource::Sidecar);
            let failed = if let InstallUi::Failed(error) = &app.install {
                Some(error.clone())
            } else {
                None
            };
            let running = if let InstallUi::Running {
                message,
                done,
                total,
                ..
            } = &app.install
            {
                Some((message.clone(), *done, *total))
            } else {
                None
            };

            if let Some((message, done, total)) = running {
                ui.label(message);
                if let Some(total) = total {
                    if total > 0 {
                        ui.add(
                            egui::ProgressBar::new(done as f32 / total as f32)
                                .text(format!("{done} / {total} bytes")),
                        );
                    }
                } else if done > 0 {
                    ui.label(format!("Downloaded {done} bytes"));
                }
            } else if let Some(error) = failed {
                ui.colored_label(Color32::from_rgb(210, 90, 80), error);
                if ui.button("Try again").clicked() {
                    app.start_install();
                }
            } else if ui
                .button(if reinstall {
                    "Reinstall Deno here"
                } else {
                    "Install Deno here"
                })
                .clicked()
            {
                app.start_install();
            }
        });
    });
}

fn scripts_panel(app: &mut ScriptMcpApp, ui: &mut Ui) {
    ui.push_id("scripts_panel", |ui| {
        ui.group(|ui| {
            ui.label(RichText::new("Scripts directory").strong());
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let response = ui.add(
                    egui::TextEdit::singleline(&mut app.scripts)
                        .id_salt("scripts_path")
                        .desired_width(360.0)
                        .hint_text("path to TypeScript/JavaScript tools"),
                );
                if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    app.refresh_detect();
                }
                if ui.button("Browse…").clicked() {
                    if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                        app.scripts = dir.display().to_string();
                        app.refresh_detect();
                    }
                }
                if ui.button("Scan").clicked() {
                    app.refresh_detect();
                }
            });
        });
    });
}

fn tools_panel(app: &ScriptMcpApp, ui: &mut Ui) {
    ui.push_id("tools_panel", |ui| {
        ui.group(|ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Tools").strong());
                ui.label(RichText::new(&app.tools_status).color(ui.visuals().weak_text_color()));
            });
            ui.add_space(4.0);
            if app.tools.is_empty() {
                ui.label("No tools loaded.");
                return;
            }
            egui::ScrollArea::vertical()
                .id_salt("tools_scroll")
                .max_height(140.0)
                .show(ui, |ui| {
                    for tool in &app.tools {
                        ui.push_id(&tool.name, |ui| {
                            ui.horizontal(|ui| {
                                ui.strong(&tool.name);
                                ui.label(
                                    RichText::new(&tool.description)
                                        .color(ui.visuals().weak_text_color()),
                                );
                            });
                            ui.small(&tool.path);
                            ui.add_space(2.0);
                        });
                    }
                });
        });
    });
}

fn calls_panel(app: &ScriptMcpApp, ui: &mut Ui) {
    ui.push_id("calls_panel", |ui| {
        ui.group(|ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Recent tool calls").strong());
                ui.label(RichText::new("last 10").color(ui.visuals().weak_text_color()));
            });
            ui.add_space(4.0);
            if !matches!(app.http, HttpState::Listening { .. }) {
                ui.label("Start the HTTP server to record tool calls.");
                return;
            }
            if app.recent_calls.is_empty() {
                ui.label("No tool calls yet.");
                return;
            }
            egui::ScrollArea::vertical()
                .id_salt("calls_scroll")
                .max_height(180.0)
                .show(ui, |ui| {
                    for (index, call) in app.recent_calls.iter().enumerate() {
                        ui.push_id(index, |ui| {
                            ui.horizontal(|ui| {
                                ui.small(call.clock());
                                ui.strong(&call.name);
                                match &call.outcome {
                                    ToolCallOutcome::Success { .. } => {
                                        ui.colored_label(Color32::from_rgb(80, 170, 110), "ok");
                                    }
                                    ToolCallOutcome::Error { .. } => {
                                        ui.colored_label(Color32::from_rgb(210, 90, 80), "error");
                                    }
                                    ToolCallOutcome::UnknownTool => {
                                        ui.colored_label(Color32::from_rgb(210, 90, 80), "unknown");
                                    }
                                }
                            });
                            ui.small(format!("in  {}", call.arguments));
                            match &call.outcome {
                                ToolCallOutcome::Success { preview } => {
                                    ui.small(format!("out {preview}"));
                                }
                                ToolCallOutcome::Error { message } => {
                                    ui.small(
                                        RichText::new(format!("out {message}"))
                                            .color(Color32::from_rgb(210, 90, 80)),
                                    );
                                }
                                ToolCallOutcome::UnknownTool => {}
                            }
                            ui.add_space(4.0);
                        });
                    }
                });
        });
    });
}

fn mcp_panel(app: &mut ScriptMcpApp, ui: &mut Ui) {
    ui.push_id("mcp_panel", |ui| {
        ui.group(|ui| {
            ui.label(RichText::new("Connect as an MCP server").strong());
            ui.label(
                RichText::new(
                    "Any MCP client can connect over Streamable HTTP on localhost, or spawn this process over stdio.",
                )
                .color(ui.visuals().weak_text_color()),
            );
            ui.add_space(8.0);

            if app.deno.is_none() {
                ui.colored_label(
                    Color32::from_rgb(210, 90, 80),
                    "Install Deno before connecting a client.",
                );
                ui.add_space(8.0);
            }

            http_section(app, ui);
            ui.add_space(10.0);
            stdio_section(app, ui);
        });
    });
}

fn http_section(app: &mut ScriptMcpApp, ui: &mut Ui) {
    ui.label(RichText::new("HTTP (Streamable HTTP)").strong());
    match &app.http {
        HttpState::Stopped => {
            ui.label("Not listening.");
            if ui.button("Start HTTP server").clicked() {
                app.start_http();
            }
        }
        HttpState::Starting { .. } => {
            ui.label("Binding localhost…");
        }
        HttpState::Listening { addr, .. } => {
            ui.horizontal(|ui| {
                ui.colored_label(Color32::from_rgb(80, 170, 110), "Listening");
                ui.monospace(mcp_config::http_url(*addr));
            });
            ui.label(
                RichText::new(
                    "Point an MCP client at this URL. Script changes notify connected clients.",
                )
                .color(ui.visuals().weak_text_color()),
            );
        }
        HttpState::Failed(error) => {
            ui.colored_label(Color32::from_rgb(210, 90, 80), error);
            if ui.button("Retry HTTP").clicked() {
                app.start_http();
            }
        }
    }

    let url = match &app.http {
        HttpState::Listening { addr, .. } => mcp_config::http_url(*addr),
        _ => match app.opts.socket_addr() {
            Ok(addr) => mcp_config::http_url(addr),
            Err(_) => format!("http://{}/mcp", app.opts.bind),
        },
    };
    let snippet = mcp_config::http_json(&url);
    ui.add_space(6.0);
    field_row(ui, "URL", &url);
    json_block(ui, "http_json", &snippet);
}

fn stdio_section(app: &ScriptMcpApp, ui: &mut Ui) {
    ui.label(RichText::new("stdio").strong());
    ui.label(
        RichText::new("The client launches ScriptMCP and talks over stdin/stdout.")
            .color(ui.visuals().weak_text_color()),
    );
    let launch = StdioLaunch::from_opts(&app.opts, app.deno.as_ref());
    ui.add_space(4.0);
    field_row(ui, "Command", &launch.command);
    field_row(ui, "Arguments", &launch.args_line());
    ui.add_space(6.0);
    json_block(ui, "stdio_json", &launch.pretty_json());
}

fn field_row(ui: &mut Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.monospace(value);
        copy_button(ui, label, "Copy", value.to_string());
    });
}

fn json_block(ui: &mut Ui, id: &str, snippet: &str) {
    egui::ScrollArea::vertical()
        .id_salt(id)
        .max_height(120.0)
        .show(ui, |ui| {
            ui.monospace(snippet);
        });
    ui.horizontal(|ui| {
        copy_button(ui, &format!("copy_{id}"), "Copy JSON", snippet.to_string());
    });
}

fn copy_button(ui: &mut Ui, id: &str, label: &str, text: impl Into<String>) {
    let text = text.into();
    ui.push_id(id, |ui| {
        if ui.button(label).clicked() {
            ui.ctx().copy_text(text);
        }
    });
}
