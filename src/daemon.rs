
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use tokio::net::UnixListener;
use tokio::sync::{broadcast, Mutex};
use tokio::time::Duration;
use anyhow::Result;

use crate::config::{watchdog_verdict, Config, WatchdogVerdict};
use crate::ipc::{socket_path, DaemonEvent, TuiRequest};
use crate::jobs::{JobMetadata, JobStatus};

/// Shared daemon state
pub struct DaemonState {
    #[allow(dead_code)]
    pub config: Config,
    pub agents: crate::config::AgentConfig,
    pub running_jobs: HashMap<String, JobMetadata>, // job_id -> metadata
    pub history: Vec<JobMetadata>,
    /// Per-job raw output buffers (last N lines)
    pub job_output: HashMap<String, VecDeque<String>>,
    /// Per-job formatted display output buffers (last N lines)
    pub job_display_output: HashMap<String, VecDeque<String>>,
    /// Child process handles for running jobs (job_id -> child)
    pub running_children: HashMap<String, tokio::process::Child>,
    /// Process group IDs for main agent processes (job_id → PGID)
    pub process_group_ids: HashMap<String, u32>,
    /// Process group IDs for active handoff sub-agent processes (job_id → [PGID, ...])
    pub sub_agent_pgids: HashMap<String, Vec<u32>>,
    /// Wall-clock timestamp of the most recent liveness event (output,
    /// display, handoff, sub-agent stream) seen for a running job. Used by
    /// the watchdog to detect hung jobs. Missing entry = no stall baseline,
    /// so only the hard-cap check applies for this tick.
    pub job_last_activity: HashMap<String, Instant>,
    /// Monotonic-clock start timestamp per running job, used by the
    /// watchdog to compute `since_start` without being fooled by wall-clock
    /// jumps (NTP step, VM suspend/resume). Populated at job start and
    /// removed on finalization.
    pub job_started_at_monotonic: HashMap<String, Instant>,
    /// Incrementing counter per job used to namespace sub-agent output
    /// files (one call to `dispatch_all` = one "dispatch"). Files land
    /// at `<job_dir>/sub-agents/dispatch-<N>-agent-<index>-<type>.*`.
    pub sub_agent_dispatch_counter: HashMap<String, u32>,
    /// Jobs currently paused at a handoff — sub-agents held until ResumeJob
    pub paused_jobs: std::collections::HashSet<String>,
    /// Remote executions being monitored: plan_path → (remote_repo, pr_number)
    pub remote_executions: HashMap<String, (String, u64)>,
    /// broadcast channel for DaemonEvent to all TUI clients
    pub event_tx: broadcast::Sender<DaemonEvent>,
}

/// Icon PNG embedded at compile time.
const ICON_PNG: &[u8] = include_bytes!("../assets/icon.png");

/// Returns the icon path, writing it to disk from the embedded bytes if missing.
fn ensure_icon() -> PathBuf {
    let path = Config::base_dir().join("icon.png");
    if !path.exists() {
        let _ = std::fs::create_dir_all(path.parent().unwrap());
        let _ = std::fs::write(&path, ICON_PNG);
    }
    path
}

/// Sends a desktop notification.
/// macOS: alerter (custom icon) with osascript fallback. Linux: notify-send.
fn notify(title: &str, body: &str) {
    tracing::info!("notification: {} — {}", title, body);
    let title = title.to_string();
    let body = body.to_string();
    let icon = ensure_icon();
    std::thread::spawn(move || {
        let result = send_notification(&title, &body, &icon);
        if let Err(e) = result {
            tracing::warn!("notification failed: {}", e);
        }
    });
}

fn send_notification(title: &str, body: &str, icon: &Path) -> std::result::Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("alerter")
            .args([
                "--title", title,
                "--message", body,
                "--app-icon", &icon.to_string_lossy(),
                "--timeout", "10",
            ])
            .output()
            .map_err(|e| format!("alerter not found: {}", e))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("alerter exit {}: {}", output.status, stderr.trim()));
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let icon_str = icon.to_string_lossy().to_string();
        let output = std::process::Command::new("notify-send")
            .args(["-i", &icon_str, title, body])
            .output()
            .map_err(|e| format!("notify-send not found: {}", e))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("notify-send exit {}: {}", output.status, stderr.trim()));
        }
    }
    Ok(())
}

/// Sends a notification for a plan filename with the given title.
fn notify_plan(title: &str, plan: &Path, detail: &str) {
    let name = plan.file_name().and_then(|n| n.to_str()).unwrap_or("plan");
    let body = if detail.is_empty() {
        name.to_string()
    } else {
        format!("{}\n{}", name, detail)
    };
    notify(title, &body);
}

/// Write a display line to the job's display.log so `plan-executor output` sees it.
fn append_display(job_id: &str, line: &str) {
    use std::io::Write;
    let path = crate::config::Config::base_dir()
        .join("jobs").join(job_id).join("display.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(format!("{}\n", line).as_bytes());
    }
}

