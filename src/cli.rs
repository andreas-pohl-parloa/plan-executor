use clap::{Parser, Subcommand};
use anyhow::Result;
use tracing_subscriber;

#[derive(Parser)]
#[command(name = "plan-executor", about = "Monitor and execute Claude plan files")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Start the background daemon
    Daemon,
    /// Attach TUI to running daemon
    Tui,
    /// Show daemon status
    Status,
}

pub fn run() {
    let cli = Cli::parse();
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    tracing_subscriber::fmt::init();

    let result: Result<()> = match cli.command {
        Commands::Daemon => rt.block_on(crate::daemon::run_daemon()),
        Commands::Tui => rt.block_on(crate::tui::run_tui()),
        Commands::Status => rt.block_on(show_status()),
    };

    if let Err(e) = result {
        tracing::error!("Error: {:#}", e);
        std::process::exit(1);
    }
}

async fn show_status() -> Result<()> {
    use crate::ipc::socket_path;
    let sock = socket_path();
    if sock.exists() {
        tracing::info!("Daemon running (socket: {})", sock.display());
    } else {
        tracing::info!("Daemon not running");
    }
    Ok(())
}
