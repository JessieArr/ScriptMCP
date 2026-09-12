use std::collections::VecDeque;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use notify::event::{EventKind, MetadataKind, ModifyKind};
use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};
use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorData,
    Implementation, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
    ToolAnnotations,
};
use rmcp::service::{NotificationContext, Peer, RequestContext};
use rmcp::{ErrorData as McpError, RoleServer};
use serde_json::Value;
use tracing::{error, info, warn};

use crate::app_state::AppState;
use crate::catalog::{self, Catalog};
use crate::config::Opts;
use crate::deno::{DenoRuntime, ScriptError};

const RELOAD_DEBOUNCE: Duration = Duration::from_millis(250);
const MAX_PEERS: usize = 64;
pub const MAX_TOOL_CALL_LOG: usize = 10;
const PREVIEW_CHARS: usize = 120;

#[derive(Clone)]
pub struct ScriptMcp {
    inner: Arc<Inner>,
}

struct Inner {
    catalog: RwLock<Catalog>,
    state: Mutex<AppState>,
    scripts_dirs: Arc<RwLock<Vec<PathBuf>>>,
    runtime: DenoRuntime,
    peers: Mutex<Vec<Peer<RoleServer>>>,
    generation: AtomicU64,
    calls: Mutex<VecDeque<ToolCallRecord>>,
    dirs_tx: tokio::sync::watch::Sender<Vec<PathBuf>>,
}

#[derive(Debug, Clone)]
pub struct ToolCallRecord {
    pub at: SystemTime,
    pub name: String,
    pub arguments: String,
    pub outcome: ToolCallOutcome,
}

#[derive(Debug, Clone)]
pub enum ToolCallOutcome {
    Success { body: String },
    Error { message: String },
    UnknownTool,
}

impl ToolCallOutcome {
    pub fn response_text(&self) -> Option<&str> {
        match self {
            Self::Success { body } => Some(body),
            Self::Error { message } => Some(message),
            Self::UnknownTool => None,
        }
    }
}

impl ToolCallRecord {
    pub fn clock(&self) -> String {
        let Ok(elapsed) = self.at.duration_since(UNIX_EPOCH) else {
            return "--:--:--".into();
        };
        let secs = elapsed.as_secs();
        format!(
            "{:02}:{:02}:{:02}",
            (secs / 3600) % 24,
            (secs / 60) % 60,
            secs % 60
        )
    }
}

impl ScriptMcp {
    pub async fn start(opts: &Opts) -> Result<Self> {
        let mut state = AppState::load();
        if state.folders.is_empty() {
            state.add_folder(opts.scripts.clone());
            let _ = state.save();
        }
        let folders = state.folders_or_fallback(&opts.scripts);
        let resolved = catalog::resolve_scripts_dirs(&folders)?;
        let scripts_dirs = Arc::new(RwLock::new(resolved.clone()));
        let runtime = DenoRuntime::from_shared_dirs(opts, scripts_dirs.clone()).await?;
        let catalog = Catalog::load_dirs(&resolved, &runtime, Some(&state)).await?;
        sync_enabled_into_state(&catalog, &mut state);
        let _ = state.save();
        info!(
            folders = resolved.len(),
            tools = catalog.tools().len(),
            enabled = catalog.enabled_tools().count(),
            "loaded script tools"
        );
        let (dirs_tx, _) = tokio::sync::watch::channel(resolved);
        let inner = Arc::new(Inner {
            catalog: RwLock::new(catalog),
            state: Mutex::new(state),
            scripts_dirs,
            runtime,
            peers: Mutex::new(Vec::new()),
            generation: AtomicU64::new(1),
            calls: Mutex::new(VecDeque::new()),
            dirs_tx,
        });
        spawn_watcher(inner.clone());
        Ok(Self { inner })
    }

    pub fn generation(&self) -> u64 {
        self.inner.generation.load(Ordering::Relaxed)
    }

