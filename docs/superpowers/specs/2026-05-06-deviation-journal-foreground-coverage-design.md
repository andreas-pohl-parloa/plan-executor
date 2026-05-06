# Deviation Journal — Foreground Executor Coverage Design

## Context

This is a follow-up to `docs/superpowers/specs/2026-05-06-deviation-journal-design.md`. That spec covers the deviation-journal contract for sub-agents dispatched through the Rust scheduler. Implementation began with PR #71 (`feat(deviation-journal): add entry types and validation`).

After the original spec was written, a question came up about parity with the foreground/in-session executor: does the deviation-journal protocol reach sub-agents when the binary runs as `plan-executor execute --foreground <tasks.json>`, or only the daemon path?

The original spec did not name the foreground mode explicitly. This follow-up clarifies coverage and adds a small verification step. It does NOT extend the journal to in-session Claude orchestrators that bypass the binary; that scope is left out of both specs.

## Goal

State explicitly which executors inherit the deviation-journal protocol and add a focused integration test that pins foreground-mode behavior.

## Non-goals

- Extend the journal to `superpowers:subagent-driven-development` or any future Claude-session orchestrator that dispatches via the Agent tool. Both bypass `handoff::dispatch_agent`. Coverage there would require a parallel implementation in the orchestrator skill itself and is intentionally out of scope here.
- Change the journal schema, file layout, write protocol, or digest format. Those are the original spec's contract.
- Add the executor-coverage statement to the original spec. The original spec is treated as a point-in-time artifact tied to its own implementation plan.

## Coverage statement

Prompt injection lives in `handoff::dispatch_agent`. Every code path that funnels through the Rust scheduler inherits the deviation-journal protocol automatically:

- `plan-executor execute <tasks.json>` (daemon mode, default)
- `plan-executor execute --foreground <tasks.json>` (in-session/foreground)
- `plan-executor execute --remote <tasks.json>` (GHA runner; the runner runs the foreground binary)

What is NOT covered:

- In-session Claude orchestrators that build per-task prompt files and dispatch via the Agent tool (e.g. `superpowers:subagent-driven-development`, hypothetical `plan-executor:execute-plan` skill). Those would need a parallel implementation that injects the same block into the prompt files they construct and that reads/digests the journal between waves.

## Verification approach

Add one focused integration test that asserts the prompt-injection helper rewrites a per-task prompt file with the deviation block, surfaces the journal path and IDs, and is idempotent. The test must not require a live Claude/Codex/Gemini binary; it inspects the file after the helper runs.

The test pins the part of the system that decides whether the foreground binary inherits the protocol. If a future change moves prompt injection out of `handoff::dispatch_agent` (for example, into a daemon-only hook), this test fails immediately.

## Trust and failure handling

Same as the original spec.

## Acceptance criteria

- A new integration test exists under `tests/` that exercises `ensure_deviation_block_in_prompt` with a `DeviationContext` and asserts:
  - the block marker is present in the rewritten prompt;
  - the journal path, `task_id`, and `job_id` are visible inside the block;
  - calling the helper a second time does not duplicate the block.
- The test compiles and runs without changes to the existing `src/handoff.rs` public API beyond what the original deviation-journal plan already adds.
- The original deviation-journal spec and plan remain unchanged.
