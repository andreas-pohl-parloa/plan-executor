use std::path::PathBuf;
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};


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
        /// Run in foreground without the daemon
        #[arg(short = 'f', long)]
        foreground: bool,
    },
    /// Stop the running daemon
    Stop,
    /// Start the daemon if it is not already running (used by shell hook)
    Ensure,
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
    /// Retry the handoff for a job whose sub-agents were never dispatched
    Retry {
        /// Job ID prefix (from `plan-executor jobs`)
        job_id: String,
    },
    /// Interactive wizard to configure remote execution secrets
    RemoteSetup,
}

/// Prints a display line to the terminal, coloring plan-executor prefix lines
/// the same way the TUI does (yellow prefix, red for failures; green ⏺ bullet).
fn print_display_line(line: &str) {
    if let Some(rest) = line.strip_prefix("⏺ [plan-executor]") {
        let is_failure = rest.contains("failed");
        if is_failure {
            println!("\x1b[31m⏺ [plan-executor]{}\x1b[0m", rest);
        } else {
            println!("\x1b[33m⏺ [plan-executor]\x1b[0m{}", rest);
        }
    } else if let Some(rest) = line.strip_prefix("⏺") {
        println!("\x1b[32m⏺\x1b[0m{}", rest);
    } else {
        println!("{}", line);
    }
}

/// Dim-indented prefix used to nest sub-agent output under the main
/// agent's display. Keeps the context visible at a glance — any line
/// wearing this prefix is coming from a spawned sub-agent.
const SUBAGENT_PREFIX: &str = "\x1b[2m│  \x1b[0m";

/// Renders one sub-agent's persisted JSONL output via sjv and prints
/// each resulting visible line with `SUBAGENT_PREFIX`. Best-effort: if
/// no file is found or reading fails, the function silently skips
/// (sub-agent output is optional context, not critical signal).
fn render_subagent_output(job_id: &str, dispatch: u32, index: usize) {
    use std::path::PathBuf;
    let base: PathBuf = crate::config::Config::base_dir()
        .join("jobs")
        .join(job_id)
        .join("sub-agents");
    let Ok(entries) = std::fs::read_dir(&base) else { return };

    let prefix_stdout = format!("dispatch-{}-agent-{}-", dispatch, index);
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with(&prefix_stdout) {
            continue;
        }
        let path = entry.path();
        let is_stderr = path.extension().and_then(|s| s.to_str()) == Some("stderr");
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let header = if is_stderr {
            format!(
                "{}\x1b[2m─── sub-agent {} stderr ───\x1b[0m",
                SUBAGENT_PREFIX, index,
            )
        } else {
            format!(
                "{}\x1b[2m─── sub-agent {} output ───\x1b[0m",
                SUBAGENT_PREFIX, index,
            )
        };
        println!("{}", header);
        for raw_line in content.lines() {
            if raw_line.is_empty() {
                continue;
            }
            let rendered = if is_stderr {
                // stderr is plain text — print raw.
                format!("\x1b[2m{}\x1b[0m", raw_line)
            } else {
                // stdout is JSONL from a streaming agent — run it
                // through sjv, same as the main agent.
                sjv::render_runtime_line(raw_line, false, true)
            };
            for visual in rendered.lines() {
                if visual.is_empty() {
                    continue;
                }
                println!("{}{}", SUBAGENT_PREFIX, visual);
            }
        }
    }
}

fn terminal_width() -> usize {
    #[cfg(unix)]
    {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_col > 0 {
            return ws.ws_col as usize;
        }
    }
    80 // fallback
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.len() > max {
        format!("{}…", &s[..max.saturating_sub(1)])
    } else {
        s.to_string()
    }
}

/// Lists `(pid, command)` pairs for every process in `pgid`. Uses
/// `ps -g <pgid> -o pid=,command=` which works on both macOS and Linux.
/// Returns an empty vec on any ps failure.
fn processes_in_pgid(pgid: u32) -> Vec<(u32, String)> {
    let output = std::process::Command::new("ps")
        .args(["-o", "pid=,command=", "-g", &pgid.to_string()])
        .output();
    let Ok(output) = output else { return Vec::new() };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|l| {
            let trimmed = l.trim_start();
            let (pid_str, cmd) = trimmed.split_once(char::is_whitespace)?;
            let pid = pid_str.parse::<u32>().ok()?;
            Some((pid, cmd.trim().to_string()))
        })
        .collect()
}

