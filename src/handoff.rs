use std::path::{Path, PathBuf};
use tokio::process::Command;
use tokio::sync::mpsc;


// ── Types ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum AgentType {
    Claude,
    Codex,
    Gemini,
    Bash,
}

#[derive(Debug, Clone)]
pub struct Handoff {
    pub index: usize,
    pub agent_type: AgentType,
    pub prompt_file: PathBuf,
    /// `canFail: true` in state file / `can-fail: true` on handoff line.
    /// A can-fail agent that exits non-zero does NOT fail the job; its slot
    /// receives an error-annotated block so the batch stays structurally complete.
    pub can_fail: bool,
}

#[derive(Debug, Clone)]
pub struct HandoffResult {
    pub index: usize,
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
    /// Mirrors `Handoff::can_fail` so callers can decide whether to propagate failures.
    pub can_fail: bool,
}

/// One streamed line from a dispatched sub-agent. The daemon uses these
/// both as a watchdog liveness signal (stamp `job_last_activity` on each
/// line) and as the raw feed for per-sub-agent output files the
/// `plan-executor output` command renders inline.
#[derive(Debug, Clone)]
pub struct SubAgentLine {
    pub index: usize,
    /// Short agent-type label used in persisted filenames so the
    /// renderer can pick the right rendering path (stream-json vs. raw).
    pub agent_type: &'static str,
    pub is_stderr: bool,
    pub line: String,
}

// ── Sub-agent dispatch ─────────────────────────────────────────────────────

/// Dispatches a single agent. For bash agents with a `live_tx`, stdout/stderr
/// are streamed line-by-line through the channel for live display instead of
/// being captured silently.
/// Injects the per-agent flag that turns on line-by-line JSONL output so
/// the daemon watchdog can observe true sub-agent liveness. No-op when
/// the flag is already present (operator override via config). Called by
/// `dispatch_agent` before the prompt-file path is appended to `args`.
///
/// Placement is agent-specific:
///  * Claude / Gemini — `--output-format stream-json` is a top-level flag,
///    inserted before `-p`/`--prompt` (which consumes the next arg).
///    Claude additionally needs `--verbose` to emit full events under
///    stream-json.
///  * Codex           — `--json` is a subcommand flag on `exec`, appended
///    after the existing template so it sits right before the prompt
///    path.
///  * Bash            — no-op; bash sub-agents already stream raw stdout.
fn inject_streaming_flags(agent_type: &AgentType, args: &mut Vec<String>) {
    match agent_type {
        AgentType::Claude => {
            let has_output_format = args.iter().any(|a| a == "--output-format");
            let has_verbose = args.iter().any(|a| a == "--verbose");
            // Insert before any `-p`/`--prompt` so it doesn't get consumed
            // by the prompt flag as its value.
            let pos = args
                .iter()
                .position(|a| a == "-p" || a == "--prompt")
                .unwrap_or(args.len());
            let mut insert_at = pos;
            if !has_output_format {
                args.insert(insert_at, "--output-format".to_string());
                insert_at += 1;
                args.insert(insert_at, "stream-json".to_string());
                insert_at += 1;
            }
            if !has_verbose {
                args.insert(insert_at, "--verbose".to_string());
            }
        }
        AgentType::Codex => {
            if !args.iter().any(|a| a == "--json") {
                args.push("--json".to_string());
            }
        }
        AgentType::Gemini => {
            let has_output_format = args
                .iter()
                .any(|a| a == "-o" || a == "--output-format");
            if !has_output_format {
                let pos = args
                    .iter()
                    .position(|a| a == "-p" || a == "--prompt")
                    .unwrap_or(args.len());
                args.insert(pos, "stream-json".to_string());
                args.insert(pos, "-o".to_string());
            }
        }
        AgentType::Bash => {}
    }
}

