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
}

#[derive(Deserialize, Default)]
struct RawState {
    /// Spec: string phase name
    #[serde(default)]
    phase: String,
    /// Actual skill: integer phase counter
    #[serde(default)]
    current_phase: u32,
    /// Spec field name
    #[serde(default)]
    handoffs: Vec<RawHandoff>,
    /// Actual skill field name
    #[serde(default)]
    expected_handoffs: Vec<RawHandoff>,
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
    } else if raw.current_phase > 0 {
        format!("phase-{}", raw.current_phase)
    } else {
        "unknown".to_string()
    };

    let raw_handoffs = if !raw.expected_handoffs.is_empty() {
        raw.expected_handoffs
    } else {
        raw.handoffs
    };

    let handoffs = raw_handoffs
        .into_iter()
        .map(|h| {
            let agent_type = match h.agent_type.as_str() {
                "codex"  => AgentType::Codex,
                "gemini" => AgentType::Gemini,
                // "claude" or absent → Claude
                other => {
                    if !other.is_empty() && other != "claude" {
                        tracing::warn!("unknown agent-type '{}', defaulting to claude", other);
                    }
                    AgentType::Claude
                }
            };
            let pf = PathBuf::from(&h.prompt_file);
            let prompt_file = if pf.is_absolute() { pf } else { base_dir.join(pf) };
            Handoff { index: h.index, agent_type, prompt_file }
        })
        .collect();

    Ok(HandoffState { phase, handoffs })
}

// ── Sub-agent dispatch ─────────────────────────────────────────────────────

async fn dispatch_agent(handoff: Handoff) -> HandoffResult {
    let path = handoff.prompt_file.to_string_lossy().into_owned();

    // Verify the prompt file exists before attempting to dispatch.
    if !handoff.prompt_file.exists() {
        return HandoffResult {
            index: handoff.index,
            stdout: String::new(),
            stderr: format!("prompt file not found: {}", path),
            success: false,
        };
    }

    let output = match &handoff.agent_type {
        AgentType::Claude => Command::new("claude")
            .args(["--dangerously-skip-permissions", "-p", &path])
            .output().await,
        AgentType::Codex => Command::new("codex")
            .args(["--dangerously-bypass-approvals-and-sandbox", "exec", &path])
            .output().await,
        AgentType::Gemini => Command::new("gemini")
            .args(["--yolo", "-p", &path])
            .output().await,
    };

    match output {
        Ok(out) => HandoffResult {
            index: handoff.index,
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            success: out.status.success(),
        },
        Err(e) => HandoffResult {
            index: handoff.index,
            stdout: String::new(),
            stderr: format!("failed to spawn agent: {}", e),
            success: false,
        },
    }
}

/// Dispatches all handoffs in a batch concurrently. Returns results sorted by index.
pub async fn dispatch_all(handoffs: Vec<Handoff>) -> Vec<HandoffResult> {
    let handles: Vec<_> = handoffs.into_iter()
        .map(|h| tokio::spawn(dispatch_agent(h)))
        .collect();
    let mut results = Vec::new();
    for handle in handles {
        if let Ok(r) = handle.await { results.push(r); }
    }
    results.sort_by_key(|r| r.index);
    results
}

// ── Continuation builder ───────────────────────────────────────────────────

/// Builds the `--resume` continuation payload per HANDOFF_PROTOCOL §6.
/// Format: `# output sub-agent N:\n<stdout>\n\n# output sub-agent M:\n<stdout>`
pub fn build_continuation(results: &[HandoffResult]) -> String {
    let mut sorted = results.to_vec();
    sorted.sort_by_key(|r| r.index);
    let mut out = String::new();
    for r in &sorted {
        out.push_str(&format!("# output sub-agent {}:\n{}\n\n", r.index, r.stdout));
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
) -> Result<(tokio::process::Child, mpsc::Receiver<ExecEvent>)> {
    use tokio::io::AsyncBufReadExt;

    let mut child = Command::new("claude")
        .args([
            "--dangerously-skip-permissions",
            "--verbose",
            "--output-format",
            "stream-json",
            "--resume",
            session_id,
            "-p",
            continuation,
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    let stdout = child.stdout.take().expect("stdout must be piped");
    let (tx, rx) = mpsc::channel::<ExecEvent>(256);
    let session_id_owned = session_id.to_string();

    tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(stdout).lines();
        let mut resumed_session_id = session_id_owned.clone();
        let mut resumed_model: Option<String> = None;
        let mut resumed_cost: Option<f64> = None;
        let mut resumed_failed = false;

        while let Ok(Some(line)) = reader.next_line().await {
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
                        if let Some(c) = ev.get("total_cost_usd").and_then(|c| c.as_f64()) {
                            resumed_cost = Some(c);
                        }
                        // Check subtype for failure
                        if ev.get("subtype").and_then(|s| s.as_str()) != Some("success") {
                            resumed_failed = true;
                        }
                    }
                    _ => {}
                }
            }
        }

        // Check for another handoff pause
        let state_file = execution_root.join(".tmp-execute-plan-state.json");
        if state_file.exists() {
            let _ = tx.send(ExecEvent::HandoffRequired {
                session_id: resumed_session_id,
                state_file,
            }).await;
            return;
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
        placeholder.cost_usd = resumed_cost;
        placeholder.status = if resumed_failed { JobStatus::Failed } else { JobStatus::Success };
        placeholder.finished_at = Some(chrono::Utc::now());
        let _ = tx.send(ExecEvent::Finished(placeholder)).await;
    });

    Ok((child, rx))
}

