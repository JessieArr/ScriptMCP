use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use anyhow::Result;
use eframe::egui::{self, Color32, RichText, TextWrapMode, Ui};
use serde_json::Value;
use tokio::runtime::Handle;
use tokio::sync::oneshot;

use crate::app_state::{normalize_path, AppState};
use crate::catalog::Catalog;
use crate::config::Opts;
use crate::deno::DenoRuntime;
use crate::install::{self, sidecar_path, DenoSource, DetectedDeno, InstallProgress};
use crate::listen::{self, HttpReady};
use crate::mcp_config::{self, StdioLaunch};
use crate::server::{self, ScriptMcp, ToolCallOutcome, ToolCallRecord};

pub fn run(opts: Opts) -> Result<()> {
    let handle = Handle::current();
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([720.0, 780.0])
            .with_min_inner_size([400.0, 360.0])
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
    tab: AppTab,
    catalog: Catalog,
    debug: DebugUi,
    history_detail: Option<ToolCallRecord>,
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum AppTab {
    #[default]
    Status,
    Tools,
    Debug,
    History,
}

struct DebugUi {
    tool_path: Option<PathBuf>,
    fields: Vec<DebugField>,
    response: DebugResponse,
    invoke: DebugInvoke,
}

impl Default for DebugUi {
    fn default() -> Self {
        Self {
            tool_path: None,
            fields: Vec::new(),
            response: DebugResponse::None,
            invoke: DebugInvoke::Idle,
        }
    }
}

impl DebugUi {
    fn apply_schema(&mut self, schema: &Value, keep_values: bool) {
        let previous = keep_values.then(|| {
            self.fields
                .iter()
                .cloned()
                .map(|field| (field.name.clone(), field))
                .collect::<HashMap<_, _>>()
        });
        self.fields = debug_fields_from_schema(schema);
        if let Some(previous) = previous {
            for field in &mut self.fields {
                if let Some(old) = previous.get(&field.name) {
                    if old.kind == field.kind {
                        field.text = old.text.clone();
                        field.checked = old.checked;
                    }
                }
            }
        }
    }
}

#[derive(Clone)]
struct DebugField {
    name: String,
    required: bool,
    description: String,
    kind: DebugFieldKind,
    text: String,
    checked: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DebugFieldKind {
    String,
    Integer,
    Number,
    Boolean,
    Json,
}

enum DebugInvoke {
    Idle,
    Running { rx: oneshot::Receiver<DebugOutcome> },
}

#[derive(Clone)]
enum DebugResponse {
    None,
    Running,
    Success(String),
    Error(String),
}

struct DebugOutcome {
    success: bool,
    body: String,
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
            tab: AppTab::Status,
            catalog: Catalog::default(),
            debug: DebugUi::default(),
            history_detail: None,
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
        self.catalog = catalog.clone();
        if let Some(path) = &self.debug.tool_path {
            if let Some(tool) = catalog.tools().iter().find(|tool| &tool.path == path) {
                self.debug.apply_schema(&tool.meta.input_schema, true);
            } else {
                self.debug.tool_path = None;
                self.debug.fields.clear();
            }
        }
        let mut name_counts = HashMap::<&str, usize>::new();
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
                conflict: name_counts
                    .get(tool.meta.name.as_str())
                    .copied()
                    .unwrap_or(0)
                    > 1,
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

    fn poll_debug(&mut self, ctx: &egui::Context) {
        let DebugInvoke::Running { rx } = &mut self.debug.invoke else {
            return;
        };
        ctx.request_repaint();
        match rx.try_recv() {
            Ok(outcome) => {
                self.debug.response = if outcome.success {
                    DebugResponse::Success(outcome.body)
                } else {
                    DebugResponse::Error(outcome.body)
                };
                self.debug.invoke = DebugInvoke::Idle;
            }
            Err(oneshot::error::TryRecvError::Empty) => {}
            Err(oneshot::error::TryRecvError::Closed) => {
                self.debug.response =
                    DebugResponse::Error("Invoke exited before returning.".into());
                self.debug.invoke = DebugInvoke::Idle;
            }
        }
    }

    fn start_debug_invoke(&mut self) {
        if matches!(self.debug.invoke, DebugInvoke::Running { .. }) {
            return;
        }
        if self.deno.is_none() {
            self.debug.response =
                DebugResponse::Error("Install Deno before invoking a tool.".into());
            return;
        }
        let Some(path) = self.debug.tool_path.clone() else {
            self.debug.response = DebugResponse::Error("Select a tool to invoke.".into());
            return;
        };
        let arguments = match collect_debug_args(&self.debug.fields) {
            Ok(value) => value,
            Err(error) => {
                self.debug.response = DebugResponse::Error(error);
                return;
            }
        };

        let (tx, rx) = oneshot::channel();
        self.debug.invoke = DebugInvoke::Running { rx };
        self.debug.response = DebugResponse::Running;

        if let HttpState::Listening { server, .. } = &self.http {
            let server = server.clone();
            self.rt.spawn(async move {
                let _ = tx.send(debug_outcome(server.invoke_script(&path, arguments).await));
            });
            return;
        }

        let Some(tool) = self
            .catalog
            .tools()
            .iter()
            .find(|tool| tool.path == path)
            .cloned()
        else {
            self.debug.invoke = DebugInvoke::Idle;
            self.debug.response = DebugResponse::Error("Selected tool is no longer loaded.".into());
            return;
        };
        let opts = self.opts.clone();
        self.rt.spawn(async move {
            let result = async {
                let runtime = DenoRuntime::new(&opts).await?;
                runtime
                    .invoke(
                        &tool.path,
                        &tool.meta.permissions,
                        &tool.source_dir,
                        &arguments,
                    )
                    .await
            }
            .await;
            let _ = tx.send(debug_outcome(result));
        });
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
        self.poll_debug(ctx);

        ctx.style_mut(|style| {
            style.wrap_mode = Some(TextWrapMode::Wrap);
        });

        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            ui.set_max_width(ui.available_width());
            ui.add_space(6.0);
            ui.heading("ScriptMCP");
            wrap_label(
                ui,
                RichText::new("Deno scripts, exposed as MCP tools")
                    .color(ui.visuals().weak_text_color()),
            );
            if !self.status.is_empty() {
                wrap_label(
                    ui,
                    RichText::new(&self.status).color(Color32::from_rgb(80, 170, 110)),
                );
            }
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                ui.selectable_value(&mut self.tab, AppTab::Status, "Status");
                ui.selectable_value(
                    &mut self.tab,
                    AppTab::Tools,
                    format!("Tools ({})", self.tools.len()),
                );
                ui.selectable_value(&mut self.tab, AppTab::Debug, "Debug");
                ui.selectable_value(
                    &mut self.tab,
                    AppTab::History,
                    format!("History ({})", self.recent_calls.len()),
                );
            });
            ui.add_space(4.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.set_max_width(ui.available_width());
            ui.with_layout(
                egui::Layout::top_down_justified(egui::Align::Min),
                |ui| match self.tab {
                    AppTab::Status => status_tab(self, ui),
                    AppTab::Tools => tools_tab(self, ui),
                    AppTab::Debug => debug_tab(self, ui),
                    AppTab::History => history_tab(self, ui),
                },
            );
        });

        history_detail_modal(self, ctx);
    }
}

