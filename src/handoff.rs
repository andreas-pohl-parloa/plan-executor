use std::path::{Path, PathBuf};
use serde::Deserialize;
use tokio::process::Command;
use tokio::sync::mpsc;
use anyhow::Result;

use crate::executor::ExecEvent;
use crate::jobs::{JobMetadata, JobStatus};

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
pub struct HandoffState {
    pub phase: String,
    pub handoffs: Vec<Handoff>,
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

// ── State file deserialization ─────────────────────────────────────────────
//
// Two schemas are supported:
//   Spec schema  — { "phase": "<string>", "handoffs": [{ "agentType", "promptFile", ... }] }
//   Actual skill — { "current_phase": <int>, "expected_handoffs": [{ "prompt_file", ... }] }

#[derive(Deserialize, Default)]
struct RawHandoff {
    #[serde(default)]
    index: usize,
    /// camelCase from spec; absent in actual skill output (defaults to "claude")
    #[serde(rename = "agentType", default)]
    agent_type: String,
    /// snake_case (actual) or camelCase alias (spec)
    #[serde(alias = "promptFile")]
    prompt_file: String,
    /// Protocol §3/§5: `canFail: true` marks this agent as optional.
    #[serde(rename = "canFail", alias = "can_fail", default)]
    can_fail: bool,
}

#[derive(Deserialize, Default)]
struct RawState {
    /// Spec: string phase name
    #[serde(default)]
    phase: String,
    /// Actual skill: integer or string phase counter
    #[serde(default, deserialize_with = "deserialize_phase")]
    current_phase: String,
    /// Spec field name
    #[serde(default)]
    handoffs: Vec<RawHandoff>,
    /// Actual skill field name
    #[serde(default)]
    expected_handoffs: Vec<RawHandoff>,
}

fn deserialize_phase<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<String, D::Error> {
    let val: serde_json::Value = serde::Deserialize::deserialize(d)?;
    Ok(match val {
        serde_json::Value::String(s) => s,
        serde_json::Value::Number(n) => n.to_string(),
        _ => String::new(),
    })
}

/// Reads `.tmp-execute-plan-state.json` and returns parsed `HandoffState`.
/// Relative `prompt_file` paths are resolved against the state file's directory.
pub fn load_state(state_file: &Path) -> Result<HandoffState> {
    let content = std::fs::read_to_string(state_file)?;
    let raw: RawState = serde_json::from_str(&content)?;

    let base_dir = state_file.parent().unwrap_or(Path::new("."));

    // Accept either field name; prefer the non-empty one.
    let phase = if !raw.phase.is_empty() {
        raw.phase.clone()
    } else if !raw.current_phase.is_empty() {
        raw.current_phase.clone()
    } else {
        "unknown".to_string()
    };

    let raw_handoffs = if !raw.expected_handoffs.is_empty() {
        raw.expected_handoffs
    } else {
        raw.handoffs
    };

    let mut handoffs: Vec<Handoff> = raw_handoffs
        .into_iter()
        .filter_map(|h| {
            let agent_type = match h.agent_type.as_str() {
                "codex"  => AgentType::Codex,
                "gemini" => AgentType::Gemini,
                "bash"   => AgentType::Bash,
                other => {
                    if !other.is_empty() && other != "claude" {
                        tracing::warn!("unknown agent-type '{}', defaulting to claude", other);
                    }
                    AgentType::Claude
                }
            };
            let pf = PathBuf::from(&h.prompt_file);
            // Resolve relative paths against base_dir; accept absolute if they
            // stay within base_dir (the skill writes absolute paths).
            let prompt_file = if pf.is_absolute() { pf } else { base_dir.join(pf) };
            let canonical = prompt_file.canonicalize().unwrap_or_else(|_| prompt_file.clone());
            let canonical_base = base_dir.canonicalize().unwrap_or_else(|_| base_dir.to_path_buf());
            if !canonical.starts_with(&canonical_base) {
                tracing::warn!("load_state: rejecting prompt_file that escapes base_dir: {}", canonical.display());
                return None;
            }
            Some(Handoff { index: h.index, agent_type, prompt_file, can_fail: h.can_fail })
        })
        .collect();

    // If the state file listed no handoffs, auto-detect from co-located
    // `.tmp-subtask-*.md` prompt files. Some skill phases (e.g. code_review)
    // omit the handoffs array and rely on named prompt files instead.
    if handoffs.is_empty() {
        let mut detected: Vec<Handoff> = std::fs::read_dir(base_dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                if !name.starts_with(".tmp-subtask-") {
                    return None;
                }
                if name.ends_with(".sh") {
                    return Some(Handoff { index: 0, agent_type: AgentType::Bash, prompt_file: e.path(), can_fail: false });
                }
                if !name.ends_with(".md") {
                    return None;
                }
                let agent_type = if name.ends_with("-claude.md") { AgentType::Claude }
                    else if name.ends_with("-codex.md")  { AgentType::Codex  }
                    else if name.ends_with("-gemini.md") { AgentType::Gemini }
                    else { AgentType::Claude };
                Some(Handoff { index: 0, agent_type, prompt_file: e.path(), can_fail: false })
            })
            .collect();
        detected.sort_by(|a, b| {
            fn numeric_suffix(p: &std::path::Path) -> u64 {
                p.file_stem()
                    .and_then(|s| s.to_str())
                    .and_then(|s| s.split(|c: char| !c.is_ascii_digit()).rfind(|p| !p.is_empty()))
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0)
            }
            numeric_suffix(&a.prompt_file).cmp(&numeric_suffix(&b.prompt_file))
                .then_with(|| a.prompt_file.cmp(&b.prompt_file))
        });
        for (i, h) in detected.iter_mut().enumerate() { h.index = i + 1; }
        if !detected.is_empty() {
            tracing::info!("auto-detected {} prompt file(s) for phase '{}'", detected.len(), phase);
        }
        handoffs = detected;
    }

    Ok(HandoffState { phase, handoffs })
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

