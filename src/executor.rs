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

/// Grace window between the first `call sub-agent` handoff line and the
/// expected stdout EOF. The skill contract requires the orchestrator to
/// stop the turn immediately after emitting the batch so the executor can
/// dispatch real sub-agents. If stdout is still producing output this long
/// after the first handoff appeared, the agent is continuing the turn
/// (often fabricating `# output sub-agent N:` blocks itself); kill the
/// process group and fail the job rather than silently running the session
/// on an uncoordinated parallel track.
pub const HANDOFF_STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(60);

/// The only `agent-type` values the sub-agent dispatcher can run. Keep in
/// lock-step with `crate::handoff::AgentType`. Exposed as a module constant so
/// the streaming `call sub-agent` validator can reject fat-fingered or
/// hallucinated values (e.g. the `Task` tool's `general-purpose`) the moment
/// the line is emitted, instead of letting the session drift for 60s.
pub const VALID_AGENT_TYPES: &[&str] = &["claude", "codex", "gemini", "bash"];

/// Returns true if a raw stream-json line represents an assistant message
/// containing at least one `tool_use` content block. Used to detect the
/// "orchestrator kept working after emitting handoff lines" failure mode:
/// after the first `call sub-agent` line arms the stop-deadline, any
/// tool_use is a protocol violation even if it arrives before the 60s
/// grace window expires.
pub fn line_has_tool_use(line: &str) -> bool {
    let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
        return false;
    };
    if val.get("type").and_then(|v| v.as_str()) != Some("assistant") {
        return false;
    }
    val.get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter().any(|b| {
                b.get("type").and_then(|t| t.as_str()) == Some("tool_use")
            })
        })
        .unwrap_or(false)
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
        // Deadline set to `first_handoff_at + HANDOFF_STOP_GRACE` once the
        // first `call sub-agent` line is seen. If stdout is still writing
        // past the deadline, the agent didn't stop the turn — kill the
        // process group and fail the job as a protocol violation.
        let mut handoff_deadline: Option<tokio::time::Instant> = None;
        // Reason is Some(..) iff a violation was detected. Drives the kill
        // path at EOF and the error message rendered to the user. Distinct
        // reasons (stop-deadline, invalid agent-type, tool_use after handoff)
        // carry their own text so the orchestrator gets actionable feedback
        // on retry instead of a generic "you broke the protocol" line.
        let mut violation_reason: Option<String> = None;

        'stream: loop {
            let line_result = match handoff_deadline {
                Some(deadline) => {
                    tokio::select! {
                        biased;
                        _ = tokio::time::sleep_until(deadline) => {
                            violation_reason = Some(format!(
                                "agent did not stop within {}s of emitting `call sub-agent` lines",
                                HANDOFF_STOP_GRACE.as_secs()
                            ));
                            kill_pgroup(task_pgid);
                            break 'stream;
                        }
                        r = reader.next_line() => r,
                    }
                }
                None => reader.next_line().await,
            };
            let line = match line_result {
                Ok(Some(l)) => l,
                _ => break 'stream,
            };

            // Any tool_use event after the first `call sub-agent` line is
            // a protocol violation — the skill contract demands the turn
            // end immediately. Catching it at the first post-handoff
            // tool_use saves 60s of wasted work that the stop-deadline
            // alone would have allowed.
            if handoff_deadline.is_some() && line_has_tool_use(&line) {
                violation_reason = Some(
                    "agent emitted a tool_use after `call sub-agent` lines in the same turn".to_string(),
                );
                kill_pgroup(task_pgid);
                break 'stream;
            }

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
                        // Reject unknown agent-types the moment they're
                        // emitted. Silent fallback to `claude` would teach
                        // the orchestrator that hallucinated values from the
                        // Task tool enum (e.g. `general-purpose`, `Explore`)
                        // are acceptable, and could mis-route reviewer
                        // batches where diversity matters. A hard kill with
                        // the exact bad value fed back to the agent is the
                        // corrective signal the retry loop needs.
                        if !VALID_AGENT_TYPES.iter().any(|v| *v == parsed.1) {
                            violation_reason = Some(format!(
                                "invalid agent-type `{}` on `call sub-agent {}` line — valid values: {}",
                                parsed.1,
                                parsed.0,
                                VALID_AGENT_TYPES.join(", ")
                            ));
                            kill_pgroup(task_pgid);
                            break 'stream;
                        }
                        // Index 1 with existing entries = new batch (agent
                        // continued from one phase to the next in the same
                        // session). Keep only the latest batch.
                        if parsed.0 == 1 && !handoff_calls.is_empty() {
                            handoff_calls.clear();
                        }
                        handoff_calls.push(parsed);
                        // Arm the stop-deadline on the very first handoff
                        // line of this session; extending it on subsequent
                        // lines in the same batch would defeat the point.
                        if handoff_deadline.is_none() {
                            handoff_deadline =
                                Some(tokio::time::Instant::now() + HANDOFF_STOP_GRACE);
                        }
                    }
                }
                if let Some(ref mut f) = display_file {
                    let _ = f.write_all(format!("{}\n", display_line).as_bytes()).await;
                }
                let _ = tx.send(ExecEvent::DisplayLine(display_line.to_string())).await;
            }
            let _ = tx.send(ExecEvent::OutputLine(line)).await;
        }

        // stdout closed — determine if the agent paused for handoffs.
        //
        // Primary transport: `call sub-agent` output lines captured above.
        // The state file is the skill's persistence store — the executor
        // only needs it for consume_handoffs (skill resume signal) and
        // crash recovery (retry_handoff_from_state).
        tracing::debug!(
            "executor: stdout EOF — handoff_calls={} session_id={:?} violation_reason={:?}",
            handoff_calls.len(), job.session_id, violation_reason
        );

        if let Some(reason) = violation_reason.as_ref() {
            let err = format!(
                "⏺ [plan-executor] handoff protocol violation: {}. Killing session. Re-run the plan.",
                reason
            );
            if let Some(ref mut f) = display_file {
                let _ = f.write_all(format!("{}\n", err).as_bytes()).await;
            }
            let _ = tx.send(ExecEvent::DisplayLine(err)).await;
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
    fn handoff_stop_grace_is_sixty_seconds() {
        // Change detector: widening or shrinking the grace window changes
        // the user-visible false-positive rate and must be intentional.
        assert_eq!(HANDOFF_STOP_GRACE.as_secs(), 60);
    }

    #[test]
    fn valid_agent_types_match_handoff_enum() {
        // Change detector: if a new AgentType is added in handoff.rs, this
        // list must grow in lock-step or the streaming validator will kill
        // valid handoffs. The executor deliberately compares agent-type
        // strings instead of going through handoff::AgentType so it can
        // emit the exact bad value in the error message.
        assert_eq!(VALID_AGENT_TYPES, &["claude", "codex", "gemini", "bash"]);
    }

    #[test]
    fn line_has_tool_use_matches_assistant_with_tool_use_block() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"ok"},{"type":"tool_use","id":"x","name":"Bash","input":{}}]}}"#;
        assert!(line_has_tool_use(line));
    }

    #[test]
    fn line_has_tool_use_rejects_text_only_assistant() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"call sub-agent 1 (agent-type: claude): /tmp/x.md"}]}}"#;
        assert!(!line_has_tool_use(line));
    }

    #[test]
    fn line_has_tool_use_rejects_non_assistant_events() {
        let system = r#"{"type":"system","subtype":"init"}"#;
        let result = r#"{"type":"result","subtype":"success","result":"done"}"#;
        let malformed = r#"not-json"#;
        assert!(!line_has_tool_use(system));
        assert!(!line_has_tool_use(result));
        assert!(!line_has_tool_use(malformed));
    }

    #[test]
    fn parse_handoff_line_exposes_invalid_agent_type_for_validation() {
        // The validator lives at the call-site; parse_handoff_line itself
        // stays lenient so diagnostic context (the bad value) survives.
        let parsed = parse_handoff_line(
            "call sub-agent 1 (agent-type: general-purpose): /tmp/x.md",
        ).expect("line matches handoff shape");
        assert_eq!(parsed.1, "general-purpose");
        assert!(!VALID_AGENT_TYPES.iter().any(|v| *v == parsed.1));
    }
}
