# Plan Executor Handoff Protocol Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the `execute-plan-non-interactive` HANDOFF_PROTOCOL multi-turn loop in `plan-executor` so the daemon can dispatch `claude`/`codex`/`gemini` sub-agents, collect their outputs, and resume the orchestrator session until execution completes.

**Architecture:** `executor.rs` owns single-subprocess lifecycle and emits `HandoffRequired` when the orchestrator stops for sub-agents. `handoff.rs` owns the HANDOFF_PROTOCOL transport layer: state-file parsing, per-type sub-agent dispatch (parallel), continuation building, and session resume. The daemon's `trigger_execution` wraps these in a loop until no state file remains.

**Tech Stack:** Rust 2024, tokio, serde_json, `plan-executor/src/executor.rs`, `plan-executor/src/handoff.rs`, `plan-executor/src/daemon.rs`

**Reference:** `~/tools/claude/my-plugin/plugins/my/skills/execute-plan-non-interactive/HANDOFF_PROTOCOL.md` — authoritative transport contract (handoff line format, state file schema, continuation format, agent-type values).

---

## File Map

| File | Change |
|---|---|
| `plan-executor/src/executor.rs` | Fix skill name, arg order; add `session_id` capture; add `execution_root` param; add `HandoffRequired` event |
| `plan-executor/src/handoff.rs` | New module: types, state parsing, sub-agent dispatch, continuation builder, resume command |
| `plan-executor/src/daemon.rs` | Replace single `spawn_execution` call in `trigger_execution` with handoff loop |
| `plan-executor/src/main.rs` | Add `mod handoff;` |
| `.my/plans/plan-plan-executor.md` | Add HANDOFF_PROTOCOL reference, fix Task 9, add Task 9b, update Task 10 |

---

### Task 1: Fix `executor.rs`

**Files:**
- Modify: `plan-executor/src/executor.rs`

- [ ] **Step 1: Add `session_id` to `JobMetadata` in `jobs.rs`**

In `plan-executor/src/jobs.rs`, add one field to `JobMetadata` after `pub id`:
```rust
/// Claude session ID (from stream-json system/init event), used for --resume
pub session_id: Option<String>,
```
Also add `session_id: None` in `JobMetadata::new`.

- [ ] **Step 2: Add `session_id` to `StreamEvent` in `executor.rs`**

In the `StreamEvent` struct, add after `model`:
```rust
session_id: Option<String>,
```

- [ ] **Step 3: Capture `session_id` in the stream-json parse loop**

In the `"system"` arm of the `match event.event_type.as_str()` block, add alongside the model capture:
```rust
"system" => {
    if let Some(model) = event.model {
        job.model = Some(model);
    }
    if let Some(sid) = event.session_id {
        job.session_id = Some(sid);
    }
}
```

- [ ] **Step 4: Fix the CLI invocation**

Replace the existing `Command::new("claude")` block with:
```rust
let plan_path_str = job.plan_path.to_string_lossy().to_string();
let prompt_arg = format!("/my:execute-plan-non-interactive {}", plan_path_str);

let mut child = Command::new("claude")
    .args([
        "--dangerously-skip-permissions",
        "--verbose",
        "--output-format",
        "stream-json",
        "-p",
        &prompt_arg,
    ])
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::null())
    .spawn()?;
```

- [ ] **Step 5: Add `execution_root` parameter and `HandoffRequired` event**

Add `HandoffRequired` to the `ExecEvent` enum:
```rust
pub enum ExecEvent {
    OutputLine(String),
    HandoffRequired { session_id: String, state_file: PathBuf },
    Finished(JobMetadata),
}
```

Change `spawn_execution` signature to accept `execution_root`:
```rust
pub fn spawn_execution(
    mut job: JobMetadata,
    execution_root: PathBuf,
) -> Result<(Child, mpsc::Receiver<ExecEvent>)>
```

- [ ] **Step 6: Emit `HandoffRequired` on process exit when state file is present**