impl DaemonState {
    pub fn snapshot_state(&self) -> DaemonEvent {
        let now = Instant::now();
        let running_processes = self
            .running_jobs
            .keys()
            .map(|job_id| crate::ipc::JobProcesses {
                job_id: job_id.clone(),
                main_pgid: self.process_group_ids.get(job_id).copied(),
                sub_agent_pgids: self
                    .sub_agent_pgids
                    .get(job_id)
                    .cloned()
                    .unwrap_or_default(),
                idle_seconds: self.job_last_activity.get(job_id).map(|t| {
                    now.saturating_duration_since(*t).as_secs()
                }),
            })
            .collect();
        DaemonEvent::State {
            running_jobs: self.running_jobs.values().cloned().collect(),
            history: self.history.clone(),
            paused_job_ids: self.paused_jobs.iter().cloned().collect(),
            running_processes,
        }
    }
}

/// Main daemon entry point
pub async fn run_daemon(config: crate::config::Config) -> Result<()> {
    tracing::info!("daemon starting (pid={})", std::process::id());

    // Write PID file on every start so restarts always reflect the current PID.
    let pid_path = Config::base_dir().join("daemon.pid");
    std::fs::create_dir_all(Config::base_dir())?;
    std::fs::write(&pid_path, format!("{}\n", std::process::id()))?;
    tracing::info!("pid file written");

    // Ensure socket cleanup on start
    let sock_path = socket_path();
    if sock_path.exists() {
        std::fs::remove_file(&sock_path)?;
    }
    std::fs::create_dir_all(sock_path.parent().unwrap())?;
    tracing::info!("socket path ready: {}", sock_path.display());

    let (event_tx, _) = broadcast::channel::<DaemonEvent>(256);

    tracing::info!("loading job history");
    // Prune old completed jobs, keeping the 50 most recent.
    JobMetadata::prune(50);
    // Any job that was Running when the daemon last died was interrupted.
    // Mark it Failed now so it shows up in history with the correct status.
    let history_on_start: Vec<JobMetadata> = JobMetadata::load_all()
        .into_iter()
        .map(|mut j| {
            if j.status == JobStatus::Running {
                tracing::warn!("job {} was Running at startup — marking Failed (daemon was killed)", j.id);
                j.status = JobStatus::Failed;
                j.finished_at = Some(chrono::Utc::now());
                let _ = j.save();
            }
            j
        })
        .collect();
    let state = Arc::new(Mutex::new(DaemonState {
        config: config.clone(),
        agents: config.agents.clone(),
        running_jobs: HashMap::new(),
        history: history_on_start,
        job_output: HashMap::new(),
        job_display_output: HashMap::new(),
        running_children: HashMap::new(),
        process_group_ids: HashMap::new(),
        sub_agent_pgids: HashMap::new(),
        job_last_activity: HashMap::new(),
        job_started_at_monotonic: HashMap::new(),
        sub_agent_dispatch_counter: HashMap::new(),
        paused_jobs: std::collections::HashSet::new(),
        remote_executions: HashMap::new(),
        event_tx: event_tx.clone(),
    }));
    tracing::info!("job history loaded");

    // Restore remote executions from persisted job metadata
    {
        let st = state.lock().await;
        let remotes: Vec<(String, String, u64)> = st.history.iter()
            .filter(|j| j.status == JobStatus::RemoteRunning)
            .filter_map(|j| {
                let repo = j.remote_repo.as_ref()?;
                let pr = j.remote_pr?;
                Some((j.plan_path.to_string_lossy().to_string(), repo.clone(), pr))
            })
            .collect();
        drop(st);
        if !remotes.is_empty() {
            let mut st = state.lock().await;
            for (path, repo, pr) in remotes {
                tracing::info!(plan = %path, pr = pr, "restoring remote execution monitor");
                st.remote_executions.insert(path, (repo, pr));
            }
        }
    }

    // Unix socket listener
    let listener = UnixListener::bind(&sock_path)?;

    // Remote execution monitor (check every 30 seconds)
    let mut remote_interval = tokio::time::interval(Duration::from_secs(30));
    remote_interval.tick().await; // consume immediate tick

    // Watchdog (check every 60 seconds). Cause-agnostic liveness guard:
    // kills any local running job that has emitted no events for
    // stall_timeout_seconds, or whose total runtime exceeds
    // hard_cap_seconds. Remote jobs have no tracked process group and are
    // intentionally skipped.
    let mut watchdog_interval = tokio::time::interval(Duration::from_secs(60));
    watchdog_interval.tick().await;

    loop {
        tokio::select! {
            // New client connection
            Ok((stream, _)) = listener.accept() => {
                let state_clone = Arc::clone(&state);
                let rx = event_tx.subscribe();
                tokio::spawn(handle_tui_client(stream, state_clone, rx));
            }

            // Monitor remote executions
            _ = remote_interval.tick() => {
                poll_remote_executions(&state).await;
            }

            // Watchdog for hung local jobs
            _ = watchdog_interval.tick() => {
                watchdog_check(&state).await;
            }
        }
    }
}

