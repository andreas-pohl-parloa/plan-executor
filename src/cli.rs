use std::path::{Path, PathBuf};
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use libc;

#[derive(Parser)]
#[command(name = "plan-executor", about = "Monitor and execute Claude plan files")]
pub struct Cli {
    /// Path to config file. Default: ~/.plan-executor/config.json
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<std::path::PathBuf>,
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
    /// Start the daemon if it is not already running (used by shell hook)
    Ensure,
    /// Attach TUI to running daemon
    Tui,
    /// Show daemon status
    Status,
    /// List job history
    Jobs,
}

pub fn run() {
    let cli = Cli::parse();

    // Synchronous commands — handle before creating the async runtime.
    match &cli.command {
        Commands::Stop => { stop_daemon(); return; }
        Commands::Jobs => { list_jobs(); return; }
        Commands::Ensure => { ensure_daemon(); return; }
        _ => {}
    }

    // Resolve --config to an absolute path NOW, before daemonize() changes
    // the working directory to `/`. Relative paths become invalid after fork.
    let config_path: Option<std::path::PathBuf> = cli.config.as_ref().map(|p| {
        std::fs::canonicalize(p)
            .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default().join(p))
    });

    // Daemonize before creating the Tokio runtime — forking after Tokio's
    // thread pool is initialized is undefined behavior.
    if let Commands::Daemon { foreground } = &cli.command {
        if !foreground {
            daemonize();
        }
    }

    tracing_subscriber::fmt::init();
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");

    let config = crate::config::Config::load(config_path.as_deref())
        .expect("failed to load config");

    let result: Result<()> = match cli.command {
        Commands::Daemon { .. } => rt.block_on(crate::daemon::run_daemon(config)),
        Commands::Execute { plan } => rt.block_on(execute_plan(plan, config)),
        Commands::Tui => rt.block_on(crate::tui::run_tui()),
        Commands::Status => rt.block_on(show_status()),
        Commands::Stop | Commands::Jobs | Commands::Ensure => unreachable!(),
    };

    if let Err(e) = result {
        eprintln!("Error: {:#}", e);
        std::process::exit(1);
    }
}

async fn execute_plan(plan_path: String, config: crate::config::Config) -> Result<()> {
    // Canonicalize early so both paths get an absolute path.
    let plan = std::fs::canonicalize(&plan_path)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default().join(&plan_path));
    if !plan.exists() {
        anyhow::bail!("Plan file not found: {}", plan_path);
    }

    // If the daemon is running, delegate to it so TUI sessions also see the job.
    if crate::ipc::socket_path().exists() {
        return execute_via_daemon(plan, config).await;
    }
    execute_standalone(plan, config).await
}

