use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use serde::Deserialize;
use anyhow::Result;
use crate::jobs::{JobMetadata, JobStatus};
use crate::pricing::{calculate_cost, load_pricing};

/// Events emitted during execution
#[derive(Debug)]
pub enum ExecEvent {
    OutputLine(String),
    DisplayLine(String),
    /// Emitted when the claude process exits and `.tmp-execute-plan-state.json` is present.
    HandoffRequired { session_id: String, state_file: PathBuf },
    Finished(JobMetadata),
}

/// Parsed fields from claude stream-json
#[derive(Debug, Deserialize, Default)]
struct StreamEvent {
    #[serde(rename = "type")]
    event_type: String,
    subtype: Option<String>,
    // For "system" type
    model: Option<String>,
    session_id: Option<String>,
    // For "result" type
    duration_ms: Option<u64>,
    usage: Option<UsageBlock>,
}

#[derive(Debug, Deserialize, Default)]
struct UsageBlock {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
}

/// Spawns claude and returns a child handle and an event receiver.
/// `execution_root` is the repo/worktree root — where `.tmp-execute-plan-state.json` will be written.
pub fn spawn_execution(
    mut job: JobMetadata,
    execution_root: PathBuf,
) -> Result<(Child, mpsc::Receiver<ExecEvent>)> {
    let plan_path = job.plan_path.to_string_lossy().to_string();
    // Quote the plan path to handle paths with spaces
    let quoted_path = plan_path.replace('"', "\\\"");
    let cmd_arg = format!("/my:execute-plan-non-interactive \"{}\"", quoted_path);

    let mut child = Command::new("claude")
        .args([
            "--dangerously-skip-permissions",
            "--verbose",
            "--output-format",
            "stream-json",
            "-p",
            &cmd_arg,
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    let stdout = child.stdout.take().expect("stdout must be piped");
    let (tx, rx) = mpsc::channel::<ExecEvent>(256);

    // Prepare output file
    std::fs::create_dir_all(job.job_dir())?;
    let output_path = job.output_path();
    job.save()?;

    tokio::spawn(async move {
        let pricing = load_pricing();
        let mut reader = BufReader::new(stdout).lines();
        let mut output_file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&output_path)
            .await
            .ok();

        while let Ok(Some(line)) = reader.next_line().await {
            // Write to output file
            if let Some(ref mut f) = output_file {
                let _ = f.write_all(format!("{}\n", line).as_bytes()).await;
            }

            // Parse stream-json
            if let Ok(event) = serde_json::from_str::<StreamEvent>(&line) {
                match event.event_type.as_str() {
                    "system" => {
                        if let Some(model) = event.model {
                            job.model = Some(model);
                        }
                        if let Some(sid) = event.session_id {
                            job.session_id = Some(sid);
                        }
                    }
                    "result" => {
                        if event.subtype.as_deref() != Some("success") {
                            job.status = JobStatus::Failed;
                        }
                        if let Some(usage) = event.usage {
                            job.input_tokens = usage.input_tokens;
                            job.output_tokens = usage.output_tokens;
                            job.cache_creation_tokens = usage.cache_creation_input_tokens;
                            job.cache_read_tokens = usage.cache_read_input_tokens;
                        }
                        job.duration_ms = event.duration_ms;
                        // Calculate cost from pricing.json
                        if let Some(model) = &job.model {
                            job.cost_usd = calculate_cost(
                                &pricing,
                                model,
                                job.input_tokens.unwrap_or(0),
                                job.output_tokens.unwrap_or(0),
                                job.cache_creation_tokens.unwrap_or(0),
                                job.cache_read_tokens.unwrap_or(0),
                            );
                        }
                    }
                    _ => {}
                }
            }

            // Emit formatted display line
            for display_line in crate::formatter::format_stream_line(&line) {
                let _ = tx.send(ExecEvent::DisplayLine(display_line)).await;
            }
            let _ = tx.send(ExecEvent::OutputLine(line)).await;
        }

        // stdout closed — check for handoff pause before declaring finished
        let state_file = execution_root.join(".tmp-execute-plan-state.json");
        if state_file.exists() {
            if let Some(sid) = job.session_id.clone() {
                // Note: intentionally not saving here — the daemon handles state persistence
                // during the handoff loop. Saving Running state here would leave a stale record
                // on crash before the handoff completes.
                let _ = tx.send(ExecEvent::HandoffRequired {
                    session_id: sid,
                    state_file,
                }).await;
                return; // caller loop will resume; do NOT emit Finished here
            }
        }

        if job.status != JobStatus::Failed {
            job.status = JobStatus::Success;
        }
        job.finished_at = Some(chrono::Utc::now());
        let _ = job.save();
        let _ = tx.send(ExecEvent::Finished(job)).await;
    });

    Ok((child, rx))
}
