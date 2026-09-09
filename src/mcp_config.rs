use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::config::Opts;
use crate::install::{DenoSource, DetectedDeno};

pub const SERVER_NAME: &str = "scriptmcp";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StdioLaunch {
    pub command: String,
    pub args: Vec<String>,
}

impl StdioLaunch {
    pub fn from_opts(opts: &Opts, deno: Option<&DetectedDeno>) -> Self {
        let command = absolute_command();
        let mut args = vec![
            "serve".to_string(),
            "--stdio".to_string(),
            "--scripts".to_string(),
            absolute_scripts(&opts.scripts).display().to_string(),
        ];
        if opts.allow_all {
            args.push("--allow-all".to_string());
        }
        if opts.timeout != 30 {
            args.push("--timeout".to_string());
            args.push(opts.timeout.to_string());
        }
        if let Some(deno) = deno {
            if deno.source != DenoSource::Path || deno.path.is_absolute() {
                if let Some(path) = absolute_if_possible(&deno.path) {
                    args.push("--deno".to_string());
                    args.push(path);
                }
            }
        }
        for extra in &opts.deno_args {
            args.push("--deno-arg".to_string());
            args.push(extra.clone());
        }
        Self { command, args }
    }

    pub fn args_line(&self) -> String {
        self.args
            .iter()
            .map(|arg| shell_quote(arg))
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub fn pretty_json(&self) -> String {
        pretty(&json!({
            "mcpServers": {
                SERVER_NAME: {
                    "type": "stdio",
                    "command": self.command,
                    "args": self.args,
                }
            }
        }))
    }
}

pub fn http_url(bind: SocketAddr) -> String {
    format!("http://{bind}/mcp")
}

pub fn http_json(url: &str) -> String {
    pretty(&json!({
        "mcpServers": {
            SERVER_NAME: {
                "type": "http",
                "url": url,
            }
        }
    }))
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".into())
}

fn absolute_command() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.canonicalize().ok().or(Some(path)))
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "scriptmcp".into())
}

fn absolute_scripts(scripts: &Path) -> PathBuf {
    let joined = if scripts.is_absolute() {
        scripts.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(scripts))
            .unwrap_or_else(|_| scripts.to_path_buf())
    };
    joined.canonicalize().unwrap_or(joined)
}

fn absolute_if_possible(path: &Path) -> Option<String> {
    if path.as_os_str().is_empty() {
        return None;
    }
    if path.is_absolute() {
        return Some(
            path.canonicalize()
                .unwrap_or_else(|_| path.to_path_buf())
                .display()
                .to_string(),
        );
    }
    None
}

fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '/' | '='))
    {
        value.to_string()
    } else {
        format!("\"{}\"", value.replace('"', "\\\""))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(scripts: &str) -> Opts {
        Opts {
            scripts: PathBuf::from(scripts),
            deno: PathBuf::from("deno"),
            allow_all: false,
            deno_args: Vec::new(),
            timeout: 30,
            bind: "127.0.0.1:8788".into(),
        }
    }

    #[test]
    fn stdio_launch_uses_serve_stdio() {
        let launch = StdioLaunch::from_opts(&opts("/tmp/tools"), None);
        assert_eq!(launch.args[0], "serve");
        assert_eq!(launch.args[1], "--stdio");
        assert!(launch.pretty_json().contains("\"type\": \"stdio\""));
    }

    #[test]
    fn http_json_uses_url() {
        let json = http_json("http://127.0.0.1:8788/mcp");
        assert!(json.contains("http://127.0.0.1:8788/mcp"));
        assert!(json.contains("\"type\": \"http\""));
    }
}
