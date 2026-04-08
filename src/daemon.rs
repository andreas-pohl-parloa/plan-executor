
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::net::UnixListener;
use tokio::sync::{broadcast, Mutex};
use tokio::time::{Duration, Instant};
use anyhow::Result;

use crate::config::Config;
use crate::executor::{spawn_execution, ExecEvent};
use crate::handoff;
use crate::ipc::{socket_path, DaemonEvent, PendingPlan, TuiRequest};
use crate::jobs::{JobMetadata, JobStatus};
use crate::notifications::{notify_execution_complete, notify_plan_ready};
use crate::plan::{find_ready_plans, parse_plan_status, PlanStatus};
use crate::watcher::start_watcher;

/// Shared daemon state
pub struct DaemonState {
    #[allow(dead_code)]
    pub config: Config,
    pub agents: crate::config::AgentConfig,
    pub running_jobs: HashMap<String, JobMetadata>, // job_id -> metadata
    pub pending_plans: HashMap<String, PendingInfo>, // plan_path -> info
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
    /// Jobs currently paused at a handoff — sub-agents held until ResumeJob
    pub paused_jobs: std::collections::HashSet<String>,
    /// broadcast channel for DaemonEvent to all TUI clients
    pub event_tx: broadcast::Sender<DaemonEvent>,
}

pub struct PendingInfo {
    pub plan_path: String,
    pub detected_at: Instant,
    pub auto_execute: bool,
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
        DaemonEvent::State {
            running_jobs: self.running_jobs.values().cloned().collect(),
            pending_plans: self.pending_plans.values().map(|p| PendingPlan {
                plan_path: p.plan_path.clone(),
                auto_execute_remaining_secs: if p.auto_execute {
                    let elapsed = p.detected_at.elapsed().as_secs();
                    Some(15u64.saturating_sub(elapsed))
                } else {
                    None
                },
            }).collect(),
            history: self.history.clone(),
            paused_job_ids: self.paused_jobs.iter().cloned().collect(),
        }
    }
}

/// Main daemon entry point
pub async fn run_daemon(config: crate::config::Config) -> Result<()> {
    tracing::info!("daemon starting (pid={})", std::process::id());

    let watch_dirs = config.expanded_watch_dirs();
    tracing::info!("config loaded, watch_dirs={:?}", watch_dirs);

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
        pending_plans: HashMap::new(),
        history: history_on_start,
        job_output: HashMap::new(),
        job_display_output: HashMap::new(),
        running_children: HashMap::new(),
        process_group_ids: HashMap::new(),
        sub_agent_pgids: HashMap::new(),
        paused_jobs: std::collections::HashSet::new(),
        event_tx: event_tx.clone(),
    }));
    tracing::info!("job history loaded");

    // Startup scan runs in the background so the daemon (socket + watcher)
    // is available immediately. Plans are added to pending_plans when found.
    {
        let state_clone = Arc::clone(&state);
        let patterns = config.plan_patterns.clone();
        let auto_execute = config.auto_execute;
        let dirs = watch_dirs.clone();
        tokio::task::spawn_blocking(move || {
            tracing::info!("background scan: scanning for ready plans");
            let ready = find_ready_plans(&dirs, &patterns);
            tracing::info!("background scan: found {} ready plan(s)", ready.len());
            // Use block_on to acquire the async mutex from the blocking thread
            let rt = tokio::runtime::Handle::current();
            rt.block_on(async {
                let mut st = state_clone.lock().await;
                for plan in ready {
                    let path_str = plan.path.to_string_lossy().to_string();
                    if st.pending_plans.contains_key(&path_str) { continue; }
                    if st.running_jobs.values().any(|j| j.plan_path == plan.path) { continue; }
                    tracing::info!("background scan: queueing {}", path_str);
                    let _ = notify_plan_ready(&path_str, auto_execute);
                    st.pending_plans.insert(path_str.clone(), PendingInfo {
                        plan_path: path_str,
                        detected_at: Instant::now(),
                        auto_execute,
                    });
                }
                let event = st.snapshot_state();
                let _ = st.event_tx.send(event);
            });
        });
    }

    // Start watcher
    tracing::info!("starting FSEvents watcher");
    let (watcher, mut watch_rx) = start_watcher(watch_dirs.clone())?;
    tracing::info!("watcher started");
    let _watcher = watcher; // keep alive

    // Unix socket listener
    let listener = UnixListener::bind(&sock_path)?;

    // Ticker for auto-execute countdown (check every second)
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    // Periodic re-scan to pick up plans in repos added while the daemon runs.
    let mut rescan_interval = tokio::time::interval(Duration::from_secs(60));
    rescan_interval.tick().await; // consume the immediate first tick

    loop {
        tokio::select! {
            // FS event
            Some(watch_event) = watch_rx.recv() => {
                handle_watch_event(&state, &config, watch_event.path).await;
            }

            // New TUI client
            Ok((stream, _)) = listener.accept() => {
                let state_clone = Arc::clone(&state);
                let rx = event_tx.subscribe();
                tokio::spawn(handle_tui_client(stream, state_clone, rx));
            }

            // Auto-execute tick
            _ = interval.tick() => {
                handle_auto_execute_tick(&state).await;
            }

            // Periodic re-scan: catch plans in repos added while running
            _ = rescan_interval.tick() => {
                handle_rescan(&state, &config).await;
            }
        }
    }
}

