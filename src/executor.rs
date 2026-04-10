use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use serde::Deserialize;
use anyhow::Result;
use crate::jobs::{JobMetadata, JobStatus};

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

/// Returns true if `s` contains no visible characters after stripping ANSI
/// escape sequences and ASCII whitespace. Used to detect blank display lines
/// regardless of embedded color reset codes.
pub fn is_visually_blank(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            let mut j = i + 2;
            while j < bytes.len() && bytes[j] != b'm' { j += 1; }
            i = j + 1;
        } else if bytes[i] == b' ' || bytes[i] == b'\t' {
            i += 1;
        } else {
            return false;
        }
    }
    true
}

/// Known state file names, checked in priority order.
const STATE_FILE_NAMES: &[&str] = &[
    ".tmp-execute-plan-state.json",
    ".tmp-review-state.json",
];

/// Returns true if the state file contains pending handoffs that the daemon
/// should dispatch. A file with an empty or absent handoffs array is not a
/// pending handoff — the orchestrator is just using it for state tracking.
fn has_pending_handoffs(path: &std::path::Path) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else { return false };
    let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) else { return false };
    // Check both field names used by different skill versions.
    for key in &["handoffs", "expected_handoffs"] {
        if let Some(arr) = val.get(key).and_then(|v| v.as_array()) {
            if !arr.is_empty() {
                return true;
            }
        }
    }
    false
}

/// Finds a handoff state file in either the repo root (non-worktree case) or
/// inside any `.my/worktrees/<slug>/` subdirectory (worktree case).
/// Checks all known state file names in priority order.
/// Only returns files that contain pending handoffs.
pub fn find_state_file(execution_root: &Path) -> Option<PathBuf> {
    // Direct placement (non-worktree execution)
    for name in STATE_FILE_NAMES {
        let candidate = execution_root.join(name);
        if candidate.exists() && has_pending_handoffs(&candidate) {
            return Some(candidate);
        }
    }
    // Worktree placement: <repo>/.my/worktrees/*/<state-file>
    // Collect ALL matching state files across all worktrees to avoid non-deterministic
    // selection when multiple concurrent plans run in different worktrees of the same repo.
    let worktrees = execution_root.join(".my").join("worktrees");
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&worktrees) {
        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            for name in STATE_FILE_NAMES {
                let candidate = entry.path().join(name);
                if candidate.exists() && has_pending_handoffs(&candidate) {
                    candidates.push(candidate);
                    break; // highest-priority name wins for this worktree
                }
            }
        }
    }
    match candidates.len() {
        1 => candidates.into_iter().next(),
        0 => None,
        n => {
            tracing::warn!(
                "find_state_file: {} worktrees have a state file — ambiguous, returning None. \
                 Candidates: {:?}",
                n,
                candidates
            );
            None
        }
    }
}

/// Clears the handoffs array in the state file so it won't re-trigger dispatch,
/// while preserving all other orchestrator state (phase, wave, etc.).
pub fn consume_handoffs(state_file: &std::path::Path) {
    let Ok(content) = std::fs::read_to_string(state_file) else { return };
    let Ok(mut val) = serde_json::from_str::<serde_json::Value>(&content) else { return };
    if let Some(obj) = val.as_object_mut() {
        for key in &["handoffs", "expected_handoffs"] {
            if obj.contains_key(*key) {
                obj.insert((*key).to_string(), serde_json::json!([]));
            }
        }
    }
    let _ = std::fs::write(state_file, serde_json::to_string_pretty(&val).unwrap_or_default());
}

