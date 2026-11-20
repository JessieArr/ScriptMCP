use std::io::IsTerminal;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use scriptmcp::config::{Cli, Command, Opts};
use scriptmcp::install::{self, InstallProgress};
use scriptmcp::listen;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    // Resolve this before detaching: FreeConsole changes stdin's terminal status.
    let command = cli.command.unwrap_or_else(default_command);
    let console_logging = !matches!(command, Command::Ui) || cli.console;
    if !console_logging {
        detach_console();
    }
    init_tracing(console_logging);
    match command {
        Command::Ui => scriptmcp::ui::run(cli.opts),
        Command::List => list_tools(&cli.opts).await,
        Command::Serve { stdio } => {
            if stdio {
                listen::serve_stdio(&cli.opts).await
            } else {
                listen::serve_http(&cli.opts, None).await
            }
        }
        Command::InstallDeno => install_deno(),
    }
}

fn default_command() -> Command {
    if std::io::stdin().is_terminal() {
        Command::Ui
    } else {
        Command::Serve { stdio: true }
    }
}

fn detach_console() {
    // Detach only this process, never hide the caller's shared terminal window.
    // Keep the console subsystem so CLI commands and piped MCP handles work normally.
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::System::Console::FreeConsole();
    }
}

fn init_tracing(console_logging: bool) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_writer(move || -> Box<dyn std::io::Write + Send> {
            if console_logging {
                Box::new(std::io::stderr())
            } else {
                Box::new(std::io::sink())
            }
        })
        .with_env_filter(filter)
        .with_ansi(console_logging && std::io::stderr().is_terminal())
        .init();
}

fn install_deno() -> Result<()> {
    let dest = install::sidecar_path()?;
    eprintln!("Installing Deno to {}", dest.display());
    let mut last_pct = 0;
    let detected = install::install_sidecar(|progress| match progress {
        InstallProgress::Message(message) => eprintln!("{message}"),
        InstallProgress::Download { done, total } => {
            if let Some(total) = total {
                if total > 0 {
                    let pct = done * 100 / total;
                    if pct >= last_pct + 10 || done == total {
                        eprintln!("Downloading… {pct}%");
                        last_pct = pct;
                    }
                }
            }
        }
    })?;
    println!(
        "Deno ready at {} ({})",
        detected.path.display(),
        detected.version
    );
    Ok(())
}

async fn list_tools(opts: &Opts) -> Result<()> {
    use scriptmcp::app_state::AppState;
    use scriptmcp::catalog::Catalog;
    use scriptmcp::deno::DenoRuntime;

    let state = AppState::load();
    let folders = state.folders_or_fallback(&opts.scripts);
    let runtime = DenoRuntime::new(opts).await?;
    let catalog = Catalog::load_dirs(&folders, &runtime, Some(&state)).await?;
    if catalog.tools().is_empty() {
        println!("No scripts found in {}", folders_display(&folders));
        return Ok(());
    }
    for tool in catalog.tools() {
        let mark = if tool.enabled { "*" } else { " " };
        println!(
            "{mark}\t{}\t{}\t{}",
            tool.meta.name,
            tool.path.display(),
            tool.meta.description
        );
    }
    Ok(())
}

fn folders_display(folders: &[std::path::PathBuf]) -> String {
    folders
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}