/// Builds the final positional argument passed to a sub-agent CLI.
///
/// For bash the script file itself is executed, so we pass the path
/// unchanged. For LLM sub-agents (claude / codex / gemini) the CLI
/// treats the positional argument as the user prompt string — sending
/// a bare file path forces the model to guess its identity from a
/// filename, which has caused sub-agents to load
/// `plan-executor:execute-plan` and recurse into the orchestrator flow.
/// We wrap the path in a one-line framing that establishes sub-agent
/// identity before the model fires any Skill discovery.
fn sub_agent_prompt_arg(agent_type: &AgentType, path: &str) -> String {
    match agent_type {
        AgentType::Bash => path.to_string(),
        _ => format!(
            "Sub-agent task: read and execute the instructions in {}. You are a plan-executor sub-agent — do NOT invoke plan-executor:* skills; they are orchestrator-only.",
            path
        ),
    }
}

/// Scans a JSONL stream (Claude `--output-format stream-json`, Codex
/// `--json`, Gemini `-o stream-json`) for the agent's terminal result
/// message and returns its text. Returns `None` when no recognizable
/// result event is found — caller should fall back to the raw stdout
/// concatenation.
///
/// Recognized shapes (newest-to-oldest, first match wins):
///  * Claude / Gemini stream-json — `{"type":"result","result":"..."}`.
///  * Codex `--json`              — `{"msg":{"type":"agent_message",
///    "message":"..."}}`. The final agent_message event contains the
///    model's last text response.
/// Canonical marker string the daemon looks for to decide whether a
/// prompt file already carries the subprocess-hygiene block. Must match
/// the block emitted by both `ensure_hygiene_in_prompt` below and the
/// plugin SKILL.md copies so we don't duplicate a block the orchestrator
/// correctly emitted.
const HYGIENE_MARKER: &str = "Subprocess hygiene (MANDATORY";

/// Subprocess-hygiene block prepended to every non-bash sub-agent
/// prompt at dispatch time. Wrapped in a Markdown blockquote and
/// preceded by a "Sub-Agent Instructions" banner so it reads as
/// framing rather than body content. Uses the same 4-rule wording the
/// plugin SKILL.md copies carry.
const HYGIENE_BLOCK: &str = concat!(
    "> **Sub-Agent Instructions (plan-executor enforced — do not remove):**\n",
    ">\n",
    "> **Subprocess hygiene (MANDATORY — the daemon watchdog kills the job after prolonged silence).**\n",
    ">\n",
    "> Any Bash command that starts a long-running or backgrounded process MUST follow these rules:\n",
    "> 1. Wrap every invocation in `timeout N` (N ≤ 600 seconds). Example: `timeout 120 ./run-tests`.\n",
    "> 2. Never call bare `wait \"$PID\"` on a backgrounded process. Use `timeout N wait \"$PID\"` or a bounded `kill -0 \"$PID\"` poll with a max iteration count instead.\n",
    "> 3. Escalate signals on cleanup: `kill -TERM \"$PID\" 2>/dev/null; sleep 1; kill -KILL \"$PID\" 2>/dev/null || true`. `SIGTERM` alone may be ignored.\n",
    "> 4. Before exiting any script that spawned children, reap the group: `pkill -P $$ 2>/dev/null || true`.\n",
    "\n---\n\n",
);

/// Prepends the canonical hygiene block to a sub-agent prompt if it
/// isn't already present. Idempotent via `HYGIENE_MARKER`. Silent on
/// read/write failures — the block is a defense-in-depth safeguard, so
/// an I/O hiccup shouldn't block dispatch; the in-daemon watchdog and
/// terminal-result grace kill still cover the worst case.
///
/// If the first non-empty line is a slash-command invocation
/// (e.g. the security reviewer's `/security:big-toni`), the block is
/// inserted immediately after that line so the slash invocation stays
/// in its conventional first-line position. Otherwise the block is
/// prepended to the very top of the file.
fn ensure_hygiene_in_prompt(path: &Path) {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "hygiene inject: read failed");
            return;
        }
    };
    if content.contains(HYGIENE_MARKER) {
        return;
    }
    let new = inject_hygiene_block(&content);
    if let Err(e) = std::fs::write(path, new) {
        tracing::warn!(path = %path.display(), error = %e, "hygiene inject: write failed");
    }
}

