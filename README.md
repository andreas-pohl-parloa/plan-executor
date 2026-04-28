# plan-executor

A daemon that executes Claude plan files via a Rust-driven scheduler. Supports local foreground execution, daemon-managed background execution with desktop notifications, and remote execution via GitHub Actions.

> **Note:** This tool is distributed as part of the `plan-executor` Claude plugin in [andreas-pohl-parloa/plan-executor-plugin](https://github.com/andreas-pohl-parloa/plan-executor-plugin). The recommended way to install is via that plugin, which handles binary downloads and plugin registration automatically. The standalone install below is for development or CI use.

## Install

```bash
bash -c "$(gh api 'repos/andreas-pohl-parloa/plan-executor/contents/install.sh?ref=main' --header 'Accept: application/vnd.github.raw')"
```

Or clone and run:

```bash
git clone git@github.com:andreas-pohl-parloa/plan-executor.git
cd plan-executor
./install.sh
```

The installer downloads a pre-built binary (macOS ARM64, Linux x86_64/ARM64), starts the daemon, adds a shell hook for auto-start, and optionally runs `remote-setup`.

## Usage

```bash
# Daemon control
plan-executor daemon                  # Start the background daemon
plan-executor daemon --foreground     # Run without daemonizing (for supervisors)
plan-executor stop                    # Stop the daemon
plan-executor status                  # Check daemon status
plan-executor ensure                  # Start daemon if not running (shell hook)

# Plan execution
plan-executor execute <manifest>      # Execute a compiled plan manifest via the daemon
plan-executor execute -f <manifest>   # Execute in foreground (no daemon)
plan-executor validate <manifest>     # Validate a tasks.json manifest against the schema

# Standalone framework jobs
plan-executor run pr-finalize --pr N [--merge | --merge-admin] [--owner X --repo Y]
                                      # Run the 5-step pr-finalize pipeline as a Job

# Job inspection + control
plan-executor jobs                    # List job history (default subcommand)
plan-executor jobs show <id>          # Show full step/attempt history for a job
plan-executor jobs cancel <id>        # Mark a job as cancelled
plan-executor jobs gc --older-than 7d # Garbage-collect completed jobs
plan-executor jobs replay <id> [--from-step N]
                                      # Re-run a job from step N
plan-executor jobs metrics [--since DURATION] [--job-kind KIND] [--format json|text]
                                      # Aggregate per-job recovery metrics

plan-executor output <id>             # Show a job's output
plan-executor output -f <id>          # Follow a running job's output
plan-executor kill <id>               # Kill a running job
plan-executor pause <id>              # Pause at the next handoff
plan-executor unpause <id>            # Resume a paused job

# Fix-loop / remote
plan-executor compile-fix-waves       # Append fix waves to tasks.json from reviewer findings
plan-executor remote-setup            # Configure remote execution
```

Job IDs support prefix matching — `plan-executor output a3f` matches `a3f7b2c1-...`.

### Compiling a plan markdown into a manifest

`plan-executor execute` takes a compiled manifest (`tasks.json`), not raw markdown. To turn a plan markdown file into a manifest, run the `plan-executor:handover` and `plan-executor:compile-plan` skills from a Claude session — both are shipped with the `plan-executor` plugin. The plugin's slash commands wrap this end-to-end.

## How It Works

The Rust scheduler drives plan jobs natively — no orchestrator skill, no file-based handoff protocol. Each `JobKind` (`Plan`, `PrFinalize`, future kinds) maps to a registry of `Step` impls that the scheduler walks in order.

### `JobKind::Plan` pipeline

1. The compiled manifest (`tasks.json`) is loaded by `crate::scheduler::load_manifest`.
2. `WaveExecutionStep` invokes `crate::scheduler::run_wave_execution` which traverses waves in topological order (Kahn's algorithm, deterministic ascending-id tie-break) and dispatches each wave's tasks as parallel Claude / Codex / Gemini / bash sub-agents via `crate::handoff::dispatch_all`.
3. `IntegrationTestingStep`, `CodeReviewStep`, `ValidationStep`, `PrCreationStep`, and `SummaryStep` run in sequence after wave execution. Code review and validation invoke helper skills (`run-reviewer-team-non-interactive`, `review-execution-output-non-interactive`, `validate-execution-plan-non-interactive`) via `crate::helper::invoke_helper` — a structured-I/O subprocess call with schema-validated output.
4. On `fix_required` from review or validation, the step calls `compile-fix-waves` in APPEND mode, scopes a sub-`Manifest` to the newly-appended waves, and re-enters `run_wave_execution`. The fix-loop is capped at 3 iterations and 2 hours wall-clock.
5. `JobMetrics` records every attempt + recovery; `JobDir::write_metrics` persists `metrics.json` on every terminal exit.

### `JobKind::PrFinalize` pipeline

1. `PrLookupStep` — `gh pr view` to capture HEAD SHA, owner, repo, draft state.
2. `MarkReadyStep` — `gh pr ready` (skipped if already ready).
3. `MonitorStep` — invokes `pr-monitor.sh` with a 45-minute bounded timeout; the script handles its own retry on transient failures.
4. `MergeStep` — only if `--merge` or `--merge-admin`; `gh pr merge`.
5. `ReportStep` — writes a summary to `.tmp-execution-summary.md`.

Each step has its own `RecoveryPolicy` (e.g., `RetryTransient { max: 3, Exponential(500ms..8s) }` for `gh` API hiccups). Subprocess hygiene: stdin nulled, 60s `wait_timeout`, drainer threads on stdout/stderr.

### Helper subprocess sandbox

Every helper invocation runs `claude -p "/plan-executor:<helper>"` with `--allowed-tools "Read,Write,Edit"` + `--dangerously-skip-permissions` + `--add-dir <ctx.workdir>` so the helper's filesystem access is jailed to the working directory. Output JSON is schema-validated against the helper's `output.schema.json`; non-success status maps to `HelperError::SemanticFailure` carrying `state_updates` so callers can extract `findings_path`, `gaps`, etc.

Desktop notifications are shown when jobs start and finish (daemon mode only).

## Remote Execution

Plans with `**execution:** remote` in their header are executed on GitHub Actions runners instead of locally.

### Setup

Run the interactive wizard:

```bash
plan-executor remote-setup
```

This will:

1. **Create the execution repo** (private, e.g. `your-org/plan-executions`) if it doesn't exist
2. **Store secrets** in the execution repo:
   - `TARGET_REPO_TOKEN` — GitHub PAT for cloning target repos and accessing releases
   - `ANTHROPIC_API_KEY` — for Claude
   - `OPENAI_API_KEY` or `CODEX_AUTH` — for Codex
   - `GEMINI_API_KEY` — for Gemini
3. **Push the workflow** to `.github/workflows/execute-plan.yml`
4. **Generate a README** explaining the execution repo

### PAT Requirements

The `TARGET_REPO_TOKEN` must be a **classic** Personal Access Token with `repo` scope.

**SSO-enabled organizations:** If your target repos or plugin marketplaces are in an org that enforces SAML SSO, you must authorize the PAT for that org:

1. Go to https://github.com/settings/tokens
2. Find your classic PAT
3. Click **Configure SSO** and **Authorize** next to each org

### Plan Headers

```markdown
**status:** READY
**execution:** remote
**non-interactive:** [x]
**add-marketplaces:** org/marketplace-repo, other-org/other-marketplace
**add-plugins:** plugin-name@marketplace, another@marketplace
```

| Header | Required | Description |
|--------|----------|-------------|
| `**status:**` | Yes | Must be `READY` to trigger execution |
| `**execution:**` | Yes | Set to `remote` for GitHub Actions execution |
| `**non-interactive:**` | Yes | Must be `[x]` (checked) |
| `**add-marketplaces:**` | No | Comma-separated Claude plugin marketplace repos to install (must be on the workflow's allow-list) |
| `**add-plugins:**` | No | Comma-separated Claude plugins to install (`name@marketplace`; must be on the workflow's allow-list) |

The `andreas-pohl-parloa/plan-executor-plugin` marketplace and `plan-executor@plan-executor` plugin are always installed automatically.

> **Security:** the GHA workflow enforces an allow-list of marketplaces and plugins — values not on the list cause the workflow to fail closed. See [`docs/remote-execution/SECURITY.md`](docs/remote-execution/SECURITY.md) for the trust model + GPG-key reuse details.

### Remote Execution Flow

1. `plan-executor execute <manifest>` detects `**execution:** remote` (read from the manifest's `plan.flags`).
2. Pushes the manifest + plan onto an `exec/` branch in the execution repo.
3. Creates a PR which triggers the GitHub Actions workflow.
4. The workflow reads `job-spec.json` to dispatch by `kind`: `plan` runs the full plan pipeline; `pr-finalize` runs `plan-executor run pr-finalize`.
5. The workflow clones the target repo, verifies binary checksums against `<asset>.sha256` sidecars, installs tools, runs the job.
6. Posts an execution summary as a PR comment, closes the PR, adds a `succeeded`/`failed` label.
7. The local daemon monitors the PR and updates the plan status to `COMPLETED` or `FAILED`.

## Configuration

Config lives at `~/.plan-executor/config.json`:

```json
{
  "agents": {
    "claude": "claude --dangerously-skip-permissions --verbose --output-format stream-json -p",
    "codex": "codex --full-auto -q",
    "gemini": "gemini -s",
    "bash": "bash"
  },
  "remote_repo": "your-org/plan-executions"
}
```

| Field | Description |
|-------|-------------|
| `agents.claude` | Claude sub-agent command for wave dispatch + helper invocation |
| `agents.codex` | Codex sub-agent command |
| `agents.gemini` | Gemini sub-agent command |
| `agents.bash` | Shell-script runner for `agent_type: bash` tasks |
| `remote_repo` | GitHub repo slug for remote execution (set via `remote-setup`) |

All agent fields have sensible defaults and can be omitted.

## Data

`~/.plan-executor/` is created with mode `0700` so other users on the host cannot enumerate jobs.

- `~/.plan-executor/config.json` — configuration
- `~/.plan-executor/daemon.pid` — daemon PID file
- `~/.plan-executor/daemon.log` — daemon log
- `~/.plan-executor/icon.png` — notification icon
- `~/.plan-executor/jobs/<id>/` — per-job data (mode 0700):
  - `job.json` — `Job` record (`JobKind` + state)
  - `metadata.json` — legacy job-status snapshot (compat with older tooling)
  - `metrics.json` — `JobMetrics` snapshot (per-`AttemptOutcomeKind` / per-`RecoveryKind` counts, timestamps)
  - `steps/<NNN>-<step-name>/attempts/<n>/` — per-attempt outputs (`outcome.json`, `stderr.log`, sub-agent transcripts)
  - `output.jsonl` — raw daemon stream-json output (legacy layout, where applicable)
  - `display.log` — rendered display output

## License

Internal use only.
