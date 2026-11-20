use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "scriptmcp",
    version,
    about = "MCP frontend that exposes Deno scripts as tools"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Keep the GUI's console attached and write logs to stderr
    #[arg(long, global = true)]
    pub console: bool,

    #[command(flatten)]
    pub opts: Opts,
}

#[derive(Debug, Clone, Args)]
pub struct Opts {
    /// Directory of Deno scripts to expose as MCP tools
    #[arg(
        long,
        env = "SCRIPTMCP_SCRIPTS",
        default_value = "scripts",
        global = true
    )]
    pub scripts: PathBuf,

    /// Deno executable
    #[arg(long, env = "SCRIPTMCP_DENO", default_value = "deno", global = true)]
    pub deno: PathBuf,

    /// Grant every Deno permission to scripts
    #[arg(long, global = true)]
    pub allow_all: bool,

    /// Extra arguments forwarded to `deno run`
    #[arg(long = "deno-arg", global = true)]
    pub deno_args: Vec<String>,

    /// Per-invocation timeout in seconds
    #[arg(long, default_value_t = 30, global = true)]
    pub timeout: u64,

    /// HTTP bind address for Streamable HTTP MCP
    #[arg(
        long,
        env = "SCRIPTMCP_BIND",
        default_value = "127.0.0.1:8788",
        global = true
    )]
    pub bind: String,
}

#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Open the setup UI
    Ui,
    /// Serve MCP (HTTP on localhost by default)
    Serve {
        /// Speak MCP over stdin/stdout instead of HTTP
        #[arg(long)]
        stdio: bool,
    },
    /// Discover scripts and print the resulting tool catalog
    List,
    /// Download Deno into the directory that contains this executable
    InstallDeno,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gui_console_is_opt_in() {
        for args in [vec!["scriptmcp"], vec!["scriptmcp", "ui"]] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert!(!cli.console);
        }
    }

    #[test]
    fn console_flag_works_before_or_after_ui() {
        for args in [
            vec!["scriptmcp", "--console"],
            vec!["scriptmcp", "--console", "ui"],
            vec!["scriptmcp", "ui", "--console"],
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert!(cli.console);
        }
    }

    #[test]
    fn stdio_command_is_unchanged() {
        let cli = Cli::try_parse_from(["scriptmcp", "serve", "--stdio"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Serve { stdio: true })));
        assert!(!cli.console);
    }
}

impl Opts {
    pub fn timeout_duration(&self) -> Duration {
        Duration::from_secs(self.timeout)
    }

    pub fn socket_addr(&self) -> Result<SocketAddr> {
        crate::listen::parse_bind(&self.bind)
    }
}