/// Pure prepend helper split out for unit-testing. Returns a new string
/// with `HYGIENE_BLOCK` placed near the top of `content`, preserving
/// a leading slash-command invocation if present.
fn inject_hygiene_block(content: &str) -> String {
    let (head, body) = split_leading_slash_invocation(content);
    let mut out = String::with_capacity(content.len() + HYGIENE_BLOCK.len() + 1);
    if !head.is_empty() {
        out.push_str(head);
        if !head.ends_with('\n') {
            out.push('\n');
        }
    }
    out.push_str(HYGIENE_BLOCK);
    out.push_str(body);
    out
}

/// If the prompt opens with a slash-command line (after optional
/// leading blank lines), returns `(head, body)` where `head` is that
/// line (including its trailing newline if present) and `body` is the
/// remainder. Otherwise returns `("", content)`.
fn split_leading_slash_invocation(content: &str) -> (&str, &str) {
    let trimmed_start = content.trim_start_matches('\n');
    let offset = content.len() - trimmed_start.len();
    let first_line_end = trimmed_start.find('\n').map(|e| e + 1).unwrap_or(trimmed_start.len());
    let first_line = &trimmed_start[..first_line_end];
    if first_line.trim_start().starts_with('/') {
        let split_at = offset + first_line_end;
        (&content[..split_at], &content[split_at..])
    } else {
        ("", content)
    }
}

/// Checks a single JSONL line for the terminal "agent is done"
/// signature. Mirrors the match set in `extract_result_text` but returns
/// a `bool` so the streaming reader can cheaply flag completion without
/// collecting text.
fn is_terminal_result_line(line: &str) -> bool {
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(line) else {
        return false;
    };
    if parsed.get("type").and_then(|v| v.as_str()) == Some("result")
        && parsed.get("result").and_then(|v| v.as_str()).is_some()
    {
        return true;
    }
    if let Some(msg) = parsed.get("msg") {
        if msg.get("type").and_then(|v| v.as_str()) == Some("agent_message")
            && msg.get("message").and_then(|v| v.as_str()).is_some()
        {
            return true;
        }
    }
    false
}

pub(crate) fn extract_result_text(lines: &[String]) -> Option<String> {
    for line in lines.iter().rev() {
        let parsed: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        // Claude / Gemini stream-json terminal event.
        if parsed.get("type").and_then(|v| v.as_str()) == Some("result") {
            if let Some(text) = parsed.get("result").and_then(|v| v.as_str()) {
                return Some(text.to_string());
            }
        }
        // Codex --json terminal event.
        if let Some(msg) = parsed.get("msg") {
            if msg.get("type").and_then(|v| v.as_str()) == Some("agent_message") {
                if let Some(text) = msg.get("message").and_then(|v| v.as_str()) {
                    return Some(text.to_string());
                }
            }
        }
    }
    None
}

