use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tracing::{info, warn};

use crate::app_state::AppState;
use crate::deno::DenoRuntime;
use crate::permissions::ScriptPermissions;

const SCRIPT_EXTENSIONS: &[&str] = &["ts", "js", "mts", "mjs"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScriptAnnotations {
    #[serde(default, rename = "readOnlyHint")]
    pub read_only_hint: Option<bool>,
    #[serde(default, rename = "destructiveHint")]
    pub destructive_hint: Option<bool>,
    #[serde(default, rename = "idempotentHint")]
    pub idempotent_hint: Option<bool>,
    #[serde(default, rename = "openWorldHint")]
    pub open_world_hint: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScriptMeta {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "empty_schema", rename = "inputSchema")]
    pub input_schema: Value,
    #[serde(default, rename = "outputSchema")]
    pub output_schema: Option<Value>,
    #[serde(default)]
    pub permissions: ScriptPermissions,
    #[serde(default)]
    pub annotations: Option<ScriptAnnotations>,
}

fn empty_schema() -> Value {
    serde_json::json!({ "type": "object", "properties": {} })
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScriptTool {
    pub meta: ScriptMeta,
    pub path: PathBuf,
    pub source_dir: PathBuf,
    /// Whether this tool is currently exposed over MCP.
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Catalog {
    tools: Vec<ScriptTool>,
}

pub fn resolve_scripts_dir(path: &Path) -> Result<PathBuf> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    joined
        .canonicalize()
        .with_context(|| format!("scripts directory not found: {}", path.display()))
}

pub fn resolve_scripts_dirs(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut dirs = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for path in paths {
        let resolved = resolve_scripts_dir(path)?;
        if seen.insert(resolved.clone()) {
            dirs.push(resolved);
        }
    }
    if dirs.is_empty() {
        anyhow::bail!("no scripts directories configured");
    }
    Ok(dirs)
}

impl Catalog {
    pub async fn load(scripts_dir: &Path, runtime: &DenoRuntime) -> Result<Self> {
        Self::load_dirs(std::slice::from_ref(&scripts_dir.to_path_buf()), runtime, None).await
    }

    pub async fn load_dirs(
        scripts_dirs: &[PathBuf],
        runtime: &DenoRuntime,
        state: Option<&AppState>,
    ) -> Result<Self> {
        let dirs = resolve_scripts_dirs(scripts_dirs)?;
        let mut tools = Vec::new();

        for scripts_dir in &dirs {
            if !scripts_dir.is_dir() {
                warn!(
                    path = %scripts_dir.display(),
                    "skipping scripts path that is not a directory"
                );
                continue;
            }

            for path in list_script_files(scripts_dir)? {
                match runtime.introspect(&path).await {
                    Ok(mut meta) => {
                        meta.name = sanitize_tool_name(&meta.name);
                        if meta.name.is_empty() {
                            warn!(path = %path.display(), "skipping script with empty tool name");
                            continue;
                        }
                        info!(name = %meta.name, path = %path.display(), "discovered script tool");
                        tools.push(ScriptTool {
                            meta,
                            path,
                            source_dir: scripts_dir.clone(),
                            enabled: true,
                        });
                    }
                    Err(error) => {
                        warn!(path = %path.display(), error = %error, "failed to load script");
                    }
                }
            }
        }

        // Keep same-name tools adjacent: sort by name, then path.
        tools.sort_by(|a, b| {
            a.meta
                .name
                .cmp(&b.meta.name)
                .then_with(|| a.path.cmp(&b.path))
        });

        apply_enabled_flags(&mut tools, state);

        Ok(Self { tools })
    }

    pub fn tools(&self) -> &[ScriptTool] {
        &self.tools
    }

    pub fn enabled_tools(&self) -> impl Iterator<Item = &ScriptTool> {
        self.tools.iter().filter(|tool| tool.enabled)
    }

    pub fn get_enabled(&self, name: &str) -> Option<&ScriptTool> {
        self.tools
            .iter()
            .find(|tool| tool.enabled && tool.meta.name == name)
    }

    pub fn input_schema_object(schema: &Value) -> Map<String, Value> {
        match schema {
            Value::Object(map) => map.clone(),
            _ => match empty_schema() {
                Value::Object(map) => map,
                _ => Map::new(),
            },
        }
    }
}

/// Apply persisted enable flags. Same-name tools are mutually exclusive: at most
/// one stays enabled. Explicit `true` entries win; otherwise unknown paths
/// default to enabled and the first candidate in name/path order is kept.
fn apply_enabled_flags(tools: &mut [ScriptTool], state: Option<&AppState>) {
    let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, tool) in tools.iter().enumerate() {
        groups
            .entry(tool.meta.name.clone())
            .or_default()
            .push(index);
    }

    for indices in groups.values() {
        let explicit_true: Vec<usize> = indices
            .iter()
            .copied()
            .filter(|&index| {
                state
                    .and_then(|s| {
                        s.enabled
                            .get(&tools[index].path.display().to_string())
                            .copied()
                    })
                    .unwrap_or(false)
            })
            .collect();

        let candidates: Vec<usize> = if !explicit_true.is_empty() {
            explicit_true
        } else {
            indices
                .iter()
                .copied()
                .filter(|&index| {
                    state
                        .map(|s| s.is_enabled(&tools[index].path, true))
                        .unwrap_or(true)
                })
                .collect()
        };

        for &index in indices {
            tools[index].enabled = false;
        }
        if let Some(&index) = candidates.first() {
            tools[index].enabled = true;
        }
    }
}

