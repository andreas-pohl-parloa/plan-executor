use clap::{Parser, Subcommand};
use anyhow::Result;

#[derive(Parser)]
#[command(name = "plan-executor", about = "Monitor and execute Claude plan files")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Start the background daemon (detaches from terminal)
    Daemon {
        /// Run in the foreground without daemonizing.
        /// Use this when managed by launchd or another supervisor.
        #[arg(long)]
        foreground: bool,
    },
    /// Attach TUI to running daemon
    Tui,
    /// Show daemon status
    Status,
}

pub fn run() {
    let cli = Cli::parse();

    // Daemonize before creating the Tokio runtime — forking after Tokio's
    // thread pool is initialized is undefined behavior.
    if let Commands::Daemon { foreground } = &cli.command {
        if !foreground {
            daemonize();
        }
    }

    tracing_subscriber::fmt::init();
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");

    let result: Result<()> = match cli.command {
        Commands::Daemon { .. } => rt.block_on(crate::daemon::run_daemon()),
        Commands::Tui => rt.block_on(crate::tui::run_tui()),
        Commands::Status => rt.block_on(show_status()),
    };

    if let Err(e) = result {
        tracing::error!("Error: {:#}", e);
        std::process::exit(1);
    }
}

/// Forks the process, exits the parent, and redirects stdout/stderr to the
/// daemon log file. The child process continues past this call.
fn daemonize() {
    use crate::config::Config;
    let base_dir = Config::base_dir();
    std::fs::create_dir_all(&base_dir).expect("failed to create daemon base directory");

    let log_path = base_dir.join("daemon.log");
    let pid_path = base_dir.join("daemon.pid");

    // Print to terminal before the parent exits.
    eprintln!(
        "Starting daemon. PID file: {}  Logs: {}",
        pid_path.display(),
        log_path.display()
    );

    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .expect("failed to open daemon log file");
    let log_stderr = log_file.try_clone().expect("failed to clone log file handle");

    daemonize::Daemonize::new()
        .pid_file(&pid_path)
        .stdout(log_file)
        .stderr(log_stderr)
        .start()
        .expect("failed to daemonize");
    // Child process continues here; parent has already exited.
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