async fn handle_watch_event(
    state: &Arc<Mutex<DaemonState>>,
    config: &Config,
    path: PathBuf,
) {
    let Ok(status) = parse_plan_status(&path) else { return };
    let path_str = path.to_string_lossy().to_string();
    let mut st = state.lock().await;

    if status != PlanStatus::Ready || !crate::plan::is_non_interactive(&path) {
        // Plan is no longer eligible — remove from pending if it was there.
        if st.pending_plans.remove(&path_str).is_some() {
            let event = st.snapshot_state();
            let _ = st.event_tx.send(event);
        }
        return;
    }

    // Skip if already pending or running
    if st.pending_plans.contains_key(&path_str) { return }
    if st.running_jobs.values().any(|j| j.plan_path == path) { return }

    let _ = notify_plan_ready(&path_str, config.auto_execute);
    st.pending_plans.insert(path_str.clone(), PendingInfo {
        plan_path: path_str.clone(),
        detected_at: Instant::now(),
        auto_execute: config.auto_execute,
    });
    let event = st.snapshot_state();
    let _ = st.event_tx.send(event);
}

async fn handle_auto_execute_tick(state: &Arc<Mutex<DaemonState>>) {
    let to_execute: Vec<String> = {
        let mut st = state.lock().await;
        let to_exec: Vec<String> = st.pending_plans.iter()
            .filter(|(_, info)| info.auto_execute && info.detected_at.elapsed() >= Duration::from_secs(15))
            .map(|(path, _)| path.clone())
            .collect();
        // Remove them while still holding the lock to prevent double-execution
        for path in &to_exec {
            st.pending_plans.remove(path);
        }
        to_exec
    };
    for path in to_execute {
        trigger_execution(state, &path).await;
    }
}

async fn handle_rescan(state: &Arc<Mutex<DaemonState>>, config: &Config) {
    let patterns = config.plan_patterns.clone();
    let dirs = config.expanded_watch_dirs();
    let auto_execute = config.auto_execute;

    let ready = tokio::task::spawn_blocking(move || find_ready_plans(&dirs, &patterns))
        .await
        .unwrap_or_default();

    let mut st = state.lock().await;
    let mut changed = false;
    for plan in ready {
        let path_str = plan.path.to_string_lossy().to_string();
        if st.pending_plans.contains_key(&path_str) { continue; }
        if st.running_jobs.values().any(|j| j.plan_path == plan.path) { continue; }
        tracing::info!("rescan: queueing new plan {}", path_str);
        let _ = notify_plan_ready(&path_str, auto_execute);
        st.pending_plans.insert(path_str.clone(), PendingInfo {
            plan_path: path_str,
            detected_at: Instant::now(),
            auto_execute,
        });
        changed = true;
    }
    if changed {
        let event = st.snapshot_state();
        let _ = st.event_tx.send(event);
    }
}