/// Prints a dimmed sub-tree of process groups and PIDs under a running
/// job. Emits nothing if the job has no tracked pgids or no live processes.
fn render_job_process_tree(procs: &crate::ipc::JobProcesses, term_w: usize) {
    const DIM: &str = "\x1b[2m";
    const RESET: &str = "\x1b[0m";

    let mut groups: Vec<(&str, u32)> = Vec::new();
    if let Some(pgid) = procs.main_pgid {
        groups.push(("main", pgid));
    }
    for pgid in &procs.sub_agent_pgids {
        groups.push(("sub-agent", *pgid));
    }
    if groups.is_empty() {
        return;
    }

    let resolved: Vec<(&str, u32, Vec<(u32, String)>)> = groups
        .into_iter()
        .map(|(label, pgid)| (label, pgid, processes_in_pgid(pgid)))
        .filter(|(_, _, procs)| !procs.is_empty())
        .collect();
    if resolved.is_empty() {
        return;
    }

    for (gi, (label, pgid, processes)) in resolved.iter().enumerate() {
        let is_last_group = gi == resolved.len() - 1;
        let group_branch = if is_last_group { "└─" } else { "├─" };
        println!(
            "{}  {} {} pgroup {}{}",
            DIM, group_branch, label, pgid, RESET
        );
        let spine = if is_last_group { " " } else { "│" };
        for (pi, (pid, cmd)) in processes.iter().enumerate() {
            let is_last_proc = pi == processes.len() - 1;
            let proc_branch = if is_last_proc { "└─" } else { "├─" };
            // 2 spaces + spine + 2 spaces + branch (2) + space + pid + space
            let pid_str = pid.to_string();
            let fixed = 2 + 1 + 2 + 2 + 1 + pid_str.len() + 1;
            let max_cmd = term_w.saturating_sub(fixed).max(20);
            let cmd_display = truncate_str(cmd, max_cmd);
            println!(
                "{}  {}  {} {} {}{}",
                DIM, spine, proc_branch, pid_str, cmd_display, RESET
            );
        }
    }
}

/// Formats a duration in seconds as a compact two-unit string:
///   <60s     → "45s"
///   <60m     → "12m34s"
///   <24h     → "2h15m"
///   >=24h    → "3d02h"
fn format_duration(total_seconds: u64) -> String {
    if total_seconds < 60 {
        format!("{}s", total_seconds)
    } else if total_seconds < 3_600 {
        let m = total_seconds / 60;
        let s = total_seconds % 60;
        format!("{}m{:02}s", m, s)
    } else if total_seconds < 86_400 {
        let h = total_seconds / 3_600;
        let m = (total_seconds % 3_600) / 60;
        format!("{}h{:02}m", h, m)
    } else {
        let d = total_seconds / 86_400;
        let h = (total_seconds % 86_400) / 3_600;
        format!("{}d{:02}h", d, h)
    }
}