/// Consumes streamed sub-agent lines from `handoff::dispatch_all` and
/// does two things per line:
///  1. Stamps `job_last_activity` — real per-line liveness for the
///     watchdog; a hung sub-agent emits no lines → no ticks.
///  2. Appends the line to a per-sub-agent output file under the job
///     directory. Stdout lands in `<job>/sub-agents/dispatch-<N>-
///     agent-<idx>-<type>.jsonl`; stderr gets a sibling `.stderr` file.
///
/// The channel closes when the last sender is dropped (after
/// `dispatch_all` returns and both streaming readers finish), at which
/// point the task exits. Replaces the earlier blind 30-second heartbeat.
fn spawn_subagent_writer(
    state: Arc<Mutex<DaemonState>>,
    job_id: String,
    dispatch_num: u32,
    event_tx: broadcast::Sender<DaemonEvent>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<crate::handoff::SubAgentLine>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let base_dir = Config::base_dir()
            .join("jobs")
            .join(&job_id)
            .join("sub-agents");
        if let Err(e) = tokio::fs::create_dir_all(&base_dir).await {
            tracing::warn!(job = %job_id, error = %e, "sub-agent dir create failed");
        }

        let mut handles: HashMap<(usize, bool), tokio::fs::File> = HashMap::new();
        while let Some(msg) = rx.recv().await {
            {
                let mut st = state.lock().await;
                if !st.running_jobs.contains_key(&job_id) {
                    break;
                }
                st.job_last_activity.insert(job_id.clone(), Instant::now());
            }

            // Broadcast live so `plan-executor output -f` can render each
            // sub-agent line as it arrives instead of waiting for the
            // "sub-agent N done" marker and re-reading from disk.
            let _ = event_tx.send(DaemonEvent::SubAgentLine {
                job_id: job_id.clone(),
                index: msg.index,
                agent_type: msg.agent_type.to_string(),
                is_stderr: msg.is_stderr,
                line: msg.line.clone(),
            });

            let key = (msg.index, msg.is_stderr);
            let file = match handles.get_mut(&key) {
                Some(f) => f,
                None => {
                    let ext = if msg.is_stderr { "stderr" } else { "jsonl" };
                    let path = base_dir.join(format!(
                        "dispatch-{}-agent-{}-{}.{}",
                        dispatch_num, msg.index, msg.agent_type, ext
                    ));
                    match tokio::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&path)
                        .await
                    {
                        Ok(f) => {
                            handles.insert(key, f);
                            handles.get_mut(&key).expect("just inserted")
                        }
                        Err(e) => {
                            tracing::warn!(
                                job = %job_id, path = %path.display(),
                                error = %e, "sub-agent file open failed",
                            );
                            continue;
                        }
                    }
                }
            };
            let _ = file.write_all(msg.line.as_bytes()).await;
            let _ = file.write_all(b"\n").await;
        }
    })
}

/// Reads sub-agent pgids from an unbounded channel and appends each one to
/// `DaemonState.sub_agent_pgids[job_id]` the instant it arrives. Spawned
/// alongside every `dispatch_all` call so a KillJob arriving mid-dispatch
/// can SIGKILL the in-flight sub-agents' process groups. The task exits
/// when the sender is dropped (after `dispatch_all` returns).
fn spawn_pgid_registrar(
    state: Arc<Mutex<DaemonState>>,
    job_id: String,
    mut pgid_rx: tokio::sync::mpsc::UnboundedReceiver<u32>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(pgid) = pgid_rx.recv().await {
            if pgid == 0 {
                continue;
            }
            let mut st = state.lock().await;
            let entry = st.sub_agent_pgids.entry(job_id.clone()).or_default();
            if !entry.contains(&pgid) {
                entry.push(pgid);
            }
        }
    })
}

use crate::proctree::collect_descendant_pgids;

/// Sends SIGKILL to a job's main process group, all active sub-agent process
/// groups, and its tracked child handle. Does NOT finalize the job — the
/// caller owns the status transition. Safe to call while holding the state
/// mutex: the only `.await` is `child.kill().await`, which is fine under a
/// tokio Mutex.
///
/// For each tracked pgroup, this also walks the live process tree and
/// signals every descendant pgroup. Necessary because claude's Bash tool
/// puts each spawned command in its own process group via `setpgid`, so
/// hanging shell scripts survive a plain pgroup-kill of the sub-agent.
async fn send_kill_signals(st: &mut DaemonState, job_id: &str) {
    let mut all_pgids: std::collections::HashSet<u32> = std::collections::HashSet::new();

    if let Some(pgid) = st.process_group_ids.remove(job_id) {
        if pgid > 0 {
            all_pgids.insert(pgid);
            for p in collect_descendant_pgids(pgid) {
                all_pgids.insert(p);
            }
        }
    }
    if let Some(pgids) = st.sub_agent_pgids.remove(job_id) {
        for pgid in pgids {
            if pgid > 0 {
                all_pgids.insert(pgid);
                for p in collect_descendant_pgids(pgid) {
                    all_pgids.insert(p);
                }
            }
        }
    }

    for pgid in all_pgids {
        #[cfg(unix)]
        unsafe { libc::kill(-(pgid as libc::pid_t), libc::SIGKILL); }
    }

    if let Some(mut child) = st.running_children.remove(job_id) {
        let _ = child.kill().await;
    }
}