/// Sends Execute to the daemon and streams JobDisplayLine events to the terminal.
async fn execute_via_daemon(plan: PathBuf, config: crate::config::Config) -> Result<()> {
    use crate::ipc::{DaemonEvent, TuiRequest};
    use crate::jobs::JobStatus;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    println!("╔══ plan-executor execute (via daemon) ════════════════════════");
    println!("║  Plan:  {}", plan.display());
    println!("║  Cmd:   {}", config.agents.main);
    println!("╚══════════════════════════════════════════════════════════════");
    println!();

    let stream = UnixStream::connect(crate::ipc::socket_path()).await
        .context("Daemon not reachable")?;
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half).lines();

    // Snapshot current running job IDs before we trigger execution.
    let gs = serde_json::to_string(&TuiRequest::GetState)?;
    write_half.write_all(format!("{}\n", gs).as_bytes()).await?;

    let mut existing_ids = std::collections::HashSet::<String>::new();
    if let Ok(Some(line)) = reader.next_line().await {
        if let Ok(DaemonEvent::State { running_jobs, .. }) = serde_json::from_str(&line) {
            existing_ids = running_jobs.iter().map(|j| j.id.clone()).collect();
        }
    }

    // Trigger execution.
    let req = serde_json::to_string(&TuiRequest::Execute {
        plan_path: plan.to_string_lossy().to_string(),
    })?;
    write_half.write_all(format!("{}\n", req).as_bytes()).await?;

    // Stream events until the job finishes.
    let mut our_job_id: Option<String> = None;

    loop {
        let line = match reader.next_line().await? {
            Some(l) => l,
            None => break,
        };
        if let Ok(event) = serde_json::from_str::<DaemonEvent>(&line) {
            match event {
                DaemonEvent::State { running_jobs, .. } => {
                    if our_job_id.is_none() {
                        for j in &running_jobs {
                            if !existing_ids.contains(&j.id) {
                                our_job_id = Some(j.id.clone());
                                break;
                            }
                        }
                    }
                }
                DaemonEvent::JobDisplayLine { job_id, line } => {
                    let is_ours = our_job_id.as_deref() == Some(&job_id)
                        || our_job_id.is_none();
                    if is_ours {
                        if our_job_id.is_none() { our_job_id = Some(job_id); }
                        // ANSI codes from sjv render natively in the terminal.
                        println!("{}", line);
                    }
                }
                DaemonEvent::JobUpdated { job } => {
                    if our_job_id.as_deref() == Some(&job.id)
                        && job.status != JobStatus::Running
                    {
                        println!();
                        println!("╔══ done ═══════════════════════════════════════════════════════");
                        match job.status {
                            JobStatus::Success => println!("║  Status:   success"),
                            JobStatus::Failed  => println!("║  Status:   FAILED"),
                            other              => println!("║  Status:   {:?}", other),
                        }
                        if let Some(ms) = job.duration_ms {
                            println!("║  Duration: {}s", ms / 1000);
                        }
                        if let Some(cost) = job.cost_usd {
                            println!("║  Cost:     ${:.4}", cost);
                        }
                        println!("╚══════════════════════════════════════════════════════════════");
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// Standalone execution — used when no daemon is running.
async fn execute_standalone(plan: PathBuf, config: crate::config::Config) -> Result<()> {
    use crate::executor::{spawn_execution, ExecEvent};
    use crate::handoff::{self, AgentType};
    use crate::jobs::{JobMetadata, JobStatus};
    let use_color = std::io::IsTerminal::is_terminal(&std::io::stdout());

    let execution_root = find_repo_root(&plan)
        .unwrap_or_else(|| plan.parent().unwrap_or(plan.as_path()).to_path_buf());

    let quoted = plan.to_string_lossy().replace('"', "\\\"");
    println!("╔══ plan-executor execute ═════════════════════════════════════");
    println!("║  Plan:  {}", plan.display());
    println!("║  Root:  {}", execution_root.display());
    println!("║  Cmd:   {} \\", config.agents.main);
    println!("║           -p \"/my:execute-plan-non-interactive \\\"{}\\\"\"", quoted);
    println!("╚══════════════════════════════════════════════════════════════");
    println!();

    let job = JobMetadata::new(plan.clone());
    let job_id = job.id.clone();
    let (_, _pgid, mut exec_rx) = spawn_execution(job, execution_root.clone(), &config.agents.main)
        .context("failed to spawn main agent")?;

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
                                "claude --dangerously-skip-permissions -p {}",
                                h.prompt_file.display()
                            ),
                            AgentType::Codex => format!(
                                "codex --dangerously-bypass-approvals-and-sandbox exec {}",
                                h.prompt_file.display()
                            ),
                            AgentType::Gemini => format!(
                                "gemini --yolo -p {}",
                                h.prompt_file.display()
                            ),
                        };
                        println!("│  [{}] {}", h.index, cmd);
                    }
                    println!("└──────────────────────────────────────────────────────────────");
                    println!();

                    let (results, _sub_pgids) = handoff::dispatch_all(
                        state.handoffs,
                        &config.agents.claude,
                        &config.agents.codex,
                        &config.agents.gemini,
                    ).await;

                    for r in &results {
                        if r.success {
                            println!("  [{}] ✓ done ({} chars)", r.index, r.stdout.len());
                        } else {
                            let first_err = r.stderr.lines().next().unwrap_or("(no stderr)");
                            println!("  [{}] ✗ failed: {}", r.index, first_err);
                        }
                    }

                    // Remove state file before resuming so the resume turn
                    // doesn't re-detect it and loop with another HandoffRequired.
                    let _ = std::fs::remove_file(&state_file);

                    let continuation = handoff::build_continuation(&results);
                    println!();
                    println!("┌── resume ─────────────────────────────────────────────────────");
                    println!("│  claude --dangerously-skip-permissions --verbose \\");
                    println!("│    --output-format stream-json --resume {} \\", session_id);
                    println!("│    -p <continuation>");
                    println!("└───────────────────────────────────────────────────────────────");
                    println!();

                    let (_, _, new_rx) = handoff::resume_execution(
                        &session_id,
                        &continuation,
                        execution_root.clone(),
                        Some(job_id.clone()),
                        Some(plan.clone()),
                        &config.agents.main,
                    )
                    .await
                    .context("failed to resume agent session")?;
                    exec_rx = new_rx;
                    continue 'outer;
        }

        break 'outer; // channel closed without Finished
    }

    // If the job never emitted Finished (error / cancelled), mark it Failed on disk
    // so it shows up in history rather than being filtered out as Running.
    mark_job_failed_if_running(&job_id);

    Ok(())
}

