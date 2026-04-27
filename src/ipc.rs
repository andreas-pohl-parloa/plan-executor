use std::path::PathBuf;
use serde::{Deserialize, Serialize};
use crate::jobs::JobMetadata;
use crate::config::Config;

pub fn socket_path() -> PathBuf {
    Config::base_dir().join("daemon.sock")
}

/// Messages sent from TUI → Daemon
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TuiRequest {
    /// Subscribe to state updates (daemon streams responses)
    Subscribe,
    /// Execute a plan immediately
    Execute { manifest_path: String },
    /// Kill a running job
    KillJob { job_id: String },
    /// Pause a running job — handoff sub-agents are held until ResumeJob
    PauseJob { job_id: String },
    /// Resume a previously paused job
    ResumeJob { job_id: String },
    /// Request full state snapshot
    GetState,
    /// Retry the handoff for a job whose sub-agents were never dispatched
    RetryHandoff { job_id: String },
    /// Track a remote execution PR for status monitoring
    TrackRemote { plan_path: String, remote_repo: String, pr_number: u64 },
}

/// Live process-group info for a locally running job. Empty for remote jobs.
#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct JobProcesses {
    pub job_id: String,
    /// Main orchestrator agent's PGID; `None` if not tracked.
    #[serde(default)]
    pub main_pgid: Option<u32>,
    /// PGIDs for every currently-dispatched sub-agent.
    #[serde(default)]
    pub sub_agent_pgids: Vec<u32>,
    /// Seconds since the last liveness event for this job. `None` if no
    /// baseline has been stamped yet. This is the same signal the
    /// watchdog uses — small values mean the job is actively producing
    /// output, large values mean it's been silent.
    #[serde(default)]
    pub idle_seconds: Option<u64>,
}

/// Messages sent from Daemon → TUI
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonEvent {
    /// Full state snapshot (response to Subscribe/GetState)
    State {
        running_jobs: Vec<JobMetadata>,
        history: Vec<JobMetadata>,
        /// IDs of jobs currently paused at a handoff
        #[serde(default)]
        paused_job_ids: Vec<String>,
        /// Live process-group info for each running local job. Defaulted
        /// to empty for backwards-compat with older daemons.
        #[serde(default)]
        running_processes: Vec<JobProcesses>,
    },
    /// A job's output line arrived
    JobOutput { job_id: String, line: String },
    /// A formatted human-readable display line for a job
    JobDisplayLine { job_id: String, line: String },
    /// One streamed line from a sub-agent, broadcast live so `output -f`
    /// can render sub-agent JSONL events as they arrive rather than only
    /// batch-rendering from disk at `sub-agent N done`.
    SubAgentLine {
        job_id: String,
        index: usize,
        agent_type: String,
        is_stderr: bool,
        line: String,
    },
    /// A job's metadata changed (status, tokens, cost)
    JobUpdated { job: JobMetadata },
    /// Error response
    Error { message: String },
}

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use anyhow::Result;

/// Sends a message as a JSON line over a UnixStream.
#[allow(dead_code)]
pub async fn send_msg<T: Serialize>(stream: &mut UnixStream, msg: &T) -> Result<()> {
    let mut line = serde_json::to_string(msg)?;
    line.push('\n');
    stream.write_all(line.as_bytes()).await?;
    Ok(())
}

/// Reads one JSON line from a BufReader<UnixStream>.
#[allow(dead_code)]
pub async fn recv_msg<T: for<'de> Deserialize<'de>>(
    reader: &mut BufReader<UnixStream>,
) -> Result<T> {
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let msg = serde_json::from_str(line.trim())?;
    Ok(msg)
}