    pub fn snapshot(&self) -> Catalog {
        self.inner
            .catalog
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn app_state(&self) -> AppState {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn folders(&self) -> Vec<PathBuf> {
        self.inner.scripts_dirs()
    }

    pub async fn set_folders(&self, folders: Vec<PathBuf>) -> Result<Catalog> {
        let resolved = catalog::resolve_scripts_dirs(&folders)?;
        {
            let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            state.folders = resolved.clone();
            state.save()?;
        }
        {
            let mut dirs = self
                .inner
                .scripts_dirs
                .write()
                .unwrap_or_else(|e| e.into_inner());
            *dirs = resolved.clone();
        }
        let _ = self.inner.dirs_tx.send(resolved);
        self.inner.reload().await?;
        Ok(self.snapshot())
    }

    pub async fn set_tool_enabled(&self, path: PathBuf, enabled: bool) -> Result<Catalog> {
        {
            let catalog = self.inner.catalog.read().unwrap_or_else(|e| e.into_inner());
            let Some(tool) = catalog.tools().iter().find(|tool| tool.path == path) else {
                anyhow::bail!("unknown tool path: {}", path.display());
            };
            let name = tool.meta.name.clone();
            let siblings: Vec<PathBuf> = catalog
                .tools()
                .iter()
                .filter(|other| other.meta.name == name)
                .map(|other| other.path.clone())
                .collect();
            drop(catalog);

            let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            if enabled {
                for sibling in &siblings {
                    state.set_enabled(sibling, sibling == &path);
                }
            } else {
                state.set_enabled(&path, false);
            }
            state.save()?;
        }
        self.inner.reload().await?;
        Ok(self.snapshot())
    }

    pub fn recent_calls(&self) -> Vec<ToolCallRecord> {
        self.inner
            .calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .rev()
            .cloned()
            .collect()
    }

    /// Invoke a catalog script by path. Disabled tools are allowed so the UI can debug them.
    pub async fn invoke_script(&self, path: &Path, arguments: Value) -> Result<Value> {
        let script = {
            let catalog = self.inner.catalog.read().unwrap_or_else(|e| e.into_inner());
            catalog
                .tools()
                .iter()
                .find(|tool| tool.path == path)
                .cloned()
        };
        let Some(script) = script else {
            self.inner.record_call(
                path.display().to_string(),
                &arguments,
                ToolCallOutcome::UnknownTool,
            );
            anyhow::bail!("unknown tool: {}", path.display());
        };
        self.inner.invoke_script(&script, &arguments).await
    }
}

impl Inner {
    fn scripts_dirs(&self) -> Vec<PathBuf> {
        self.scripts_dirs
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    async fn invoke_script(
        &self,
        script: &crate::catalog::ScriptTool,
        arguments: &Value,
    ) -> Result<Value> {
        let name = script.meta.name.clone();
        match self
            .runtime
            .invoke(
                &script.path,
                &script.meta.permissions,
                &script.source_dir,
                arguments,
            )
            .await
        {
            Ok(value) => {
                self.record_call(
                    name,
                    arguments,
                    ToolCallOutcome::Success {
                        body: format_logged_value(&value),
                    },
                );
                Ok(value)
            }
            Err(error) => {
                let message = error.to_string();
                if error.downcast_ref::<ScriptError>().is_some() {
                    warn!(tool = %name, error = %message, "script threw");
                } else {
                    error!(tool = %name, error = %error, "script tool failed");
                }
                self.record_call(
                    name,
                    arguments,
                    ToolCallOutcome::Error {
                        message: message.clone(),
                    },
                );
                Err(error)
            }
        }
    }

    fn record_call(&self, name: String, arguments: &Value, outcome: ToolCallOutcome) {
        let record = ToolCallRecord {
            at: SystemTime::now(),
            name,
            arguments: preview_value(arguments),
            outcome,
        };
        let mut calls = self.calls.lock().unwrap_or_else(|e| e.into_inner());
        push_call(&mut calls, record);
    }

    fn track_peer(&self, peer: &Peer<RoleServer>) {
        let mut peers = self.peers.lock().unwrap_or_else(|e| e.into_inner());
        if peers.len() >= MAX_PEERS {
            peers.remove(0);
        }
        peers.push(peer.clone());
    }

    async fn reload(&self) -> Result<()> {
        let dirs = self.scripts_dirs();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let catalog = Catalog::load_dirs(&dirs, &self.runtime, Some(&state))
            .await
            .with_context(|| "failed to reload scripts")?;
        sync_enabled_into_state(&catalog, &mut state);
        let _ = state.save();
        let tools = catalog.tools().len();
        let enabled = catalog.enabled_tools().count();
        *self.catalog.write().unwrap_or_else(|e| e.into_inner()) = catalog;
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = state;
        self.generation.fetch_add(1, Ordering::Relaxed);
        info!(
            folders = dirs.len(),
            tools, enabled, "reloaded script tools"
        );
        self.broadcast_list_changed().await;
        Ok(())
    }

    async fn broadcast_list_changed(&self) {
        let peers = self.peers.lock().unwrap_or_else(|e| e.into_inner()).clone();
        for peer in peers {
            if let Err(error) = peer.notify_tool_list_changed().await {
                tracing::debug!(error = %error, "failed to send tools/list_changed");
            }
        }
    }
}

fn sync_enabled_into_state(catalog: &Catalog, state: &mut AppState) {
    for tool in catalog.tools() {
        state.set_enabled(&tool.path, tool.enabled);
    }
}

fn spawn_watcher(inner: Arc<Inner>) {
    tokio::spawn(async move {
        if let Err(error) = watch_loop(inner).await {
            warn!(error = %error, "script directory watcher stopped");
        }
    });
}

async fn watch_loop(inner: Arc<Inner>) -> Result<()> {
    let mut dirs_rx = inner.dirs_tx.subscribe();
    dirs_rx.mark_unchanged();
    loop {
        let dirs = inner.scripts_dirs();
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut watcher = RecommendedWatcher::new(
            move |result: notify::Result<Event>| {
                let _ = event_tx.send(result);
            },
            Config::default(),
        )
        .context("failed to start script directory watcher")?;

        let mut watching = false;
        for dir in &dirs {
            if let Err(error) = watcher.watch(dir, RecursiveMode::NonRecursive) {
                warn!(
                    path = %dir.display(),
                    error = %error,
                    "could not watch scripts directory"
                );
            } else {
                watching = true;
                info!(path = %dir.display(), "watching scripts directory");
            }
        }

        if !watching {
            tokio::select! {
                changed = dirs_rx.changed() => {
                    if changed.is_err() {
                        return Ok(());
                    }
                }
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
            }
            continue;
        }

        loop {
            tokio::select! {
                changed = dirs_rx.changed() => {
                    if changed.is_err() {
                        return Ok(());
                    }
                    break;
                }
                event = event_rx.recv() => {
                    let Some(event) = event else {
                        break;
                    };
                    match event {
                        Ok(event) if event_should_reload(&dirs, &event) => {
                            tokio::time::sleep(RELOAD_DEBOUNCE).await;
                            while event_rx.try_recv().is_ok() {}
                            if let Err(error) = inner.reload().await {
                                warn!(error = %error, "failed to reload scripts after a file change");
                            }
                        }
                        Ok(_) => {}
                        Err(error) => warn!(error = %error, "script directory watch error"),
                    }
                }
            }
        }
    }
}

fn event_should_reload(scripts_dirs: &[PathBuf], event: &Event) -> bool {
    if event.need_rescan() {
        return true;
    }
    match event.kind {
        EventKind::Access(_) => return false,
        EventKind::Modify(ModifyKind::Metadata(MetadataKind::AccessTime)) => return false,
        _ => {}
    }
    catalog::should_reload_for_paths(scripts_dirs, &event.paths)
}

impl ServerHandler for ScriptMcp {
    fn get_info(&self) -> ServerInfo {
        let dirs = self.inner.scripts_dirs();
        let folders = dirs
            .iter()
            .map(|dir| dir.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build(),
        )
        .with_server_info(
            Implementation::new("scriptmcp", env!("CARGO_PKG_VERSION"))
                .with_title("ScriptMCP")
                .with_description("Expose Deno scripts as MCP tools"),
        )
        .with_instructions(format!(
            "Each enabled JavaScript/TypeScript file in [{folders}] is an MCP tool. Call a tool to run that script in Deno. Arguments are passed as JSON matching the tool's input schema. The tool list updates when scripts change."
        ))
    }

    fn on_initialized(
        &self,
        context: NotificationContext<RoleServer>,
    ) -> impl Future<Output = ()> + Send + '_ {
        self.inner.track_peer(&context.peer);
        std::future::ready(())
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.inner
            .catalog
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get_enabled(name)
            .map(script_to_tool)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        self.inner.track_peer(&context.peer);
        Ok(ListToolsResult {
            tools: self
                .inner
                .catalog
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .enabled_tools()
                .map(script_to_tool)
                .collect(),
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let name = request.name.to_string();
        let arguments = match request.arguments {
            Some(map) => Value::Object(map),
            None => Value::Object(Default::default()),
        };
        let script = {
            let catalog = self.inner.catalog.read().unwrap_or_else(|e| e.into_inner());
            catalog.get_enabled(&name).cloned()
        };
        let Some(script) = script else {
            self.inner
                .record_call(name.clone(), &arguments, ToolCallOutcome::UnknownTool);
            return Err(ErrorData::invalid_params(
                format!("unknown tool: {name}"),
                None,
            ));
        };

        match self.inner.invoke_script(&script, &arguments).await {
            Ok(value) => Ok(value_to_result(value).into()),
            Err(error) => {
                Ok(CallToolResult::error(vec![ContentBlock::text(error.to_string())]).into())
            }
        }
    }
}

fn script_to_tool(script: &crate::catalog::ScriptTool) -> Tool {
    let mut tool = Tool::new(
        script.meta.name.clone(),
        script.meta.description.clone(),
        Catalog::input_schema_object(&script.meta.input_schema),
    );
    if let Some(schema) = script.meta.output_schema.as_ref() {
        if let Value::Object(map) = schema {
            tool = tool.with_raw_output_schema(std::sync::Arc::new(map.clone()));
        }
    }
    if let Some(annotations) = script.meta.annotations.as_ref() {
        tool = tool.with_annotations(ToolAnnotations::from_raw(
            None,
            annotations.read_only_hint,
            annotations.destructive_hint,
            annotations.idempotent_hint,
            annotations.open_world_hint,
        ));
    }
    tool
}

fn push_call(calls: &mut VecDeque<ToolCallRecord>, record: ToolCallRecord) {
    if calls.len() >= MAX_TOOL_CALL_LOG {
        calls.pop_front();
    }
    calls.push_back(record);
}

fn preview_value(value: &Value) -> String {
    let raw = format_logged_value(value);
    if raw.chars().count() <= PREVIEW_CHARS {
        raw
    } else {
        let mut out: String = raw.chars().take(PREVIEW_CHARS).collect();
        out.push('…');
        out
    }
}

fn format_logged_value(value: &Value) -> String {
    format_response_text(value)
}

/// Human-readable tool output. Strings keep real newlines; objects/arrays are indented.
pub fn format_response_text(value: &Value) -> String {
    let mut out = String::new();
    write_response_text(&mut out, value, 0);
    out
}

fn write_response_text(out: &mut String, value: &Value, indent: usize) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => out.push_str(&number.to_string()),
        Value::String(text) => write_multiline(out, text, indent),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                out.push('\n');
                out.push_str(&indent_prefix(indent + 1));
                write_response_text(out, item, indent + 1);
                if index + 1 < items.len() {
                    out.push(',');
                }
            }
            out.push('\n');
            out.push_str(&indent_prefix(indent));
            out.push(']');
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (index, (key, item)) in map.iter().enumerate() {
                out.push('\n');
                out.push_str(&indent_prefix(indent + 1));
                out.push_str(key);
                out.push_str(": ");
                write_response_text(out, item, indent + 1);
                if index + 1 < map.len() {
                    out.push(',');
                }
            }
            out.push('\n');
            out.push_str(&indent_prefix(indent));
            out.push('}');
        }
    }
}