fn status_tab(app: &mut ScriptMcpApp, ui: &mut Ui) {
    egui::ScrollArea::vertical()
        .id_salt("status_scroll")
        .auto_shrink([false, false])
        .hscroll(false)
        .show(ui, |ui| {
            ui.set_max_width(ui.available_width());
            ui.with_layout(egui::Layout::top_down_justified(egui::Align::Min), |ui| {
                deno_panel(app, ui);
                ui.add_space(12.0);
                mcp_panel(app, ui);
            });
        });
}

fn tools_tab(app: &mut ScriptMcpApp, ui: &mut Ui) {
    scripts_panel(app, ui);
    ui.add_space(12.0);
    tools_panel(app, ui);
}

fn debug_tab(app: &mut ScriptMcpApp, ui: &mut Ui) {
    egui::ScrollArea::vertical()
        .id_salt("debug_scroll")
        .auto_shrink([false, false])
        .hscroll(false)
        .show(ui, |ui| {
            ui.set_max_width(ui.available_width());
            ui.with_layout(egui::Layout::top_down_justified(egui::Align::Min), |ui| {
                debug_invoke_panel(app, ui);
                ui.add_space(12.0);
                debug_response_panel(app, ui);
            });
        });
}

fn history_tab(app: &mut ScriptMcpApp, ui: &mut Ui) {
    calls_panel(app, ui);
}

