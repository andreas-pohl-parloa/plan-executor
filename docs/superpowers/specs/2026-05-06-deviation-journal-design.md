# Deviation Journal Design

## Goal

Make plan execution more effective by letting execution sub-agents pass structured, evidence-backed discoveries to later stages. The journal prevents silent plan/code mismatches from turning into false validator loops while avoiding a new way for agents to rationalize incomplete work.

The motivating failure mode is an implementation agent discovering that a plan task does not match code reality, or claiming it did, without leaving durable evidence. Review, validation, and fix agents then re-discover the mismatch independently, or worse, hallucinate that the work already exists. A durable deviation journal gives later stages a concrete trail to verify.

## Non-goals

- Do not make journal entries a standalone pass condition.
- Do not pause daemon runs for every deviation.
- Do not rewrite compiled task bodies. The compile-plan contract remains plan-body passthrough.
- Do not embed raw large command output in prompts or journal entries.
- Do not allow interactive user questions from non-interactive agents.

## Core model

Each execution worktree has an append-only JSONL journal:

```text
<execution_root>/.plan-executor/deviations.jsonl
```

Each line is one deviation entry:

```json
{
  "version": 1,
  "job_id": "07cfc3cd-...",
  "phase": "wave_execution",
  "wave_id": 3,
  "task_id": "18",
  "agent_index": 2,
  "category": "skip",
  "severity": "warning",
  "claim": "Task 18 appears already implemented in SKILL.md",
  "plan_anchor": "Task 18: Update SKILL.md to a loop",
  "evidence": [
    {
      "kind": "file_line",
      "path": "plugins/inline-discussion/skills/discuss/SKILL.md",
      "lines": "31-59",
      "summary": "Current file already contains the loop on signal files"
    }
  ],
  "impact": "Validator should verify SKILL.md lines 31-59 before flagging Task 18 missing",
  "recommended_followup": "No code change if evidence still matches",
  "created_at": "2026-05-06T15:30:00Z"
}
```

### Categories

- `skip` — the agent intentionally skipped part or all of a task because code reality made the planned edit unnecessary or invalid.
- `substitute` — the agent implemented an equivalent but different solution than the plan described.
- `scope_change` — the agent had to touch a different file or shape than the plan anticipated.
- `discovery` — useful factual context for later stages that does not change scope.
- `blocker` — the task cannot be completed as written without a prerequisite or contradiction being resolved.

### Severities

- `info` — context only.
- `warning` — later stages should verify before acting.
- `critical` — validation must fail unless the blocker has been resolved by later code or a later corrective journal entry.

## Validator-gated writes

Sub-agents must not append raw JSON blindly. The plan-executor binary provides schemas for agents to use before writing:

```bash
plan-executor validate --schema=deviation-journal-entry -
plan-executor validate --schema=deviation-journal <path>
```

Sub-agent write contract:

1. Build one proposed entry as JSON.
2. Validate it:
   ```bash
   printf '%s\n' '<entry-json>' | plan-executor validate --schema=deviation-journal-entry -
   ```
3. Append it as exactly one line to `<execution_root>/.plan-executor/deviations.jsonl` only after validation returns `VALID:`.
4. If validation fails, fix the entry and retry once.
5. If validation still fails, do not append; mention the validator error in the final report.

### Entry-schema checks

The `deviation-journal-entry` schema validates one JSON object:

- `version == 1`.
- `job_id`, `phase`, `category`, `severity`, `claim`, `plan_anchor`, `evidence`, `impact`, `created_at` are present.
- `category` is one of `skip`, `substitute`, `scope_change`, `discovery`, `blocker`.
- `severity` is one of `info`, `warning`, `critical`.
- `skip`, `substitute`, and `scope_change` entries have at least one evidence item.
- Evidence item `kind` is one of `file_line`, `command_log`, `test_result`, `commit`.
- `created_at` parses as RFC3339.
- Entry size is under a fixed cap, initially 16 KiB.

### File-schema checks

The `deviation-journal` schema validates a JSONL file:

