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
    scripts_dir: Arc<RwLock<PathBuf>>,
    runtime: DenoRuntime,
    peers: Mutex<Vec<Peer<RoleServer>>>,
    generation: AtomicU64,
    calls: Mutex<VecDeque<ToolCallRecord>>,
    dir_tx: tokio::sync::watch::Sender<PathBuf>,
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
    Success { preview: String },
    Error { message: String },
    UnknownTool,
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
        let resolved = catalog::resolve_scripts_dir(&opts.scripts)?;
        let scripts_dir = Arc::new(RwLock::new(resolved.clone()));
        let runtime = DenoRuntime::from_shared_dir(opts, scripts_dir.clone()).await?;
        let catalog = Catalog::load(&resolved, &runtime).await?;
        info!(
            scripts = %resolved.display(),
            tools = catalog.tools().len(),
            "loaded script tools"
        );
        let (dir_tx, _) = tokio::sync::watch::channel(resolved);
        let inner = Arc::new(Inner {
            catalog: RwLock::new(catalog),
            scripts_dir,
            runtime,
            peers: Mutex::new(Vec::new()),
            generation: AtomicU64::new(1),
            calls: Mutex::new(VecDeque::new()),
            dir_tx,
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

    pub async fn set_scripts_dir(&self, path: PathBuf) -> Result<Catalog> {
        let resolved = catalog::resolve_scripts_dir(&path)?;
        {
            let mut dir = self
                .inner
                .scripts_dir
                .write()
                .unwrap_or_else(|e| e.into_inner());
            *dir = resolved.clone();
        }
        let _ = self.inner.dir_tx.send(resolved);
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
}

impl Inner {
    fn scripts_dir(&self) -> PathBuf {
        self.scripts_dir
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
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
        let dir = self.scripts_dir();
        let catalog = Catalog::load(&dir, &self.runtime)
            .await
            .with_context(|| format!("failed to reload scripts from {}", dir.display()))?;
        let tools = catalog.tools().len();
        *self.catalog.write().unwrap_or_else(|e| e.into_inner()) = catalog;
        self.generation.fetch_add(1, Ordering::Relaxed);
        info!(scripts = %dir.display(), tools, "reloaded script tools");
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

fn spawn_watcher(inner: Arc<Inner>) {
    tokio::spawn(async move {
        if let Err(error) = watch_loop(inner).await {
            warn!(error = %error, "script directory watcher stopped");
        }
    });
}

async fn watch_loop(inner: Arc<Inner>) -> Result<()> {
    let mut dir_rx = inner.dir_tx.subscribe();
    dir_rx.mark_unchanged();
    loop {
        let dir = inner.scripts_dir();
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut watcher = RecommendedWatcher::new(
            move |result: notify::Result<Event>| {
                let _ = event_tx.send(result);
            },
            Config::default(),
        )
        .context("failed to start script directory watcher")?;

        if let Err(error) = watcher.watch(&dir, RecursiveMode::NonRecursive) {
            warn!(
                path = %dir.display(),
                error = %error,
                "could not watch scripts directory"
            );
            tokio::select! {
                changed = dir_rx.changed() => {
                    if changed.is_err() {
                        return Ok(());
                    }
                }
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
            }
            continue;
        }

        info!(path = %dir.display(), "watching scripts directory");
        loop {
            tokio::select! {
                changed = dir_rx.changed() => {
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
                        Ok(event) if event_should_reload(&dir, &event) => {
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

fn event_should_reload(scripts_dir: &Path, event: &Event) -> bool {
    if event.need_rescan() {
        return true;
    }
    match event.kind {
        EventKind::Access(_) => return false,
        EventKind::Modify(ModifyKind::Metadata(MetadataKind::AccessTime)) => return false,
        _ => {}
    }
    catalog::should_reload_for_paths(scripts_dir, &event.paths)
}

impl ServerHandler for ScriptMcp {
    fn get_info(&self) -> ServerInfo {
        let scripts_dir = self.inner.scripts_dir();
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
            "Each JavaScript/TypeScript file in {} is an MCP tool. Call a tool to run that script in Deno. Arguments are passed as JSON matching the tool's input schema. The tool list updates when scripts change.",
            scripts_dir.display()
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
            .get(name)
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
                .tools()
                .iter()
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
            catalog.get(&name).cloned()
        };
        let Some(script) = script else {
            self.inner
                .record_call(name.clone(), &arguments, ToolCallOutcome::UnknownTool);
            return Err(ErrorData::invalid_params(
                format!("unknown tool: {name}"),
                None,
            ));
        };

        match self
            .inner
            .runtime
            .invoke(&script.path, &script.meta.permissions, &arguments)
            .await
        {
            Ok(value) => {
                self.inner.record_call(
                    name,
                    &arguments,
                    ToolCallOutcome::Success {
                        preview: preview_value(&value),
                    },
                );
                Ok(value_to_result(value).into())
            }
            Err(error) => {
                let message = error.to_string();
                if error.downcast_ref::<ScriptError>().is_some() {
                    warn!(tool = %name, error = %message, "script threw");
                } else {
                    error!(tool = %name, error = %error, "script tool failed");
                }
                self.inner.record_call(
                    name,
                    &arguments,
                    ToolCallOutcome::Error {
                        message: message.clone(),
                    },
                );
                Ok(CallToolResult::error(vec![ContentBlock::text(message)]).into())
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
    let raw = match value {
        Value::String(text) => text.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| "{}".into()),
    };
    if raw.chars().count() <= PREVIEW_CHARS {
        raw
    } else {
        let mut out: String = raw.chars().take(PREVIEW_CHARS).collect();
        out.push('…');
        out
    }
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
        let script = Event {
            kind: EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Content)),
            paths: vec![dir.join("hello.ts")],
            attrs: Default::default(),
        };
        assert!(event_should_reload(&dir, &script));

        let access = Event {
            kind: EventKind::Access(notify::event::AccessKind::Read),
            paths: vec![dir.join("hello.ts")],
            attrs: Default::default(),
        };
        assert!(!event_should_reload(&dir, &access));
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
}