pub fn run() {
    let cli = Cli::parse();

    // Synchronous commands — handle before creating the async runtime.
    match &cli.command {
        Commands::Stop   => { stop_daemon(); return; }
        Commands::Jobs   => { list_jobs(); return; }
        Commands::Ensure => { ensure_daemon(); return; }
        Commands::RemoteSetup => { remote_setup(); return; }
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

    // Default to info-level logging when RUST_LOG is not set.
    // After daemonize(), stderr points to ~/.plan-executor/daemon.log.
    if std::env::var("RUST_LOG").is_err() {
        unsafe { std::env::set_var("RUST_LOG", "plan_executor=info"); }
    }
    tracing_subscriber::fmt::init();
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");

    let config = crate::config::Config::load(config_path.as_deref())
        .expect("failed to load config");

    let result: Result<()> = match cli.command {
        Commands::Daemon { .. } => rt.block_on(crate::daemon::run_daemon(config)),
        Commands::Execute { plan, foreground } => {
            if foreground {
                rt.block_on(execute_foreground(plan, config))
            } else {
                rt.block_on(execute_plan(plan, config))
            }
        }
        Commands::Status => rt.block_on(show_status()),
        Commands::Output { job_id, follow } => rt.block_on(output_job(job_id, follow)),
        Commands::Retry { job_id } => rt.block_on(retry_job(job_id)),
        Commands::Stop | Commands::Jobs | Commands::Ensure | Commands::RemoteSetup
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

    // Print stored display output from display.log (pre-rendered, includes
    // [plan-executor] lines). Sub-agent output is interleaved inline: each
    // `dispatching N sub-agent(s)` line advances a dispatch counter, and
    // before each `sub-agent <N> done` / `sub-agent <N> failed` line we
    // render the matching persisted JSONL file with a dim indented prefix
    // so the context is clear.
    let display_path = Config::base_dir().join("jobs").join(&job_id).join("display.log");
    if display_path.exists() {
        let content = std::fs::read_to_string(&display_path)?;
        let mut dispatch_counter: u32 = 0;
        for line in content.lines() {
            if line.contains("⏺ [plan-executor] dispatching")
                && line.contains("sub-agent(s)")
            {
                dispatch_counter += 1;
            }
            if let Some(idx) = parse_subagent_done_index(line) {
                render_subagent_output(&job_id, dispatch_counter, idx);
            }
            print_display_line(line);
        }
    }

    if !follow || !is_running {
        return Ok(());
    }

    // Follow mode: stream live JobDisplayLine events for this job.
    // Use the same interleave logic so live output matches the replay.
    eprintln!("[following {} — Ctrl+C to stop]", &job_id[..job_id.len().min(8)]);
    let mut dispatch_counter: u32 = 0;
    while let Some(line) = reader.next_line().await? {
                if let Ok(DaemonEvent::JobDisplayLine { job_id: jid, line: text }) =
                    serde_json::from_str::<DaemonEvent>(&line)
                {
                    if jid == job_id {
                        if text.contains("⏺ [plan-executor] dispatching")
                            && text.contains("sub-agent(s)")
                        {
                            dispatch_counter += 1;
                        }
                        if let Some(idx) = parse_subagent_done_index(&text) {
                            render_subagent_output(&job_id, dispatch_counter, idx);
                        }
                        print_display_line(&text);
                    }
                } else if let Ok(DaemonEvent::JobUpdated { job }) =
                    serde_json::from_str::<DaemonEvent>(&line)
                {
                    if job.id == job_id && job.status != crate::jobs::JobStatus::Running {
                        eprintln!("[job finished: {:?}]", job.status);
                        break;
                    }
                }
    }
    Ok(())
}

/// Parses `⏺ [plan-executor] sub-agent <N> done` / `failed` /
/// `skipped (can-fail)` lines and returns the sub-agent index. Used by
/// `output_job` to look up the matching persisted sub-agent output.
fn parse_subagent_done_index(line: &str) -> Option<usize> {
    // Match any of: "sub-agent <N> done", "sub-agent <N> failed:",
    // "sub-agent <N> skipped (can-fail):".
    let after = line.strip_prefix("⏺ [plan-executor] sub-agent ")?;
    let (num, rest) = after.split_once(' ')?;
    let idx: usize = num.parse().ok()?;
    if rest.starts_with("done")
        || rest.starts_with("failed")
        || rest.starts_with("skipped")
    {
        Some(idx)
    } else {
        None
    }
}

async fn retry_job(job_id_prefix: String) -> Result<()> {
    use crate::ipc::{DaemonEvent, TuiRequest};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    if !crate::ipc::socket_path().exists() {
        anyhow::bail!("Daemon not running. Start with: plan-executor daemon");
    }

    // Resolve prefix → full job ID from history.
    let job = crate::jobs::JobMetadata::load_by_id_prefix(&job_id_prefix)
        .ok_or_else(|| anyhow::anyhow!("No job matching '{}'", job_id_prefix))?;
    let job_id = job.id.clone();

    println!("Retrying handoff for job {} ({})", &job_id[..job_id.len().min(8)],
        job.plan_path.file_name().and_then(|n| n.to_str()).unwrap_or("?"));

    let stream = UnixStream::connect(crate::ipc::socket_path()).await?;
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half).lines();

    let req = serde_json::to_string(&TuiRequest::RetryHandoff { job_id: job_id.clone() })?;
    write_half.write_all(format!("{}\n", req).as_bytes()).await?;

    // Wait briefly for confirmation that the job moved to Running, then detach.
    let short_id = &job_id[..job_id.len().min(8)];
    let timeout = tokio::time::sleep(std::time::Duration::from_secs(2));
    tokio::pin!(timeout);

    loop {
        tokio::select! {
            line = reader.next_line() => {
                let Ok(Some(line)) = line else { break };
                if let Ok(event) = serde_json::from_str::<DaemonEvent>(&line) {
                    match event {
                        DaemonEvent::State { running_jobs, .. } => {
                            if running_jobs.iter().any(|j| j.id == job_id) {
                                println!("Retrying (job {})", short_id);
                                println!("Watch: plan-executor output -f {}", short_id);
                                return Ok(());
                            }
                        }
                        DaemonEvent::Error { message } => {
                            eprintln!("Error: {}", message);
                            return Ok(());
                        }
                        _ => {}
                    }
                }
            }
            _ = &mut timeout => {
                // Timed out waiting for confirmation — assume it started.
                println!("Retrying (job {})", short_id);
                println!("Watch: plan-executor output -f {}", short_id);
                return Ok(());
            }
        }
    }
    Ok(())
}

async fn execute_plan(plan_path: String, config: crate::config::Config) -> Result<()> {
    // If the argument looks like a job ID prefix, resolve it to a plan path.
    let resolved_path = resolve_plan_path(&plan_path);

    // Canonicalize to absolute path.
    let plan = std::fs::canonicalize(&resolved_path)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default().join(&resolved_path));
    if !plan.exists() {
        anyhow::bail!("Plan file not found: {}", resolved_path);
    }

    // Fail fast if the plan is not in READY state.
    let status = crate::plan::parse_plan_status(&plan)?;
    if status != crate::plan::PlanStatus::Ready {
        anyhow::bail!("Plan status is {}, expected READY", status);
    }

    // Both local and remote execution go through the daemon.
    if !crate::ipc::socket_path().exists() {
        anyhow::bail!("Daemon not running. Start with: plan-executor daemon");
    }
    execute_via_daemon(plan, config).await
}