async fn dispatch_agent(
    handoff: Handoff,
    cmd: String,
    live_tx: Option<mpsc::Sender<(usize, String)>>,
    pgid_tx: Option<mpsc::UnboundedSender<u32>>,
    subagent_tx: Option<mpsc::UnboundedSender<SubAgentLine>>,
) -> (HandoffResult, u32) {
    let path = handoff.prompt_file.to_string_lossy().into_owned();
    let can_fail = handoff.can_fail;
    let index = handoff.index;
    let is_bash = matches!(handoff.agent_type, AgentType::Bash);
    let agent_type_label: &'static str = match handoff.agent_type {
        AgentType::Claude => "claude",
        AgentType::Codex  => "codex",
        AgentType::Gemini => "gemini",
        AgentType::Bash   => "bash",
    };

    // Verify the prompt file exists before attempting to dispatch.
    if !handoff.prompt_file.exists() {
        return (HandoffResult {
            index,
            stdout: String::new(),
            stderr: format!("prompt file not found: {}", path),
            success: false,
            can_fail,
        }, 0);
    }

    // Defense-in-depth: guarantee every non-bash sub-agent prompt
    // carries the subprocess-hygiene rule even if the orchestrator LLM
    // forgot to emit it. Bash handoffs are shell scripts — injecting
    // markdown into them would break execution, so we skip bash here.
    if !is_bash {
        ensure_hygiene_in_prompt(&handoff.prompt_file);
    }

    let (program, mut args) = crate::config::Config::parse_cmd(&cmd);
    inject_streaming_flags(&handoff.agent_type, &mut args);
    args.push(sub_agent_prompt_arg(&handoff.agent_type, &path));

    let child_result = {
        let mut c = Command::new(&program);
        c.args(&args);
        #[cfg(unix)]
        c.process_group(0);
        c.stdout(std::process::Stdio::piped())
         .stderr(std::process::Stdio::piped())
         .spawn()
    };

    let mut child = match child_result {
        Ok(c) => c,
        Err(e) => return (HandoffResult {
            index,
            stdout: String::new(),
            stderr: format!("failed to spawn agent: {}", e),
            success: false,
            can_fail,
        }, 0),
    };

    let pgid = child.id().unwrap_or(0);

    // Register the pgid with the daemon immediately — BEFORE awaiting the
    // child's output. This is the only way a KillJob arriving while
    // dispatch_all is still suspended can SIGKILL this sub-agent's
    // process group. A channel send is non-blocking (unbounded), so there
    // is no await between spawn and registration.
    if let Some(ref tx) = pgid_tx {
        if pgid > 0 {
            let _ = tx.send(pgid);
        }
    }

    // Bash agents with a live channel: stream stdout/stderr in real-time
    // for display. The continuation payload for bash is just the exit
    // status.
    if is_bash && live_tx.is_some() {
        let tx = live_tx.unwrap();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let tx_out = tx.clone();
        let sa_out = subagent_tx.clone();
        let stdout_handle = tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut lines = Vec::new();
            if let Some(out) = stdout {
                let mut reader = BufReader::new(out).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    if let Some(ref s) = sa_out {
                        let _ = s.send(SubAgentLine {
                            index,
                            agent_type: agent_type_label,
                            is_stderr: false,
                            line: line.clone(),
                        });
                    }
                    let _ = tx_out.send((index, line.clone())).await;
                    lines.push(line);
                }
            }
            lines.join("\n")
        });

        let tx_err = tx;
        let sa_err = subagent_tx.clone();
        let stderr_handle = tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut lines = Vec::new();
            if let Some(err) = stderr {
                let mut reader = BufReader::new(err).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    if let Some(ref s) = sa_err {
                        let _ = s.send(SubAgentLine {
                            index,
                            agent_type: agent_type_label,
                            is_stderr: true,
                            line: line.clone(),
                        });
                    }
                    let _ = tx_err.send((index, line.clone())).await;
                    lines.push(line);
                }
            }
            lines.join("\n")
        });

        let status = child.wait().await;
        let stdout_str = stdout_handle.await.unwrap_or_default();
        let stderr_str = stderr_handle.await.unwrap_or_default();
        let success = status.map(|s| s.success()).unwrap_or(false);

        return (HandoffResult { index, stdout: stdout_str, stderr: stderr_str, success, can_fail }, pgid);
    }

    // Non-bash JSONL streaming path. Every stdout/stderr line is read as
    // it arrives so the daemon can stamp `job_last_activity` per tick.
    //
    // Critically, the sub-agent process doesn't always exit when it
    // emits its terminal result: claude CLI spawns Bash-tool commands
    // with `setpgid`, and a hung shell script (e.g. stuck on
    // `wait $PROXY_PID`) keeps claude's stdout pipe open via inherited
    // fds, so `child.wait()` would block forever even though the agent's
    // work is done. To unblock the main-agent resume, we detect the
    // terminal `result` event as it streams by, start a short grace
    // timer, then force-kill the entire descendant process tree. The
    // result text is already captured in `stdout_lines` before the kill,
    // so the continuation payload is unaffected.
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let child_pid = child.id().unwrap_or(0);

    let terminal_notify = std::sync::Arc::new(tokio::sync::Notify::new());
    let notify_reader = std::sync::Arc::clone(&terminal_notify);

    let sa_out = subagent_tx.clone();
    let stdout_handle = tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let mut lines: Vec<String> = Vec::new();
        let mut notified = false;
        if let Some(out) = stdout {
            let mut reader = BufReader::new(out).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if let Some(ref s) = sa_out {
                    let _ = s.send(SubAgentLine {
                        index,
                        agent_type: agent_type_label,
                        is_stderr: false,
                        line: line.clone(),
                    });
                }
                if !notified && is_terminal_result_line(&line) {
                    notified = true;
                    notify_reader.notify_one();
                }
                lines.push(line);
            }
        }
        lines
    });

    let sa_err = subagent_tx.clone();
    let stderr_handle = tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let mut lines: Vec<String> = Vec::new();
        if let Some(err) = stderr {
            let mut reader = BufReader::new(err).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if let Some(ref s) = sa_err {
                    let _ = s.send(SubAgentLine {
                        index,
                        agent_type: agent_type_label,
                        is_stderr: true,
                        line: line.clone(),
                    });
                }
                lines.push(line);
            }
        }
        lines
    });

    let status = {
        let terminal_fired = async {
            terminal_notify.notified().await;
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        };
        tokio::pin!(terminal_fired);
        tokio::select! {
            biased;
            s = child.wait() => s,
            _ = &mut terminal_fired => {
                // Agent emitted its result event but is still alive —
                // almost always a Bash-tool grand-child deadlock holding
                // stdout open. Force-kill the whole descendant pgroup
                // tree so our streams close and we can move on.
                tracing::warn!(
                    agent = %agent_type_label, index,
                    "sub-agent emitted terminal result but did not exit; force-killing tree",
                );
                crate::proctree::kill_descendant_pgids(child_pid);
                let _ = child.kill().await;
                child.wait().await
            }
        }
    };
    let stdout_lines = stdout_handle.await.unwrap_or_default();
    let stderr_lines = stderr_handle.await.unwrap_or_default();
    let success = status.map(|s| s.success()).unwrap_or(false);

    let stdout_str = extract_result_text(&stdout_lines)
        .unwrap_or_else(|| stdout_lines.join("\n"));
    let stderr_str = stderr_lines.join("\n");

    (HandoffResult { index, stdout: stdout_str, stderr: stderr_str, success, can_fail }, pgid)
}

