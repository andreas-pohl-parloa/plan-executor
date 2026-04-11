
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::net::UnixListener;
use tokio::sync::{broadcast, Mutex};
use tokio::time::Duration;
use anyhow::Result;

use crate::config::Config;
use crate::executor::{spawn_execution, ExecEvent};
use crate::handoff;
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
    /// Jobs currently paused at a handoff — sub-agents held until ResumeJob
    pub paused_jobs: std::collections::HashSet<String>,
    /// Remote executions being monitored: plan_path → (remote_repo, pr_number)
    pub remote_executions: HashMap<String, (String, u64)>,
    /// broadcast channel for DaemonEvent to all TUI clients
    pub event_tx: broadcast::Sender<DaemonEvent>,
}

/// Sends a desktop notification using OS-native tools.
/// macOS: osascript (always available). Linux: notify-send (best-effort).
fn notify(title: &str, body: &str) {
    let title = title.to_string();
    let body = body.to_string();
    std::thread::spawn(move || {
        #[cfg(target_os = "macos")]
        {
            // osascript display notification — works from daemons, no deps.
            let escaped_body = body.replace('\\', "\\\\").replace('"', "\\\"");
            let escaped_title = title.replace('\\', "\\\\").replace('"', "\\\"");
            let script = format!(
                "display notification \"{}\" with title \"{}\"",
                escaped_body, escaped_title
            );
            let _ = std::process::Command::new("osascript")
                .args(["-e", &script])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = std::process::Command::new("notify-send")
                .args([&title, &body])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    });
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
        DaemonEvent::State {
            running_jobs: self.running_jobs.values().cloned().collect(),
            history: self.history.clone(),
            paused_job_ids: self.paused_jobs.iter().cloned().collect(),
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
                        Ok(url) => {
                            tracing::info!(url = %url, "remote execution triggered");
                            notify_plan("Remote execution started", &plan, "");
                            // Update plan status and store PR number
                            let _ = crate::plan::set_plan_header(&plan, "status", "EXECUTING");
                            if let Some(pr_num) = crate::remote::pr_number_from_url(&url) {
                                let _ = crate::plan::set_plan_header(&plan, "remote-pr", &pr_num.to_string());
                                let mut st = state.lock().await;
                                st.remote_executions.insert(
                                    plan_path.to_string(),
                                    (remote_repo.to_string(), pr_num),
                                );
                            }
                        }
                        Err(e) => tracing::error!(error = %e, "remote execution failed"),
                    }
                }
                Err(e) => {
                    tracing::error!(plan = %plan_path, error = %e, "remote execution: could not determine git context");
                }
            }
            let st = state.lock().await;
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

    let Ok((child, pgid, exec_rx)) = spawn_execution(job.clone(), execution_root.clone(), &main_cmd) else {
        tracing::error!("failed to spawn execution for plan: {}", plan_path);
        return;
    };

    notify_plan("Execution started", &plan, "");

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
            &agents.bash,
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
            crate::executor::consume_handoffs(&state_file);
            fail_job_cleanup(&state_clone, &job_id_full).await;
            return;
        }

        crate::executor::consume_handoffs(&state_file);

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
    _plan_path_owned: String,
    execution_root: PathBuf,
    mut exec_rx: tokio::sync::mpsc::Receiver<crate::executor::ExecEvent>,
) {
    let mut last_display_blank = false;
    let mut completion_retried = false;
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
                    // Store session_id on the running job and persist to disk
                    // before the (potentially long) sub-agent dispatch. A daemon
                    // kill during dispatch will then show this job as Failed on
                    // next startup and `retry` can resume with the session_id.
                    {
                        let mut st = state.lock().await;
                        if let Some(job) = st.running_jobs.get_mut(&job_id) {
                            job.session_id = Some(session_id.clone());
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
                        &agents.bash,
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
                        crate::executor::consume_handoffs(&state_file);
                        break 'outer;
                    }

                    crate::executor::consume_handoffs(&state_file);

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
                    // If the agent returned success but the plan is still
                    // EXECUTING, the skill bailed out mid-execution (e.g.
                    // after a handoff resume it completed the triage but
                    // didn't continue to the remaining phases). Resume the
                    // session once with an explicit instruction to finish.
                    let plan_still_executing = finished_job.status == JobStatus::Success
                        && crate::plan::parse_plan_status(&plan)
                            .map(|s| matches!(s, crate::plan::PlanStatus::Executing))
                            .unwrap_or(false);

                    if plan_still_executing && !completion_retried {
                        completion_retried = true;
                        let session_id = {
                            let st = state.lock().await;
                            st.running_jobs.get(&job_id)
                                .and_then(|j| j.session_id.clone())
                                .or_else(|| finished_job.session_id.clone())
                        };

                        if let Some(sid) = session_id {
                            let line = "⏺ [plan-executor] plan still EXECUTING after agent returned success — resuming to complete remaining phases";
                            append_display(&job_id, line);
                            {
                                let mut st = state.lock().await;
                                st.job_display_output.entry(job_id.clone()).or_default().push_back(line.to_string());
                                let _ = st.event_tx.send(DaemonEvent::JobDisplayLine { job_id: job_id.clone(), line: line.to_string() });
                                let _ = st.event_tx.send(DaemonEvent::JobOutput { job_id: job_id.clone(), line: line.to_string() });
                            }

                            let agents = { state.lock().await.agents.clone() };
                            let continuation = "The plan execution is incomplete — the plan status is still EXECUTING. \
                                You returned from a handoff resume but did not complete the remaining execution phases. \
                                Continue from where you left off. Complete all remaining phases (plan validation, \
                                cleanup/PR, execution summary) until the plan status is set to COMPLETED.";

                            match handoff::resume_execution(
                                &sid,
                                continuation,
                                execution_root.clone(),
                                Some(job_id.clone()),
                                Some(plan.clone()),
                                &agents.main,
                            ).await {
                                Ok((new_child, new_pgid, new_rx)) => {
                                    let mut st = state.lock().await;
                                    st.running_children.insert(job_id.clone(), new_child);
                                    st.process_group_ids.insert(job_id.clone(), new_pgid);
                                    exec_rx = new_rx;
                                    continue 'outer;
                                }
                                Err(e) => {
                                    let line = format!("⏺ [plan-executor] completion retry failed to resume: {}", e);
                                    append_display(&job_id, &line);
                                    let mut st = state.lock().await;
                                    st.job_display_output.entry(job_id.clone()).or_default().push_back(line.clone());
                                    let _ = st.event_tx.send(DaemonEvent::JobDisplayLine { job_id: job_id.clone(), line: line.clone() });
                                    // Fall through to finish as failed
                                }
                            }
                        }
                    }

                    // After the retry (or if no retry was needed), finalize the job.
                    // If plan is still EXECUTING after a retry, force fail.
                    let force_fail = plan_still_executing;

                    let mut st = state.lock().await;
                    // Merge result fields from the resume placeholder into
                    // the original running job (which has the correct
                    // started_at from initial trigger_execution).
                    let mut job = if let Some(running) = st.running_jobs.remove(&job_id) {
                        running
                    } else {
                        finished_job.clone()
                    };
                    job.status = if force_fail { JobStatus::Failed } else { finished_job.status };
                    job.model = job.model.or(finished_job.model);
                    job.session_id = job.session_id.or(finished_job.session_id);
                    job.input_tokens = finished_job.input_tokens.or(job.input_tokens);
                    job.output_tokens = finished_job.output_tokens.or(job.output_tokens);
                    job.cache_creation_tokens = finished_job.cache_creation_tokens.or(job.cache_creation_tokens);
                    job.cache_read_tokens = finished_job.cache_read_tokens.or(job.cache_read_tokens);
                    job.finished_at = Some(chrono::Utc::now());
                    // Wall-clock duration from actual start to now, not
                    // the per-turn duration_ms from the result event.
                    job.duration_ms = Some(
                        (chrono::Utc::now() - job.started_at)
                            .num_milliseconds()
                            .max(0) as u64,
                    );
                    if force_fail {
                        let line = "⏺ [plan-executor] plan still EXECUTING after completion retry — marking job FAILED";
                        append_display(&job_id, line);
                        st.job_display_output.entry(job_id.clone()).or_default().push_back(line.to_string());
                        let _ = st.event_tx.send(DaemonEvent::JobDisplayLine { job_id: job_id.clone(), line: line.to_string() });
                    }
                    let _ = job.save();
                    st.running_children.remove(&job_id);
                    st.process_group_ids.remove(&job_id);
                    st.sub_agent_pgids.remove(&job_id);
                    st.job_output.remove(&job_id);
                    st.job_display_output.remove(&job_id);
                    st.history.insert(0, job.clone());
                    let status_str = match job.status {
                        JobStatus::Success => "succeeded",
                        JobStatus::Failed => "failed",
                        JobStatus::Killed => "killed",
                        _ => "finished",
                    };
                    notify_plan(&format!("Execution {}", status_str), &job.plan_path, "");
                    let _ = st.event_tx.send(DaemonEvent::JobUpdated { job });
                    break 'outer;
                }
            }
        }
        break; // exec_rx closed without Finished
    } // end 'outer loop

    // Clean up job if it never finished (resume failure or channel closed)
    {
        let mut st = state.lock().await;
        if let Some(mut job) = st.running_jobs.remove(&job_id) {
            if job.status == JobStatus::Running {
                job.status = JobStatus::Failed;
                job.finished_at = Some(chrono::Utc::now());
                let _ = job.save();
                notify_plan("Execution failed", &job.plan_path, "");
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
        TuiRequest::Execute { plan_path } => {
            trigger_execution(state, &plan_path).await;
        }
        TuiRequest::KillJob { job_id } => {
            let mut st = state.lock().await;

            // Kill the main agent's process group
            if let Some(pgid) = st.process_group_ids.remove(&job_id) {
                if pgid > 0 {
                    #[cfg(unix)]
                    unsafe { libc::kill(-(pgid as libc::pid_t), libc::SIGKILL); }
                }
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