pub async fn trigger_execution(state: &Arc<Mutex<DaemonState>>, plan_path: &str) {
    let plan = PathBuf::from(plan_path);

    // Route remote plans to GitHub PR trigger instead of local execution.
    if crate::plan::parse_execution_mode(&plan) == crate::plan::ExecutionMode::Remote {
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
                    match crate::remote::trigger_remote_execution(remote_repo, &plan, &meta) {
                        Ok(url) => tracing::info!(url = %url, "remote execution triggered"),
                        Err(e) => tracing::error!(error = %e, "remote execution failed"),
                    }
                }
                Err(e) => {
                    tracing::error!(plan = %plan_path, error = %e, "remote execution: could not determine git context");
                }
            }
            let mut st = state.lock().await;
            st.pending_plans.remove(plan_path);
            let event = st.snapshot_state();
            let _ = st.event_tx.send(event);
            return;
        } else {
            tracing::error!("remote execution: remote_repo not configured");
        }
        return;
    }

    let execution_root = find_repo_root(&plan)
        .unwrap_or_else(|| plan.parent().unwrap_or(&plan).to_path_buf());

    let job = JobMetadata::new(plan.clone());
    let job_id = job.id.clone();

    let main_cmd = {
        let st = state.lock().await;
        st.agents.main.clone()
    };
    // Remove from pending before spawning to prevent re-trigger
    {
        let mut st = state.lock().await;
        st.pending_plans.remove(plan_path);
    }

    let Ok((child, pgid, exec_rx)) = spawn_execution(job.clone(), execution_root.clone(), &main_cmd) else {
        tracing::error!("failed to spawn execution for plan: {}", plan_path);
        return;
    };

    {
        let mut st = state.lock().await;
        st.running_jobs.insert(job_id.clone(), job.clone());
        st.job_output.insert(job_id.clone(), VecDeque::new());
        st.job_display_output.insert(job_id.clone(), VecDeque::new());
        st.running_children.insert(job_id.clone(), child);
        st.process_group_ids.insert(job_id.clone(), pgid);
        let event = st.snapshot_state();
        let _ = st.event_tx.send(event);
    }

    let state_clone = Arc::clone(state);
    let plan_path_owned = plan_path.to_string();
    tokio::spawn(run_exec_event_loop(
        state_clone, job_id, plan, plan_path_owned, execution_root, exec_rx,
    ));
}