fn write_multiline(out: &mut String, text: &str, indent: usize) {
    let pad = indent_prefix(indent);
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            out.push('\n');
            out.push_str(&pad);
        }
        out.push_str(line);
    }
}

fn indent_prefix(indent: usize) -> String {
    "  ".repeat(indent)
}

fn value_to_result(value: Value) -> CallToolResult {
    match value {
        Value::Null => CallToolResult::success(vec![ContentBlock::text("null")]),
        Value::String(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
        other => CallToolResult::structured(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_access_events_and_hidden_files() {
        let dir = PathBuf::from("/tmp/scriptmcp-tools");
        let dirs = vec![dir.clone()];
        let script = Event {
            kind: EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Content)),
            paths: vec![dir.join("hello.ts")],
            attrs: Default::default(),
        };
        assert!(event_should_reload(&dirs, &script));

        let access = Event {
            kind: EventKind::Access(notify::event::AccessKind::Read),
            paths: vec![dir.join("hello.ts")],
            attrs: Default::default(),
        };
        assert!(!event_should_reload(&dirs, &access));
    }

    #[test]
    fn call_log_keeps_the_ten_newest() {
        let mut calls = VecDeque::new();
        for i in 0..(MAX_TOOL_CALL_LOG + 3) {
            push_call(
                &mut calls,
                ToolCallRecord {
                    at: UNIX_EPOCH,
                    name: format!("t{i}"),
                    arguments: "{}".into(),
                    outcome: ToolCallOutcome::UnknownTool,
                },
            );
        }
        let names: Vec<_> = calls.iter().map(|call| call.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["t3", "t4", "t5", "t6", "t7", "t8", "t9", "t10", "t11", "t12"]
        );
    }

    #[test]
    fn truncates_long_previews() {
        let long = "x".repeat(PREVIEW_CHARS + 8);
        let preview = preview_value(&Value::String(long));
        assert!(preview.ends_with('…'));
        assert_eq!(preview.chars().count(), PREVIEW_CHARS + 1);
    }

    #[test]
    fn formats_multiline_strings_and_objects() {
        assert_eq!(format_response_text(&Value::String("a\nb".into())), "a\nb");
        assert_eq!(
            format_response_text(&serde_json::json!({"result": "hello\nworld"})),
            "{\n  result: hello\n  world\n}"
        );
    }
}
