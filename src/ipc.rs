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
    Execute { plan_path: String },
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
    },
    /// A job's output line arrived
    JobOutput { job_id: String, line: String },
    /// A formatted human-readable display line for a job
    JobDisplayLine { job_id: String, line: String },
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
