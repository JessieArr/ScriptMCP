use std::fmt;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::debug;

use crate::app_state::AppState;
use crate::catalog::ScriptMeta;
use crate::config::Opts;
use crate::permissions::{to_deno_flags, ScriptPermissions};

const HOST_TS: &str = include_str!("../runtime/host.ts");

/// A user script threw or rejected during invoke/introspect.
#[derive(Debug)]
pub struct ScriptError(pub String);

impl fmt::Display for ScriptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ScriptError {}

#[derive(Debug, Clone)]
pub struct DenoRuntime {
    deno: PathBuf,
    host: PathBuf,
    allow_all: bool,
    extra_args: Vec<String>,
    timeout: Duration,
    scripts_dirs: Arc<RwLock<Vec<PathBuf>>>,
}

impl DenoRuntime {
    pub async fn new(opts: &Opts) -> Result<Self> {
        let state = AppState::load();
        let folders = state.folders_or_fallback(&opts.scripts);
        let scripts_dirs = crate::catalog::resolve_scripts_dirs(&folders).unwrap_or_else(|_| {
            vec![crate::catalog::resolve_scripts_dir(&opts.scripts).unwrap_or(opts.scripts.clone())]
        });
        Self::from_shared_dirs(opts, Arc::new(RwLock::new(scripts_dirs))).await
    }

    pub async fn from_shared_dirs(
        opts: &Opts,
        scripts_dirs: Arc<RwLock<Vec<PathBuf>>>,
    ) -> Result<Self> {
        let deno = crate::install::resolve_deno(&opts.deno);
        check_deno(&deno).await?;
        let host = write_host_script()?;
        Ok(Self {
            deno,
            host,
            allow_all: opts.allow_all,
            extra_args: opts.deno_args.clone(),
            timeout: opts.timeout_duration(),
            scripts_dirs,
        })
    }

    pub async fn introspect(&self, script: &Path) -> Result<ScriptMeta> {
        let workspace = script
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let output = self
            .run_host(
                "introspect",
                script,
                &ScriptPermissions::default(),
                &workspace,
                None,
            )
            .await
            .with_context(|| format!("introspect {}", script.display()))?;
        let meta: ScriptMeta = serde_json::from_value(output)
            .with_context(|| format!("invalid introspect payload from {}", script.display()))?;
        Ok(meta)
    }

    pub async fn invoke(
        &self,
        script: &Path,
        permissions: &ScriptPermissions,
        workspace: &Path,
        arguments: &Value,
    ) -> Result<Value> {
        let payload = serde_json::to_vec(arguments)?;
        let output = self
            .run_host("invoke", script, permissions, workspace, Some(&payload))
            .await?;
        if output.get("ok").and_then(Value::as_bool) == Some(true) {
            return Ok(output.get("result").cloned().unwrap_or(Value::Null));
        }
        Ok(output)
    }

    async fn run_host(
        &self,
        command: &str,
        script: &Path,
        permissions: &ScriptPermissions,
        workspace: &Path,
        stdin: Option<&[u8]>,
    ) -> Result<Value> {
        let mut cmd = Command::new(&self.deno);
        cmd.arg("run")
            .arg("--no-prompt")
            .arg("--quiet")
            .args(self.base_permissions())
            .args(self.extra_args.iter())
            .args(to_deno_flags(permissions, workspace))
            .arg(&self.host)
            .arg(command)
            .arg(script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        debug!(?cmd, "spawning Deno host");
        let mut child = cmd.spawn().with_context(|| {
            format!(
                "failed to spawn Deno at {} (is Deno installed and on PATH?)",
                self.deno.display()
            )
        })?;

        let run = async {
            if let Some(mut handle) = child.stdin.take() {
                if let Some(bytes) = stdin {
                    handle.write_all(bytes).await?;
                }
                handle.shutdown().await.ok();
            }
            child.wait_with_output().await
        };

        let output = match timeout(self.timeout, run).await {
            Ok(result) => result?,
            Err(_) => {
                bail!(
                    "Deno {} timed out after {}s for {}",
                    command,
                    self.timeout.as_secs(),
                    script.display()
                )
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if !stderr.is_empty() {
            debug!(stderr = %stderr, "Deno host stderr");
        }

        match parse_host_json(&stdout) {
            Ok(value) => {
                if let Some(false) = value.get("ok").and_then(Value::as_bool) {
                    bail!(ScriptError(format_host_error(value.get("error"))));
                }
                Ok(value)
            }
            Err(parse_error) if !output.status.success() => {
                bail!(
                    "Deno host failed (status {}): {}",
                    output.status,
                    if stderr.is_empty() {
                        parse_error.to_string()
                    } else {
                        stderr
                    }
                )
            }
            Err(error) => Err(error),
        }
    }

    fn base_permissions(&self) -> Vec<String> {
        if self.allow_all {
            return vec!["--allow-all".to_string()];
        }
        let mut flags = Vec::new();
        let dirs = self
            .scripts_dirs
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        for dir in dirs {
            flags.push(format!("--allow-read={}", dir.display()));
        }
        flags.push(format!("--allow-read={}", self.host.display()));
        flags
    }
}

async fn check_deno(deno: &Path) -> Result<()> {
    let output = Command::new(deno)
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| {
            format!(
                "Deno was not found at {}.\nRun `scriptmcp` to install it next to this executable, or pass --deno.",
                deno.display()
            )
        })?;
    if !output.status.success() {
        bail!(
            "failed to execute `{} --version`: {}",
            deno.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

fn write_host_script() -> Result<PathBuf> {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    HOST_TS.hash(&mut hasher);
    let path = std::env::temp_dir().join(format!("scriptmcp-host-{:x}.ts", hasher.finish()));
    if !path.exists() {
        std::fs::write(&path, HOST_TS)
            .with_context(|| format!("failed to write Deno host to {}", path.display()))?;
    }
    Ok(path)
}

fn format_host_error(error: Option<&Value>) -> String {
    let Some(error) = error else {
        return "script failed".into();
    };
    if let Some(text) = error.as_str() {
        return text.to_string();
    }
    let Some(object) = error.as_object() else {
        return error.to_string();
    };
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let message = object
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match (name, message) {
        ("", "") => "script failed".into(),
        ("", message) => message.to_string(),
        (name, "") => name.to_string(),
        (name, message) if message.starts_with(name) => message.to_string(),
        (name, message) => format!("{name}: {message}"),
    }
}

fn parse_host_json(stdout: &str) -> Result<Value> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        bail!("Deno host produced no output");
    }
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        return Ok(value);
    }
    if let Some(line) = trimmed
        .lines()
        .rev()
        .find(|line| line.trim_start().starts_with('{'))
    {
        return serde_json::from_str(line)
            .with_context(|| format!("failed to parse Deno host output: {line}"));
    }
    bail!("Deno host did not emit JSON: {trimmed}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_last_json_line() {
        let raw = "ignore me\n{\"ok\":true,\"result\":1}\n";
        assert_eq!(
            parse_host_json(raw).unwrap(),
            json!({"ok": true, "result": 1})
        );
    }

    #[test]
    fn formats_structured_and_string_host_errors() {
        assert_eq!(
            format_host_error(Some(&json!({
                "name": "TypeError",
                "message": "x is not a function"
            }))),
            "TypeError: x is not a function"
        );
        assert_eq!(
            format_host_error(Some(&json!("plain failure"))),
            "plain failure"
        );
        assert_eq!(format_host_error(None), "script failed");
    }
}