/// Computes the per-job timing inputs the watchdog needs.
///
/// Missing `last_activity` is not fatal — we fall back to `Instant::now()`
/// so the stall check is effectively Ok for this tick, but the hard-cap
/// still applies. This preserves defense-in-depth if a future code path
/// forgets to stamp activity on some transition.
fn compute_watchdog_timings(
    now: Instant,
    last_activity: Option<Instant>,
    started_monotonic: Instant,
) -> (Duration, Duration) {
    let since_start = now.saturating_duration_since(started_monotonic);
    let since_activity = now.saturating_duration_since(last_activity.unwrap_or(now));
    (since_start, since_activity)
}

/// Watchdog tick: kills any running local job whose last-activity age
/// exceeds `stall_timeout_seconds` or whose total runtime exceeds
/// `hard_cap_seconds`. Uses the pure `watchdog_verdict` helper to decide.
///
/// The function takes two locks: one to snapshot candidate timings, and a
/// second per-victim to perform the kill atomically. Between them, the
/// event loop may update `job_last_activity`, so the verdict is
/// re-evaluated under the second lock before SIGKILL is sent. This avoids
/// false-killing a job that came back to life right at the boundary.
async fn watchdog_check(state: &Arc<Mutex<DaemonState>>) {
    let (stall_timeout, hard_cap, candidates): (
        Duration,
        Duration,
        Vec<(String, Duration, Duration)>,
    ) = {
        let st = state.lock().await;
        let stall = Duration::from_secs(st.config.stall_timeout_seconds);
        let cap = Duration::from_secs(st.config.hard_cap_seconds);
        let now = Instant::now();
        let list: Vec<(String, Duration, Duration)> = st
            .running_jobs
            .iter()
            .filter(|(id, j)| {
                j.status == JobStatus::Running && !st.paused_jobs.contains(*id)
            })
            .filter_map(|(id, _j)| {
                // A job without a monotonic start entry predates this
                // bookkeeping or lost its entry to a bug — skip safely
                // rather than guessing.
                let started = st.job_started_at_monotonic.get(id).copied()?;
                let last = st.job_last_activity.get(id).copied();
                let (since_start, since_activity) =
                    compute_watchdog_timings(now, last, started);
                Some((id.clone(), since_start, since_activity))
            })
            .collect();
        (stall, cap, list)
    };

    for (job_id, since_start, since_activity) in candidates {
        let verdict = watchdog_verdict(since_start, since_activity, stall_timeout, hard_cap);
        if matches!(verdict, WatchdogVerdict::Ok) {
            continue;
        }

        // Re-acquire the lock and re-check the verdict against fresh state
        // before sending SIGKILL. A job that emitted output between the
        // snapshot and now should not be killed.
        let mut st = state.lock().await;

        if !st.running_jobs.contains_key(&job_id) {
            // Already finalized.
            continue;
        }
        if st.paused_jobs.contains(&job_id) {
            // User paused the job between snapshot and kill.
            continue;
        }

        let fresh_now = Instant::now();
        let Some(fresh_started) =
            st.job_started_at_monotonic.get(&job_id).copied()
        else {
            // Entry cleared between snapshot and kill — nothing to do.
            continue;
        };
        let fresh_last = st.job_last_activity.get(&job_id).copied();
        let (fresh_since_start, fresh_since_activity) =
            compute_watchdog_timings(fresh_now, fresh_last, fresh_started);
        let fresh_verdict = watchdog_verdict(
            fresh_since_start,
            fresh_since_activity,
            stall_timeout,
            hard_cap,
        );

        let reason = match fresh_verdict {
            WatchdogVerdict::Ok => continue,
            WatchdogVerdict::Stalled { silent_seconds } => format!(
                "no output for {}s (stall_timeout={}s)",
                silent_seconds,
                stall_timeout.as_secs()
            ),
            WatchdogVerdict::HardCapped { total_seconds } => format!(
                "runtime {}s exceeds hard_cap={}s",
                total_seconds,
                hard_cap.as_secs()
            ),
        };

        tracing::warn!(job = %job_id, %reason, "watchdog: killing hung job");

        let banner = format!(
            "⏺ [plan-executor] watchdog killed job: {}",
            reason
        );
        append_display(&job_id, &banner);

        send_kill_signals(&mut st, &job_id).await;
        if let Some(mut job) = st.running_jobs.remove(&job_id) {
            job.status = JobStatus::Failed;
            job.finished_at = Some(chrono::Utc::now());
            job.duration_ms = Some(
                (chrono::Utc::now() - job.started_at)
                    .num_milliseconds()
                    .max(0) as u64,
            );
            let _ = job.save();
            let _ = st
                .event_tx
                .send(DaemonEvent::JobDisplayLine {
                    job_id: job_id.clone(),
                    line: banner,
                });
            st.history.insert(0, job.clone());
            st.job_output.remove(&job_id);
            st.job_display_output.remove(&job_id);
            st.job_last_activity.remove(&job_id);
            st.job_started_at_monotonic.remove(&job_id);
            st.sub_agent_dispatch_counter.remove(&job_id);
            notify_plan("Execution killed by watchdog", &job.plan_path, &reason);
            let _ = st.event_tx.send(DaemonEvent::JobUpdated { job });
        }
    }
}