- every non-empty line parses,
- every line matches `deviation-journal-entry`,
- malformed lines are reported with line numbers,
- max journal size is bounded,
- syntactic `task_id` and `wave_id` checks are applied where present.

Semantic cross-checks against the manifest may be added later, but the first version should avoid needing the manifest path for per-agent validation.

## Executor coverage

Prompt injection lives in `handoff::dispatch_agent`. Every code path that funnels through the Rust scheduler inherits the deviation-journal protocol automatically:

- `plan-executor execute <tasks.json>` (default daemon mode)
- `plan-executor execute --foreground <tasks.json>` (in-session/foreground)
- `plan-executor execute --remote <tasks.json>` (GHA runner; foreground binary on the runner)

Out of scope for this design: in-session Claude orchestrators that dispatch sub-agents through the Agent tool instead of the binary (e.g. `superpowers:subagent-driven-development`, hypothetical `plan-executor:execute-plan` skill). Those would need a parallel implementation that injects the same block into the per-task prompt files they construct and that reads/digests the journal between waves. They are intentionally not included in this work.

## Prompt injection

The daemon injects a deviation-journal block into non-bash sub-agent prompts at dispatch time, next to the existing hygiene preamble. Compile-plan does not write this block into task files.

The injected block includes:

- journal path,
- `job_id`,
- phase name,
- `wave_id`,
- `task_id`,
- `agent_index`,
- validator command.

Example:

```markdown
## Deviation journal

If you discover a mismatch between this task and the codebase, or you intentionally skip/substitute/scope-change part of the task, write a validated journal entry.

Constants for this task:
- journal_path: `<execution_root>/.plan-executor/deviations.jsonl`
- job_id: `07cfc3cd-...`
- phase: `wave_execution`
- wave_id: `3`
- task_id: `18`
- agent_index: `2`

Protocol:
1. Create one JSON object matching `plan-executor validate --schema=deviation-journal-entry`.
2. Validate it with `plan-executor validate --schema=deviation-journal-entry -`.
3. Append it as one line to `journal_path` only after validation passes.
4. Do not ask the user. Do not use the journal to justify incomplete work. If a required task cannot be completed, fail explicitly.
```

The scheduler should inject IDs instead of making agents infer them. This avoids bad entries caused by guessed task IDs or paths.

## Read and consume path

Later phases consume a digest, not the raw unbounded journal. The digest is generated by Rust after validating the journal and re-checking size bounds.

Example digest:

```text
Prior deviation journal entries relevant to this phase:
- Task 18 / skip / warning:
  Claim: Task 18 appears already implemented in SKILL.md.
  Evidence: plugins/inline-discussion/skills/discuss/SKILL.md:31-59.
  Impact: Validator should verify those lines before flagging Task 18 missing.
```

### Consumers

- **Later wave prompts** receive entries from completed earlier tasks when those tasks are direct dependencies or same-file context.
- **Code review helper input** includes all valid entries, summarized, and tells reviewers to verify evidence before accepting a claim.
- **Validation helper input** includes all valid entries and changes validator policy to: pass only when each plan requirement is implemented in code or covered by an evidenced deviation whose evidence still verifies.
- **Fix-wave prompts** include entries for the same file plus repository-wide critical entries near the cumulative diff.
- **Summary and PR body** include a short `Plan deviations` section for `skip`, `substitute`, `scope_change`, and unresolved `blocker`; routine `discovery` entries are omitted unless critical.

Journal entries are advisory. They never auto-pass a requirement. They tell later stages where to look and why.

## Trust model

The journal is untrusted input until validated and re-verified.

- `claim`, `impact`, and `recommended_followup` are hints.
- `evidence` is the only load-bearing part.
- Later stages must re-read file lines, command logs, test logs, or commits before relying on an entry.
- Unevidenced `skip`, `substitute`, and `scope_change` entries are invalid.
- Stale evidence fails closed: if the file/lines no longer support the claim, validation treats the requirement as unmet.

## Failure handling