pub fn list_script_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("failed to read {}", dir.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        if is_script_path(&path) && path.is_file() {
            files.push(path);
        }
    }
    Ok(files)
}

pub fn is_script_path(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if name.starts_with('.') || name.starts_with('_') {
        return false;
    }
    let ext = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    SCRIPT_EXTENSIONS.contains(&ext.as_str())
}

/// True when a filesystem event on `path` might change the tool catalog.
pub fn should_reload_for_paths(scripts_dirs: &[PathBuf], paths: &[PathBuf]) -> bool {
    if paths.is_empty() {
        return true;
    }
    paths.iter().any(|path| {
        scripts_dirs.iter().any(|scripts_dir| {
            if path == scripts_dir {
                return true;
            }
            path.parent().is_some_and(|parent| parent == scripts_dir) && is_script_path(path)
        })
    })
}

pub fn sanitize_tool_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            out.push(ch);
        } else if (ch.is_whitespace() || ch == '.' || ch == '/') && !out.ends_with('_') {
            out.push('_');
        }
    }
    out.trim_matches('_').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn sanitizes_tool_names() {
        assert_eq!(sanitize_tool_name("hello world"), "hello_world");
        assert_eq!(sanitize_tool_name("foo/bar.ts"), "foo_bar_ts");
        assert_eq!(sanitize_tool_name("ok-name_1"), "ok-name_1");
    }

    #[test]
    fn lists_only_public_scripts() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("hello.ts"), "export default function () {}").unwrap();
        fs::write(dir.path().join("_helper.ts"), "").unwrap();
        fs::write(dir.path().join(".hidden.ts"), "").unwrap();
        fs::write(dir.path().join("readme.md"), "").unwrap();
        fs::write(dir.path().join("echo.js"), "").unwrap();

        let files = list_script_files(dir.path()).unwrap();
        let names: Vec<_> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["echo.js", "hello.ts"]);
    }

    #[test]
    fn reloads_for_script_events_in_the_scripts_dir() {
        let dir = PathBuf::from("/tmp/scriptmcp-tools");
        let dirs = vec![dir.clone()];
        assert!(should_reload_for_paths(&dirs, &[dir.join("hello.ts")]));
        assert!(should_reload_for_paths(&dirs, std::slice::from_ref(&dir)));
        assert!(should_reload_for_paths(&dirs, &[]));
        assert!(!should_reload_for_paths(&dirs, &[dir.join("_helper.ts")]));
        assert!(!should_reload_for_paths(&dirs, &[dir.join("readme.md")]));
        assert!(!should_reload_for_paths(
            &dirs,
            &[PathBuf::from("/elsewhere/hello.ts")]
        ));
    }

    #[test]
    fn apply_enabled_keeps_duplicates_mutually_exclusive() {
        let mut tools = vec![
            ScriptTool {
                meta: ScriptMeta {
                    name: "hello".into(),
                    description: "a".into(),
                    input_schema: empty_schema(),
                    output_schema: None,
                    permissions: ScriptPermissions::default(),
                    annotations: None,
                },
                path: PathBuf::from("/a/hello.ts"),
                source_dir: PathBuf::from("/a"),
                enabled: true,
            },
            ScriptTool {
                meta: ScriptMeta {
                    name: "hello".into(),
                    description: "b".into(),
                    input_schema: empty_schema(),
                    output_schema: None,
                    permissions: ScriptPermissions::default(),
                    annotations: None,
                },
                path: PathBuf::from("/b/hello.ts"),
                source_dir: PathBuf::from("/b"),
                enabled: true,
            },
        ];
        let mut state = AppState::default();
        state.set_enabled(Path::new("/b/hello.ts"), true);
        apply_enabled_flags(&mut tools, Some(&state));
        assert!(!tools[0].enabled);
        assert!(tools[1].enabled);
    }
}