/// Polls all tracked remote executions to check if their PRs have closed.
async fn poll_remote_executions(state: &Arc<Mutex<DaemonState>>) {
    let executions: Vec<(String, String, u64)> = {
        let st = state.lock().await;
        st.remote_executions.iter()
            .map(|(plan, (repo, pr))| (plan.clone(), repo.clone(), *pr))
            .collect()
    };

    if executions.is_empty() { return; }

    for (plan_path, remote_repo, pr_number) in executions {
        let result = tokio::task::spawn_blocking({
            let repo = remote_repo.clone();
            move || crate::remote::get_pr_status(&repo, pr_number)
        }).await;

        let Ok(Ok((pr_state, labels))) = result else { continue };

        match pr_state.as_str() {
            "OPEN" => {} // still running
            "CLOSED" | "MERGED" => {
                let plan = PathBuf::from(&plan_path);
                let success = labels.iter().any(|l| l == "succeeded");
                let new_status = if success { "COMPLETED" } else { "FAILED" };

                tracing::info!(plan = %plan_path, pr = pr_number, status = new_status, "remote execution finished");
                let status_label = if success { "succeeded" } else { "failed" };
                notify_plan(&format!("Remote execution {}", status_label), &plan, "");
                let _ = crate::plan::set_plan_header(&plan, "status", new_status);

                // Update the persisted job metadata
                let all_jobs = JobMetadata::load_all();
                if let Some(mut job) = all_jobs.into_iter().find(|j| {
                    j.remote_pr == Some(pr_number) && j.remote_repo.as_deref() == Some(&remote_repo)
                }) {
                    job.status = if success { JobStatus::Success } else { JobStatus::Failed };
                    job.finished_at = Some(chrono::Utc::now());
                    job.duration_ms = Some(
                        (chrono::Utc::now() - job.started_at).num_milliseconds().max(0) as u64,
                    );
                    let _ = job.save();
                }

                let mut st = state.lock().await;
                st.remote_executions.remove(&plan_path);
            }
            _ => {}
        }
    }
}

pub async fn trigger_execution(state: &Arc<Mutex<DaemonState>>, manifest_path: &str) {
    let manifest = match crate::cli::resolve_manifest_path(manifest_path) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("trigger_execution: {e}");
            let st = state.lock().await;
            let _ = st.event_tx.send(DaemonEvent::Error { message: format!("{e}") });
            return;
        }
    };
    let (plan, status, execution_mode) = match crate::cli::read_manifest_plan_block(&manifest) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!("trigger_execution: {e}");
            let st = state.lock().await;
            let _ = st.event_tx.send(DaemonEvent::Error { message: format!("{e}") });
            return;
        }
    };
    if status != "READY" {
        let msg = format!("trigger_execution: manifest plan.status is {}, expected READY", status);
        tracing::error!("{}", msg);
        let st = state.lock().await;
        let _ = st.event_tx.send(DaemonEvent::Error { message: msg });
        return;
    }
    let plan_path = plan.to_string_lossy().to_string();

    // Route remote plans to GitHub PR trigger instead of local execution.
    if execution_mode == "remote" {
        let config = { state.lock().await.config.clone() };
        if let Some(remote_repo) = config.remote_repo.as_deref() {
            let plan_filename = plan.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("plan.md")
                .to_string();
            let plan_dir = plan.parent().unwrap_or(&plan).to_path_buf();
            match crate::remote::gather_git_context(&plan_dir) {
                Ok((target_repo, target_ref, target_branch)) => {
                    let meta = crate::remote::ExecutionMetadata {
                        target_repo,
                        target_ref,
                        target_branch,
                        plan_filename,
                        started_at: chrono::Utc::now().to_rfc3339(),
                    };
                    let _ = crate::remote::push_codex_auth(remote_repo);
                    match crate::remote::trigger_remote_execution(remote_repo, &plan, &manifest, &meta) {
                        Ok(url) => {
                            tracing::info!(url = %url, "remote execution triggered");
                            notify_plan("Remote execution started", &plan, "");
                            if let Some(pr_num) = crate::remote::pr_number_from_url(&url) {
                                let _ = crate::plan::set_plan_header(&plan, "remote-pr", &pr_num.to_string());
                                // Create and persist a job entry so `jobs` shows it.
                                let job = JobMetadata::new_remote(
                                    plan.clone(), remote_repo.to_string(), pr_num,
                                );
                                let _ = job.save();
                                let mut st = state.lock().await;
                                st.history.insert(0, job);
                                st.remote_executions.insert(
                                    plan_path.to_string(),
                                    (remote_repo.to_string(), pr_num),
                                );
                            }
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "remote execution failed");
                            let st = state.lock().await;
                            let _ = st.event_tx.send(DaemonEvent::Error {
                                message: format!("remote execution failed: {}", e),
                            });
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(plan = %plan_path, error = %e, "remote execution: could not determine git context");
                    let st = state.lock().await;
                    let _ = st.event_tx.send(DaemonEvent::Error {
                        message: format!("remote execution: could not determine git context: {}", e),
                    });
                }
            }
            let st = state.lock().await;
            let event = st.snapshot_state();
            let _ = st.event_tx.send(event);
            return;
        } else {
            tracing::error!("remote execution: remote_repo not configured");
            let st = state.lock().await;
            let _ = st.event_tx.send(DaemonEvent::Error {
                message: "remote execution requires remote_repo — run 'plan-executor remote-setup'".to_string(),
            });
        }
        return;
    }

    let execution_root = find_repo_root(&plan)
        .unwrap_or_else(|| plan.parent().unwrap_or(&plan).to_path_buf());

    tracing::info!(plan = %plan_path, "dispatching plan job");

    spawn_rust_scheduler_job(state, plan.clone(), manifest.clone(), execution_root).await;
}

