# Design: plan-executor Handoff Protocol Integration

**Date:** 2026-04-01
**Status:** Approved
**Scope:** Fixes to Task 9 (`executor.rs`) and a new `handoff.rs` module to implement the HANDOFF_PROTOCOL multi-turn loop in `plan-executor`.

---

## Problem

The existing plan (`.my/plans/plan-plan-executor.md`) Task 9 treats `claude` as a single-shot subprocess. However, `my:execute-plan-non-interactive` is a multi-turn protocol defined in `execute-plan-non-interactive/HANDOFF_PROTOCOL.md`:

1. The skill emits handoff lines (`call sub-agent N (agent-type: type): path`) and stops (process exits).
2. An external executor must dispatch sub-agents per type, collect their outputs, and resume the original claude session with continuation input (`# output sub-agent N:` blocks).
3. This loop repeats until no `.tmp-execute-plan-state.json` remains.

The plan also has two bugs: wrong skill name (`/my:execute-plan` instead of `/my:execute-plan-non-interactive`) and swapped CLI arguments.

---

## Design

### Section 1 — `executor.rs` fixes (Task 9)

**Bug fixes:**

- Skill name corrected to `/my:execute-plan-non-interactive <path>`
- Argument order corrected:
  ```
  claude --dangerously-skip-permissions --verbose --output-format stream-json -p "/my:execute-plan-non-interactive <path>"
  ```

**New field on `JobMetadata`:**
```rust
pub session_id: Option<String>,
```
Populated from the `session_id` field in the `system/init` stream-json event.

**New `ExecEvent` variant:**
```rust
ExecEvent::HandoffRequired { session_id: String, state_file: PathBuf }
```
Emitted when the subprocess exits with code 0 and `<execution_root>/.tmp-execute-plan-state.json` exists. `ExecEvent::Finished` is emitted only when exit code 0 and no state file is present.

**New parameter on `spawn_execution`:**
```rust
pub fn spawn_execution(job: JobMetadata, execution_root: PathBuf) -> Result<(Child, Receiver<ExecEvent>)>
```
`execution_root` is the worktree root or repository root — where the state file will be written.

---

### Section 2 — new `handoff.rs` module (new task)

Owns the full HANDOFF_PROTOCOL transport layer.

**Types:**
```rust
pub struct HandoffState {
    pub phase: String,
    pub handoffs: Vec<Handoff>,
}

pub struct Handoff {
    pub index: usize,       // 1-based, matches state file
    pub agent_type: AgentType,
    pub prompt_file: PathBuf,
}

pub enum AgentType { Claude, Codex, Gemini }

pub struct HandoffResult {
    pub index: usize,
    pub stdout: String,
    pub stderr: String,
    pub success: bool,      // exit code 0
}
```

**State file parsing:**
```rust
pub fn load_state(state_file: &Path) -> Result<HandoffState>
```
Reads `.tmp-execute-plan-state.json` and maps `handoffs[]` to `Vec<Handoff>`.

**Sub-agent dispatch (per type and batch):**
```rust
pub async fn dispatch_all(handoffs: Vec<Handoff>) -> Vec<HandoffResult>
```
Spawns all handoffs concurrently, waits for all to complete, returns results sorted by index.

Prompt file content is read and passed inline as the prompt argument:

| Agent type | Command |
|---|---|
| `claude` | `claude --dangerously-skip-permissions -p "<prompt_content>"` |
| `codex` | `codex --dangerously-bypass-approvals-and-sandbox exec "<prompt_content>"` |
| `gemini` | `gemini --yolo -p "<prompt_content>"` |

All handoffs in a batch run in parallel (`tokio::spawn` per handoff). Both stdout and stderr are captured. `HandoffResult.success` reflects the exit code.

**Continuation builder:**
```rust
pub fn build_continuation(results: &[HandoffResult]) -> String
```
Produces (results sorted by index):
```
# output sub-agent 1:
<stdout>

# output sub-agent 2:
<stdout>
```

**Resume command:**
```rust
pub async fn resume_execution(
    session_id: &str,
    continuation: &str,
    execution_root: PathBuf,
) -> Result<(Child, Receiver<ExecEvent>)>
```
Runs:
```
claude --dangerously-skip-permissions --verbose --output-format stream-json \
       --resume <session_id> -p "<continuation>"
```
Returns the same `(Child, Receiver<ExecEvent>)` shape as `spawn_execution` so the caller loop is uniform.

---

### Section 3 — execution loop (daemon orchestration)

The daemon replaces the single `spawn_execution` call with a loop:

```rust
async fn run_job(plan_path: PathBuf, execution_root: PathBuf) {
    let job = JobMetadata::new(plan_path);
    let (mut child, mut rx) = spawn_execution(job.clone(), execution_root.clone())?;

    'outer: loop {
        while let Some(event) = rx.recv().await {
            match event {
                ExecEvent::OutputLine(line) => {
                    // stream to TUI subscribers, append to output.jsonl
                }
                ExecEvent::HandoffRequired { session_id, state_file } => {
                    let state = handoff::load_state(&state_file)?;
                    let results = handoff::dispatch_all(state.handoffs).await;

                    // Surface failed sub-agents to TUI
                    for r in &results {
                        if !r.success {
                            // emit DaemonEvent::JobOutput with stderr context
                        }
                    }

                    let continuation = handoff::build_continuation(&results);
                    let (new_child, new_rx) = handoff::resume_execution(
                        &session_id, &continuation, execution_root.clone()
                    ).await?;
                    child = new_child;
                    rx = new_rx;
                    // continue outer loop with resumed process
                    continue 'outer;
                }
                ExecEvent::Finished(meta) => {
                    // job done — no state file present
                    break 'outer;
                }
            }
        }
        break;
    }
}
```

**Error handling for failed sub-agents:**
- Non-zero exit code → `HandoffResult.success = false`
- The daemon emits a `DaemonEvent::JobOutput` line to TUI subscribers with the sub-agent's stderr (prefixed with agent type and index for context)
- The continuation is always sent regardless — the skill's own triage logic handles sub-agent failures
- This ensures TUI shows actionable error context without aborting the job prematurely

---

## Plan changes required

The following changes must be made to `.my/plans/plan-plan-executor.md`:

1. **Task 9**: Apply all fixes from Section 1 (skill name, arg order, `session_id` capture, `HandoffRequired` event, `execution_root` parameter).
2. **New Task 9b** (insert after Task 9, before Task 10): Implement `handoff.rs` as described in Section 2.
3. **Task 10 (daemon)**: Replace single `spawn_execution` call with the loop from Section 3.
4. **Context section**: Add reference to `execute-plan-non-interactive/HANDOFF_PROTOCOL.md` as the authoritative transport contract.
5. **Acceptance Criteria**: Add criteria for handoff loop, sub-agent dispatch, and resume.

---

## References

- `~/tools/claude/my-plugin/plugins/my/skills/execute-plan-non-interactive/HANDOFF_PROTOCOL.md` — transport contract (prompt file naming, handoff line format, state file schema, continuation format)
- `~/tools/claude/my-plugin/plugins/my/skills/execute-plan-non-interactive/SKILL.md` — orchestrator skill (phase contract, resume behavior)
