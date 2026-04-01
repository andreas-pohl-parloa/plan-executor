use std::path::{Path, PathBuf};
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

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
    /// Execute a plan file directly, streaming output to the terminal
    Execute {
        /// Path to the plan file
        plan: String,
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
        Commands::Execute { plan } => rt.block_on(execute_plan(plan)),
        Commands::Tui => rt.block_on(crate::tui::run_tui()),
        Commands::Status => rt.block_on(show_status()),
        Commands::Stop => unreachable!(),
    };

    if let Err(e) = result {
        eprintln!("Error: {:#}", e);
        std::process::exit(1);
    }
}

async fn execute_plan(plan_path: String) -> Result<()> {
    use crate::executor::{spawn_execution, ExecEvent};
    use crate::handoff::{self, AgentType};
    use crate::jobs::{JobMetadata, JobStatus};
    let use_color = std::io::IsTerminal::is_terminal(&std::io::stdout());

    let plan = PathBuf::from(&plan_path);
    if !plan.exists() {
        anyhow::bail!("Plan file not found: {}", plan_path);
    }

    let execution_root = find_repo_root(&plan)
        .unwrap_or_else(|| plan.parent().unwrap_or(plan.as_path()).to_path_buf());

    let quoted = plan_path.replace('"', "\\\"");
    println!("╔══ plan-executor execute ═════════════════════════════════════");
    println!("║  Plan:  {}", plan_path);
    println!("║  Root:  {}", execution_root.display());
    println!("║  Cmd:   claude --dangerously-skip-permissions --verbose \\");
    println!("║           --output-format stream-json \\");
    println!("║           -p \"/my:execute-plan-non-interactive \\\"{}\\\"\"", quoted);
    println!("╚══════════════════════════════════════════════════════════════");
    println!();

    let job = JobMetadata::new(plan.clone());
    let job_id = job.id.clone();
    let (_, mut exec_rx) = spawn_execution(job, execution_root.clone())
        .context("failed to spawn claude")?;

    'outer: loop {
        let mut handoff_event: Option<(String, std::path::PathBuf)> = None;
        let mut finished_event: Option<crate::jobs::JobMetadata> = None;

        while let Some(event) = exec_rx.recv().await {
            match event {
                ExecEvent::OutputLine(line) => {
                    let rendered = sjv::render_runtime_line(&line, false, use_color);
                    if !rendered.is_empty() {
                        println!("{}", rendered);
                    }
                }
                ExecEvent::DisplayLine(_) => {} // rendered via OutputLine above
                ExecEvent::HandoffRequired { session_id, state_file } => {
                    handoff_event = Some((session_id, state_file));
                    break;
                }
                ExecEvent::Finished(job) => {
                    finished_event = Some(job);
                    break;
                }
            }
        }

        if let Some(finished) = finished_event {
            println!();
            println!("╔══ done ═══════════════════════════════════════════════════════");
            match finished.status {
                JobStatus::Success => println!("║  Status:   success"),
                JobStatus::Failed  => println!("║  Status:   FAILED"),
                other              => println!("║  Status:   {:?}", other),
            }
            if let Some(ms) = finished.duration_ms {
                println!("║  Duration: {}s", ms / 1000);
            }
            if let Some(cost) = finished.cost_usd {
                println!("║  Cost:     ${:.4}", cost);
            }
            if let (Some(i), Some(o)) = (finished.input_tokens, finished.output_tokens) {
                println!("║  Tokens:   {} in / {} out", i, o);
            }
            println!("╚══════════════════════════════════════════════════════════════");
            break 'outer;
        }

        if let Some((session_id, state_file)) = handoff_event {
                    let state = handoff::load_state(&state_file)
                        .context("failed to read handoff state file")?;

                    println!();
                    println!("┌── handoff: phase={} ──────────────────────────────────────────", state.phase);
                    println!("│  Session: {}", session_id);
                    println!("│  Dispatching {} sub-agent(s):", state.handoffs.len());
                    for h in &state.handoffs {
                        let cmd = match h.agent_type {
                            AgentType::Claude => format!(
                                "claude --dangerously-skip-permissions -p <{}>",
                                h.prompt_file.display()
                            ),
                            AgentType::Codex => format!(
                                "codex --dangerously-bypass-approvals-and-sandbox exec <{}>",
                                h.prompt_file.display()
                            ),
                            AgentType::Gemini => format!(
                                "gemini --yolo -p <{}>",
                                h.prompt_file.display()
                            ),
                        };
                        println!("│  [{}] {}", h.index, cmd);
                    }
                    println!("└──────────────────────────────────────────────────────────────");
                    println!();

                    let results = handoff::dispatch_all(state.handoffs).await;

                    for r in &results {
                        if r.success {
                            println!("  [{}] ✓ done ({} chars)", r.index, r.stdout.len());
                        } else {
                            let first_err = r.stderr.lines().next().unwrap_or("(no stderr)");
                            println!("  [{}] ✗ failed: {}", r.index, first_err);
                        }
                    }

                    let continuation = handoff::build_continuation(&results);
                    println!();
                    println!("┌── resume ─────────────────────────────────────────────────────");
                    println!("│  claude --dangerously-skip-permissions --verbose \\");
                    println!("│    --output-format stream-json --resume {} \\", session_id);
                    println!("│    -p <continuation>");
                    println!("└───────────────────────────────────────────────────────────────");
                    println!();

                    let (_, new_rx) = handoff::resume_execution(
                        &session_id,
                        &continuation,
                        execution_root.clone(),
                        Some(job_id.clone()),
                        Some(plan.clone()),
                    )
                    .await
                    .context("failed to resume claude session")?;
                    exec_rx = new_rx;
                    continue 'outer;
        }

        break 'outer; // channel closed without Finished
    }

    Ok(())
}

fn find_repo_root(path: &Path) -> Option<PathBuf> {
    let mut dir = if path.is_file() {
        path.parent()?.to_path_buf()
    } else {
        path.to_path_buf()
    };
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        dir = dir.parent()?.to_path_buf();
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
