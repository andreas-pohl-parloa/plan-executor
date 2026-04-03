use std::path::PathBuf;
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
    /// Execute a plan file or re-execute a job by ID prefix
    Execute {
        /// Plan file path or job ID prefix (from `plan-executor jobs`)
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
    /// Kill a running job by job ID (prefix match)
    Kill { job_id: String },
    /// Pause a running job at the next handoff
    Pause { job_id: String },
    /// Resume a paused job
    Unpause { job_id: String },
    /// Show output of a job; use -f to follow a running job
    Output {
        /// Job ID prefix (from `plan-executor jobs`)
        job_id: String,
        /// Follow live output of a running job
        #[arg(short = 'f', long)]
        follow: bool,
    },
}

pub fn run() {
    let cli = Cli::parse();

    // Synchronous commands — handle before creating the async runtime.
    match &cli.command {
        Commands::Stop   => { stop_daemon(); return; }
        Commands::Jobs   => { list_jobs(); return; }
        Commands::Ensure => { ensure_daemon(); return; }
        Commands::Kill   { job_id } => { daemon_job_request("kill",    job_id); return; }
        Commands::Pause  { job_id } => { daemon_job_request("pause",   job_id); return; }
        Commands::Unpause{ job_id } => { daemon_job_request("unpause", job_id); return; }
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
        Commands::Output { job_id, follow } => rt.block_on(output_job(job_id, follow)),
        Commands::Stop | Commands::Jobs | Commands::Ensure
        | Commands::Kill { .. } | Commands::Pause { .. } | Commands::Unpause { .. } => unreachable!(),
    };

    if let Err(e) = result {
        eprintln!("Error: {:#}", e);
        std::process::exit(1);
    }
}

async fn output_job(job_id_prefix: String, follow: bool) -> Result<()> {
    use crate::config::Config;
    use crate::ipc::{DaemonEvent, TuiRequest};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    if !crate::ipc::socket_path().exists() {
        anyhow::bail!("Daemon not running. Start with: plan-executor daemon");
    }

    // Resolve job ID prefix → full ID via daemon state.
    let stream = UnixStream::connect(crate::ipc::socket_path()).await?;
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half).lines();

    let gs = serde_json::to_string(&TuiRequest::GetState)?;
    write_half.write_all(format!("{}\n", gs).as_bytes()).await?;

    let state_line = reader.next_line().await?.unwrap_or_default();
    let (job_id, is_running) = if let Ok(DaemonEvent::State { running_jobs, history, .. }) =
        serde_json::from_str::<DaemonEvent>(&state_line)
    {
        let running_match = running_jobs.iter().find(|j| j.id.starts_with(&job_id_prefix));
        let history_match = history.iter().find(|j| j.id.starts_with(&job_id_prefix));
        match (running_match, history_match) {
            (Some(j), _) => (j.id.clone(), true),
            (_, Some(j)) => (j.id.clone(), false),
            _ => anyhow::bail!("No job matching '{}'", job_id_prefix),
        }
    } else {
        anyhow::bail!("Unexpected response from daemon");
    };

    // Print stored output from output.jsonl.
    let output_path = Config::base_dir().join("jobs").join(&job_id).join("output.jsonl");
    if output_path.exists() {
        let content = std::fs::read_to_string(&output_path)?;
        for raw in content.lines() {
            let rendered = sjv::render_runtime_line(raw, false, true);
            for line in rendered.lines().filter(|l| !l.is_empty()) {
                println!("{}", line);
            }
        }
    }

    if !follow || !is_running {
        return Ok(());
    }

    // Follow mode: stream live JobDisplayLine events for this job.
    eprintln!("[following {} — Ctrl+C to stop]", &job_id[..job_id.len().min(8)]);
    loop {
        match reader.next_line().await? {
            Some(line) => {
                if let Ok(DaemonEvent::JobDisplayLine { job_id: jid, line: text }) =
                    serde_json::from_str::<DaemonEvent>(&line)
                {
                    if jid == job_id {
                        println!("{}", text);
                    }
                } else if let Ok(DaemonEvent::JobUpdated { job }) =
                    serde_json::from_str::<DaemonEvent>(&line)
                {
                    if job.id == job_id
                        && job.status != crate::jobs::JobStatus::Running
                    {
                        eprintln!("[job finished: {:?}]", job.status);
                        break;
                    }
                }
            }
            None => break,
        }
    }
    Ok(())
}

async fn execute_plan(plan_path: String, config: crate::config::Config) -> Result<()> {
    if !crate::ipc::socket_path().exists() {
        anyhow::bail!("Daemon not running. Start with: plan-executor daemon");
    }

    // If the argument looks like a job ID prefix, resolve it to a plan path.
    let resolved_path = resolve_plan_path(&plan_path);

    // Canonicalize to absolute path.
    let plan = std::fs::canonicalize(&resolved_path)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default().join(&resolved_path));
    if !plan.exists() {
        anyhow::bail!("Plan file not found: {}", resolved_path);
    }

    execute_via_daemon(plan, config).await
}

