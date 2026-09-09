use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::debug;

use crate::catalog::ScriptMeta;
use crate::config::Opts;
use crate::permissions::to_deno_flags;

const HOST_TS: &str = include_str!("../runtime/host.ts");

#[derive(Debug, Clone)]
pub struct DenoRuntime {
    deno: PathBuf,
    host: PathBuf,
    allow_all: bool,
    extra_args: Vec<String>,
    timeout: Duration,
    scripts_dir: PathBuf,
}

#[derive(Debug, Deserialize)]
struct HostEnvelope {
    ok: Option<bool>,
    result: Option<Value>,
    error: Option<String>,
}

impl DenoRuntime {
    pub async fn new(opts: &Opts) -> Result<Self> {
        let scripts_dir = opts
            .scripts
            .canonicalize()
            .with_context(|| format!("scripts directory not found: {}", opts.scripts.display()))?;
        let deno = crate::install::resolve_deno(&opts.deno);
        check_deno(&deno).await?;
        let host = write_host_script()?;
        Ok(Self {
            deno,
            host,
            allow_all: opts.allow_all,
            extra_args: opts.deno_args.clone(),
            timeout: opts.timeout_duration(),
            scripts_dir,
        })
    }

    pub async fn introspect(&self, script: &Path) -> Result<ScriptMeta> {
        let output = self
            .run_host("introspect", script, &[], None)
            .await
            .with_context(|| format!("introspect {}", script.display()))?;
        let meta: ScriptMeta = serde_json::from_value(output)
            .with_context(|| format!("invalid introspect payload from {}", script.display()))?;
        Ok(meta)
    }

    pub async fn invoke(
        &self,
        script: &Path,
        permissions: &[String],
        arguments: &Value,
    ) -> Result<Value> {
        let payload = serde_json::to_vec(arguments)?;
        let output = self
            .run_host("invoke", script, permissions, Some(&payload))
            .await
            .with_context(|| format!("invoke {}", script.display()))?;

        let envelope: HostEnvelope =
            serde_json::from_value(output.clone()).unwrap_or(HostEnvelope {
                ok: Some(true),
                result: Some(output),
                error: None,
            });

        if envelope.ok == Some(false) {
            bail!(
                "{}",
                envelope
                    .error
                    .unwrap_or_else(|| "script returned an error".into())
            );
        }
        Ok(envelope.result.unwrap_or(Value::Null))
    }

    async fn run_host(
        &self,
        command: &str,
        script: &Path,
        permissions: &[String],
        stdin: Option<&[u8]>,
    ) -> Result<Value> {
        let mut cmd = Command::new(&self.deno);
        cmd.arg("run")
            .arg("--no-prompt")
            .arg("--quiet")
            .args(self.base_permissions())
            .args(self.extra_args.iter())
            .args(to_deno_flags(permissions))
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
                    let error = value
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("script failed");
                    bail!("{error}");
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
        vec![
            format!("--allow-read={}", self.scripts_dir.display()),
            format!("--allow-read={}", self.host.display()),
        ]
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
}