/// Phase D4 default: dispatch the plan through the new Rust scheduler.
///
/// Builds a [`crate::job::types::Job`] for [`crate::job::types::JobKind::Plan`],
/// persists it via [`crate::job::storage::JobStore`], registers a matching
/// `JobMetadata` row in the daemon's running-jobs table (so existing TUI /
/// notification / watchdog plumbing keeps working unchanged), and spawns a
/// background task that walks every step from
/// [`crate::job::registry::steps_for`] in order. A non-success
/// [`crate::job::types::AttemptOutcome`] aborts the pipeline and finalizes
/// the job as `Failed`. Sub-agent dispatch inside each step still flows
/// through [`crate::handoff::dispatch_all`] — only the orchestrator
/// subprocess is removed.
async fn spawn_rust_scheduler_job(
    state: &Arc<Mutex<DaemonState>>,
    plan: PathBuf,
    manifest_path: PathBuf,
    execution_root: PathBuf,
) {
    use crate::job::registry;
    use crate::job::storage::JobStore;
    use crate::job::types::{Job, JobId, JobKind, JobState, StepInstance, StepStatus};

    let kind = JobKind::Plan {
        manifest_path: manifest_path.clone(),
    };
    let runtime_steps = registry::steps_for(&kind);
    let step_instances: Vec<StepInstance> = runtime_steps
        .iter()
        .enumerate()
        .map(|(idx, step)| {
            let seq = u32::try_from(idx + 1).unwrap_or(u32::MAX);
            StepInstance {
                seq,
                name: step.name().to_string(),
                status: StepStatus::Pending,
                attempts: Vec::new(),
                idempotent: step.idempotent(),
            }
        })
        .collect();

    let job_struct = Job {
        id: JobId(uuid::Uuid::new_v4().to_string()),
        kind,
        state: JobState::Running,
        created_at: chrono::Utc::now().to_rfc3339(),
        steps: step_instances,
    };
    let job_id = job_struct.id.0.clone();

    let store = match JobStore::new() {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "rust scheduler: opening job store failed");
            let st = state.lock().await;
            let _ = st
                .event_tx
                .send(DaemonEvent::Error { message: format!("job store: {e}") });
            return;
        }
    };
    let job_dir = match store.create(&job_struct) {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(error = %e, "rust scheduler: persisting job.json failed");
            let st = state.lock().await;
            let _ = st
                .event_tx
                .send(DaemonEvent::Error { message: format!("persist job.json: {e}") });
            return;
        }
    };
    let job_dir_path = job_dir.path().to_path_buf();

    // Mirror the legacy pathway's bookkeeping: a `JobMetadata` row in
    // `running_jobs` keeps the watchdog, notification, and `output` CLI
    // working without modification. We override the auto-generated id so
    // the persisted Job (job.json) and the in-memory metadata share the
    // same id — this is what `plan-executor output -f <prefix>` resolves.
    let mut metadata = JobMetadata::new(plan.clone());
    metadata.id = job_id.clone();

    notify_plan("Execution started", &plan, "");

    {
        let mut st = state.lock().await;
        st.running_jobs.insert(job_id.clone(), metadata.clone());
        st.job_output.insert(job_id.clone(), VecDeque::new());
        st.job_display_output
            .insert(job_id.clone(), VecDeque::new());
        let now = Instant::now();
        st.job_last_activity.insert(job_id.clone(), now);
        st.job_started_at_monotonic.insert(job_id.clone(), now);
        let event = st.snapshot_state();
        let _ = st.event_tx.send(event);
    }

    let state_clone = Arc::clone(state);
    tokio::spawn(async move {
        let success = run_rust_scheduler_steps(
            runtime_steps,
            job_dir_path,
            execution_root,
            &state_clone,
            &job_id,
        )
        .await;

        let mut st = state_clone.lock().await;
        if let Some(mut job) = st.running_jobs.remove(&job_id) {
            job.status = if success {
                JobStatus::Success
            } else {
                JobStatus::Failed
            };
            job.finished_at = Some(chrono::Utc::now());
            job.duration_ms = Some(
                (chrono::Utc::now() - job.started_at)
                    .num_milliseconds()
                    .max(0) as u64,
            );
            let _ = job.save();
            st.history.insert(0, job.clone());
            st.job_output.remove(&job_id);
            st.job_display_output.remove(&job_id);
            st.job_last_activity.remove(&job_id);
            st.job_started_at_monotonic.remove(&job_id);
            st.sub_agent_dispatch_counter.remove(&job_id);
            let status_str = if success { "succeeded" } else { "failed" };
            notify_plan(&format!("Execution {}", status_str), &job.plan_path, "");
            let _ = st.event_tx.send(DaemonEvent::JobUpdated { job });
        }
    });
}