After the `while let Ok(Some(line)) = reader.next_line().await` loop ends (process stdout closed), check for the state file before emitting `Finished`:
```rust
// stdout closed — process has exited
let state_file = execution_root.join(".tmp-execute-plan-state.json");
if state_file.exists() {
    if let Some(sid) = job.session_id.clone() {
        let _ = tx.send(ExecEvent::HandoffRequired {
            session_id: sid,
            state_file,
        }).await;
        return; // caller loop will resume; do NOT emit Finished here
    }
}
// No state file or no session_id — execution complete
job.status = JobStatus::Success;
job.finished_at = Some(chrono::Utc::now());
let _ = job.save();
let _ = tx.send(ExecEvent::Finished(job)).await;
```

- [ ] **Step 7: Verify**

```bash
cd /Users/andreas.pohl/tools/plan-executor && cargo check
```
Expected: no errors.

- [ ] **Step 8: Commit**
```bash
git add plan-executor/src/executor.rs plan-executor/src/jobs.rs
git commit -m "fix(plan-executor): fix skill name, arg order, add session_id and HandoffRequired event"
```

---

### Task 2: Create `handoff.rs`

**Files:**
- Create: `plan-executor/src/handoff.rs`
- Modify: `plan-executor/src/main.rs` (add `mod handoff;`)

- [ ] **Step 1: Write failing tests for `build_continuation` and `load_state`**

At the bottom of the new `plan-executor/src/handoff.rs` file, add:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn build_continuation_orders_by_index() {
        let results = vec![
            HandoffResult { index: 2, stdout: "out2".to_string(), stderr: String::new(), success: true },
            HandoffResult { index: 1, stdout: "out1".to_string(), stderr: String::new(), success: true },
        ];
        let s = build_continuation(&results);
        assert!(s.find("# output sub-agent 1:").unwrap() < s.find("# output sub-agent 2:").unwrap());
        assert!(s.contains("out1"));
        assert!(s.contains("out2"));
    }

    #[test]
    fn build_continuation_includes_empty_stdout_for_failed_agents() {
        let results = vec![
            HandoffResult { index: 1, stdout: String::new(), stderr: "error".to_string(), success: false },
        ];
        let s = build_continuation(&results);
        assert!(s.contains("# output sub-agent 1:"));
    }

    #[test]
    fn load_state_parses_handoffs() {
        let json = r#"{
            "phase": "wave_execution",
            "wave": 1,
            "attempt": 1,
            "batch": 1,
            "handoffs": [
                {"index": 1, "agentType": "claude", "promptFile": "/tmp/prompt-1.md"},
                {"index": 2, "agentType": "codex",  "promptFile": "/tmp/prompt-2.md"},
                {"index": 3, "agentType": "gemini", "promptFile": "/tmp/prompt-3.md"}
            ]
        }"#;
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(json.as_bytes()).unwrap();
        let state = load_state(f.path()).unwrap();
        assert_eq!(state.handoffs.len(), 3);
        assert!(matches!(state.handoffs[0].agent_type, AgentType::Claude));
        assert!(matches!(state.handoffs[1].agent_type, AgentType::Codex));
        assert!(matches!(state.handoffs[2].agent_type, AgentType::Gemini));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cd /Users/andreas.pohl/tools/plan-executor && cargo test -p plan-executor handoff 2>&1 | head -20
```
Expected: compile error (module not found).

- [ ] **Step 3: Implement `handoff.rs`**

Create `plan-executor/src/handoff.rs`:
```rust
use std::path::{Path, PathBuf};
use serde::Deserialize;
use tokio::process::Command;
use tokio::sync::mpsc;
use anyhow::Result;

use crate::executor::ExecEvent;

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

#[derive(Deserialize)]
struct RawHandoff {
    index: usize,
    #[serde(rename = "agentType")]
    agent_type: String,
    #[serde(rename = "promptFile")]
    prompt_file: String,
}

#[derive(Deserialize)]
struct RawState {
    phase: String,
    handoffs: Vec<RawHandoff>,
}

/// Reads `.tmp-execute-plan-state.json` and returns parsed `HandoffState`.
pub fn load_state(state_file: &Path) -> Result<HandoffState> {
    let content = std::fs::read_to_string(state_file)?;
    let raw: RawState = serde_json::from_str(&content)?;
    let handoffs = raw
        .handoffs
        .into_iter()
        .map(|h| Handoff {
            index: h.index,
            agent_type: match h.agent_type.as_str() {
                "claude" => AgentType::Claude,
                "codex" => AgentType::Codex,
                "gemini" => AgentType::Gemini,
                other => {
                    tracing::warn!("unknown agent-type '{}', defaulting to claude", other);
                    AgentType::Claude
                }
            },
            prompt_file: PathBuf::from(h.prompt_file),
        })
        .collect();
    Ok(HandoffState {
        phase: raw.phase,
        handoffs,
    })
}

