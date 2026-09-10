use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use anyhow::Result;
use eframe::egui::{self, Color32, RichText, Ui};
use tokio::runtime::Handle;
use tokio::sync::oneshot;

use crate::app_state::{normalize_path, AppState};
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
            .with_inner_size([980.0, 820.0])
            .with_min_inner_size([780.0, 640.0])
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
    state: AppState,
    folders: Vec<PathBuf>,
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
    path: PathBuf,
    path_label: String,
    description: String,
    enabled: bool,
    conflict: bool,
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
        let mut state = AppState::load();
        if state.folders.is_empty() {
            state.add_folder(opts.scripts.clone());
            let _ = state.save();
        }
        let folders = state
            .folders
            .iter()
            .map(|path| normalize_path(path))
            .collect();
        let sidecar = sidecar_path()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| "(cannot locate executable directory)".into());
        let deno = install::detect(&opts.deno);
        let mut app = Self {
            opts,
            rt,
            state,
            folders,
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

    fn persist_folders(&mut self) {
        self.state.folders = self.folders.clone();
        if let Err(error) = self.state.save() {
            self.status = format!("Failed to save config: {error}");
        }
    }

    fn add_folder(&mut self, path: PathBuf) {
        let normalized = normalize_path(&path);
        if self.folders.iter().any(|existing| existing == &normalized) {
            self.status = format!("Already added: {}", normalized.display());
            return;
        }
        self.folders.push(normalized.clone());
        self.state.add_folder(normalized);
        let _ = self.state.save();
        self.refresh_tools();
    }

    fn remove_folder(&mut self, index: usize) {
        if index >= self.folders.len() {
            return;
        }
        let removed = self.folders.remove(index);
        self.state.remove_folder(&removed);
        let _ = self.state.save();
        if self.folders.is_empty() {
            let fallback = normalize_path(&self.opts.scripts);
            self.folders.push(fallback.clone());
            self.state.add_folder(fallback);
            let _ = self.state.save();
        }
        self.refresh_tools();
    }

    fn refresh_tools(&mut self) {
        if self.deno.is_none() {
            self.tools.clear();
            self.tools_status = "Install Deno to scan scripts.".into();
            return;
        }
        self.tools_status = "Scanning scripts…".into();
        let handle = self.rt.clone();
        let folders = self.folders.clone();
        let state = self.state.clone();
        let result = if let HttpState::Listening { server, .. } = &self.http {
            let server = server.clone();
            thread::spawn(move || handle.block_on(async { server.set_folders(folders).await }))
                .join()
                .unwrap_or_else(|_| Err(anyhow::anyhow!("script scan thread panicked")))
        } else {
            let opts = self.opts.clone();
            thread::spawn(move || {
                handle.block_on(async {
                    let runtime = DenoRuntime::new(&opts).await?;
                    Catalog::load_dirs(&folders, &runtime, Some(&state)).await
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
                    self.state = server.app_state();
                }
            }
            Err(error) => {
                self.tools.clear();
                self.tools_status = error.to_string();
            }
        }
    }

    fn toggle_tool(&mut self, path: PathBuf, enabled: bool) {
        let handle = self.rt.clone();
        let result = if let HttpState::Listening { server, .. } = &self.http {
            let server = server.clone();
            thread::spawn(move || {
                handle.block_on(async { server.set_tool_enabled(path, enabled).await })
            })
            .join()
            .unwrap_or_else(|_| Err(anyhow::anyhow!("tool toggle thread panicked")))
        } else {
            let name = self
                .tools
                .iter()
                .find(|tool| tool.path == path)
                .map(|tool| tool.name.clone());
            if let Some(name) = name {
                if enabled {
                    for tool in &self.tools {
                        if tool.name == name {
                            self.state.set_enabled(&tool.path, tool.path == path);
                        }
                    }
                } else {
                    self.state.set_enabled(&path, false);
                }
                let _ = self.state.save();
            }
            let opts = self.opts.clone();
            let folders = self.folders.clone();
            let state = self.state.clone();
            thread::spawn(move || {
                handle.block_on(async {
                    let runtime = DenoRuntime::new(&opts).await?;
                    Catalog::load_dirs(&folders, &runtime, Some(&state)).await
                })
            })
            .join()
            .unwrap_or_else(|_| Err(anyhow::anyhow!("tool toggle thread panicked")))
        };

        match result {
            Ok(catalog) => {
                self.apply_catalog(&catalog);
                if let HttpState::Listening {
                    server, generation, ..
                } = &mut self.http
                {
                    *generation = server.generation();
                    self.state = server.app_state();
                }
            }
            Err(error) => {
                self.status = error.to_string();
            }
        }
    }

    fn apply_catalog(&mut self, catalog: &Catalog) {
        let mut name_counts = std::collections::HashMap::<&str, usize>::new();
        for tool in catalog.tools() {
            *name_counts.entry(tool.meta.name.as_str()).or_default() += 1;
        }
        self.tools = catalog
            .tools()
            .iter()
            .map(|tool| ToolRow {
                name: tool.meta.name.clone(),
                path: tool.path.clone(),
                path_label: tool.path.display().to_string(),
                description: tool.meta.description.clone(),
                enabled: tool.enabled,
                conflict: name_counts.get(tool.meta.name.as_str()).copied().unwrap_or(0) > 1,
            })
            .collect();
        let enabled = self.tools.iter().filter(|tool| tool.enabled).count();
        self.tools_status = if self.tools.is_empty() {
            "No scripts found in the configured folders".into()
        } else {
            format!("{} tool(s), {} exposed", self.tools.len(), enabled)
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
        self.persist_folders();
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
                self.state = ready.server.app_state();
                self.folders = ready.server.folders();
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
        let (current, calls, catalog, state, folders) = {
            let HttpState::Listening {
                server, generation, ..
            } = &self.http
            else {
                return;
            };
            ctx.request_repaint_after(std::time::Duration::from_millis(300));
            let current = server.generation();
            let calls = server.recent_calls();
            let changed = current != *generation;
            let catalog = changed.then(|| server.snapshot());
            let state = changed.then(|| server.app_state());
            let folders = changed.then(|| server.folders());
            (current, calls, catalog, state, folders)
        };
        self.recent_calls = calls;
        if let Some(catalog) = catalog {
            self.apply_catalog(&catalog);
            if let Some(state) = state {
                self.state = state;
            }
            if let Some(folders) = folders {
                self.folders = folders;
            }
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

            let available = ui.available_width();
            let left_width = (available * 0.58).max(420.0);
            ui.horizontal_top(|ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(left_width, ui.available_height()),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        deno_panel(self, ui);
                        ui.add_space(12.0);
                        scripts_panel(self, ui);
                        ui.add_space(12.0);
                        tools_panel(self, ui);
                        ui.add_space(12.0);
                        mcp_panel(self, ui);
                        if !self.status.is_empty() {
                            ui.add_space(8.0);
                            ui.label(
                                RichText::new(&self.status).color(Color32::from_rgb(80, 170, 110)),
                            );
                        }
                    },
                );
                ui.add_space(12.0);
                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), ui.available_height()),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        calls_panel(self, ui);
                    },
                );
            });
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
            ui.horizontal(|ui| {
                ui.label(RichText::new("Script folders").strong());
                ui.label(
                    RichText::new(AppState::config_path().display().to_string())
                        .small()
                        .color(ui.visuals().weak_text_color()),
                );
            });
            ui.add_space(4.0);

            let mut remove_index = None;
            for (index, folder) in app.folders.iter().enumerate() {
                ui.push_id(index, |ui| {
                    ui.horizontal(|ui| {
                        ui.monospace(folder.display().to_string());
                        if ui.small_button("Remove").clicked() {
                            remove_index = Some(index);
                        }
                    });
                });
            }

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("Add folder…").clicked() {
                    if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                        app.add_folder(dir);
                    }
                }
                if ui.button("Scan").clicked() {
                    app.refresh_tools();
                }
            });

            if let Some(index) = remove_index {
                app.remove_folder(index);
            }
        });
    });
}