/// Walks every step in `steps` for the given Rust-scheduler job, stamping
/// per-line liveness on `job_last_activity` (so the watchdog still sees a
/// live job) and broadcasting display-line events for each step transition.
/// Returns `true` only when every step reached
/// [`crate::job::types::AttemptOutcome::Success`] (or `Pending` for the
/// placeholder shells); any other outcome aborts the pipeline and yields
/// `false`.
async fn run_rust_scheduler_steps(
    steps: Vec<Box<dyn crate::job::step::Step>>,
    job_dir: PathBuf,
    workdir: PathBuf,
    state: &Arc<Mutex<DaemonState>>,
    job_id: &str,
) -> bool {
    use crate::job::metrics::JobMetrics;
    use crate::job::step::StepContext;
    use crate::job::storage::JobStore;
    use crate::job::types::{AttemptOutcome, JobId};

    let job_id_owned = JobId(job_id.to_string());
    let mut metrics = JobMetrics::new(job_id_owned.clone());
    let mut pipeline_ok = true;

    for (idx, step) in steps.iter().enumerate() {
        let seq = u32::try_from(idx + 1).unwrap_or(u32::MAX);

        // Bail out cleanly if a KillJob / watchdog kill removed the job
        // from `running_jobs` while a previous step was running.
        {
            let st = state.lock().await;
            if !st.running_jobs.contains_key(job_id) {
                tracing::info!(job = %job_id, "rust scheduler: job no longer running, aborting pipeline");
                pipeline_ok = false;
                break;
            }
        }

        let mut ctx = StepContext {
            job_dir: job_dir.clone(),
            step_seq: seq,
            attempt_n: 1,
            workdir: workdir.clone(),
        };

        let line = format!("⏺ [plan-executor] step {} ({}) starting", seq, step.name());
        append_display(job_id, &line);
        {
            let mut st = state.lock().await;
            st.job_last_activity.insert(job_id.to_string(), Instant::now());
            st.job_display_output
                .entry(job_id.to_string())
                .or_default()
                .push_back(line.clone());
            let _ = st.event_tx.send(DaemonEvent::JobDisplayLine {
                job_id: job_id.to_string(),
                line,
            });
        }

        // Step boundary — count once per step before running attempts.
        metrics.record_step();

        tracing::info!(step = step.name(), seq, "running step");
        let outcome = step.run(&mut ctx).await;

        let policy = step.recovery_policy();
        metrics.record_attempt(&outcome, Some(&policy));

        let (ok, summary) = match &outcome {
            AttemptOutcome::Success => (true, "success".to_string()),
            // Placeholder shells (`PreflightStep`, `PrFinalizeStep`) emit
            // `Pending` today; treat as a pass so the pipeline reaches
            // the real step bodies that follow.
            AttemptOutcome::Pending => (true, "pending (placeholder)".to_string()),
            AttemptOutcome::TransientInfra { error } => (false, format!("transient_infra: {error}")),
            AttemptOutcome::HardInfra { error } => (false, format!("hard_infra: {error}")),
            AttemptOutcome::ProtocolViolation { category, detail } => {
                (false, format!("protocol_violation [{category}]: {detail}"))
            }
            AttemptOutcome::SemanticMistake { fix_loop_round } => {
                (false, format!("semantic_mistake (round {fix_loop_round})"))
            }
            AttemptOutcome::SpecDrift { gap } => (false, format!("spec_drift: {gap}")),
        };

        let line = format!(
            "⏺ [plan-executor] step {} ({}) {}",
            seq,
            step.name(),
            summary
        );
        append_display(job_id, &line);
        {
            let mut st = state.lock().await;
            st.job_last_activity.insert(job_id.to_string(), Instant::now());
            st.job_display_output
                .entry(job_id.to_string())
                .or_default()
                .push_back(line.clone());
            let _ = st.event_tx.send(DaemonEvent::JobDisplayLine {
                job_id: job_id.to_string(),
                line,
            });
        }

        if !ok {
            tracing::error!(
                step = step.name(),
                seq,
                outcome = ?outcome,
                "step failed; aborting pipeline",
            );
            pipeline_ok = false;
            break;
        }
    }

    // Finalize and persist metrics on every terminal state (success or
    // failure). Errors here are logged but do not change the return value;
    // the metrics file is best-effort observability data.
    metrics.finalize();
    match JobStore::new().and_then(|s| s.open(&job_id_owned)) {
        Ok(dir) => match dir.write_metrics(&metrics) {
            Ok(()) => {
                tracing::debug!(
                    job = %job_id,
                    attempts = metrics.attempts_total,
                    steps = metrics.step_count,
                    "wrote job metrics.json",
                );
            }
            Err(e) => {
                tracing::warn!(job = %job_id, error = %e, "failed to write metrics.json");
            }
        },
        Err(e) => {
            tracing::warn!(job = %job_id, error = %e, "failed to open job dir for metrics");
        }
    }

    pipeline_ok
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

async fn handle_tui_client(
    stream: tokio::net::UnixStream,
    state: Arc<Mutex<DaemonState>>,
    mut event_rx: broadcast::Receiver<DaemonEvent>,
) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half).lines();

    // Send initial state snapshot
    {
        let st = state.lock().await;
        let snapshot = st.snapshot_state();
        if let Ok(json) = serde_json::to_string(&snapshot) {
            let _ = write_half.write_all(format!("{}\n", json).as_bytes()).await;
        }
    }

    loop {
        tokio::select! {
            // Incoming TUI request
            line = reader.next_line() => {
                match line {
                    Ok(Some(l)) => {
                        if let Ok(req) = serde_json::from_str::<TuiRequest>(&l) {
                            handle_tui_request(req, &state, &mut write_half).await;
                        }
                    }
                    _ => break, // client disconnected
                }
            }
            // Outgoing daemon event
            Ok(event) = event_rx.recv() => {
                if let Ok(json) = serde_json::to_string(&event) {
                    if write_half.write_all(format!("{}\n", json).as_bytes()).await.is_err() {
                        break;
                    }
                }
            }
        }
    }
}

