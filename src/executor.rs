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

/// Sends SIGKILL to the process group `pgid`. No-op for pgid 0. Safe to call
/// when the group has already exited. Used to force-terminate the main agent
/// when it commits a handoff-protocol violation.
#[cfg(unix)]
pub fn kill_pgroup(pgid: u32) {
    if pgid == 0 {
        return;
    }
    // Safety: sending SIGKILL to a pgroup is defined; a stale pgid just
    // yields ESRCH, which we ignore.
    unsafe {
        libc::kill(-(pgid as libc::pid_t), libc::SIGKILL);
    }
}

#[cfg(not(unix))]
pub fn kill_pgroup(_pgid: u32) {}

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

/// Returns true if `line` is a Claude stream-json event representing
/// assistant (or thinking) text containing `# output sub-agent <N>:`.
///
/// The orchestrator skill is contractually required to stop the turn
/// immediately after emitting `call sub-agent` handoff lines — the executor
/// dispatches the real sub-agents and re-enters the session with a
/// `# output sub-agent <N>:` continuation prompt on resume. If the agent
/// instead fabricates `# output sub-agent <N>:` blocks in its own assistant
/// text, the handoff is simulated, no real work happens, and the session
/// context becomes corrupted. This function detects that pattern so the
/// executor can kill the session and fail the job deterministically.
///
/// Only assistant-role content is inspected — user-role messages legitimately
/// contain `# output sub-agent` because that is the continuation prompt the
/// executor itself injects on resume.
pub fn assistant_emits_output_marker(line: &str) -> bool {
    let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else { return false };
    if val.get("type").and_then(|t| t.as_str()) != Some("assistant") {
        return false;
    }
    let Some(items) = val
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
    else {
        return false;
    };
    for item in items {
        let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let text = match item_type {
            "text" => item.get("text").and_then(|t| t.as_str()).unwrap_or(""),
            "thinking" => item
                .get("thinking")
                .or_else(|| item.get("text"))
                .and_then(|t| t.as_str())
                .unwrap_or(""),
            _ => continue,
        };
        if text.contains("# output sub-agent ") {
            return true;
        }
    }
    false
}

/// Parses a `call sub-agent N (agent-type: <type>[, can-fail: true]): <path>` line
/// into an (index, agentType, promptFile, canFail) tuple. Returns None if the
/// line doesn't match the handoff format. Strips ANSI escapes before matching.
pub fn parse_handoff_line(raw: &str) -> Option<(usize, String, String, bool)> {
    // Strip ANSI escape sequences first.
    let mut s = String::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            let mut j = i + 2;
            while j < bytes.len() && bytes[j] != b'm' { j += 1; }
            i = j + 1;
        } else {
            s.push(bytes[i] as char);
            i += 1;
        }
    }

    let s = s.trim();
    let rest = s.strip_prefix("call sub-agent ")?;
    // Parse index: "N (agent-type: ...): path"
    let (idx_str, rest) = rest.split_once(' ')?;
    let index: usize = idx_str.parse().ok()?;
    // Parse metadata block: "(agent-type: claude[, can-fail: true]): path"
    let rest = rest.strip_prefix('(')?;
    let (meta, path) = rest.split_once("): ")?;
    let mut agent_type = "claude".to_string();
    let mut can_fail = false;
    for part in meta.split(',') {
        let part = part.trim();
        if let Some(at) = part.strip_prefix("agent-type:") {
            agent_type = at.trim().to_string();
        } else if part == "can-fail: true" {
            can_fail = true;
        }
    }
    Some((index, agent_type, path.trim().to_string(), can_fail))
}