// ── Sub-agent dispatch ─────────────────────────────────────────────────────

/// Dispatches a single sub-agent synchronously, returning its combined output.
async fn dispatch_agent(handoff: Handoff) -> HandoffResult {
    let prompt = match std::fs::read_to_string(&handoff.prompt_file) {
        Ok(content) => content,
        Err(e) => {
            return HandoffResult {
                index: handoff.index,
                stdout: String::new(),
                stderr: format!("failed to read prompt file {:?}: {}", handoff.prompt_file, e),
                success: false,
            };
        }
    };

    let output = match &handoff.agent_type {
        AgentType::Claude => {
            Command::new("claude")
                .args(["--dangerously-skip-permissions", "-p", &prompt])
                .output()
                .await
        }
        AgentType::Codex => {
            Command::new("codex")
                .args(["--dangerously-bypass-approvals-and-sandbox", "exec", &prompt])
                .output()
                .await
        }
        AgentType::Gemini => {
            Command::new("gemini")
                .args(["--yolo", "-p", &prompt])
                .output()
                .await
        }
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

/// Dispatches all handoffs in a batch concurrently.
/// Returns results sorted by index (ascending).
pub async fn dispatch_all(handoffs: Vec<Handoff>) -> Vec<HandoffResult> {
    let handles: Vec<_> = handoffs
        .into_iter()
        .map(|h| tokio::spawn(dispatch_agent(h)))
        .collect();

    let mut results = Vec::new();
    for handle in handles {
        if let Ok(result) = handle.await {
            results.push(result);
        }
    }
    results.sort_by_key(|r| r.index);
    results
}

// ── Continuation builder ───────────────────────────────────────────────────

/// Builds the continuation payload for `--resume`.
/// Format per HANDOFF_PROTOCOL §6:
///   # output sub-agent N:
///   <stdout>
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
/// Returns a new (Child, Receiver<ExecEvent>) pair with the same shape as `spawn_execution`.
pub async fn resume_execution(
    session_id: &str,
    continuation: &str,
    execution_root: PathBuf,
) -> Result<(tokio::process::Child, mpsc::Receiver<ExecEvent>)> {
    use crate::jobs::{JobMetadata, JobStatus};
    use crate::pricing::{calculate_cost, load_pricing};
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
    let execution_root_owned = execution_root.clone();

    tokio::spawn(async move {
        let pricing = load_pricing();
        let mut reader = tokio::io::BufReader::new(stdout).lines();
        // Resumed job — we don't have a JobMetadata here; use a placeholder to carry
        // session_id and track any final result event for token accounting.
        let mut resumed_session_id = Some(session_id_owned.clone());
        let mut resumed_model: Option<String> = None;
        let mut resumed_cost: Option<f64> = None;

        while let Ok(Some(line)) = reader.next_line().await {
            // Forward every line as OutputLine
            let _ = tx.send(ExecEvent::OutputLine(line.clone())).await;

            // Parse to track session continuity and detect another handoff pause
            if let Ok(ev) = serde_json::from_str::<serde_json::Value>(&line) {
                match ev.get("type").and_then(|t| t.as_str()) {
                    Some("system") => {
                        if let Some(sid) = ev.get("session_id").and_then(|s| s.as_str()) {
                            resumed_session_id = Some(sid.to_string());
                        }
                        if let Some(m) = ev.get("model").and_then(|m| m.as_str()) {
                            resumed_model = Some(m.to_string());
                        }
                    }
                    Some("result") => {
                        if let Some(cost) = ev.get("total_cost_usd").and_then(|c| c.as_f64()) {
                            resumed_cost = Some(cost);
                        }
                    }
                    _ => {}
                }
            }
        }

        // stdout closed — check for another handoff pause
        let state_file = execution_root_owned.join(".tmp-execute-plan-state.json");
        if state_file.exists() {
            if let Some(sid) = resumed_session_id {
                let _ = tx.send(ExecEvent::HandoffRequired {
                    session_id: sid,
                    state_file,
                }).await;
                return;
            }
        }

        // Execution complete — emit a synthetic Finished event
        // (We don't have full JobMetadata here; daemon updates its in-memory job with resumed_cost)
        let mut placeholder = JobMetadata::new(PathBuf::from("<resumed>"));
        placeholder.session_id = Some(session_id_owned);
        placeholder.model = resumed_model;
        placeholder.cost_usd = resumed_cost;
        placeholder.status = JobStatus::Success;
        placeholder.finished_at = Some(chrono::Utc::now());
        let _ = tx.send(ExecEvent::Finished(placeholder)).await;
    });

    Ok((child, rx))
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn build_continuation_orders_by_index() {
        let results = vec![
            HandoffResult { index: 2, stdout: "out2".to_string(), stderr: String::new(), success: true },
            HandoffResult { index: 1, stdout: "out1".to_string(), stderr: String::new(), success: true },
        ];
        let s = build_continuation(&results);
        assert!(s.find("# output sub-agent 1:").unwrap() < s.find("# output sub-agent 2:").unwrap());
        assert!(s.contains("out1"));
        assert!(s.contains("out2"));
    }

    #[test]
    fn build_continuation_includes_empty_stdout_for_failed_agents() {
        let results = vec![
            HandoffResult { index: 1, stdout: String::new(), stderr: "error".to_string(), success: false },
        ];
        let s = build_continuation(&results);
        assert!(s.contains("# output sub-agent 1:"));
    }

    #[test]
    fn load_state_parses_handoffs() {
        let json = r#"{
            "phase": "wave_execution",
            "wave": 1,
            "attempt": 1,
            "batch": 1,
            "handoffs": [
                {"index": 1, "agentType": "claude", "promptFile": "/tmp/prompt-1.md"},
                {"index": 2, "agentType": "codex",  "promptFile": "/tmp/prompt-2.md"},
                {"index": 3, "agentType": "gemini", "promptFile": "/tmp/prompt-3.md"}
            ]
        }"#;
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(json.as_bytes()).unwrap();
        let state = load_state(f.path()).unwrap();
        assert_eq!(state.handoffs.len(), 3);
        assert!(matches!(state.handoffs[0].agent_type, AgentType::Claude));
        assert!(matches!(state.handoffs[1].agent_type, AgentType::Codex));
        assert!(matches!(state.handoffs[2].agent_type, AgentType::Gemini));
    }
}
```

- [ ] **Step 4: Add `mod handoff;` to `main.rs`**

In `plan-executor/src/main.rs`, add alongside the other `mod` declarations:
```rust
mod handoff;
```

- [ ] **Step 5: Run tests**

```bash
cd /Users/andreas.pohl/tools/plan-executor && cargo test -p plan-executor handoff -- --nocapture
```
Expected: 3 tests pass.

- [ ] **Step 6: Verify full compile**

```bash
cd /Users/andreas.pohl/tools/plan-executor && cargo check
```
Expected: no errors.

- [ ] **Step 7: Commit**
```bash
git add plan-executor/src/handoff.rs plan-executor/src/main.rs
git commit -m "feat(plan-executor): add handoff module with state parsing, sub-agent dispatch, and resume"
```

---

### Task 3: Update `daemon.rs` — handoff loop in `trigger_execution`

**Files:**
- Modify: `plan-executor/src/daemon.rs`

The current `trigger_execution` calls `spawn_execution` once and handles only `OutputLine` and `Finished`. It must be extended to handle `HandoffRequired` by dispatching sub-agents, building the continuation, and resuming.

- [ ] **Step 1: Add `handoff` import to `daemon.rs`**

Add to the imports at the top of `daemon.rs`:
```rust
use crate::handoff;
```

- [ ] **Step 2: Determine `execution_root` from the plan path**

In `trigger_execution`, derive `execution_root` before calling `spawn_execution`. The execution root is the repository root (the directory containing `.git`) walking up from the plan file, falling back to the plan file's parent directory:

```rust
pub async fn trigger_execution(state: &Arc<Mutex<DaemonState>>, plan_path: &str) {
    let plan = PathBuf::from(plan_path);
    let execution_root = find_repo_root(&plan)
        .unwrap_or_else(|| plan.parent().unwrap_or(&plan).to_path_buf());

    let job = JobMetadata::new(plan.clone());
    let job_id = job.id.clone();

    let Ok((mut _child, mut exec_rx)) = spawn_execution(job.clone(), execution_root.clone()) else { return };
    // ...
```

Add helper below `trigger_execution`:
```rust
fn find_repo_root(path: &Path) -> Option<PathBuf> {
    let mut dir = if path.is_file() {
        path.parent()?.to_path_buf()
    } else {
        path.to_path_buf()
    };
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        dir = dir.parent()?.to_path_buf();
    }
}
```

- [ ] **Step 3: Replace the event loop with the handoff loop**

Replace the `tokio::spawn(async move { while let Some(event) = exec_rx.recv().await { ... } })` block in `trigger_execution` with:

```rust
let state_clone = Arc::clone(state);
let plan_path_owned = plan_path.to_string();
tokio::spawn(async move {
    'outer: loop {
        while let Some(event) = exec_rx.recv().await {
            match event {
                ExecEvent::OutputLine(line) => {
                    let mut st = state_clone.lock().await;
                    let buf = st.job_output.entry(job_id.clone()).or_default();
                    buf.push(line.clone());
                    if buf.len() > 10000 { buf.remove(0); }
                    let _ = st.event_tx.send(DaemonEvent::JobOutput {
                        job_id: job_id.clone(),
                        line,
                    });
                }
                ExecEvent::HandoffRequired { session_id, state_file } => {
                    // Load state file and dispatch sub-agents
                    let state_data = match handoff::load_state(&state_file) {
                        Ok(s) => s,
                        Err(e) => {
                            let mut st = state_clone.lock().await;
                            let _ = st.event_tx.send(DaemonEvent::JobOutput {
                                job_id: job_id.clone(),
                                line: format!("[plan-executor] failed to read state file: {}", e),
                            });
                            break 'outer;
                        }
                    };

                    // Log handoff start to TUI
                    {
                        let mut st = state_clone.lock().await;
                        let _ = st.event_tx.send(DaemonEvent::JobOutput {
                            job_id: job_id.clone(),
                            line: format!(
                                "[plan-executor] dispatching {} sub-agent(s) (phase: {})",
                                state_data.handoffs.len(),
                                state_data.phase
                            ),
                        });
                    }

                    let results = handoff::dispatch_all(state_data.handoffs).await;

                    // Surface any failed sub-agents to TUI
                    for r in &results {
                        if !r.success {
                            let mut st = state_clone.lock().await;
                            let _ = st.event_tx.send(DaemonEvent::JobOutput {
                                job_id: job_id.clone(),
                                line: format!(
                                    "[plan-executor] sub-agent {} failed: {}",
                                    r.index,
                                    r.stderr.lines().next().unwrap_or("(no stderr)")
                                ),
                            });
                        }
                    }

                    let continuation = handoff::build_continuation(&results);

                    match handoff::resume_execution(&session_id, &continuation, execution_root.clone()).await {
                        Ok((_new_child, new_rx)) => {
                            exec_rx = new_rx;
                            continue 'outer;
                        }
                        Err(e) => {
                            let mut st = state_clone.lock().await;
                            let _ = st.event_tx.send(DaemonEvent::JobOutput {
                                job_id: job_id.clone(),
                                line: format!("[plan-executor] failed to resume session: {}", e),
                            });
                            break 'outer;
                        }
                    }
                }
                ExecEvent::Finished(finished_job) => {
                    let success = finished_job.status == JobStatus::Success;
                    let cost = finished_job.cost_usd;
                    let mut st = state_clone.lock().await;
                    st.running_jobs.remove(&job_id);
                    st.history.insert(0, finished_job.clone());
                    let _ = notify_execution_complete(&plan_path_owned, success, cost);
                    let _ = st.event_tx.send(DaemonEvent::JobUpdated { job: finished_job });
                    break 'outer;
                }
            }
        }
        break; // exec_rx closed without Finished — treat as done
    }
});
```

- [ ] **Step 4: Verify**

```bash
cd /Users/andreas.pohl/tools/plan-executor && cargo check
```
Expected: no errors.

- [ ] **Step 5: Commit**
```bash
git add plan-executor/src/daemon.rs
git commit -m "feat(plan-executor): implement handoff loop in trigger_execution"
```

---

### Task 4: Update `plan-plan-executor.md`

**Files:**
- Modify: `.my/plans/plan-plan-executor.md`

- [ ] **Step 1: Add HANDOFF_PROTOCOL reference to the Context section**

After the line:
```
4. Executes plans via: `claude --dangerously-skip-permissions --verbose --output-format stream-json -p "/my:execute-plan-non-interactive <path>"`
```

Add (as a new bullet below item 4, before item 5):
```
   The execution is multi-turn: the skill stops and emits handoff lines per `execute-plan-non-interactive/HANDOFF_PROTOCOL.md`. The daemon dispatches sub-agents (claude/codex/gemini), collects outputs, and resumes via `claude --resume <session_id>` until no `.tmp-execute-plan-state.json` remains.