fn extract_result_text(lines: &[String]) -> Option<String> {
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
    args.push(path.clone());

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

// ── Continuation builder ───────────────────────────────────────────────────

/// Builds the `--resume` continuation payload per HANDOFF_PROTOCOL §6.
/// Format: `# output sub-agent N:\n<stdout>\n\n# output sub-agent M:\n<stdout>`
pub fn build_continuation(results: &[HandoffResult]) -> String {
    let mut sorted = results.to_vec();
    sorted.sort_by_key(|r| r.index);
    let mut out = String::new();
    for r in &sorted {
        let body = if r.success {
            r.stdout.clone()
        } else {
            // §7 can-fail: inject error-annotated block to keep batch structurally complete.
            debug_assert!(r.can_fail, "build_continuation called with failed required agent (index={})", r.index);
            let stderr_summary = r.stderr.lines()
                .filter(|l| !l.trim().is_empty())
                .take(3)
                .collect::<Vec<_>>()
                .join("; ");
            format!("[SKIPPED — agent exited non-zero. stderr: {}]",
                if stderr_summary.is_empty() { "(none)".to_string() } else { stderr_summary })
        };
        out.push_str(&format!("# output sub-agent {}:\n{}\n\n", r.index, body));
    }
    out.trim_end().to_string()
}

// ── Resume ─────────────────────────────────────────────────────────────────

/// Resumes the orchestrator session via `claude --resume <session_id> -p <continuation>`.
/// Returns a new (Child, Receiver<ExecEvent>) with the same shape as `spawn_execution`.
pub async fn resume_execution(
    session_id: &str,
    continuation: &str,
    execution_root: PathBuf,
    original_job_id: Option<String>,
    original_plan_path: Option<PathBuf>,
    main_cmd: &str,
) -> Result<(tokio::process::Child, u32, mpsc::Receiver<ExecEvent>)> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    // Derive the output file path from the original job id so resume lines
    // are appended to the same file the initial turn wrote to.
    let output_path = original_job_id.as_deref().map(|id| {
        crate::config::Config::base_dir().join("jobs").join(id).join("output.jsonl")
    });

    // Linux caps a single argv entry at MAX_ARG_STRLEN (32 × page_size =
    // 128 KB on x86_64). Review-phase continuations routinely exceed that
    // when reviewers produce large triage output (>150 KB observed). Pass
    // the continuation via stdin so argv stays tiny and the resume can
    // handle arbitrarily large payloads.
    let (program, mut base_args) = crate::config::Config::parse_cmd(main_cmd);
    base_args.extend_from_slice(&[
        "--resume".to_string(),
        session_id.to_string(),
        "-p".to_string(),
    ]);

    let mut child = {
        let mut cmd = tokio::process::Command::new(&program);
        cmd.args(&base_args)
           .stdin(std::process::Stdio::piped())
           .stdout(std::process::Stdio::piped())
           .stderr(std::process::Stdio::null());
        #[cfg(unix)]
        cmd.process_group(0);
        cmd.spawn()?
    };

    let resume_pgid = child.id().unwrap_or(0);

    // Feed the continuation via stdin and close — claude reads the prompt
    // from stdin when `-p` is passed with no value. Spawn the write as a
    // detached task so the caller can return immediately; we drop any
    // write error because the child will exit-code its own failure.
    let mut child_stdin = child.stdin.take().expect("stdin must be piped");
    let continuation_owned = continuation.to_string();
    tokio::spawn(async move {
        let _ = child_stdin.write_all(continuation_owned.as_bytes()).await;
        let _ = child_stdin.shutdown().await;
        drop(child_stdin);
    });

    let stdout = child.stdout.take().expect("stdout must be piped");
    let (tx, rx) = mpsc::channel::<ExecEvent>(256);
    let session_id_owned = session_id.to_string();

    tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(stdout).lines();
        let mut resumed_session_id = session_id_owned.clone();
        let mut resumed_model: Option<String> = None;
        let mut resumed_duration_ms: Option<u64> = None;
        let mut resumed_input_tokens: Option<u64> = None;
        let mut resumed_output_tokens: Option<u64> = None;
        let mut resumed_failed = false;
        let mut got_result = false;

        // Open the output and display files for appending.
        let mut out_file = if let Some(ref path) = output_path {
            tokio::fs::OpenOptions::new()
                .create(true).append(true).open(path).await.ok()
        } else {
            None
        };
        let mut disp_file = if let Some(ref path) = output_path {
            let dp = path.parent().unwrap_or(path).join("display.log");
            tokio::fs::OpenOptions::new()
                .create(true).append(true).open(dp).await.ok()
        } else {
            None
        };

        let mut last_display_blank = false;
        let mut handoff_calls: Vec<(usize, String, String, bool)> = Vec::new();
        let mut handoff_deadline: Option<tokio::time::Instant> = None;
        let mut protocol_violation = false;
        loop {
            let line_result = match handoff_deadline {
                Some(deadline) => {
                    tokio::select! {
                        biased;
                        _ = tokio::time::sleep_until(deadline) => {
                            protocol_violation = true;
                            crate::executor::kill_pgroup(resume_pgid);
                            break;
                        }
                        r = reader.next_line() => r,
                    }
                }
                None => reader.next_line().await,
            };
            let line = match line_result {
                Ok(Some(l)) => l,
                _ => break,
            };

            // Append raw line to output file (same as initial turn).
            if let Some(ref mut f) = out_file {
                let _ = f.write_all(format!("{}\n", line).as_bytes()).await;
            }
            // Emit sjv-rendered display line (same as initial turn).
            let rendered = sjv::render_runtime_line(&line, false, true);
            for display_line in rendered.lines().filter(|l| !l.is_empty()) {
                let is_blank = crate::executor::is_visually_blank(display_line);
                if is_blank && last_display_blank {
                    continue;
                }
                last_display_blank = is_blank;
                if display_line.contains("call sub-agent") {
                    if let Some(parsed) = crate::executor::parse_handoff_line(display_line) {
                        if parsed.0 == 1 && !handoff_calls.is_empty() {
                            handoff_calls.clear();
                        }
                        handoff_calls.push(parsed);
                        if handoff_deadline.is_none() {
                            handoff_deadline = Some(
                                tokio::time::Instant::now()
                                    + crate::executor::HANDOFF_STOP_GRACE,
                            );
                        }
                    }
                }
                if let Some(ref mut f) = disp_file {
                    let _ = f.write_all(format!("{}\n", display_line).as_bytes()).await;
                }
                let _ = tx.send(ExecEvent::DisplayLine(display_line.to_string())).await;
            }
            let _ = tx.send(ExecEvent::OutputLine(line.clone())).await;

            if let Ok(ev) = serde_json::from_str::<serde_json::Value>(&line) {
                match ev.get("type").and_then(|t| t.as_str()) {
                    Some("system") => {
                        if let Some(sid) = ev.get("session_id").and_then(|s| s.as_str()) {
                            resumed_session_id = sid.to_string();
                        }
                        if let Some(m) = ev.get("model").and_then(|m| m.as_str()) {
                            resumed_model = Some(m.to_string());
                        }
                    }
                    Some("result") => {
                        got_result = true;
                        if let Some(d) = ev.get("duration_ms").and_then(|d| d.as_u64()) {
                            resumed_duration_ms = Some(d);
                        }
                        if let Some(u) = ev.get("usage") {
                            resumed_input_tokens = u.get("input_tokens").and_then(|v| v.as_u64());
                            resumed_output_tokens = u.get("output_tokens").and_then(|v| v.as_u64());
                        }
                        if ev.get("subtype").and_then(|s| s.as_str()) != Some("success") {
                            resumed_failed = true;
                        }
                    }
                    _ => {}
                }
            }
        }

        if protocol_violation {
            let err = format!(
                "⏺ [plan-executor] handoff protocol violation: agent did not stop within {}s of emitting `call sub-agent` lines on the resumed turn. Killing session — the orchestrator kept the turn open and no sub-agents were actually dispatched. Re-run the plan.",
                crate::executor::HANDOFF_STOP_GRACE.as_secs()
            );
            if let Some(ref mut f) = disp_file {
                let _ = f.write_all(format!("{}\n", err).as_bytes()).await;
            }
            let _ = tx.send(ExecEvent::DisplayLine(err)).await;
            resumed_failed = true;
        } else if !handoff_calls.is_empty() {
            // Primary transport: output lines. Same logic as spawn_execution.
            if let Some(state_file) = crate::executor::find_state_file(&execution_root) {
                crate::executor::inject_handoffs_into_state_file(&state_file, &handoff_calls);
                let _ = tx.send(ExecEvent::HandoffRequired {
                    session_id: resumed_session_id,
                    state_file,
                }).await;
                return;
            }
            let err = "⏺ [plan-executor] handoff protocol error: agent requested sub-agent calls but did not write the state file (.tmp-execute-plan-state.json)";
            if let Some(ref mut f) = disp_file {
                let _ = f.write_all(format!("{}\n", err).as_bytes()).await;
            }
            let _ = tx.send(ExecEvent::DisplayLine(err.to_string())).await;
            resumed_failed = true;
        }

        // Execution complete
        let mut placeholder = match (original_job_id, original_plan_path) {
            (Some(id), Some(path)) => {
                let mut m = JobMetadata::new(path);
                m.id = id; // preserve original job id
                m
            }
            (Some(id), None) => {
                tracing::warn!("resume_execution: original_plan_path missing for job {}", id);
                let mut m = JobMetadata::new(PathBuf::from("<resumed>"));
                m.id = id;
                m
            }
            _ => {
                tracing::warn!("resume_execution: original job context missing, creating placeholder");
                JobMetadata::new(PathBuf::from("<resumed>"))
            }
        };
        placeholder.session_id = Some(session_id_owned);
        placeholder.model = resumed_model;
        placeholder.duration_ms = resumed_duration_ms;
        placeholder.input_tokens = resumed_input_tokens;
        placeholder.output_tokens = resumed_output_tokens;
        // No result event = process crashed or was killed before finishing.
        if !got_result {
            resumed_failed = true;
            let err = "⏺ [plan-executor] resume exited without a result event — process crashed or was killed";
            if let Some(ref mut f) = disp_file {
                let _ = f.write_all(format!("{}\n", err).as_bytes()).await;
            }
            let _ = tx.send(ExecEvent::DisplayLine(err.to_string())).await;
        }
        placeholder.status = if resumed_failed { JobStatus::Failed } else { JobStatus::Success };
        placeholder.finished_at = Some(chrono::Utc::now());
        let _ = tx.send(ExecEvent::Finished(placeholder)).await;
    });

    Ok((child, resume_pgid, rx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_continuation_orders_by_index() {
        let results = vec![
            HandoffResult { index: 2, stdout: "out2".to_string(), stderr: String::new(), success: true, can_fail: false },
            HandoffResult { index: 1, stdout: "out1".to_string(), stderr: String::new(), success: true, can_fail: false },
        ];
        let s = build_continuation(&results);
        assert!(s.find("# output sub-agent 1:").unwrap() < s.find("# output sub-agent 2:").unwrap());
        assert!(s.contains("out1"));
        assert!(s.contains("out2"));
    }

    #[test]
    fn build_continuation_includes_empty_stdout_for_failed_agents() {
        let results = vec![
            HandoffResult { index: 1, stdout: String::new(), stderr: "error".to_string(), success: false, can_fail: true },
        ];
        let s = build_continuation(&results);
        assert!(s.contains("# output sub-agent 1:"));
    }

    #[test]
    fn load_state_parses_all_agent_types() {
        // Create a temp dir so prompt files are co-located with the state file
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("state.json");

        // Create the prompt files so canonicalize succeeds
        let p1 = dir.path().join("p1.md");
        let p2 = dir.path().join("p2.md");
        let p3 = dir.path().join("p3.md");
        std::fs::write(&p1, "prompt 1").unwrap();
        std::fs::write(&p2, "prompt 2").unwrap();
        std::fs::write(&p3, "prompt 3").unwrap();

        let json = format!(r#"{{
            "phase": "wave_execution",
            "wave": 1, "attempt": 1, "batch": 1,
            "handoffs": [
                {{"index": 1, "agentType": "claude", "promptFile": "{}"}},
                {{"index": 2, "agentType": "codex",  "promptFile": "{}"}},
                {{"index": 3, "agentType": "gemini", "promptFile": "{}"}}
            ]
        }}"#, p1.display(), p2.display(), p3.display());
        std::fs::write(&state_path, json).unwrap();

        let state = load_state(&state_path).unwrap();
        assert_eq!(state.handoffs.len(), 3);
        assert!(matches!(state.handoffs[0].agent_type, AgentType::Claude));
        assert!(matches!(state.handoffs[1].agent_type, AgentType::Codex));
        assert!(matches!(state.handoffs[2].agent_type, AgentType::Gemini));
    }

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