/// If `arg` matches a job ID prefix in daemon state, returns the plan path.
/// Otherwise returns `arg` unchanged (treat as plan file path).
/// Never reads from disk.
fn resolve_plan_path(arg: &str) -> String {
    use crate::ipc::{DaemonEvent, TuiRequest};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    if let Ok(mut s) = UnixStream::connect(crate::ipc::socket_path()) {
        let gs = serde_json::to_string(&TuiRequest::GetState).unwrap_or_default();
        let _ = s.write_all(format!("{}\n", gs).as_bytes());
        let mut reader = BufReader::new(s);
        let mut line = String::new();
        let _ = reader.read_line(&mut line);
        if let Ok(DaemonEvent::State { running_jobs, history, pending_plans, .. }) = serde_json::from_str(&line) {
            // 1. Match pending plan by filename or full path prefix
            if let Some(p) = pending_plans.iter().find(|p| {
                let fname = std::path::Path::new(&p.plan_path)
                    .file_name().and_then(|n| n.to_str()).unwrap_or("");
                fname.starts_with(arg) || p.plan_path.starts_with(arg)
            }) {
                return p.plan_path.clone();
            }
            // 2. Match job ID prefix (history or running)
            if let Some(job) = running_jobs.into_iter().chain(history)
                .find(|j| j.id.starts_with(arg))
            {
                return job.plan_path.to_string_lossy().into_owned();
            }
        }
    }
    arg.to_string()
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

/// Resolves a job ID prefix to a full ID from running jobs, sends the
/// corresponding daemon request, and prints the result.
fn daemon_job_request(action: &str, job_id_prefix: &str) {
    use crate::ipc::{DaemonEvent, TuiRequest};
    use std::os::unix::net::UnixStream;
    use std::io::{BufRead, BufReader, Write};

    let sock = crate::ipc::socket_path();
    if !sock.exists() {
        eprintln!("Daemon not running.");
        std::process::exit(1);
    }

    let mut stream = match UnixStream::connect(&sock) {
        Ok(s) => s,
        Err(e) => { eprintln!("Cannot connect to daemon: {}", e); std::process::exit(1); }
    };

    // Get state to resolve job ID prefix.
    let gs = serde_json::to_string(&TuiRequest::GetState).unwrap();
    let _ = stream.write_all(format!("{}\n", gs).as_bytes());
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    let _ = reader.read_line(&mut line);

    let full_id = if let Ok(DaemonEvent::State { running_jobs, .. }) = serde_json::from_str(&line) {
        running_jobs.into_iter()
            .find(|j| j.id.starts_with(job_id_prefix))
            .map(|j| j.id)
    } else {
        None
    };

    let Some(job_id) = full_id else {
        eprintln!("No running job matching '{}'.", job_id_prefix);
        std::process::exit(1);
    };

    let req = match action {
        "kill"    => TuiRequest::KillJob   { job_id: job_id.clone() },
        "pause"   => TuiRequest::PauseJob  { job_id: job_id.clone() },
        "unpause" => TuiRequest::ResumeJob { job_id: job_id.clone() },
        _ => unreachable!(),
    };

    let _ = stream.write_all(format!("{}\n", serde_json::to_string(&req).unwrap()).as_bytes());
    println!("{} job {}.", action, &job_id[..job_id.len().min(8)]);
}

fn list_jobs() {
    use crate::ipc::{DaemonEvent, TuiRequest};
    use crate::jobs::{JobMetadata, JobStatus};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    if !crate::ipc::socket_path().exists() {
        eprintln!("Daemon not running. Start with: plan-executor daemon");
        std::process::exit(1);
    }

    let mut s = match UnixStream::connect(crate::ipc::socket_path()) {
        Ok(s) => s,
        Err(e) => { eprintln!("Cannot connect to daemon: {}", e); std::process::exit(1); }
    };
    let gs = serde_json::to_string(&TuiRequest::GetState).unwrap_or_default();
    let _ = s.write_all(format!("{}\n", gs).as_bytes());
    let mut reader = BufReader::new(s);
    let mut line = String::new();
    let _ = reader.read_line(&mut line);

    let (jobs, pending) = if let Ok(DaemonEvent::State { running_jobs, history, pending_plans, .. }) = serde_json::from_str::<DaemonEvent>(&line) {
        let jobs: Vec<JobMetadata> = running_jobs.into_iter().chain(history).collect();
        (jobs, pending_plans)
    } else {
        eprintln!("Unexpected response from daemon.");
        std::process::exit(1);
    };

    if jobs.is_empty() && pending.is_empty() {
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

    // Show pending (READY) plans first.
    for p in &pending {
        let plan = std::path::Path::new(&p.plan_path)
            .file_name().and_then(|n| n.to_str()).unwrap_or(&p.plan_path);
        let plan_truncated = if plan.len() > plan_w {
            format!("{}…", &plan[..plan_w - 1])
        } else {
            plan.to_string()
        };
        println!(
            "{:<id_w$}  {:<plan_w$}  {:<status_w$}  {:>dur_w$}  {:>cost_w$}",
            "-", plan_truncated, "ready", "-", "-",
            id_w = id_w, plan_w = plan_w, status_w = status_w,
            dur_w = dur_w, cost_w = cost_w,
        );
    }

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
