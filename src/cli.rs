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
    /// Stop the running daemon
    Stop,
    /// Attach TUI to running daemon
    Tui,
    /// Show daemon status
    Status,
}

pub fn run() {
    let cli = Cli::parse();

    // Stop is synchronous — handle it before creating the async runtime.
    if matches!(cli.command, Commands::Stop) {
        stop_daemon();
        return;
    }

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
        Commands::Stop => unreachable!(),
    };

    if let Err(e) = result {
        tracing::error!("Error: {:#}", e);
        std::process::exit(1);
    }
}

fn stop_daemon() {
    use crate::config::Config;
    let pid_path = Config::base_dir().join("daemon.pid");

    let pid = match std::fs::read_to_string(&pid_path) {
        Ok(s) => s.trim().to_string(),
        Err(_) => {
            println!("Daemon is not running (no PID file).");
            return;
        }
    };

    match std::process::Command::new("kill").arg(&pid).status() {
        Ok(s) if s.success() => {
            let _ = std::fs::remove_file(&pid_path);
            println!("Daemon stopped (pid={}).", pid);
        }
        _ => {
            eprintln!("Failed to stop daemon (pid={}). It may have already exited.", pid);
            std::process::exit(1);
        }
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
}

async fn show_status() -> Result<()> {
    use crate::config::Config;
    use crate::ipc::socket_path;

    let sock = socket_path();
    let pid_path = Config::base_dir().join("daemon.pid");

    if sock.exists() {
        let pid = std::fs::read_to_string(&pid_path)
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "?".to_string());
        println!("Daemon running  pid={}  socket={}", pid, sock.display());
    } else {
        println!("Daemon not running");
    }
    Ok(())
}