async fn trigger_remote(plan: PathBuf, config: crate::config::Config) -> Result<()> {
    let remote_repo = config.remote_repo.as_deref()
        .ok_or_else(|| anyhow::anyhow!(
            "remote execution requires 'remote_repo' in config — run 'plan-executor remote-setup'"
        ))?;

    let plan_filename = plan.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("plan.md")
        .to_string();

    let repo_root = find_repo_root(&plan)
        .ok_or_else(|| anyhow::anyhow!("could not find git repo root for {}", plan.display()))?;
    let (target_repo, target_ref, target_branch) = crate::remote::gather_git_context(&repo_root)?;
    let started_at = chrono::Utc::now().to_rfc3339();

    let meta = crate::remote::ExecutionMetadata {
        target_repo,
        target_ref,
        target_branch,
        plan_filename,
        started_at,
    };

    // Push Codex OAuth token (idempotent, skips if no auth file)
    let _ = crate::remote::push_codex_auth(remote_repo);

    let pr_url = crate::remote::trigger_remote_execution(remote_repo, &plan, &meta)?;
    let pr_num = crate::remote::pr_number_from_url(&pr_url);

    // Update plan status and store PR number
    let _ = crate::plan::set_plan_header(&plan, "status", "EXECUTING");
    if let Some(n) = pr_num {
        let _ = crate::plan::set_plan_header(&plan, "remote-pr", &n.to_string());
    }

    // Create a job entry and persist it
    if let Some(n) = pr_num {
        let job = crate::jobs::JobMetadata::new_remote(
            plan.clone(), remote_repo.to_string(), n,
        );
        let short_id = job.id[..job.id.len().min(8)].to_string();
        let _ = job.save();

        // Notify daemon to start monitoring (if running)
        if crate::ipc::socket_path().exists() {
            let _ = notify_daemon_track_remote(
                plan.to_string_lossy().to_string(),
                remote_repo.to_string(),
                n,
            );
        }

        println!("Remote execution triggered (job {}).", short_id);
    } else {
        println!("Remote execution triggered.");
    }
    println!("PR: {}", pr_url);

    Ok(())
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
        if let Ok(DaemonEvent::State { running_jobs, history, .. }) = serde_json::from_str(&line) {
            // Match job ID prefix (history or running)
            if let Some(job) = running_jobs.into_iter().chain(history)
                .find(|j| j.id.starts_with(arg))
            {
                return job.plan_path.to_string_lossy().into_owned();
            }
        }
    }
    arg.to_string()
}

async fn execute_foreground(plan_path: String, config: crate::config::Config) -> Result<()> {
    use crate::executor::{spawn_execution, ExecEvent};
    use crate::handoff;
    use crate::jobs::JobMetadata;

    let resolved_path = std::fs::canonicalize(&plan_path)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default().join(&plan_path));
    if !resolved_path.exists() {
        anyhow::bail!("Plan file not found: {}", plan_path);
    }

    // Remote plans trigger remotely unless PLAN_EXECUTOR_LOCAL=1 is set
    // (used by the GitHub Actions runner to force local execution).
    if crate::plan::parse_execution_mode(&resolved_path) == crate::plan::ExecutionMode::Remote
        && std::env::var("PLAN_EXECUTOR_LOCAL").as_deref() != Ok("1")
    {
        return trigger_remote(resolved_path, config).await;
    }

    // Fail fast if the plan is not in READY state.
    let status = crate::plan::parse_plan_status(&resolved_path)?;
    if status != crate::plan::PlanStatus::Ready {
        anyhow::bail!("Plan status is {}, expected READY", status);
    }

    let execution_root = find_repo_root(&resolved_path)
        .unwrap_or_else(|| resolved_path.parent().unwrap_or(&resolved_path).to_path_buf());

    let job = JobMetadata::new(resolved_path.clone());
    let job_id = job.id.clone();

    let (mut child, _pgid, mut exec_rx) = spawn_execution(
        job, execution_root.clone(), &config.agents.main,
    )?;

    let mut last_display_blank = false;
    let mut final_status = None;
    let mut completion_retried = false;

    'outer: loop {
        while let Some(event) = exec_rx.recv().await {
            match event {
                ExecEvent::OutputLine(_) => {}
                ExecEvent::DisplayLine(line) => {
                    let is_blank = crate::executor::is_visually_blank(&line);
                    if is_blank && last_display_blank {
                        continue;
                    }
                    last_display_blank = is_blank;
                    print_display_line(&line);
                }
                ExecEvent::HandoffRequired { session_id, state_file } => {
                    let state_data = handoff::load_state(&state_file)?;

                    println!("\x1b[33m\u{23fa} [plan-executor]\x1b[0m dispatching {} sub-agent(s) (phase: {})",
                        state_data.handoffs.len(), state_data.phase);

                    // Channel for bash agents to stream live output to terminal.
                    let (live_tx, mut live_rx) = tokio::sync::mpsc::channel::<(usize, String)>(256);
                    let live_task = tokio::spawn(async move {
                        while let Some((_idx, line)) = live_rx.recv().await {
                            println!("{}", line);
                        }
                    });

                    let (results, _pgids) = handoff::dispatch_all(
                        state_data.handoffs,
                        &config.agents.claude,
                        &config.agents.codex,
                        &config.agents.gemini,
                        &config.agents.bash,
                        Some(live_tx),
                        None, // no pgid tracking in foreground path
                        None, // no watchdog in foreground path
                    ).await;
                    let _ = live_task.await;

                    for r in &results {
                        if r.success {
                            println!("\x1b[33m\u{23fa} [plan-executor]\x1b[0m sub-agent {} done ({} chars)",
                                r.index, r.stdout.len());
                        } else if r.can_fail {
                            println!("\x1b[33m\u{23fa} [plan-executor]\x1b[0m sub-agent {} skipped (can-fail): {}",
                                r.index, r.stderr.lines().next().unwrap_or("(no stderr)"));
                        } else {
                            eprintln!("\x1b[31m\u{23fa} [plan-executor] sub-agent {} failed: {}\x1b[0m",
                                r.index, r.stderr.lines().next().unwrap_or("(no stderr)"));
                        }
                    }

                    if results.iter().any(|r| !r.success && !r.can_fail) {
                        crate::executor::consume_handoffs(&state_file);
                        final_status = Some(false);
                        break 'outer;
                    }

                    crate::executor::consume_handoffs(&state_file);

                    println!("\x1b[33m\u{23fa} [plan-executor]\x1b[0m resuming session {}",
                        &session_id[..session_id.len().min(16)]);

                    let continuation = handoff::build_continuation(&results);
                    match handoff::resume_execution(
                        &session_id,
                        &continuation,
                        execution_root.clone(),
                        Some(job_id.clone()),
                        Some(resolved_path.clone()),
                        &config.agents.main,
                    ).await {
                        Ok((new_child, _new_pgid, new_rx)) => {
                            child = new_child;
                            exec_rx = new_rx;
                            continue 'outer;
                        }
                        Err(e) => {
                            eprintln!("\x1b[31m\u{23fa} [plan-executor] failed to resume session: {}\x1b[0m", e);
                            final_status = Some(false);
                            break 'outer;
                        }
                    }
                }
                ExecEvent::Finished(finished_job) => {
                    let is_success = finished_job.status == crate::jobs::JobStatus::Success;

                    // If the agent returned success but the plan is still
                    // EXECUTING, the skill bailed out mid-execution. Resume
                    // the session once with an explicit instruction to finish.
                    let plan_still_executing = is_success
                        && crate::plan::parse_plan_status(&resolved_path)
                            .map(|s| matches!(s, crate::plan::PlanStatus::Executing))
                            .unwrap_or(false);

                    if plan_still_executing && !completion_retried {
                        completion_retried = true;
                        let session_id = finished_job.session_id.clone();

                        if let Some(sid) = session_id {
                            println!("\x1b[33m\u{23fa} [plan-executor]\x1b[0m plan still EXECUTING after agent returned success — resuming to complete remaining phases");

                            let continuation = "The plan execution is incomplete — the plan status is still EXECUTING. \
                                You returned from a handoff resume but did not complete the remaining execution phases. \
                                Continue from where you left off. Complete all remaining phases (plan validation, \
                                cleanup/PR, execution summary) until the plan status is set to COMPLETED.";

                            match handoff::resume_execution(
                                &sid,
                                continuation,
                                execution_root.clone(),
                                Some(job_id.clone()),
                                Some(resolved_path.clone()),
                                &config.agents.main,
                            ).await {
                                Ok((new_child, _new_pgid, new_rx)) => {
                                    child = new_child;
                                    exec_rx = new_rx;
                                    continue 'outer;
                                }
                                Err(e) => {
                                    eprintln!("\x1b[31m\u{23fa} [plan-executor] completion retry failed to resume: {}\x1b[0m", e);
                                }
                            }
                        }
                    }

                    if plan_still_executing {
                        eprintln!("\x1b[31m\u{23fa} [plan-executor] plan still EXECUTING after completion retry — marking FAILED\x1b[0m");
                        final_status = Some(false);
                    } else {
                        final_status = Some(is_success);
                    }
                    break 'outer;
                }
            }
        }
        break;
    }

    let _ = child;
    let success = final_status.unwrap_or(false);
    if !success {
        std::process::exit(1);
    }
    Ok(())
}

