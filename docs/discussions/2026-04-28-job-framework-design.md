# Job Framework + Protocol-Recovery Supervisor — Design

- **Date:** 2026-04-28
- **Status:** Implementation in progress (Phase A + B)

## 1. Motivation

Two pain points motivate this work:

1. **Plan-only runtime.** Today the runtime only knows about "plans". Every operation that is not a plan — `pr-finalize`, `review`, `validate`, `compile-fix-waves` — has to be shoehorned into a fake plan to reuse the orchestrator. The fake plans add ceremony, hide intent, and make recovery semantics fuzzy because everything looks like a plan step even when it is a pure idempotent operation.
2. **Non-interactive runs die on protocol violations.** The LLM occasionally emits output that violates the runtime's interaction protocol: missing `handoffs[]`, a `tool_use` after a handoff has been declared, calls to forbidden tools, `ScheduleWakeup` in non-interactive mode, malformed state JSON, dangling narration without a tool call, etc. Every one of those is recoverable in principle (re-prompt with a corrective system message, or roll back to the previous checkpoint and retry), but today they terminate the run. A non-interactive batch job should not fail because of a recoverable model glitch.

The Job Framework introduces a first-class `Job` abstraction that knows about its `Step`s and their recovery policies, and the Protocol-Recovery Supervisor wraps LLM-driven steps so protocol violations become recovered attempts instead of terminal failures.

## 2. Failure-mode taxonomy

Every failure the runtime sees falls into one of five categories. The recovery policy is determined by the category, not by the calling site.

| # | Category | Examples | Recovery policy |
|---|---|---|---|
| 1 | Hard infra | OOM, disk full, gh auth expired, GPG key missing | `RecoveryPolicy::None` — fail fast |
| 2 | Transient infra | API 5xx, rate-limit, MCP reconnect, transient git push reject | `RetryTransient { max: 3, backoff: Exponential }` |
| 3 | LLM protocol violation | missing `handoffs[]`, post-handoff `tool_use`, forbidden tools, `ScheduleWakeup`, malformed state JSON, dangling narration | `RetryProtocol { max: 3, corrective_prompt: <category-specific> }` then `Rollback { to: previous_checkpoint, then: RetryProtocol { max: 1 } }` |
| 4 | LLM semantic mistake | broken test, wrong code, invalid manifest, missed acceptance criterion | Existing fix-loop (review-fix-validate) — no change |
| 5 | Plan/spec drift | plan refers to deleted file, prerequisite not landed | Detect at preflight or step input-validation; hard-fail with specific guidance |

## 3. Success criterion

Non-interactive runs only fail on row 1 (hard infra) or row 5 (spec drift). Row 3 violations show up as recovered attempts in `plan-executor jobs show <id>` but never terminate the job. Row 2 transient infra failures are retried with backoff and recorded the same way. Row 4 semantic mistakes continue to flow through the existing fix-loop unchanged.

## 4. Type architecture

The framework introduces these types. The actual Rust definitions live in `src/job/types.rs`.

- `Job { id, kind, state, steps }` — top-level unit of work persisted on disk.
- `JobKind` — `Plan` | `PrFinalize` | `Review` | `Validate` | `CompileFixWaves`. Determines which `Step` registry the job uses.
- `JobState` — `Pending` | `Running` | `Suspended` | `Succeeded` | `Failed`.
- `StepInstance { seq, name, status, attempts, idempotent }` — one logical step within a job; carries its attempt history.
- `StepStatus` — per-step lifecycle (pending / running / succeeded / failed / skipped).
- `StepAttempt` — a single execution of a step, with timestamps, outcome, and stdio refs.
- `AttemptOutcome` — `Success` | `HardInfra` | `TransientInfra` | `ProtocolViolation` | `SemanticMistake` | `SpecDrift` | `Pending`. Maps directly onto rows 1-5 of the taxonomy plus the live `Pending` state.

## 5. Recovery policies

`RecoveryPolicy` is the supervisor's vocabulary for deciding what to do after an `AttemptOutcome` other than `Success`. Variants:

- `None` — fail the step immediately (used for `HardInfra` and `SpecDrift`).
- `RetryTransient { max, backoff }` — retry up to `max` times with the given `Backoff`. Used for `TransientInfra`.
- `RetryProtocol { max, corrective: CorrectivePromptKey }` — retry the step with a corrective system message prepended. Used for `ProtocolViolation`.
- `Rollback { to: CheckpointTarget, then: Box<RecoveryPolicy> }` — restore to a checkpoint and apply the inner policy.
- `Compose(Vec<RecoveryPolicy>)` — try policies in order; first one that returns a non-failure outcome wins.
- `OperatorDecision { decision_key }` — suspend the job and surface a decision via the existing operator-decision channel.

`Backoff` — `Fixed { interval }` | `Exponential { initial, max, factor }`.

`CheckpointTarget` — `PreviousAttempt` | `PreviousStep` | `PreviousPhase` | `Named(String)`.

`CorrectivePromptKey` is a string identifier (e.g. `missing_handoffs`, `post_handoff_tool_use`). The corresponding template lives at `src/supervisor/prompts/<key>.md` and is loaded at compile time via `include_str!`. New violations only require dropping a new `<key>.md` file and adding the variant to the key list.

## 6. Step trait

Every step implements:

```text
trait Step {
    fn name(&self) -> &str;
    fn idempotent(&self) -> bool;
    fn recovery_policy(&self) -> RecoveryPolicy;
    fn checkpoint_before(&self) -> bool;
    async fn run(&self, ctx: &mut StepContext) -> AttemptOutcome;
}
```

A `JobKind → Vec<Box<dyn Step>>` registry returns the ordered step list for each kind. Phase A introduces a single registry entry — `JobKind::Plan` returns the existing wave-execution flow wrapped as a `Step`. Other `JobKind` variants land in later phases or post-Phase B.

## 7. Persistent storage layout

Each job has a directory on disk. The layout is independent of `JobKind` so all subcommands inspect jobs the same way:

```
~/.plan-executor/jobs/<job-id>/
  job.json
  steps/
    NNN-<name>/
      input.json
      checkpoint.json
      attempts/
        1/{started_at, finished_at, outcome.json, stdout.log, stderr.log}
        2/...
      output.json
```

`job.json` holds the `Job` envelope; per-step `input.json` / `output.json` capture the step boundary; `attempts/<n>/` keeps each attempt's stdio and outcome separately so a recovered run shows the full history without overwriting prior evidence.

## 8. Migration strategy

- **Phase A** introduces `Job` / `Step` / `RecoveryPolicy` types and the on-disk layout. `JobKind::Plan` is the only variant wired up; its registry returns shells that delegate to the existing wave-execution behavior. Behavior is unchanged from the operator's perspective.
- **Phase B** adds the protocol-violation supervisor and wires it into `WaveExecutionStep`. After Phase B, row-3 violations no longer terminate runs.
- **Phase C, D, F** are deferred. The orchestrator skill stays in place. When Phase D ships, it replaces the orchestrator with a Rust scheduler behind a `--legacy-orchestrator` escape hatch so existing automation can opt out during the transition.

## 9. Out of scope

- Web UI for job inspection.
- Multi-machine distribution / job sharding.
- SQLite or postgres-backed storage (filesystem is the source of truth for this revision).
- Daemon-mode replacement (the runtime stays one-shot per invocation).
- Interactive `/plan-executor:execute-plan` slash command (still owned by the plugin).