async fn handle_tui_request(
    req: TuiRequest,
    state: &Arc<Mutex<DaemonState>>,
    write_half: &mut (impl tokio::io::AsyncWrite + Unpin),
) {
    use tokio::io::AsyncWriteExt;
    match req {
        TuiRequest::Execute { manifest_path } => {
            trigger_execution(state, &manifest_path).await;
        }
        TuiRequest::KillJob { job_id } => {
            let mut st = state.lock().await;
            send_kill_signals(&mut st, &job_id).await;
            if let Some(mut job) = st.running_jobs.remove(&job_id) {
                job.status = JobStatus::Killed;
                job.finished_at = Some(chrono::Utc::now());
                let _ = job.save();
                st.history.insert(0, job.clone());
                st.job_last_activity.remove(&job_id);
                st.job_started_at_monotonic.remove(&job_id);
            st.sub_agent_dispatch_counter.remove(&job_id);
                let _ = st.event_tx.send(DaemonEvent::JobUpdated { job });
            }
        }
        TuiRequest::PauseJob { job_id } => {
            let mut st = state.lock().await;
            if st.running_jobs.contains_key(&job_id) {
                st.paused_jobs.insert(job_id);
                let event = st.snapshot_state();
                let _ = st.event_tx.send(event);
            }
        }
        TuiRequest::ResumeJob { job_id } => {
            let mut st = state.lock().await;
            st.paused_jobs.remove(&job_id);
            let event = st.snapshot_state();
            let _ = st.event_tx.send(event);
        }
        TuiRequest::GetState => {
            let st = state.lock().await;
            let snapshot = st.snapshot_state();
            if let Ok(json) = serde_json::to_string(&snapshot) {
                let _ = write_half.write_all(format!("{}\n", json).as_bytes()).await;
            }
        }
        TuiRequest::TrackRemote { plan_path, remote_repo, pr_number } => {
            let mut st = state.lock().await;
            st.remote_executions.insert(plan_path.clone(), (remote_repo, pr_number));
            tracing::info!(plan = %plan_path, pr = pr_number, "tracking remote execution");
        }
        TuiRequest::Subscribe => {
            // Already subscribed via broadcast channel; no-op
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_watchdog_timings_uses_monotonic_start() {
        let start = Instant::now();
        let last = start + Duration::from_secs(100);
        let now = start + Duration::from_secs(200);
        let (since_start, since_activity) =
            compute_watchdog_timings(now, Some(last), start);
        assert_eq!(since_start, Duration::from_secs(200));
        assert_eq!(since_activity, Duration::from_secs(100));
    }

    #[test]
    fn compute_watchdog_timings_missing_last_activity_collapses_silence_to_zero() {
        // F5: when last_activity is absent the stall branch must be Ok
        // (since_activity = 0) while hard-cap still applies via since_start.
        let start = Instant::now();
        let now = start + Duration::from_secs(600);
        let (since_start, since_activity) =
            compute_watchdog_timings(now, None, start);
        assert_eq!(since_start, Duration::from_secs(600));
        assert_eq!(since_activity, Duration::from_secs(0));

        // Hard-cap fires for this synthetic input even without activity.
        let v = watchdog_verdict(
            since_start,
            since_activity,
            Duration::from_secs(300),
            Duration::from_secs(500),
        );
        assert_eq!(v, WatchdogVerdict::HardCapped { total_seconds: 600 });
    }

    #[test]
    fn compute_watchdog_timings_saturates_on_backward_clock_jump() {
        // F2: a monotonic Instant cannot go backward, but if some future
        // bug passes an `older > now` pair we saturate to zero rather than
        // panic. Covers the defensive branch in saturating_duration_since.
        let start = Instant::now();
        let last = start + Duration::from_secs(500);
        let now = start + Duration::from_secs(100); // now < last
        let (since_start, since_activity) =
            compute_watchdog_timings(now, Some(last), start);
        assert_eq!(since_start, Duration::from_secs(100));
        assert_eq!(since_activity, Duration::from_secs(0));
    }

    // Process-tree helpers moved to crate::proctree; their tests live
    // there.
}