/// Walk up from a path to find the closest directory containing `.git`.
fn find_repo_root(path: &std::path::Path) -> Option<std::path::PathBuf> {
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

/// Sends Execute to the daemon, waits just long enough to identify the new
/// job ID, prints it, and returns immediately.  Use `plan-executor output -f
/// <job-id>` to watch the live output of local jobs.
async fn execute_via_daemon(plan: PathBuf, _config: crate::config::Config) -> Result<()> {
    use crate::ipc::{DaemonEvent, TuiRequest};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    let is_remote = crate::plan::parse_execution_mode(&plan) == crate::plan::ExecutionMode::Remote;

    let stream = UnixStream::connect(crate::ipc::socket_path()).await
        .context("Daemon not reachable")?;
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half).lines();

    // Snapshot current running job IDs before we trigger execution.
    let gs = serde_json::to_string(&TuiRequest::GetState)?;
    write_half.write_all(format!("{}\n", gs).as_bytes()).await?;

    let mut existing_ids = std::collections::HashSet::<String>::new();
    if let Ok(Some(line)) = reader.next_line().await {
        if let Ok(DaemonEvent::State { running_jobs, history, .. }) = serde_json::from_str(&line) {
            existing_ids = running_jobs.iter().chain(history.iter()).map(|j| j.id.clone()).collect();
        }
    }

    // Trigger execution.
    let plan_str = plan.to_string_lossy().to_string();
    let req = serde_json::to_string(&TuiRequest::Execute { plan_path: plan_str.clone() })?;
    write_half.write_all(format!("{}\n", req).as_bytes()).await?;

    let filename = plan.file_name().and_then(|n| n.to_str()).unwrap_or("?");

    // Remote plans need longer: creating branch + pushing files + opening PR via
    // the GitHub API can take 10-20 seconds.  Local plans resolve in <1 second.
    let timeout_secs = if is_remote { 30 } else { 2 };
    let timeout = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs));
    tokio::pin!(timeout);

    loop {
        tokio::select! {
            line = reader.next_line() => {
                let Ok(Some(line)) = line else { break };
                if let Ok(event) = serde_json::from_str::<DaemonEvent>(&line) {
                    match event {
                        DaemonEvent::State { running_jobs, history, .. } => {
                            // Check both running_jobs (local) and history (remote)
                            // for a newly created job.
                            let new_job = running_jobs.iter().chain(history.iter())
                                .find(|j| !existing_ids.contains(&j.id));
                            if let Some(j) = new_job {
                                if let (Some(repo), Some(pr)) = (&j.remote_repo, j.remote_pr) {
                                    println!("https://github.com/{}/pull/{}", repo, pr);
                                } else {
                                    let short_id = &j.id[..j.id.len().min(8)];
                                    println!("Queued {} (job {})", filename, short_id);
                                    println!("Watch: plan-executor output -f {}", short_id);
                                }
                                return Ok(());
                            }
                        }
                        DaemonEvent::Error { message } => {
                            eprintln!("Error: {}", message);
                            return Ok(());
                        }
                        _ => {}
                    }
                }
            }
            _ = &mut timeout => {
                if is_remote {
                    eprintln!("Timed out waiting for PR creation. Check: plan-executor jobs");
                } else {
                    println!("Queued {}", filename);
                    println!("Watch: plan-executor output -f <job-id>");
                }
                return Ok(());
            }
        }
    }

    println!("Queued {}", filename);
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

    let (jobs, job_processes): (Vec<JobMetadata>, std::collections::HashMap<String, crate::ipc::JobProcesses>) =
        if let Ok(DaemonEvent::State { running_jobs, history, running_processes, .. }) =
            serde_json::from_str::<DaemonEvent>(&line)
        {
            let jobs: Vec<JobMetadata> = running_jobs.into_iter().chain(history).collect();
            let map: std::collections::HashMap<_, _> = running_processes
                .into_iter()
                .map(|p| (p.job_id.clone(), p))
                .collect();
            (jobs, map)
        } else {
            eprintln!("Unexpected response from daemon.");
            std::process::exit(1);
        };

    if jobs.is_empty() {
        println!("No jobs found.");
        return;
    }

    let term_w = terminal_width();

    // Fixed-width columns; PLAN gets all remaining space
    let id_w = 10;
    let status_w = 8;
    let last_w = 8;
    let dur_w = 10;
    let gaps = 8; // 4 gaps × 2 spaces each
    let plan_w = term_w
        .saturating_sub(id_w + status_w + last_w + dur_w + gaps)
        .max(20);

    println!(
        "{:<id_w$}  {:<plan_w$}  {:<status_w$}  {:>last_w$}  {:>dur_w$}",
        "ID", "PLAN", "STATUS", "LAST", "DURATION",
    );
    println!("{}", "─".repeat(term_w));

    for job in &jobs {
        let id = &job.id[..job.id.len().min(8)];
        let plan = job.plan_path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
        let plan_display = truncate_str(plan, plan_w);
        let status = match job.status {
            JobStatus::Success       => "success",
            JobStatus::Failed        => "failed",
            JobStatus::Killed        => "killed",
            JobStatus::Running       => "running",
            JobStatus::RemoteRunning => "remote",
        };
        let duration = job.duration_ms
            .map(|ms| format_duration(ms / 1000))
            .unwrap_or_else(|| "-".to_string());
        let last = if matches!(job.status, JobStatus::Running) {
            job_processes
                .get(&job.id)
                .and_then(|p| p.idle_seconds)
                .map(format_duration)
                .unwrap_or_else(|| "-".to_string())
        } else {
            "-".to_string()
        };
        let line = format!(
            "{:<id_w$}  {:<plan_w$}  {:<status_w$}  {:>last_w$}  {:>dur_w$}",
            id, plan_display, status, last, duration,
        );
        let is_running = matches!(job.status, JobStatus::Running | JobStatus::RemoteRunning);
        if is_running {
            println!("\x1b[33m{}\x1b[0m", line);
        } else {
            println!("{}", line);
        }
        if matches!(job.status, JobStatus::Running) {
            if let Some(procs) = job_processes.get(&job.id) {
                render_job_process_tree(procs, term_w);
            }
        }
    }

    // Show remote executions if remote_repo is configured
    let config = crate::config::Config::load(None).ok();
    if let Some(remote_repo) = config.and_then(|c| c.remote_repo) {
        match crate::remote::list_remote_executions(&remote_repo) {
            Ok(remote_jobs) if !remote_jobs.is_empty() => {
                println!();
                println!("Remote ({}):", remote_repo);
                let pr_w = 6;
                // Grow status/target columns to fit the widest value. The
                // local table uses a fixed status_w=8, but remote statuses
                // can be wider ("succeeded" is 9 chars), and a hard-coded
                // 8 here silently pushes the target past term_w and wraps
                // the last character.
                let r_status_w = remote_jobs.iter()
                    .map(|rj| rj.status.len())
                    .max().unwrap_or(8).max(8);
                let r_target_w = remote_jobs.iter()
                    .map(|rj| rj.target.len())
                    .max().unwrap_or(6).max(6);
                let r_dur_w = 10;
                let r_gaps = 8; // 4 gaps × 2 spaces
                let r_plan_w = term_w
                    .saturating_sub(pr_w + r_status_w + r_target_w + r_dur_w + r_gaps)
                    .max(20);
                println!(
                    "{:<pr_w$}  {:<r_plan_w$}  {:<r_status_w$}  {:<r_target_w$}  {:>r_dur_w$}",
                    "PR", "PLAN", "STATUS", "TARGET", "DURATION",
                );
                println!("{}", "─".repeat(term_w));
                for rj in &remote_jobs {
                    let plan_display = truncate_str(&rj.plan_name, r_plan_w);
                    let target_display = truncate_str(&rj.target, r_target_w);
                    let duration = rj
                        .duration_seconds
                        .map(format_duration)
                        .unwrap_or_else(|| "-".to_string());
                    let line = format!(
                        "#{:<width$}  {:<r_plan_w$}  {:<r_status_w$}  {:<r_target_w$}  {:>r_dur_w$}",
                        rj.number, plan_display, rj.status, target_display, duration,
                        width = pr_w - 1,
                    );
                    if rj.status == "running" {
                        println!("\x1b[33m{}\x1b[0m", line);
                    } else {
                        println!("{}", line);
                    }
                }
            }
            Ok(_) => {} // no remote jobs, don't print header
            Err(e) => {
                eprintln!("(could not fetch remote jobs: {})", e);
            }
        }
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

    let pid: u32 = match pid.trim().parse() {
        Ok(n) => n,
        Err(_) => {
            eprintln!("Invalid PID in pid file: {:?}", pid);
            std::process::exit(1);
        }
    };

    // Safety: pid > 0 guaranteed by parse into u32; we only send SIGTERM.
    let ret = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    if ret == 0 {
        let _ = std::fs::remove_file(&pid_path);
        println!("Daemon stopped (pid={}).", pid);
    } else {
        eprintln!("Failed to stop daemon (pid={}). It may have already exited.", pid);
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

fn remote_setup() {
    use std::io::{self, BufRead, Write};

    let stdin = io::stdin();
    let mut stdout = io::stdout();

    // Check gh CLI
    if std::process::Command::new("gh").arg("--version").output().is_err() {
        eprintln!("Error: gh CLI not found. Install: https://cli.github.com");
        std::process::exit(1);
    }

    // Step 1: Execution repo
    let current_repo = crate::config::Config::load(None)
        .ok()
        .and_then(|c| c.remote_repo);
    let default_display = current_repo.unwrap_or_else(|| {
        // Use the current gh account as the default owner.
        let gh_user = std::process::Command::new("gh")
            .args(["api", "user", "--jq", ".login"])
            .output()
            .ok()
            .and_then(|o| if o.status.success() {
                String::from_utf8(o.stdout).ok().map(|s| s.trim().to_string())
            } else { None });
        match gh_user {
            Some(user) if !user.is_empty() => format!("{}/plan-executions", user),
            _ => "owner/plan-executions".to_string(),
        }
    });
    print!("Execution repo [{}]: ", default_display);
    let _ = stdout.flush();
    let mut repo_input = String::new();
    stdin.lock().read_line(&mut repo_input).unwrap();
    let repo_input = repo_input.trim();
    let remote_repo = if repo_input.is_empty() {
        default_display.to_string()
    } else {
        repo_input.to_string()
    };

    if !crate::remote::validate_repo_slug(&remote_repo) {
        eprintln!("Error: invalid repo slug '{}'. Expected format: owner/repo", remote_repo);
        std::process::exit(1);
    }

    // Ensure repo exists
    if crate::remote::repo_exists(&remote_repo) {
        println!("  Repo exists.");
    } else {
        println!("  Repo not found. Creating...");
        match crate::remote::create_repo(&remote_repo) {
            Ok(()) => println!("  Created {}", remote_repo),
            Err(e) => {
                eprintln!("  Error creating repo: {}", e);
                std::process::exit(1);
            }
        }
    }

    // Save to config
    match crate::config::Config::load(None) {
        Ok(mut config) => {
            config.remote_repo = Some(remote_repo.clone());
            let config_path = crate::config::Config::config_path();
            if let Ok(json) = serde_json::to_string_pretty(&config) {
                let _ = std::fs::write(&config_path, json);
                println!("  Saved to {}", config_path.display());
            }
        }
        Err(e) => {
            eprintln!("  Warning: could not update config: {}", e);
        }
    }

    // Step 2: GitHub PAT
    println!();
    println!("GitHub PAT for cloning org repos:");
    println!("  Create one at: https://github.com/settings/personal-access-tokens/new");
    println!("  Scope: your org, permission: Contents -> Read");
    print!("  Paste token (enter to skip): ");
    let _ = stdout.flush();
    let mut pat = String::new();
    stdin.lock().read_line(&mut pat).unwrap();
    let pat = pat.trim();
    if !pat.is_empty() {
        match gh_secret_set(&remote_repo, "TARGET_REPO_TOKEN", pat) {
            Ok(()) => println!("  Stored as TARGET_REPO_TOKEN"),
            Err(e) => eprintln!("  Error: {}", e),
        }
    } else {
        println!("  Skipped.");
    }

    // Step 3: Anthropic API key
    println!();
    print!("Anthropic API key (enter to skip): ");
    let _ = stdout.flush();
    let mut anthropic = String::new();
    stdin.lock().read_line(&mut anthropic).unwrap();
    let anthropic = anthropic.trim();
    if !anthropic.is_empty() {
        match gh_secret_set(&remote_repo, "ANTHROPIC_API_KEY", anthropic) {
            Ok(()) => println!("  Stored as ANTHROPIC_API_KEY"),
            Err(e) => eprintln!("  Error: {}", e),
        }
    } else {
        println!("  Skipped.");
    }

    // Step 4: Codex auth
    println!();
    print!("Codex auth — (o)auth / (a)pi key / (s)kip: ");
    let _ = stdout.flush();
    let mut codex_choice = String::new();
    stdin.lock().read_line(&mut codex_choice).unwrap();
    match codex_choice.trim() {
        "o" | "oauth" => {
            let auth_path = dirs::home_dir()
                .expect("home dir")
                .join(".codex")
                .join("auth.json");
            if auth_path.exists() {
                match std::fs::read_to_string(&auth_path) {
                    Ok(content) => {
                        println!("  Read {}", auth_path.display());
                        match gh_secret_set(&remote_repo, "CODEX_AUTH", &content) {
                            Ok(()) => println!("  Stored as CODEX_AUTH"),
                            Err(e) => eprintln!("  Error: {}", e),
                        }
                    }
                    Err(e) => eprintln!("  Error reading {}: {}", auth_path.display(), e),
                }
            } else {
                eprintln!("  {} not found. Run codex login first.", auth_path.display());
            }
        }
        "a" | "api" => {
            print!("  OpenAI API key (enter to skip): ");
            let _ = stdout.flush();
            let mut openai = String::new();
            stdin.lock().read_line(&mut openai).unwrap();
            let openai = openai.trim();
            if !openai.is_empty() {
                match gh_secret_set(&remote_repo, "OPENAI_API_KEY", openai) {
                    Ok(()) => println!("  Stored as OPENAI_API_KEY"),
                    Err(e) => eprintln!("  Error: {}", e),
                }
            }
        }
        _ => println!("  Skipped."),
    }

    // Step 5: Gemini API key
    println!();
    print!("Gemini API key (enter to skip): ");
    let _ = stdout.flush();
    let mut gemini = String::new();
    stdin.lock().read_line(&mut gemini).unwrap();
    let gemini = gemini.trim();
    if !gemini.is_empty() {
        match gh_secret_set(&remote_repo, "GEMINI_API_KEY", gemini) {
            Ok(()) => println!("  Stored as GEMINI_API_KEY"),
            Err(e) => eprintln!("  Error: {}", e),
        }
    } else {
        println!("  Skipped.");
    }

    // Step 6: Push workflow to execution repo
    println!("Pushing execute-plan workflow...");
    match crate::remote::push_workflow(&remote_repo) {
        Ok(()) => println!("  Pushed to .github/workflows/execute-plan.yml"),
        Err(e) => eprintln!("  Error pushing workflow: {}", e),
    }

    println!();
    println!("Setup complete. Remote execution ready.");
}

fn notify_daemon_track_remote(plan_path: String, remote_repo: String, pr_number: u64) -> Result<()> {
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    let mut s = UnixStream::connect(crate::ipc::socket_path())?;
    let req = serde_json::to_string(&crate::ipc::TuiRequest::TrackRemote {
        plan_path, remote_repo, pr_number,
    })?;
    s.write_all(format!("{}\n", req).as_bytes())?;
    Ok(())
}

fn gh_secret_set(repo: &str, name: &str, value: &str) -> Result<()> {
    crate::remote::gh_secret_set_stdin(name, repo, value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_duration_seconds_only() {
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(1), "1s");
        assert_eq!(format_duration(59), "59s");
    }

    #[test]
    fn format_duration_minutes_and_seconds() {
        assert_eq!(format_duration(60), "1m00s");
        assert_eq!(format_duration(125), "2m05s");
        assert_eq!(format_duration(3_599), "59m59s");
    }

    #[test]
    fn format_duration_hours_and_minutes() {
        assert_eq!(format_duration(3_600), "1h00m");
        assert_eq!(format_duration(5_579), "1h32m");
        assert_eq!(format_duration(86_399), "23h59m");
    }

    #[test]
    fn format_duration_days_and_hours() {
        assert_eq!(format_duration(86_400), "1d00h");
        assert_eq!(format_duration(100_000), "1d03h");
        assert_eq!(format_duration(604_800), "7d00h");
    }

    #[test]
    fn parse_subagent_done_index_accepts_done() {
        assert_eq!(
            parse_subagent_done_index("⏺ [plan-executor] sub-agent 3 done (1234 chars)"),
            Some(3)
        );
    }

    #[test]
    fn parse_subagent_done_index_accepts_failed() {
        assert_eq!(
            parse_subagent_done_index("⏺ [plan-executor] sub-agent 1 failed: boom"),
            Some(1)
        );
    }

    #[test]
    fn parse_subagent_done_index_accepts_skipped() {
        assert_eq!(
            parse_subagent_done_index("⏺ [plan-executor] sub-agent 2 skipped (can-fail): reason"),
            Some(2)
        );
    }

    #[test]
    fn parse_subagent_done_index_ignores_dispatching() {
        assert_eq!(
            parse_subagent_done_index("⏺ [plan-executor] dispatching 4 sub-agent(s) (phase: x)"),
            None
        );
    }

    #[test]
    fn parse_subagent_done_index_ignores_unrelated() {
        assert_eq!(parse_subagent_done_index("some other line"), None);
    }
}