- If an agent cannot validate an entry, it does not append it.
- If append fails, the task may still complete, but the final report must include `DEVIATION_JOURNAL_APPEND_FAILED`.
- If the journal contains malformed lines, consumers skip those lines, emit a display/log warning, and do not trust them.
- If a `critical` `blocker` entry exists, validation fails unless later code or a later corrective journal entry resolves it.
- A journal write never changes wave success by itself. The task still succeeds or fails based on the actual task outcome.

## Bounds and prompt control

Initial bounds:

- Max entry size: 16 KiB.
- Max journal file size consumed into one digest: 256 KiB.
- Max digest injected into a prompt: 200 lines or 32 KiB.

Digest selection prefers:

1. critical entries,
2. entries for files touched by the current prompt,
3. entries in the task dependency chain,
4. entries with category `skip`, `substitute`, or `scope_change`,
5. recent `discovery` entries.

Raw command output is not embedded. Journal entries reference log paths and summarize.

## State, resume, and artifacts

The journal lives under the execution worktree so resume sees the same file. The job store also copies it into job artifacts on step completion or final summary so it survives worktree cleanup.

Append-only writes avoid conflict-prone rewrites during parallel waves. Agents append one JSON object plus newline. The reader tolerates line order; ordering is by `created_at` and file order only for display.

## Implementation components

### 1. Schema and validator

Add Rust types and schemas:

- `DeviationJournalEntry`
- `DeviationEvidence`
- `DeviationCategory`
- `DeviationSeverity`

Extend `plan-executor validate --schema=...` with:

- `deviation-journal-entry`
- `deviation-journal`

Tests:

- valid entry passes,
- `skip` without evidence fails,
- malformed JSONL line fails file validation,
- oversized entry fails,
- invalid enum value fails,
- RFC3339 timestamp parse is enforced.

### 2. Journal module

Add `src/deviation_journal.rs` with:

- `journal_path(execution_root: &Path) -> PathBuf`,
- `validate_entry_json(bytes: &[u8]) -> Result<DeviationJournalEntry, Error>`,
- `read_valid_entries(path: &Path) -> (Vec<DeviationJournalEntry>, Vec<Warning>)`,
- `digest(entries: &[DeviationJournalEntry], scope: DigestScope) -> String`,
- `archive_to_job(job_dir: &Path, execution_root: &Path)`.

`DigestScope` variants:

- `All`,
- `ChangedFiles(Vec<PathBuf>)`,
- `TaskDependencyContext { task_ids: Vec<String> }`,
- `FileSpecific(PathBuf)`.

### 3. Handoff prompt injection

Extend the dispatch path so each non-bash handoff prompt gets the deviation-journal block. The scheduler already knows the manifest wave/task structure; if the current `Handoff` type does not carry enough metadata, add optional metadata fields or a wrapper used before dispatch.

Do not put this in compile-plan. Compiled prompt files remain plan-body passthrough.

### 4. Helper inputs

Extend helper sidecars with optional journal fields:

- `deviation_journal_path`,
- `deviation_digest`.

Update helper schemas and prompt bodies so review/validation helpers include the digest and enforce the trust model.

### 5. Fix prompts

When compiling fix waves or invoking fix helpers, include file-specific journal digest near the cumulative diff. Fix agents should preserve prior evidenced deviations unless directly contradicted by the finding they are fixing.

### 6. Summary/PR output

Add `Plan deviations` to final summary and PR body when notable entries exist:

- `skip`,
- `substitute`,
- `scope_change`,
- unresolved `blocker`.

Each summary item includes task id, category, claim, and evidence pointer.

## Acceptance criteria

- A daemon wave sub-agent can validate and append a `skip` deviation entry with file-line evidence.
- A malformed entry is rejected by `plan-executor validate --schema=deviation-journal-entry -` before append.
- `plan-executor validate --schema=deviation-journal <path>` validates JSONL line-by-line and reports line-specific errors.
- Review and validation helper prompts include a bounded journal digest.
- Validation does not pass solely because a deviation exists; it passes only when evidence still verifies.
- Resume preserves and consumes the existing journal.
- Final summary includes notable deviations.
- Existing plan execution without deviations behaves unchanged.