```

- [ ] **Step 2: Add acceptance criteria for handoff loop**

After the line:
```
- [ ] Execution runs `claude` subprocess with correct flags, streams output to job output file
```

Add:
```
- [ ] Execution follows HANDOFF_PROTOCOL: daemon detects `.tmp-execute-plan-state.json` on process exit and enters handoff loop
- [ ] Sub-agents dispatched in parallel per `agent-type` field: `claude`, `codex`, `gemini`
- [ ] Sub-agent failures surfaced to TUI as job output lines with stderr context
- [ ] Orchestrator session resumed via `claude --resume <session_id> -p "<continuation>"` after each batch
- [ ] Loop repeats until no state file present (execution complete)
```

- [ ] **Step 3: Fix Task 9 CLI invocation**

In Task 9 `executor.rs`, replace:
```rust
let cmd_arg = format!("/my:execute-plan {}", plan_path);

let mut child = Command::new("claude")
    .args([
        "--dangerously-skip-permissions",
        "-p",
        "--verbose",
        "--output-format",
        "stream-json",
        &cmd_arg,
    ])
```
With:
```rust
let cmd_arg = format!("/my:execute-plan-non-interactive {}", plan_path);

let mut child = Command::new("claude")
    .args([
        "--dangerously-skip-permissions",
        "--verbose",
        "--output-format",
        "stream-json",
        "-p",
        &cmd_arg,
    ])
