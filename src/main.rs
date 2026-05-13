use std::env;

use anyhow::Result;
use clap::Parser;

mod app;
mod status;
mod store;
mod tmux;

#[derive(Debug, Parser)]
#[command(author, version, about)]
struct Cli {
    /// Connect to a known Copilot remote session or task ID inside managed tmux.
    #[arg(long)]
    connect: Option<String>,

    /// Start new Copilot sessions with Copilot remote control enabled.
    #[arg(long)]
    remote: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    app::run(
        env::current_dir()?,
        app::AppOptions {
            connect: cli.connect,
            remote_enabled: cli.remote,
        },
    )
}
