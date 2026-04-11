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
                if !name.starts_with(".tmp-subtask-") || !name.ends_with(".md") {
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

async fn dispatch_agent(handoff: Handoff, cmd: String) -> (HandoffResult, u32) {
    let path = handoff.prompt_file.to_string_lossy().into_owned();
    let can_fail = handoff.can_fail;

    // Verify the prompt file exists before attempting to dispatch.
    if !handoff.prompt_file.exists() {
        return (HandoffResult {
            index: handoff.index,
            stdout: String::new(),
            stderr: format!("prompt file not found: {}", path),
            success: false,
            can_fail,
        }, 0);
    }

    let (program, mut args) = crate::config::Config::parse_cmd(&cmd);
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

    let child = match child_result {
        Ok(c) => c,
        Err(e) => return (HandoffResult {
            index: handoff.index,
            stdout: String::new(),
            stderr: format!("failed to spawn agent: {}", e),
            success: false,
            can_fail,
        }, 0),
    };

    let pgid = child.id().unwrap_or(0);

    let output = child.wait_with_output().await;

    let result = match output {
        Ok(out) => HandoffResult {
            index: handoff.index,
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            success: out.status.success(),
            can_fail,
        },
        Err(e) => HandoffResult {
            index: handoff.index,
            stdout: String::new(),
            stderr: format!("failed waiting for agent: {}", e),
            success: false,
            can_fail,
        },
    };
    (result, pgid)
}

/// Dispatches all handoffs in a batch concurrently. Returns results sorted by index and PGIDs.
pub async fn dispatch_all(
    handoffs: Vec<Handoff>,
    claude_cmd: &str,
    codex_cmd: &str,
    gemini_cmd: &str,
) -> (Vec<HandoffResult>, Vec<u32>) {
    let handles: Vec<_> = handoffs.into_iter()
        .map(|h| {
            let cmd = match h.agent_type {
                AgentType::Claude => claude_cmd.to_string(),
                AgentType::Codex  => codex_cmd.to_string(),
                AgentType::Gemini => gemini_cmd.to_string(),
            };
            tokio::spawn(dispatch_agent(h, cmd))
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

    let (program, mut base_args) = crate::config::Config::parse_cmd(main_cmd);
    base_args.extend_from_slice(&[
        "--resume".to_string(),
        session_id.to_string(),
        "-p".to_string(),
        continuation.to_string(),
    ]);

    let mut child = {
        let mut cmd = tokio::process::Command::new(&program);
        cmd.args(&base_args)
           .stdout(std::process::Stdio::piped())
           .stderr(std::process::Stdio::null());
        #[cfg(unix)]
        cmd.process_group(0);
        cmd.spawn()?
    };

    let resume_pgid = child.id().unwrap_or(0);

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
        let mut saw_handoff_call = false;
        while let Ok(Some(line)) = reader.next_line().await {
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
                    saw_handoff_call = true;
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

        // Check for another handoff pause (check repo root AND worktree paths).
        if let Some(state_file) = crate::executor::find_state_file(&execution_root) {
            let _ = tx.send(ExecEvent::HandoffRequired {
                session_id: resumed_session_id,
                state_file,
            }).await;
            return;
        } else if saw_handoff_call {
            // Relaxed fallback: state file may exist but lack the handoffs
            // array (protocol drift after many resumes). load_state has
            // auto-detection from co-located .tmp-subtask-*.md files.
            if let Some(relaxed_file) = crate::executor::find_state_file_any(&execution_root) {
                tracing::warn!("resume: state file exists at {:?} but has no pending handoffs — using auto-detection fallback", relaxed_file);
                let warn = format!(
                    "⏺ [plan-executor] state file missing handoffs array — falling back to prompt-file auto-detection ({})",
                    relaxed_file.display()
                );
                if let Some(ref mut f) = disp_file {
                    let _ = f.write_all(format!("{}\n", warn).as_bytes()).await;
                }
                let _ = tx.send(ExecEvent::DisplayLine(warn)).await;
                let _ = tx.send(ExecEvent::HandoffRequired {
                    session_id: resumed_session_id,
                    state_file: relaxed_file,
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
}