/// Retry a job whose handoff was never dispatched: re-register the job as
/// running, dispatch the sub-agents from the existing state file, resume the
/// session, and process all subsequent events exactly like a normal execution.
pub async fn retry_handoff_from_state(
    state: &Arc<Mutex<DaemonState>>,
    job_id: String,
) {
    let job = match crate::jobs::JobMetadata::load_by_id_prefix(&job_id) {
        Some(j) => j,
        None => {
            let msg = format!("retry: job not found: {}", job_id);
            tracing::error!("{}", msg);
            let st = state.lock().await;
            let _ = st.event_tx.send(crate::ipc::DaemonEvent::Error { message: msg });
            return;
        }
    };
    let session_id = match job.session_id.clone() {
        Some(s) => s,
        None => {
            let msg = format!("retry: job {} has no session_id — cannot resume", job_id);
            tracing::error!("{}", msg);
            let st = state.lock().await;
            let _ = st.event_tx.send(crate::ipc::DaemonEvent::Error { message: msg });
            return;
        }
    };
    let plan = job.plan_path.clone();
    let execution_root = find_repo_root(&plan)
        .unwrap_or_else(|| plan.parent().unwrap_or(&plan).to_path_buf());
    let state_file = match crate::executor::find_state_file(&execution_root) {
        Some(f) => f,
        None => {
            let msg = format!(
                "retry: no state file found under {} — nothing to retry",
                execution_root.display()
            );
            tracing::error!("{}", msg);
            let st = state.lock().await;
            let _ = st.event_tx.send(crate::ipc::DaemonEvent::Error { message: msg });
            return;
        }
    };

    // Re-register the job as running (remove from history if present).
    {
        let mut st = state.lock().await;
        st.history.retain(|j| j.id != job.id);
        let mut running_job = job.clone();
        running_job.status = JobStatus::Running;
        running_job.finished_at = None;
        // Persist Running status to disk so a daemon kill marks this job Failed
        // on next startup rather than leaving it with the old Success status.
        let _ = running_job.save();
        st.running_jobs.insert(job.id.clone(), running_job);
        st.job_output.insert(job.id.clone(), VecDeque::new());
        st.job_display_output.insert(job.id.clone(), VecDeque::new());
        let event = st.snapshot_state();
        let _ = st.event_tx.send(event);
    }

    let state_clone = Arc::clone(state);
    let plan_path_owned = plan.to_string_lossy().to_string();
    let job_id_full = job.id.clone();

    tokio::spawn(async move {
        let agents = { state_clone.lock().await.agents.clone() };

        // Load the persisted handoff state.
        let state_data = match handoff::load_state(&state_file) {
            Ok(s) => s,
            Err(e) => {
                let line = format!("⏺ [plan-executor] failed to read state file: {}", e);
                append_display(&job_id_full, &line);
                fail_job_cleanup(&state_clone, &job_id_full).await;
                return;
            }
        };

        // Announce dispatch.
        {
            let line = format!(
                "⏺ [plan-executor] dispatching {} sub-agent(s) (phase: {})",
                state_data.handoffs.len(), state_data.phase
            );
            append_display(&job_id_full, &line);
            let mut st = state_clone.lock().await;
            st.job_display_output.entry(job_id_full.clone()).or_default().push_back(line.clone());
            let _ = st.event_tx.send(DaemonEvent::JobDisplayLine { job_id: job_id_full.clone(), line: line.clone() });
            let _ = st.event_tx.send(DaemonEvent::JobOutput { job_id: job_id_full.clone(), line });
        }

        let (results, sub_pgids) = handoff::dispatch_all(
            state_data.handoffs,
            &agents.claude,
            &agents.codex,
            &agents.gemini,
        ).await;

        {
            let mut st = state_clone.lock().await;
            st.sub_agent_pgids.insert(job_id_full.clone(), sub_pgids);
        }

        for r in &results {
            let line = if r.success {
                format!("⏺ [plan-executor] sub-agent {} done ({} chars)", r.index, r.stdout.len())
            } else {
                format!("⏺ [plan-executor] sub-agent {} failed: {}", r.index,
                    r.stderr.lines().next().unwrap_or("(no stderr)"))
            };
            append_display(&job_id_full, &line);
            let mut st = state_clone.lock().await;
            st.job_display_output.entry(job_id_full.clone()).or_default().push_back(line.clone());
            let _ = st.event_tx.send(DaemonEvent::JobDisplayLine { job_id: job_id_full.clone(), line: line.clone() });
            let _ = st.event_tx.send(DaemonEvent::JobOutput { job_id: job_id_full.clone(), line });
        }

        if results.iter().any(|r| !r.success && !r.can_fail) {
            let _ = std::fs::remove_file(&state_file);
            fail_job_cleanup(&state_clone, &job_id_full).await;
            return;
        }

        let _ = std::fs::remove_file(&state_file);

        {
            let line = format!("⏺ [plan-executor] resuming session {}", &session_id[..session_id.len().min(16)]);
            append_display(&job_id_full, &line);
            let mut st = state_clone.lock().await;
            st.job_display_output.entry(job_id_full.clone()).or_default().push_back(line.clone());
            let _ = st.event_tx.send(DaemonEvent::JobDisplayLine { job_id: job_id_full.clone(), line: line.clone() });
            let _ = st.event_tx.send(DaemonEvent::JobOutput { job_id: job_id_full.clone(), line });
        }

        let continuation = handoff::build_continuation(&results);
        let exec_rx = match handoff::resume_execution(
            &session_id,
            &continuation,
            execution_root.clone(),
            Some(job_id_full.clone()),
            Some(plan.clone()),
            &agents.main,
        ).await {
            Ok((new_child, new_pgid, rx)) => {
                let mut st = state_clone.lock().await;
                st.running_children.insert(job_id_full.clone(), new_child);
                st.process_group_ids.insert(job_id_full.clone(), new_pgid);
                rx
            }
            Err(e) => {
                let line = format!("⏺ [plan-executor] failed to resume session: {}", e);
                append_display(&job_id_full, &line);
                let st = state_clone.lock().await;
                let _ = st.event_tx.send(DaemonEvent::JobOutput { job_id: job_id_full.clone(), line });
                drop(st);
                fail_job_cleanup(&state_clone, &job_id_full).await;
                return;
            }
        };

        run_exec_event_loop(
            state_clone, job_id_full, plan, plan_path_owned, execution_root, exec_rx,
        ).await;
    });
}

