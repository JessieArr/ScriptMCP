use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tracing::{info, warn};

use crate::config::Opts;
use crate::deno::DenoRuntime;

const SCRIPT_EXTENSIONS: &[&str] = &["ts", "js", "mts", "mjs"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptMeta {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "empty_schema", rename = "inputSchema")]
    pub input_schema: Value,
    #[serde(default)]
    pub permissions: Vec<String>,
}

fn empty_schema() -> Value {
    serde_json::json!({ "type": "object", "properties": {} })
}

#[derive(Debug, Clone)]
pub struct ScriptTool {
    pub meta: ScriptMeta,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub struct Catalog {
    tools: Vec<ScriptTool>,
}

impl Catalog {
    pub async fn load(opts: &Opts, runtime: &DenoRuntime) -> Result<Self> {
        let scripts_dir = opts
            .scripts
            .canonicalize()
            .with_context(|| format!("scripts directory not found: {}", opts.scripts.display()))?;

        if !scripts_dir.is_dir() {
            anyhow::bail!("scripts path is not a directory: {}", scripts_dir.display());
        }

        let mut tools = Vec::new();
        let mut names = HashSet::new();

        for path in list_script_files(&scripts_dir)? {
            match runtime.introspect(&path).await {
                Ok(mut meta) => {
                    meta.name = sanitize_tool_name(&meta.name);
                    if meta.name.is_empty() {
                        warn!(path = %path.display(), "skipping script with empty tool name");
                        continue;
                    }
                    if !names.insert(meta.name.clone()) {
                        warn!(
                            name = %meta.name,
                            path = %path.display(),
                            "skipping script because the tool name is already registered"
                        );
                        continue;
                    }
                    info!(name = %meta.name, path = %path.display(), "registered script tool");
                    tools.push(ScriptTool { meta, path });
                }
                Err(error) => {
                    warn!(path = %path.display(), error = %error, "failed to load script");
                }
            }
        }

        tools.sort_by(|a, b| a.meta.name.cmp(&b.meta.name));
        Ok(Self { tools })
    }

    pub fn tools(&self) -> &[ScriptTool] {
        &self.tools
    }

    pub fn get(&self, name: &str) -> Option<&ScriptTool> {
        self.tools.iter().find(|tool| tool.meta.name == name)
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

pub fn list_script_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("failed to read {}", dir.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.') || name.starts_with('_') {
            continue;
        }
        if !path.is_file() {
            continue;
        }
        let ext = path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if SCRIPT_EXTENSIONS.contains(&ext.as_str()) {
            files.push(path);
        }
    }
    Ok(files)
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
}