fn tools_panel(app: &mut ScriptMcpApp, ui: &mut Ui) {
    ui.push_id("tools_panel", |ui| {
        ui.group(|ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Tools").strong());
                ui.label(RichText::new(&app.tools_status).color(ui.visuals().weak_text_color()));
            });
            ui.label(
                RichText::new("Check a tool to expose it over MCP. Duplicate names are mutually exclusive.")
                    .small()
                    .color(ui.visuals().weak_text_color()),
            );
            ui.add_space(4.0);
            if app.tools.is_empty() {
                ui.label("No tools loaded.");
                return;
            }

            let mut toggle = None;
            egui::ScrollArea::vertical()
                .id_salt("tools_scroll")
                .max_height(220.0)
                .show(ui, |ui| {
                    for (index, tool) in app.tools.iter().enumerate() {
                        ui.push_id(index, |ui| {
                            ui.horizontal(|ui| {
                                let mut enabled = tool.enabled;
                                if ui.checkbox(&mut enabled, "").changed() {
                                    toggle = Some((tool.path.clone(), enabled));
                                }
                                ui.strong(&tool.name);
                                if tool.conflict {
                                    ui.colored_label(
                                        Color32::from_rgb(180, 140, 60),
                                        "duplicate",
                                    );
                                }
                                ui.label(
                                    RichText::new(&tool.description)
                                        .color(ui.visuals().weak_text_color()),
                                );
                            });
                            ui.small(&tool.path_label);
                            ui.add_space(2.0);
                        });
                    }
                });

            if let Some((path, enabled)) = toggle {
                app.toggle_tool(path, enabled);
            }
        });
    });
}

fn calls_panel(app: &ScriptMcpApp, ui: &mut Ui) {
    ui.push_id("calls_panel", |ui| {
        ui.group(|ui| {
            ui.set_min_width(280.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("MCP activity").strong());
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
                .auto_shrink([false, false])
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
        RichText::new(
            "The client launches ScriptMCP and talks over stdin/stdout. Folders and exposed tools load from the saved config.",
        )
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
