# plan-executor

A daemon that executes and orchestrates Claude plan files. Supports local foreground execution, daemon-managed background execution with desktop notifications, and remote execution via GitHub Actions.

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
plan-executor daemon             # Start the background daemon
plan-executor daemon --foreground # Run without daemonizing (for supervisors)
plan-executor execute <plan>     # Execute a plan via the daemon
plan-executor execute -f <plan>  # Execute in foreground (no daemon)
plan-executor jobs               # List job history
plan-executor output <id>        # Show a job's output
plan-executor output -f <id>     # Follow a running job's output
plan-executor kill <id>          # Kill a running job
plan-executor pause <id>         # Pause at the next handoff
plan-executor unpause <id>       # Resume a paused job
plan-executor retry <id>         # Re-dispatch a failed job's handoff
plan-executor status             # Check daemon status
plan-executor stop               # Stop the daemon
plan-executor ensure             # Start daemon if not running (shell hook)
plan-executor remote-setup       # Configure remote execution
```

Job IDs support prefix matching -- `plan-executor output a3f` matches `a3f7b2c1-...`.

## How It Works

plan-executor orchestrates the `plan-executor:execute-plan-non-interactive` skill through a file-based handoff protocol:

1. The main agent reads the plan, decomposes it into waves, writes prompt files, and outputs `call sub-agent` lines
2. plan-executor captures the handoff lines, dispatches sub-agents (Claude, Codex, Gemini, or bash scripts) in parallel
3. Sub-agent outputs are collected and fed back to the main agent via `--resume`
4. The cycle repeats through implementation, code review, plan validation, and PR creation

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
   - `TARGET_REPO_TOKEN` -- GitHub PAT for cloning target repos and accessing releases
   - `ANTHROPIC_API_KEY` -- for Claude
   - `OPENAI_API_KEY` or `CODEX_AUTH` -- for Codex
   - `GEMINI_API_KEY` -- for Gemini
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
| `**add-marketplaces:**` | No | Comma-separated Claude plugin marketplace repos to install |
| `**add-plugins:**` | No | Comma-separated Claude plugins to install (`name@marketplace`) |

The `andreas-pohl-parloa/plan-executor-plugin` marketplace and `plan-executor@plan-executor` plugin are always installed automatically.

### Remote Execution Flow

1. `plan-executor execute <plan>` detects `**execution:** remote`
2. Pushes the plan + metadata to the execution repo on an `exec/` branch
3. Creates a PR which triggers the GitHub Actions workflow
4. The workflow clones the target repo, installs tools, runs the plan
5. Posts an execution summary as a PR comment, closes the PR, adds a `succeeded`/`failed` label
6. The local daemon monitors the PR and updates the plan status to `COMPLETED` or `FAILED`

## Configuration

Config lives at `~/.plan-executor/config.json`:

```json
{
  "agents": {
    "main": "claude --dangerously-skip-permissions --verbose --output-format stream-json",
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
| `agents.main` | Main orchestrator command (must produce stream-json output) |
| `agents.claude` | Claude sub-agent command |
| `agents.codex` | Codex sub-agent command |
| `agents.gemini` | Gemini sub-agent command |
| `agents.bash` | Shell script runner for bash handoffs |
| `remote_repo` | GitHub repo slug for remote execution (set via `remote-setup`) |

All agent fields have sensible defaults and can be omitted.

## Data

- `~/.plan-executor/config.json` -- configuration
- `~/.plan-executor/daemon.pid` -- daemon PID file
- `~/.plan-executor/daemon.log` -- daemon log
- `~/.plan-executor/icon.png` -- notification icon
- `~/.plan-executor/jobs/<id>/` -- per-job data:
  - `metadata.json` -- job status, tokens, timing
  - `output.jsonl` -- raw stream-json output
  - `display.log` -- rendered display output

## License

Internal use only.