fn debug_invoke_panel(app: &mut ScriptMcpApp, ui: &mut Ui) {
    ui.push_id("debug_invoke_panel", |ui| {
        panel_group(ui, |ui| {
            ui.label(RichText::new("Invoke a tool").strong());
            wrap_label(
                ui,
                RichText::new("Pick any loaded script, fill in its inputs, and run it locally.")
                    .color(ui.visuals().weak_text_color()),
            );
            ui.add_space(6.0);

            if app.deno.is_none() {
                wrap_label(
                    ui,
                    RichText::new("Install Deno before invoking a tool.")
                        .color(Color32::from_rgb(210, 90, 80)),
                );
            }

            if app.tools.is_empty() {
                wrap_label(ui, "Scan scripts on the Tools tab first.");
                return;
            }

            let selected_label = app
                .debug
                .tool_path
                .as_ref()
                .and_then(|path| app.tools.iter().find(|tool| &tool.path == path))
                .map(debug_tool_label)
                .unwrap_or_else(|| "Select a tool…".into());
            let mut selected = app.debug.tool_path.clone();
            egui::ComboBox::from_id_salt("debug_tool")
                .width(ui.available_width())
                .selected_text(selected_label)
                .show_ui(ui, |ui| {
                    for tool in &app.tools {
                        ui.selectable_value(
                            &mut selected,
                            Some(tool.path.clone()),
                            debug_tool_label(tool),
                        );
                    }
                });
            if selected != app.debug.tool_path {
                app.debug.tool_path = selected;
                let schema = app
                    .catalog
                    .tools()
                    .iter()
                    .find(|tool| Some(&tool.path) == app.debug.tool_path.as_ref())
                    .map(|tool| tool.meta.input_schema.clone())
                    .unwrap_or_else(|| serde_json::json!({ "type": "object", "properties": {} }));
                app.debug.apply_schema(&schema, false);
                app.debug.response = DebugResponse::None;
            }

            if let Some(tool) = app
                .catalog
                .tools()
                .iter()
                .find(|tool| Some(&tool.path) == app.debug.tool_path.as_ref())
            {
                if !tool.meta.description.is_empty() {
                    wrap_label(
                        ui,
                        RichText::new(&tool.meta.description).color(ui.visuals().weak_text_color()),
                    );
                }
                wrap_label(
                    ui,
                    RichText::new(tool.path.display().to_string())
                        .small()
                        .monospace(),
                );
                if !tool.enabled {
                    wrap_label(
                        ui,
                        RichText::new(
                            "This tool is not exposed over MCP, but you can still run it here.",
                        )
                        .small()
                        .color(ui.visuals().weak_text_color()),
                    );
                }
            }

            ui.add_space(8.0);
            debug_input_fields(app, ui);
            ui.add_space(6.0);

            let running = matches!(app.debug.invoke, DebugInvoke::Running { .. });
            let can_run = app.deno.is_some() && app.debug.tool_path.is_some() && !running;
            let button = ui.add_enabled(
                can_run,
                egui::Button::new(if running { "Running…" } else { "Invoke" }),
            );
            if button.clicked() {
                app.start_debug_invoke();
            }
        });
    });
}

fn debug_response_panel(app: &ScriptMcpApp, ui: &mut Ui) {
    ui.push_id("debug_response_panel", |ui| {
        panel_group(ui, |ui| {
            ui.label(RichText::new("Response").strong());
            ui.add_space(4.0);
            let (text, color) = match &app.debug.response {
                DebugResponse::None => (
                    "Invoke a tool to see the full result here.".to_string(),
                    ui.visuals().weak_text_color(),
                ),
                DebugResponse::Running => ("Running…".to_string(), ui.visuals().weak_text_color()),
                DebugResponse::Success(body) => (body.clone(), ui.visuals().text_color()),
                DebugResponse::Error(body) => (body.clone(), Color32::from_rgb(210, 90, 80)),
            };
            egui::ScrollArea::vertical()
                .id_salt("debug_response")
                .max_height(260.0)
                .auto_shrink([false, true])
                .hscroll(false)
                .show(ui, |ui| {
                    ui.set_max_width(ui.available_width());
                    formatted_output(ui, &text, color);
                });
        });
    });
}