/// Dispatches all handoffs in a batch concurrently. Returns results sorted by index and PGIDs.
///
/// `live_tx` streams (agent_index, line) for bash agents' live output to
/// the daemon's display buffer.
/// `pgid_tx` (if provided) receives each sub-agent's pgid the instant it is
/// spawned — before the await on child output — so an in-flight KillJob
/// can SIGKILL the sub-agent process groups.
/// `subagent_tx` (if provided) receives a `SubAgentLine` per stdout/stderr
/// line from any sub-agent. The daemon uses this both as the watchdog
/// liveness signal (true sub-agent progress, not a blind timer) and as
/// the feed for per-sub-agent output files rendered inline by
/// `plan-executor output`.
pub async fn dispatch_all(
    handoffs: Vec<Handoff>,
    claude_cmd: &str,
    codex_cmd: &str,
    gemini_cmd: &str,
    bash_cmd: &str,
    live_tx: Option<mpsc::Sender<(usize, String)>>,
    pgid_tx: Option<mpsc::UnboundedSender<u32>>,
    subagent_tx: Option<mpsc::UnboundedSender<SubAgentLine>>,
) -> (Vec<HandoffResult>, Vec<u32>) {
    let handles: Vec<_> = handoffs.into_iter()
        .map(|h| {
            let cmd = match h.agent_type {
                AgentType::Claude => claude_cmd.to_string(),
                AgentType::Codex  => codex_cmd.to_string(),
                AgentType::Gemini => gemini_cmd.to_string(),
                AgentType::Bash   => bash_cmd.to_string(),
            };
            let tx = if matches!(h.agent_type, AgentType::Bash) { live_tx.clone() } else { None };
            let pg_tx = pgid_tx.clone();
            let sa_tx = subagent_tx.clone();
            tokio::spawn(dispatch_agent(h, cmd, tx, pg_tx, sa_tx))
        })
        .collect();
    let mut results = Vec::new();
    let mut pgids = Vec::new();
    for handle in handles {
        if let Ok((r, pgid)) = handle.await {
            results.push(r);
            pgids.push(pgid);
        }
    }
    results.sort_by_key(|r| r.index);
    (results, pgids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inject_streaming_flags_claude_inserts_before_prompt() {
        let mut args = vec![
            "--dangerously-skip-permissions".to_string(),
            "-p".to_string(),
        ];
        inject_streaming_flags(&AgentType::Claude, &mut args);
        assert_eq!(
            args,
            vec![
                "--dangerously-skip-permissions",
                "--output-format",
                "stream-json",
                "--verbose",
                "-p",
            ]
        );
    }

    #[test]
    fn inject_streaming_flags_claude_is_idempotent() {
        let mut args = vec![
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
            "-p".to_string(),
        ];
        let before = args.clone();
        inject_streaming_flags(&AgentType::Claude, &mut args);
        assert_eq!(args, before);
    }

    #[test]
    fn inject_streaming_flags_codex_appends_json() {
        let mut args = vec![
            "--dangerously-bypass-approvals-and-sandbox".to_string(),
            "exec".to_string(),
        ];
        inject_streaming_flags(&AgentType::Codex, &mut args);
        assert_eq!(
            args,
            vec![
                "--dangerously-bypass-approvals-and-sandbox",
                "exec",
                "--json",
            ]
        );
    }

    #[test]
    fn inject_streaming_flags_gemini_inserts_before_prompt() {
        let mut args = vec!["--yolo".to_string(), "-p".to_string()];
        inject_streaming_flags(&AgentType::Gemini, &mut args);
        assert_eq!(args, vec!["--yolo", "-o", "stream-json", "-p"]);
    }

    #[test]
    fn inject_streaming_flags_bash_is_noop() {
        let mut args = vec!["-c".to_string()];
        let before = args.clone();
        inject_streaming_flags(&AgentType::Bash, &mut args);
        assert_eq!(args, before);
    }

    #[test]
    fn sub_agent_prompt_arg_wraps_llm_path_with_identity_frame() {
        let arg = sub_agent_prompt_arg(&AgentType::Claude, "/tmp/.tmp-subtask-1.md");
        assert!(arg.contains("/tmp/.tmp-subtask-1.md"));
        assert!(arg.contains("Sub-agent task"));
        assert!(arg.contains("plan-executor sub-agent"));
        assert!(arg.contains("do NOT invoke plan-executor:"));
        // Same framing applies to codex / gemini.
        assert_eq!(
            arg,
            sub_agent_prompt_arg(&AgentType::Codex, "/tmp/.tmp-subtask-1.md"),
        );
        assert_eq!(
            arg,
            sub_agent_prompt_arg(&AgentType::Gemini, "/tmp/.tmp-subtask-1.md"),
        );
    }

    #[test]
    fn sub_agent_prompt_arg_passes_bash_path_through_unchanged() {
        let arg = sub_agent_prompt_arg(&AgentType::Bash, "/tmp/script.sh");
        assert_eq!(arg, "/tmp/script.sh");
    }

    #[test]
    fn extract_result_text_finds_claude_result_event() {
        let lines = vec![
            r#"{"type":"system","subtype":"init"}"#.to_string(),
            r#"{"type":"assistant","message":{"content":[]}}"#.to_string(),
            r#"{"type":"result","subtype":"success","result":"final text"}"#.to_string(),
        ];
        assert_eq!(extract_result_text(&lines), Some("final text".to_string()));
    }

    #[test]
    fn extract_result_text_finds_codex_agent_message() {
        let lines = vec![
            r#"{"id":"1","msg":{"type":"some_other"}}"#.to_string(),
            r#"{"id":"2","msg":{"type":"agent_message","message":"codex final"}}"#.to_string(),
        ];
        assert_eq!(extract_result_text(&lines), Some("codex final".to_string()));
    }

    #[test]
    fn extract_result_text_prefers_last_result_event() {
        // Scans newest-first; the last line wins.
        let lines = vec![
            r#"{"type":"result","result":"first"}"#.to_string(),
            r#"{"type":"result","result":"second"}"#.to_string(),
        ];
        assert_eq!(extract_result_text(&lines), Some("second".to_string()));
    }

    #[test]
    fn extract_result_text_returns_none_on_no_recognized_event() {
        let lines = vec![
            r#"{"type":"assistant","message":{"content":"nope"}}"#.to_string(),
            r#"plain text not json"#.to_string(),
        ];
        assert_eq!(extract_result_text(&lines), None);
    }

    #[test]
    fn is_terminal_result_line_detects_claude_result() {
        assert!(is_terminal_result_line(
            r#"{"type":"result","subtype":"success","result":"done"}"#
        ));
    }

    #[test]
    fn is_terminal_result_line_detects_codex_agent_message() {
        assert!(is_terminal_result_line(
            r#"{"id":"1","msg":{"type":"agent_message","message":"all good"}}"#
        ));
    }

    #[test]
    fn is_terminal_result_line_rejects_result_without_text() {
        // A result event without a string `result` field is not usable
        // as a terminal signal.
        assert!(!is_terminal_result_line(
            r#"{"type":"result","subtype":"error"}"#
        ));
    }

    #[test]
    fn is_terminal_result_line_rejects_other_events() {
        assert!(!is_terminal_result_line(
            r#"{"type":"assistant","message":{"content":"mid"}}"#
        ));
        assert!(!is_terminal_result_line(""));
        assert!(!is_terminal_result_line("not json at all"));
    }

    #[test]
    fn ensure_hygiene_in_prompt_prepends_when_missing() {
        let tmp = std::env::temp_dir().join(format!(
            "plan-executor-hygiene-missing-{}.md",
            std::process::id()
        ));
        std::fs::write(&tmp, "Original prompt body\nsecond line").unwrap();
        ensure_hygiene_in_prompt(&tmp);
        let updated = std::fs::read_to_string(&tmp).unwrap();
        // Block is at the top (blockquote banner precedes the body).
        assert!(updated.starts_with("> **Sub-Agent Instructions"));
        assert!(updated.contains(HYGIENE_MARKER));
        assert!(updated.contains("Original prompt body"));
        // Marker comes before body.
        let marker_pos = updated.find(HYGIENE_MARKER).unwrap();
        let body_pos = updated.find("Original prompt body").unwrap();
        assert!(marker_pos < body_pos);
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn ensure_hygiene_in_prompt_is_idempotent() {
        let tmp = std::env::temp_dir().join(format!(
            "plan-executor-hygiene-idempotent-{}.md",
            std::process::id()
        ));
        let original = format!(
            "Prompt header\n\n**{}** some body text\nmore\n",
            HYGIENE_MARKER
        );
        std::fs::write(&tmp, &original).unwrap();
        ensure_hygiene_in_prompt(&tmp);
        let after = std::fs::read_to_string(&tmp).unwrap();
        // File untouched because marker was already present.
        assert_eq!(after, original);
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn inject_hygiene_block_preserves_slash_command_first_line() {
        let prompt = "/security:big-toni review the repo\n\nRest of prompt.\n";
        let out = inject_hygiene_block(prompt);
        // Slash command stays on line 1.
        assert!(out.starts_with("/security:big-toni review the repo\n"));
        // Block comes immediately after.
        let first_newline = out.find('\n').unwrap();
        assert!(out[first_newline..].starts_with("\n> **Sub-Agent Instructions"));
        // Body is after the block.
        assert!(out.contains("Rest of prompt."));
    }

    #[test]
    fn inject_hygiene_block_prepends_when_no_slash_line() {
        let prompt = "You are a focused agent. Do X.\n";
        let out = inject_hygiene_block(prompt);
        assert!(out.starts_with("> **Sub-Agent Instructions"));
        assert!(out.contains("You are a focused agent. Do X."));
    }

    #[test]
    fn split_leading_slash_invocation_detects_slash() {
        let (head, body) = split_leading_slash_invocation("/foo:bar arg\nrest\n");
        assert_eq!(head, "/foo:bar arg\n");
        assert_eq!(body, "rest\n");
    }

    #[test]
    fn split_leading_slash_invocation_ignores_non_slash() {
        let (head, body) = split_leading_slash_invocation("You are a reviewer.\nrest\n");
        assert_eq!(head, "");
        assert_eq!(body, "You are a reviewer.\nrest\n");
    }

    #[test]
    fn split_leading_slash_invocation_skips_blank_lines() {
        let (head, body) = split_leading_slash_invocation("\n\n/foo:bar\nrest\n");
        assert_eq!(head, "\n\n/foo:bar\n");
        assert_eq!(body, "rest\n");
    }
}
