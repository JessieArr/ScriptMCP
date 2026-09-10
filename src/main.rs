use std::io::IsTerminal;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use scriptmcp::config::{Cli, Command, Opts};
use scriptmcp::install::{self, InstallProgress};
use scriptmcp::listen;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();
    match cli.command.unwrap_or_else(default_command) {
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

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .with_ansi(std::io::stderr().is_terminal())
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
    use scriptmcp::catalog::Catalog;
    use scriptmcp::deno::DenoRuntime;

    let runtime = DenoRuntime::new(opts).await?;
    let catalog = Catalog::load(&opts.scripts, &runtime).await?;
    if catalog.tools().is_empty() {
        println!("No scripts found in {}", opts.scripts.display());
        return Ok(());
    }
    for tool in catalog.tools() {
        println!(
            "{}\t{}\t{}",
            tool.meta.name,
            tool.path.display(),
            tool.meta.description
        );
    }
    Ok(())
}