fn mark_job_failed_if_running(job_id: &str) {
    use crate::config::Config;
    use crate::jobs::{JobMetadata, JobStatus};

    let meta_path = Config::base_dir().join("jobs").join(job_id).join("metadata.json");
    let Ok(content) = std::fs::read_to_string(&meta_path) else { return };
    let Ok(mut meta) = serde_json::from_str::<JobMetadata>(&content) else { return };
    if meta.status == JobStatus::Running {
        meta.status = JobStatus::Failed;
        meta.finished_at = Some(chrono::Utc::now());
        let _ = meta.save();
    }
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

fn list_jobs() {
    use crate::jobs::{JobMetadata, JobStatus};

    let jobs = JobMetadata::load_all();
    if jobs.is_empty() {
        println!("No jobs found.");
        return;
    }

    let id_w = 8;
    let plan_w = 34;
    let status_w = 9;
    let dur_w = 10;
    let cost_w = 8;

    println!(
        "{:<id_w$}  {:<plan_w$}  {:<status_w$}  {:>dur_w$}  {:>cost_w$}",
        "ID", "PLAN", "STATUS", "DURATION", "COST",
        id_w = id_w, plan_w = plan_w, status_w = status_w,
        dur_w = dur_w, cost_w = cost_w,
    );
    println!("{}", "─".repeat(id_w + 2 + plan_w + 2 + status_w + 2 + dur_w + 2 + cost_w));

    for job in &jobs {
        let id = &job.id[..job.id.len().min(6)];

        let plan = job.plan_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?");
        let plan_truncated = if plan.len() > plan_w {
            format!("{}…", &plan[..plan_w - 1])
        } else {
            plan.to_string()
        };

        let status = match job.status {
            JobStatus::Success => "success",
            JobStatus::Failed  => "failed",
            JobStatus::Killed  => "killed",
            JobStatus::Running => "running",
        };

        let duration = job.duration_ms
            .map(|ms| format!("{}s", ms / 1000))
            .unwrap_or_else(|| "-".to_string());

        let cost = job.cost_usd
            .map(|c| format!("${:.4}", c))
            .unwrap_or_else(|| "-".to_string());

        println!(
            "{:<id_w$}  {:<plan_w$}  {:<status_w$}  {:>dur_w$}  {:>cost_w$}",
            id, plan_truncated, status, duration, cost,
            id_w = id_w, plan_w = plan_w, status_w = status_w,
            dur_w = dur_w, cost_w = cost_w,
        );
    }
}

/// Start the daemon if it is not already running. Used by the shell hook.
fn ensure_daemon() {
    use crate::ipc::socket_path;
    if socket_path().exists() {
        return; // already running, nothing to do
    }
    // Daemonize and start — same path as `plan-executor daemon`
    daemonize();
    // After daemonize() the child continues here; start the runtime and daemon.
    tracing_subscriber::fmt::init();
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let config = crate::config::Config::load(None).expect("failed to load config");
    if let Err(e) = rt.block_on(crate::daemon::run_daemon(config)) {
        tracing::error!("daemon error: {:#}", e);
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

    // Kill ALL running plan-executor daemon processes, not just the one in
    // the PID file (there may be leftover instances from previous runs).
    let our_pid = std::process::id().to_string();
    if let Ok(out) = std::process::Command::new("pgrep")
        .args(["-f", "plan-executor.*daemon"])
        .output()
    {
        let killed = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|p| p.trim() != our_pid)
            .filter_map(|p| p.trim().parse::<libc::pid_t>().ok())
            .inspect(|&pid| { unsafe { libc::kill(pid, libc::SIGTERM); } })
            .count();
        if killed > 0 {
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
    }

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

    // No .pid_file() — we write the PID ourselves in run_daemon() after fork.
    // Using pid_file() here creates a lock that conflicts when restarting.
    daemonize::Daemonize::new()
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