/// Marks a job as Failed and moves it to history. Used when retry dispatch or
/// resume fails before the normal event loop can handle cleanup.
async fn fail_job_cleanup(state: &Arc<Mutex<DaemonState>>, job_id: &str) {
    let mut st = state.lock().await;
    if let Some(mut job) = st.running_jobs.remove(job_id) {
        job.status = JobStatus::Failed;
        job.finished_at = Some(chrono::Utc::now());
        let _ = job.save();
        st.history.insert(0, job.clone());
        st.running_children.remove(job_id);
        st.process_group_ids.remove(job_id);
        st.sub_agent_pgids.remove(job_id);
        st.job_output.remove(job_id);
        st.job_display_output.remove(job_id);
        let _ = st.event_tx.send(DaemonEvent::JobUpdated { job });
    }
}

/// Core event loop shared by trigger_execution and retry_handoff_from_state.
/// Processes OutputLine, DisplayLine, HandoffRequired, and Finished events,
/// including recursive handoff dispatch and session resume.
async fn run_exec_event_loop(
    state: Arc<Mutex<DaemonState>>,
    job_id: String,
    plan: PathBuf,
    plan_path_owned: String,
    execution_root: PathBuf,
    mut exec_rx: tokio::sync::mpsc::Receiver<crate::executor::ExecEvent>,
) {
    let mut last_display_blank = false;
    'outer: loop {
        while let Some(event) = exec_rx.recv().await {
            match event {
                ExecEvent::OutputLine(line) => {
                    let mut st = state.lock().await;
                    let buf = st.job_output.entry(job_id.clone()).or_default();
                    buf.push_back(line.clone());
                    if buf.len() > 10000 { buf.pop_front(); }
                    let _ = st.event_tx.send(DaemonEvent::JobOutput {
                        job_id: job_id.clone(),
                        line,
                    });
                }
                ExecEvent::DisplayLine(line) => {
                    let is_blank = crate::executor::is_visually_blank(&line);
                    if is_blank && last_display_blank {
                        // drop consecutive blank line before it reaches the TUI or display buffer
                    } else {
                        last_display_blank = is_blank;
                        let mut st = state.lock().await;
                        let buf = st.job_display_output.entry(job_id.clone()).or_default();
                        buf.push_back(line.clone());
                        if buf.len() > 10000 { buf.pop_front(); }
                        let _ = st.event_tx.send(DaemonEvent::JobDisplayLine {
                            job_id: job_id.clone(),
                            line,
                        });
                    }
                }
                ExecEvent::HandoffRequired { session_id, state_file } => {
                    // Persist Running status to disk before the (potentially long)
                    // sub-agent dispatch. A daemon kill during dispatch will then
                    // show this job as Failed on next startup, not Success.
                    {
                        let st = state.lock().await;
                        if let Some(job) = st.running_jobs.get(&job_id) {
                            let _ = job.save();
                        }
                    }

                    let state_data = match handoff::load_state(&state_file) {
                        Ok(s) => s,
                        Err(e) => {
                            let st = state.lock().await;
                            let _ = st.event_tx.send(DaemonEvent::JobOutput {
                                job_id: job_id.clone(),
                                line: format!("⏺ [plan-executor] failed to read state file: {}", e),
                            });
                            break 'outer;
                        }
                    };

                    loop {
                        let is_paused = { state.lock().await.paused_jobs.contains(&job_id) };
                        if !is_paused { break; }
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }

                    {
                        let line = format!(
                            "⏺ [plan-executor] dispatching {} sub-agent(s) (phase: {})",
                            state_data.handoffs.len(), state_data.phase
                        );
                        append_display(&job_id, &line);
                        let mut st = state.lock().await;
                        st.job_display_output.entry(job_id.clone()).or_default().push_back(line.clone());
                        let _ = st.event_tx.send(DaemonEvent::JobDisplayLine { job_id: job_id.clone(), line: line.clone() });
                        let _ = st.event_tx.send(DaemonEvent::JobOutput { job_id: job_id.clone(), line });
                    }

                    let agents = { state.lock().await.agents.clone() };
                    let (results, sub_pgids) = handoff::dispatch_all(
                        state_data.handoffs,
                        &agents.claude,
                        &agents.codex,
                        &agents.gemini,
                    ).await;

                    {
                        let mut st = state.lock().await;
                        st.sub_agent_pgids.insert(job_id.clone(), sub_pgids);
                    }

                    for r in &results {
                        let line = if r.success {
                            format!("⏺ [plan-executor] sub-agent {} done ({} chars)", r.index, r.stdout.len())
                        } else {
                            let stderr_preview: String = r.stderr.lines()
                                .filter(|l| !l.trim().is_empty())
                                .take(3)
                                .collect::<Vec<_>>()
                                .join(" | ");
                            let stderr_str = if stderr_preview.is_empty() {
                                "(no stderr)".to_string()
                            } else {
                                stderr_preview
                            };
                            if r.can_fail {
                                format!("⏺ [plan-executor] sub-agent {} skipped (can-fail): {}", r.index, stderr_str)
                            } else {
                                format!("⏺ [plan-executor] sub-agent {} failed: {}", r.index, stderr_str)
                            }
                        };
                        append_display(&job_id, &line);
                        let mut st = state.lock().await;
                        st.job_display_output.entry(job_id.clone()).or_default().push_back(line.clone());
                        let _ = st.event_tx.send(DaemonEvent::JobDisplayLine { job_id: job_id.clone(), line: line.clone() });
                        let _ = st.event_tx.send(DaemonEvent::JobOutput { job_id: job_id.clone(), line });
                    }

                    if results.iter().any(|r| !r.success && !r.can_fail) {
                        let _ = std::fs::remove_file(&state_file);
                        break 'outer;
                    }

                    let _ = std::fs::remove_file(&state_file);

                    {
                        let line = format!("⏺ [plan-executor] resuming session {}", &session_id[..session_id.len().min(16)]);
                        append_display(&job_id, &line);
                        let mut st = state.lock().await;
                        st.job_display_output.entry(job_id.clone()).or_default().push_back(line.clone());
                        let _ = st.event_tx.send(DaemonEvent::JobDisplayLine { job_id: job_id.clone(), line: line.clone() });
                        let _ = st.event_tx.send(DaemonEvent::JobOutput { job_id: job_id.clone(), line });
                    }

                    let continuation = handoff::build_continuation(&results);
                    match handoff::resume_execution(
                        &session_id,
                        &continuation,
                        execution_root.clone(),
                        Some(job_id.clone()),
                        Some(plan.clone()),
                        &agents.main,
                    ).await {
                        Ok((new_child, new_pgid, new_rx)) => {
                            {
                                let mut st = state.lock().await;
                                st.running_children.insert(job_id.clone(), new_child);
                                st.process_group_ids.insert(job_id.clone(), new_pgid);
                            }
                            exec_rx = new_rx;
                            continue 'outer;
                        }
                        Err(e) => {
                            let st = state.lock().await;
                            let _ = st.event_tx.send(DaemonEvent::JobOutput {
                                job_id: job_id.clone(),
                                line: format!("⏺ [plan-executor] failed to resume session: {}", e),
                            });
                            break 'outer;
                        }
                    }
                }
                ExecEvent::Finished(finished_job) => {
                    let _ = finished_job.save();
                    let success = finished_job.status == JobStatus::Success;
                    let mut st = state.lock().await;
                    st.running_jobs.remove(&job_id);
                    st.running_children.remove(&job_id);
                    st.process_group_ids.remove(&job_id);
                    st.sub_agent_pgids.remove(&job_id);
                    st.job_output.remove(&job_id);
                    st.job_display_output.remove(&job_id);
                    st.history.insert(0, finished_job.clone());
                    let _ = notify_execution_complete(&plan_path_owned, success);
                    let _ = st.event_tx.send(DaemonEvent::JobUpdated { job: finished_job });
                    break 'outer;
                }
            }
        }
        break; // exec_rx closed without Finished
    } // end 'outer loop

    // Clean up job if it never finished (resume failure or channel closed)
    {
        let mut st = state.lock().await;
        if let Some(mut job) = st.running_jobs.remove(&job_id)
            && job.status == JobStatus::Running {
                job.status = JobStatus::Failed;
                job.finished_at = Some(chrono::Utc::now());
                let _ = job.save();
                st.history.insert(0, job.clone());
                st.running_children.remove(&job_id);
                st.process_group_ids.remove(&job_id);
                st.sub_agent_pgids.remove(&job_id);
                st.job_output.remove(&job_id);
                st.job_display_output.remove(&job_id);
                let _ = st.event_tx.send(DaemonEvent::JobUpdated { job });
            }
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
                if let Ok(json) = serde_json::to_string(&event)
                    && write_half.write_all(format!("{}\n", json).as_bytes()).await.is_err() {
                        break;
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
        TuiRequest::Execute { plan_path } => {
            trigger_execution(state, &plan_path).await;
        }
        TuiRequest::CancelPending { plan_path } => {
            // Rewrite **Status:** READY → CANCELLED in the plan file so it
            // is not re-detected by FSEvents after removal from pending_plans.
            if let Ok(content) = std::fs::read_to_string(&plan_path) {
                let updated = content.replacen("**Status:** READY", "**Status:** CANCELLED", 1);
                if updated != content {
                    let _ = std::fs::write(&plan_path, updated);
                }
            }
            let mut st = state.lock().await;
            st.pending_plans.remove(&plan_path);
            let event = st.snapshot_state();
            let _ = st.event_tx.send(event);
        }
        TuiRequest::KillJob { job_id } => {
            let mut st = state.lock().await;

            // Kill the main agent's process group
            if let Some(pgid) = st.process_group_ids.remove(&job_id)
                && pgid > 0 {
                    #[cfg(unix)]
                    unsafe { libc::kill(-(pgid as libc::pid_t), libc::SIGKILL); }
                }
            // Kill any active handoff sub-agent process groups
            if let Some(pgids) = st.sub_agent_pgids.remove(&job_id) {
                for pgid in pgids {
                    if pgid > 0 {
                        #[cfg(unix)]
                        unsafe { libc::kill(-(pgid as libc::pid_t), libc::SIGKILL); }
                    }
                }
            }
            // Belt-and-suspenders: kill the tracked child handle too
            if let Some(mut child) = st.running_children.remove(&job_id) {
                let _ = child.kill().await;
            }
            if let Some(mut job) = st.running_jobs.remove(&job_id) {
                job.status = JobStatus::Killed;
                job.finished_at = Some(chrono::Utc::now());
                let _ = job.save();
                st.history.insert(0, job.clone());
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
        TuiRequest::RetryHandoff { job_id } => {
            retry_handoff_from_state(state, job_id).await;
        }
        TuiRequest::Subscribe => {
            // Already subscribed via broadcast channel; no-op
        }
    }
}
