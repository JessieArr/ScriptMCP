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

impl Opts {
    pub fn timeout_duration(&self) -> Duration {
        Duration::from_secs(self.timeout)
    }

    pub fn socket_addr(&self) -> Result<SocketAddr> {
        crate::listen::parse_bind(&self.bind)
    }
}