/// Spawns claude and returns a child handle and an event receiver.
/// `execution_root` is the repo/worktree root — where `.tmp-execute-plan-state.json` will be written.
pub fn spawn_execution(
    mut job: JobMetadata,
    execution_root: PathBuf,
    main_cmd: &str,
) -> Result<(Child, u32, mpsc::Receiver<ExecEvent>)> {
    let plan_path = job.plan_path.to_string_lossy().to_string();
    let quoted_path = plan_path.replace('"', "\\\"");

    // When PLAN_EXECUTOR_LOCAL=1 (GitHub Actions runner), give Claude the plan
    // content directly instead of invoking a skill.
    let cmd_arg = if std::env::var("PLAN_EXECUTOR_LOCAL").as_deref() == Ok("1") {
        let plan_content = std::fs::read_to_string(&job.plan_path)
            .unwrap_or_else(|_| String::new());
        format!(
            "You are executing a plan on a CI runner. Do NOT use any skills or slash commands. \
             Do NOT invoke /my:execute-plan or any other skill. \
             Read the tasks below and implement them directly. \
             Make the code changes, then commit with a descriptive message. \
             Do NOT create PRs or branches — just commit to the current branch.\n\n{}", plan_content
        )
    } else {
        format!("/my:execute-plan-non-interactive \"{}\"", quoted_path)
    };

    let (program, mut base_args) = crate::config::Config::parse_cmd(main_cmd);
    base_args.push("-p".to_string());
    base_args.push(cmd_arg);

    let mut child = {
        let mut cmd = Command::new(&program);
        cmd.args(&base_args)
           .stdout(std::process::Stdio::piped())
           .stderr(std::process::Stdio::null());
        #[cfg(unix)]
        cmd.process_group(0);
        cmd.spawn()?
    };

    let pgid = child.id().unwrap_or(0);

    let stdout = child.stdout.take().expect("stdout must be piped");
    let (tx, rx) = mpsc::channel::<ExecEvent>(256);

    // Prepare output files
    std::fs::create_dir_all(job.job_dir())?;
    let output_path  = job.output_path();
    let display_path = job.display_path();
    job.save()?;

    tokio::spawn(async move {
        let mut got_result = false;
        let mut reader = BufReader::new(stdout).lines();
        let mut output_file = tokio::fs::OpenOptions::new()
            .create(true).append(true).open(&output_path).await.ok();
        let mut display_file = tokio::fs::OpenOptions::new()
            .create(true).append(true).open(&display_path).await.ok();
        // Collapse consecutive blank display lines at the source.
        let mut last_display_blank = false;
        // Detect when the agent output handoff instructions but forgot the state file.
        let mut saw_handoff_call = false;

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
                            tracing::debug!("executor: captured session_id={}", sid);
                            job.session_id = Some(sid);
                        }
                    }
                    "result" => {
                        got_result = true;
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
                    }
                    _ => {}
                }
            }

            // Emit one DisplayLine per visual line (sjv may return multi-line
            // strings). Use color=true so ANSI codes are embedded; the TUI
            // parses them via ansi-to-tui.
            let rendered = sjv::render_runtime_line(&line, false, true);
            for display_line in rendered.lines().filter(|l| !l.is_empty()) {
                let is_blank = is_visually_blank(display_line);
                if is_blank && last_display_blank {
                    continue; // suppress consecutive blank lines
                }
                last_display_blank = is_blank;
                if display_line.contains("call sub-agent") {
                    saw_handoff_call = true;
                }
                if let Some(ref mut f) = display_file {
                    let _ = f.write_all(format!("{}\n", display_line).as_bytes()).await;
                }
                let _ = tx.send(ExecEvent::DisplayLine(display_line.to_string())).await;
            }
            let _ = tx.send(ExecEvent::OutputLine(line)).await;
        }

        // stdout closed — check for handoff pause before declaring finished.
        // The state file may be in the repo root (no-worktree case) OR inside
        // a worktree the agent created at .my/worktrees/<slug>/.
        let state_file = find_state_file(&execution_root);
        tracing::debug!("executor: stdout EOF — state_file={:?} session_id={:?}",
            state_file, job.session_id);
        if let Some(state_file) = state_file {
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
        } else if saw_handoff_call {
            // Agent output "call sub-agent" instructions but never wrote the state file.
            // This is a handoff protocol violation — fail the job with a clear error.
            let err = "⏺ [plan-executor] handoff protocol error: agent requested sub-agent calls but did not write the state file (.tmp-execute-plan-state.json)";
            if let Some(ref mut f) = display_file {
                let _ = f.write_all(format!("{}\n", err).as_bytes()).await;
            }
            let _ = tx.send(ExecEvent::DisplayLine(err.to_string())).await;
            job.status = JobStatus::Failed;
            job.finished_at = Some(chrono::Utc::now());
            let _ = job.save();
            let _ = tx.send(ExecEvent::Finished(job)).await;
            return;
        }

        // No result event = process was killed or crashed before finishing
        if !got_result {
            job.status = JobStatus::Failed;
        } else if job.status != JobStatus::Failed {
            job.status = JobStatus::Success;
        }
        job.finished_at = Some(chrono::Utc::now());
        let _ = job.save();
        let _ = tx.send(ExecEvent::Finished(job)).await;
    });

    Ok((child, pgid, rx))
}