fn debug_tool_label(tool: &ToolRow) -> String {
    if tool.conflict {
        format!("{} — {}", tool.name, tool.path_label)
    } else {
        tool.name.clone()
    }
}

fn debug_input_fields(app: &mut ScriptMcpApp, ui: &mut Ui) {
    if app.debug.tool_path.is_none() {
        return;
    }
    ui.label(RichText::new("Inputs").strong());
    if app.debug.fields.is_empty() {
        wrap_label(
            ui,
            RichText::new("This tool has no inputs.").color(ui.visuals().weak_text_color()),
        );
        return;
    }

    let width = ui.available_width();
    let weak = ui.visuals().weak_text_color();
    for field in &mut app.debug.fields {
        ui.push_id(&field.name, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.strong(&field.name);
                if !field.required {
                    ui.label(RichText::new("(optional)").color(weak));
                }
            });
            if !field.description.is_empty() {
                wrap_label(ui, RichText::new(&field.description).small().color(weak));
            }
            match field.kind {
                DebugFieldKind::Boolean => {
                    let label = if field.checked { "true" } else { "false" };
                    ui.checkbox(&mut field.checked, label);
                }
                DebugFieldKind::Json => {
                    ui.add(
                        egui::TextEdit::multiline(&mut field.text)
                            .desired_width(width)
                            .desired_rows(3)
                            .code_editor()
                            .font(egui::TextStyle::Monospace)
                            .hint_text(if field.required {
                                "JSON value"
                            } else {
                                "JSON value, or leave empty"
                            }),
                    );
                }
                DebugFieldKind::String => {
                    ui.add(
                        egui::TextEdit::singleline(&mut field.text)
                            .desired_width(width)
                            .hint_text(if field.required {
                                ""
                            } else {
                                "leave empty to omit"
                            }),
                    );
                }
                DebugFieldKind::Integer | DebugFieldKind::Number => {
                    ui.add(
                        egui::TextEdit::singleline(&mut field.text)
                            .desired_width(width)
                            .hint_text(if field.required {
                                "number"
                            } else {
                                "number, or leave empty"
                            }),
                    );
                }
            }
            ui.add_space(8.0);
        });
    }
}