/// Injects a `handoffs` array into a state file JSON from parsed `call sub-agent`
/// lines, then rewrites the file. This patches protocol-drifted state files that
/// exist but lack the handoffs array.
pub fn inject_handoffs_into_state_file(
    state_file: &std::path::Path,
    handoff_lines: &[(usize, String, String, bool)],
) {
    let Ok(content) = std::fs::read_to_string(state_file) else { return };
    let Ok(mut val) = serde_json::from_str::<serde_json::Value>(&content) else { return };
    let arr: Vec<serde_json::Value> = handoff_lines.iter().map(|(idx, at, pf, cf)| {
        serde_json::json!({
            "index": idx,
            "agentType": at,
            "promptFile": pf,
            "canFail": cf,
        })
    }).collect();
    if let Some(obj) = val.as_object_mut() {
        obj.insert("handoffs".to_string(), serde_json::Value::Array(arr));
    }
    let _ = std::fs::write(state_file, serde_json::to_string_pretty(&val).unwrap_or_default());
}

/// Known state file names, checked in priority order.
const STATE_FILE_NAMES: &[&str] = &[
    ".tmp-execute-plan-state.json",
    ".tmp-review-state.json",
];

/// Searches for a state file by existence across all known locations:
/// 1. The execution root directly
/// 2. `.my/worktrees/*/` subdirectories
/// 3. Git worktrees (via `git worktree list`)
/// 4. Sibling directories of the execution root
pub fn find_state_file(execution_root: &Path) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    // 1. Direct placement (non-worktree execution)
    for name in STATE_FILE_NAMES {
        let candidate = execution_root.join(name);
        if candidate.exists() {
            return Some(candidate); // fast path
        }
    }

    // 2. .my/worktrees/*/ subdirectories
    let worktrees = execution_root.join(".my").join("worktrees");
    if let Ok(entries) = std::fs::read_dir(&worktrees) {
        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) { continue; }
            for name in STATE_FILE_NAMES {
                let candidate = entry.path().join(name);
                if candidate.exists() {
                    candidates.push(candidate);
                    break;
                }
            }
        }
    }

    // 3. Git worktrees (handles worktrees created anywhere on disk)
    if candidates.is_empty() {
        if let Ok(output) = std::process::Command::new("git")
            .arg("-C").arg(execution_root)
            .args(["worktree", "list", "--porcelain"])
            .output()
        {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                for line in stdout.lines() {
                    if let Some(wt_path) = line.strip_prefix("worktree ") {
                        let wt = PathBuf::from(wt_path);
                        if wt == execution_root { continue; }
                        for name in STATE_FILE_NAMES {
                            let candidate = wt.join(name);
                            if candidate.exists() {
                                candidates.push(candidate);
                                break;
                            }
                        }
                    }
                }
            }
        }
    }

    // 4. Sibling directories (agent may create worktrees as siblings like workspace-foo/)
    if candidates.is_empty() {
        if let Some(parent) = execution_root.parent() {
            if let Ok(entries) = std::fs::read_dir(parent) {
                for entry in entries.flatten() {
                    if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) { continue; }
                    let path = entry.path();
                    if path == execution_root { continue; }
                    for name in STATE_FILE_NAMES {
                        let candidate = path.join(name);
                        if candidate.exists() {
                            candidates.push(candidate);
                            break;
                        }
                    }
                }
            }
        }
    }

    match candidates.len() {
        1 => candidates.into_iter().next(),
        0 => None,
        n => {
            tracing::warn!(
                "find_state_file: {} locations have a state file — ambiguous, returning None. \
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

    let cmd_arg = format!("/plan-executor:execute-plan-non-interactive \"{}\"", quoted_path);

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

    let task_pgid = pgid;
    tokio::spawn(async move {
        let mut got_result = false;
        let mut reader = BufReader::new(stdout).lines();
        let mut output_file = tokio::fs::OpenOptions::new()
            .create(true).append(true).open(&output_path).await.ok();
        let mut display_file = tokio::fs::OpenOptions::new()
            .create(true).append(true).open(&display_path).await.ok();
        // Collapse consecutive blank display lines at the source.
        let mut last_display_blank = false;
        // Capture parsed handoff lines so we can inject them into the state file
        // if the agent wrote it without a proper handoffs array.
        let mut handoff_calls: Vec<(usize, String, String, bool)> = Vec::new();
        // Set if the agent fabricates `# output sub-agent <N>:` blocks in its
        // own assistant text after emitting handoff lines. The child is killed
        // and the job is failed deterministically.
        let mut protocol_violation = false;

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
                    if let Some(parsed) = parse_handoff_line(display_line) {
                        // Index 1 with existing entries = new batch (agent
                        // continued from one phase to the next in the same
                        // session). Keep only the latest batch.
                        if parsed.0 == 1 && !handoff_calls.is_empty() {
                            handoff_calls.clear();
                        }
                        handoff_calls.push(parsed);
                    }
                }
                if let Some(ref mut f) = display_file {
                    let _ = f.write_all(format!("{}\n", display_line).as_bytes()).await;
                }
                let _ = tx.send(ExecEvent::DisplayLine(display_line.to_string())).await;
            }
            let _ = tx.send(ExecEvent::OutputLine(line.clone())).await;

            // Protocol-violation detection: the skill must stop the turn
            // immediately after emitting its handoff lines. If the agent
            // instead fabricates `# output sub-agent N:` blocks in assistant
            // text (playing both sides of the handoff), kill the session —
            // its context is corrupted and the dispatched work never ran.
            if !handoff_calls.is_empty() && assistant_emits_output_marker(&line) {
                protocol_violation = true;
                kill_pgroup(task_pgid);
                break;
            }
        }

        // stdout closed — determine if the agent paused for handoffs.
        //
        // Primary transport: `call sub-agent` output lines captured above.
        // The state file is the skill's persistence store — the executor
        // only needs it for consume_handoffs (skill resume signal) and
        // crash recovery (retry_handoff_from_state).
        tracing::debug!(
            "executor: stdout EOF — handoff_calls={} session_id={:?} protocol_violation={}",
            handoff_calls.len(), job.session_id, protocol_violation
        );

        if protocol_violation {
            let err = "⏺ [plan-executor] handoff protocol violation: agent fabricated `# output sub-agent N:` blocks in the same turn as `call sub-agent` lines instead of stopping. Killing session — no sub-agents were actually dispatched. Re-run the plan.";
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

        if !handoff_calls.is_empty() {
            // Agent requested sub-agent dispatch via output lines.
            if let Some(state_file) = find_state_file(&execution_root) {
                // Inject handoffs from output lines into the state file so
                // load_state always finds the correct batch regardless of
                // whether the skill wrote the handoffs array properly.
                inject_handoffs_into_state_file(&state_file, &handoff_calls);
                if let Some(sid) = job.session_id.clone() {
                    let _ = tx.send(ExecEvent::HandoffRequired {
                        session_id: sid,
                        state_file,
                    }).await;
                    return;
                }
            }
            // State file doesn't exist — protocol violation.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assistant_emits_output_marker_detects_assistant_text() {
        let line = r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"call sub-agent 1 (agent-type: claude): /x.md\n\n# output sub-agent 1:\nTask complete."}]}}"##;
        assert!(assistant_emits_output_marker(line));
    }

    #[test]
    fn assistant_emits_output_marker_detects_thinking_text() {
        let line = r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"Now I should fabricate # output sub-agent 1: ..."}]}}"##;
        assert!(assistant_emits_output_marker(line));
    }

    #[test]
    fn assistant_emits_output_marker_ignores_user_role() {
        // Legitimate: the executor's resume prompt is delivered as a user turn.
        let line = r##"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"# output sub-agent 1:\nreal output from dispatched agent"}]}}"##;
        assert!(!assistant_emits_output_marker(line));
    }

    #[test]
    fn assistant_emits_output_marker_ignores_assistant_without_marker() {
        let line = r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"call sub-agent 1 (agent-type: claude): /x.md"}]}}"##;
        assert!(!assistant_emits_output_marker(line));
    }

    #[test]
    fn assistant_emits_output_marker_ignores_non_json() {
        assert!(!assistant_emits_output_marker("plain log line"));
        assert!(!assistant_emits_output_marker(""));
    }

    #[test]
    fn assistant_emits_output_marker_ignores_system_and_result() {
        assert!(!assistant_emits_output_marker(
            r#"{"type":"system","session_id":"abc"}"#
        ));
        assert!(!assistant_emits_output_marker(
            r#"{"type":"result","subtype":"success"}"#
        ));
    }
}