```

- [ ] **Step 4: Add `session_id` field and `HandoffRequired` event to Task 9**

In the `ExecEvent` enum in Task 9's code, add:
```rust
HandoffRequired { session_id: String, state_file: PathBuf },
```

In `StreamEvent`, add `session_id: Option<String>`.

In `JobMetadata`, add `pub session_id: Option<String>`.

Add the handoff-detection block after the stdout loop (as implemented in Task 1 Step 6 above).

Change `spawn_execution` signature to include `execution_root: PathBuf`.

- [ ] **Step 5: Add Task 9b section to the plan**

Insert a new `### Task 9b: Handoff protocol module` section between Task 9 and Task 10, containing the full `handoff.rs` implementation as written in Task 2 Step 3 above (types, `load_state`, `dispatch_agent`, `dispatch_all`, `build_continuation`, `resume_execution`, tests).

- [ ] **Step 6: Update Task 10 daemon section**

In the `trigger_execution` function code in Task 10, replace the single-event-loop body with the `'outer` handoff loop from Task 3 Step 3 above. Add the `find_repo_root` helper. Add the `use crate::handoff;` import.

- [ ] **Step 7: Verify plan is self-consistent**

Read through Tasks 9, 9b, and 10 in the updated plan and confirm:
- All type names match across tasks (`ExecEvent`, `HandoffRequired`, `HandoffState`, `HandoffResult`)
- `spawn_execution` and `resume_execution` both accept `execution_root: PathBuf`
- `dispatch_all` signature matches between 9b definition and Task 10 call site
- No references to `/my:execute-plan` remain (only `/my:execute-plan-non-interactive`)

- [ ] **Step 8: Commit**
```bash
git add .my/plans/plan-plan-executor.md
git commit -m "docs(plan-executor): add handoff protocol to plan (Task 9 fix, Task 9b, Task 10 loop)"
```