fn deno_panel(app: &mut ScriptMcpApp, ui: &mut Ui) {
    ui.push_id("deno_panel", |ui| {
        panel_group(ui, |ui| {
            ui.label(RichText::new("Deno runtime").strong());
            ui.add_space(4.0);
            match &app.deno {
                Some(deno) => {
                    ui.horizontal_wrapped(|ui| {
                        ui.colored_label(Color32::from_rgb(80, 170, 110), "Installed");
                        ui.label(deno.version.clone());
                    });
                    wrap_label(
                        ui,
                        RichText::new(format!("{} · {}", deno.path.display(), deno.source.label()))
                            .monospace(),
                    );
                }
                None => {
                    ui.colored_label(Color32::from_rgb(210, 90, 80), "Deno is not installed");
                    wrap_label(ui, "ScriptMCP can download the official Deno binary into:");
                    wrap_label(ui, RichText::new(&app.sidecar).monospace());
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
                wrap_label(ui, message);
                if let Some(total) = total {
                    if total > 0 {
                        ui.add(
                            egui::ProgressBar::new(done as f32 / total as f32)
                                .desired_width(ui.available_width())
                                .text(format!("{done} / {total} bytes")),
                        );
                    }
                } else if done > 0 {
                    ui.label(format!("Downloaded {done} bytes"));
                }
            } else if let Some(error) = failed {
                wrap_label(
                    ui,
                    RichText::new(error).color(Color32::from_rgb(210, 90, 80)),
                );
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
        panel_group(ui, |ui| {
            ui.label(RichText::new("Script folders").strong());
            wrap_label(
                ui,
                RichText::new(AppState::config_path().display().to_string())
                    .small()
                    .color(ui.visuals().weak_text_color()),
            );
            ui.add_space(4.0);

            let mut remove_index = None;
            egui::ScrollArea::vertical()
                .id_salt("folders_scroll")
                .max_height(140.0)
                .auto_shrink([false, true])
                .hscroll(false)
                .show(ui, |ui| {
                    ui.set_max_width(ui.available_width());
                    for (index, folder) in app.folders.iter().enumerate() {
                        ui.push_id(index, |ui| {
                            ui.horizontal(|ui| {
                                ui.set_max_width(ui.available_width());
                                if ui.small_button("Remove").clicked() {
                                    remove_index = Some(index);
                                }
                                wrap_label(
                                    ui,
                                    RichText::new(folder.display().to_string()).monospace(),
                                );
                            });
                        });
                    }
                });

            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
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
        panel_group(ui, |ui| {
            ui.label(RichText::new("Tools").strong());
            wrap_label(
                ui,
                RichText::new(&app.tools_status).color(ui.visuals().weak_text_color()),
            );
            wrap_label(
                ui,
                RichText::new(
                    "Check a tool to expose it over MCP. Duplicate names are mutually exclusive.",
                )
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
                .auto_shrink([false, false])
                .hscroll(false)
                .show(ui, |ui| {
                    ui.set_max_width(ui.available_width());
                    ui.with_layout(egui::Layout::top_down_justified(egui::Align::Min), |ui| {
                        for (index, tool) in app.tools.iter().enumerate() {
                            ui.push_id(index, |ui| {
                                ui.horizontal(|ui| {
                                    ui.set_max_width(ui.available_width());
                                    let mut enabled = tool.enabled;
                                    if ui.checkbox(&mut enabled, "").changed() {
                                        toggle = Some((tool.path.clone(), enabled));
                                    }
                                    ui.vertical(|ui| {
                                        ui.set_max_width(ui.available_width());
                                        ui.horizontal_wrapped(|ui| {
                                            ui.strong(&tool.name);
                                            if tool.conflict {
                                                ui.colored_label(
                                                    Color32::from_rgb(180, 140, 60),
                                                    "duplicate",
                                                );
                                            }
                                        });
                                        if !tool.description.is_empty() {
                                            wrap_label(
                                                ui,
                                                RichText::new(&tool.description)
                                                    .color(ui.visuals().weak_text_color()),
                                            );
                                        }
                                        wrap_label(
                                            ui,
                                            RichText::new(&tool.path_label).small().monospace(),
                                        );
                                    });
                                });
                                ui.add_space(6.0);
                            });
                        }
                    });
                });

            if let Some((path, enabled)) = toggle {
                app.toggle_tool(path, enabled);
            }
        });
    });
}

fn calls_panel(app: &mut ScriptMcpApp, ui: &mut Ui) {
    ui.push_id("calls_panel", |ui| {
        panel_group(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Action history").strong());
                ui.label(RichText::new("last 10").color(ui.visuals().weak_text_color()));
            });
            ui.add_space(4.0);
            if !matches!(app.http, HttpState::Listening { .. }) {
                wrap_label(ui, "Start the HTTP server to record tool calls.");
                return;
            }
            if app.recent_calls.is_empty() {
                ui.label("No tool calls yet.");
                return;
            }
            let mut open_detail = None;
            egui::ScrollArea::vertical()
                .id_salt("calls_scroll")
                .auto_shrink([false, false])
                .hscroll(false)
                .show(ui, |ui| {
                    ui.set_max_width(ui.available_width());
                    ui.with_layout(egui::Layout::top_down_justified(egui::Align::Min), |ui| {
                        for (index, call) in app.recent_calls.iter().enumerate() {
                            ui.push_id(index, |ui| {
                                ui.horizontal_wrapped(|ui| {
                                    ui.small(call.clock());
                                    ui.strong(&call.name);
                                    match &call.outcome {
                                        ToolCallOutcome::Success { .. } => {
                                            ui.colored_label(Color32::from_rgb(80, 170, 110), "ok");
                                        }
                                        ToolCallOutcome::Error { .. } => {
                                            ui.colored_label(
                                                Color32::from_rgb(210, 90, 80),
                                                "error",
                                            );
                                        }
                                        ToolCallOutcome::UnknownTool => {
                                            ui.colored_label(
                                                Color32::from_rgb(210, 90, 80),
                                                "unknown",
                                            );
                                        }
                                    }
                                });
                                wrap_label(
                                    ui,
                                    RichText::new(format!("in  {}", call.arguments)).small(),
                                );
                                if let Some(body) = call.outcome.response_text() {
                                    let preview = first_line_preview(body);
                                    let (lines, chars) = text_stats(body);
                                    let color = match &call.outcome {
                                        ToolCallOutcome::Error { .. } => {
                                            Color32::from_rgb(210, 90, 80)
                                        }
                                        _ => ui.visuals().text_color(),
                                    };
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(format!("out {preview}"))
                                                .small()
                                                .color(color),
                                        )
                                        .wrap_mode(TextWrapMode::Truncate)
                                        .selectable(true),
                                    );
                                    ui.horizontal_wrapped(|ui| {
                                        ui.label(
                                            RichText::new(format_text_stats(lines, chars))
                                                .small()
                                                .color(ui.visuals().weak_text_color()),
                                        );
                                        if ui.small_button("Details").clicked() {
                                            open_detail = Some(call.clone());
                                        }
                                    });
                                }
                                ui.add_space(8.0);
                            });
                        }
                    });
                });
            if let Some(call) = open_detail {
                app.history_detail = Some(call);
            }
        });
    });
}

fn history_detail_modal(app: &mut ScriptMcpApp, ctx: &egui::Context) {
    let Some(call) = app.history_detail.clone() else {
        return;
    };
    let body = call
        .outcome
        .response_text()
        .unwrap_or("(no response)")
        .to_string();
    let (lines, chars) = text_stats(&body);
    let error = matches!(call.outcome, ToolCallOutcome::Error { .. });
    let max = ctx.content_rect().size();
    let response = egui::Modal::new(egui::Id::new("history_detail")).show(ctx, |ui| {
        ui.set_min_width(360.0);
        ui.set_max_width((max.x * 0.85).max(360.0));
        ui.set_max_height((max.y * 0.85).max(240.0));
        ui.heading(&call.name);
        ui.horizontal_wrapped(|ui| {
            ui.small(call.clock());
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
            ui.label(
                RichText::new(format_text_stats(lines, chars))
                    .small()
                    .color(ui.visuals().weak_text_color()),
            );
        });
        ui.add_space(8.0);
        ui.label(RichText::new("Arguments").strong());
        wrap_label(ui, RichText::new(&call.arguments).monospace());
        ui.add_space(8.0);
        ui.label(RichText::new("Response").strong());
        let color = if error {
            Color32::from_rgb(210, 90, 80)
        } else {
            ui.visuals().text_color()
        };
        egui::ScrollArea::vertical()
            .id_salt("history_detail_body")
            .max_height((max.y * 0.5).max(160.0))
            .auto_shrink([false, true])
            .hscroll(false)
            .show(ui, |ui| {
                ui.set_max_width(ui.available_width());
                formatted_output(ui, &body, color);
            });
        ui.add_space(10.0);
        ui.horizontal_wrapped(|ui| {
            let close = ui.button("Close").clicked();
            copy_button(ui, "history_detail_copy", "Copy response", body.clone());
            close
        })
        .inner
    });
    if response.should_close() || response.inner {
        app.history_detail = None;
    }
}

fn mcp_panel(app: &mut ScriptMcpApp, ui: &mut Ui) {
    ui.push_id("mcp_panel", |ui| {
        panel_group(ui, |ui| {
            ui.label(RichText::new("Connect as an MCP server").strong());
            wrap_label(
                ui,
                RichText::new(
                    "Any MCP client can connect over Streamable HTTP on localhost, or spawn this process over stdio.",
                )
                .color(ui.visuals().weak_text_color()),
            );
            ui.add_space(8.0);

            if app.deno.is_none() {
                wrap_label(
                    ui,
                    RichText::new("Install Deno before connecting a client.")
                        .color(Color32::from_rgb(210, 90, 80)),
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
            ui.horizontal_wrapped(|ui| {
                ui.colored_label(Color32::from_rgb(80, 170, 110), "Listening");
                wrap_label(ui, RichText::new(mcp_config::http_url(*addr)).monospace());
            });
            wrap_label(
                ui,
                RichText::new(
                    "Point an MCP client at this URL. Script changes notify connected clients.",
                )
                .color(ui.visuals().weak_text_color()),
            );
        }
        HttpState::Failed(error) => {
            wrap_label(
                ui,
                RichText::new(error).color(Color32::from_rgb(210, 90, 80)),
            );
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
    wrap_label(
        ui,
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
    ui.horizontal_wrapped(|ui| {
        ui.label(label);
        copy_button(ui, label, "Copy", value.to_string());
    });
    wrap_label(ui, RichText::new(value).monospace());
}

fn json_block(ui: &mut Ui, id: &str, snippet: &str) {
    let width = ui.available_width();
    let color = ui.visuals().text_color();
    egui::ScrollArea::vertical()
        .id_salt(id)
        .max_height(200.0)
        .max_width(width)
        .auto_shrink([false, true])
        .hscroll(false)
        .show(ui, |ui| {
            ui.set_max_width(width);
            formatted_output(ui, snippet, color);
        });
    ui.horizontal_wrapped(|ui| {
        copy_button(ui, &format!("copy_{id}"), "Copy JSON", snippet.to_string());
    });
}

fn panel_group(ui: &mut Ui, add_contents: impl FnOnce(&mut Ui)) {
    ui.group(|ui| {
        ui.set_width(ui.available_width());
        ui.set_max_width(ui.available_width());
        add_contents(ui);
    });
}

fn wrap_label(ui: &mut Ui, text: impl Into<RichText>) {
    ui.add(
        egui::Label::new(text.into())
            .wrap_mode(TextWrapMode::Wrap)
            .selectable(true),
    );
}

fn formatted_output(ui: &mut Ui, text: &str, color: Color32) {
    // Labels in a justified layout strip leading spaces during alignment.
    // A read-only TextEdit lays the galley out left-aligned, so indent is kept.
    let mut text = text;
    let rows = text.split('\n').count().max(1);
    ui.add(
        egui::TextEdit::multiline(&mut text)
            .font(egui::TextStyle::Monospace)
            .text_color(color)
            .desired_width(ui.available_width())
            .desired_rows(rows)
            .frame(false),
    );
}

fn copy_button(ui: &mut Ui, id: &str, label: &str, text: impl Into<String>) {
    let text = text.into();
    ui.push_id(id, |ui| {
        if ui.button(label).clicked() {
            ui.ctx().copy_text(text);
        }
    });
}

fn debug_outcome(result: Result<Value, impl ToString>) -> DebugOutcome {
    match result {
        Ok(value) => DebugOutcome {
            success: true,
            body: server::format_response_text(&value),
        },
        Err(error) => DebugOutcome {
            success: false,
            body: error.to_string(),
        },
    }
}

fn debug_fields_from_schema(schema: &Value) -> Vec<DebugField> {
    let required = required_names(schema);
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return Vec::new();
    };
    properties
        .iter()
        .map(|(name, spec)| DebugField {
            name: name.clone(),
            required: required.contains(name),
            description: spec
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            kind: field_kind(spec),
            text: String::new(),
            checked: false,
        })
        .collect()
}

fn required_names(schema: &Value) -> HashSet<String> {
    schema
        .get("required")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn field_kind(spec: &Value) -> DebugFieldKind {
    match schema_type(spec) {
        Some("string") => DebugFieldKind::String,
        Some("integer") => DebugFieldKind::Integer,
        Some("number") => DebugFieldKind::Number,
        Some("boolean") => DebugFieldKind::Boolean,
        _ => DebugFieldKind::Json,
    }
}

fn schema_type(spec: &Value) -> Option<&str> {
    match spec.get("type") {
        Some(Value::String(kind)) => Some(kind.as_str()),
        Some(Value::Array(kinds)) => kinds
            .iter()
            .filter_map(Value::as_str)
            .find(|kind| *kind != "null"),
        _ => None,
    }
}

fn collect_debug_args(fields: &[DebugField]) -> Result<Value, String> {
    let mut object = serde_json::Map::new();
    for field in fields {
        match field.kind {
            DebugFieldKind::Boolean => {
                object.insert(field.name.clone(), Value::Bool(field.checked));
            }
            DebugFieldKind::String => {
                if field.text.is_empty() {
                    if field.required {
                        object.insert(field.name.clone(), Value::String(String::new()));
                    }
                } else {
                    object.insert(field.name.clone(), Value::String(field.text.clone()));
                }
            }
            DebugFieldKind::Integer => match parse_optional_number(&field.text, true) {
                Ok(Some(value)) => {
                    object.insert(field.name.clone(), value);
                }
                Ok(None) if field.required => {
                    return Err(format!("{} is required.", field.name));
                }
                Ok(None) => {}
                Err(error) => return Err(format!("{}: {error}", field.name)),
            },
            DebugFieldKind::Number => match parse_optional_number(&field.text, false) {
                Ok(Some(value)) => {
                    object.insert(field.name.clone(), value);
                }
                Ok(None) if field.required => {
                    return Err(format!("{} is required.", field.name));
                }
                Ok(None) => {}
                Err(error) => return Err(format!("{}: {error}", field.name)),
            },
            DebugFieldKind::Json => {
                let trimmed = field.text.trim();
                if trimmed.is_empty() {
                    if field.required {
                        return Err(format!("{} is required.", field.name));
                    }
                } else {
                    let value: Value = serde_json::from_str(trimmed)
                        .map_err(|error| format!("{}: invalid JSON ({error})", field.name))?;
                    object.insert(field.name.clone(), value);
                }
            }
        }
    }
    Ok(Value::Object(object))
}

fn parse_optional_number(text: &str, integer: bool) -> Result<Option<Value>, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if integer {
        trimmed
            .parse::<i64>()
            .map(|value| Some(Value::from(value)))
            .map_err(|_| "must be an integer".into())
    } else {
        trimmed
            .parse::<f64>()
            .map(|value| {
                Some(serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number))
            })
            .map_err(|_| "must be a number".into())
    }
}

fn first_line_preview(text: &str) -> &str {
    text.lines().next().unwrap_or("")
}

fn text_stats(text: &str) -> (usize, usize) {
    let chars = text.chars().count();
    let lines = if text.is_empty() {
        0
    } else {
        text.lines().count()
    };
    (lines, chars)
}

fn format_text_stats(lines: usize, chars: usize) -> String {
    let line_label = if lines == 1 { "line" } else { "lines" };
    let char_label = if chars == 1 {
        "character"
    } else {
        "characters"
    };
    format!("{lines} {line_label}, {chars} {char_label}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn debug_fields_mark_optional_properties() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Name to greet" },
                "title": { "type": "string" }
            },
            "required": ["name"]
        });
        let fields = debug_fields_from_schema(&schema);
        assert_eq!(fields.len(), 2);
        assert!(
            fields
                .iter()
                .find(|field| field.name == "name")
                .unwrap()
                .required
        );
        assert!(
            !fields
                .iter()
                .find(|field| field.name == "title")
                .unwrap()
                .required
        );
    }

    #[test]
    fn collect_debug_args_omits_empty_optional_values() {
        let fields = vec![
            DebugField {
                name: "name".into(),
                required: true,
                description: String::new(),
                kind: DebugFieldKind::String,
                text: "Ada".into(),
                checked: false,
            },
            DebugField {
                name: "title".into(),
                required: false,
                description: String::new(),
                kind: DebugFieldKind::String,
                text: String::new(),
                checked: false,
            },
            DebugField {
                name: "count".into(),
                required: false,
                description: String::new(),
                kind: DebugFieldKind::Integer,
                text: String::new(),
                checked: false,
            },
            DebugField {
                name: "flag".into(),
                required: true,
                description: String::new(),
                kind: DebugFieldKind::Boolean,
                text: String::new(),
                checked: true,
            },
        ];
        assert_eq!(
            collect_debug_args(&fields).unwrap(),
            json!({ "name": "Ada", "flag": true })
        );
    }

    #[test]
    fn collect_debug_args_requires_numbers() {
        let fields = vec![DebugField {
            name: "count".into(),
            required: true,
            description: String::new(),
            kind: DebugFieldKind::Integer,
            text: String::new(),
            checked: false,
        }];
        assert!(collect_debug_args(&fields)
            .unwrap_err()
            .contains("required"));
    }

    #[test]
    fn history_preview_uses_first_line_and_counts() {
        let text = "hello\nworld\n";
        assert_eq!(first_line_preview(text), "hello");
        assert_eq!(text_stats(text), (2, 12));
        assert_eq!(format_text_stats(1, 1), "1 line, 1 character");
        assert_eq!(format_text_stats(2, 12), "2 lines, 12 characters");
    }
}
