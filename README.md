# plan-executor

A daemon that monitors, executes, and orchestrates Claude plan files. Supports local foreground execution, daemon-managed background execution, and remote execution via GitHub Actions.

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

## Usage

```bash
plan-executor daemon          # Start the background daemon
plan-executor execute <plan>  # Execute a plan (local or remote)
plan-executor execute -f <plan>  # Execute in foreground
plan-executor tui             # Open the TUI dashboard
plan-executor jobs            # List job history
plan-executor output -f <id>  # Follow a running job's output
plan-executor status          # Check daemon status
plan-executor scan            # Debug: list all discovered plans
plan-executor remote-setup    # Configure remote execution
```

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
3. **Push the workflow** to `.github/workflows/execute-plan.yml` in the execution repo

### PAT Requirements

The `TARGET_REPO_TOKEN` must be a **classic** Personal Access Token with `repo` scope.

**SSO-enabled organizations:** If your target repos or plugin marketplaces are in an org that enforces SAML SSO (e.g. Parloa), you must authorize the PAT for that org:

1. Go to https://github.com/settings/tokens
2. Find your classic PAT
3. Click **Configure SSO**
4. Click **Authorize** next to each org that enforces SSO

Without SSO authorization, cloning org repos will fail with a 403 error mentioning SAML SSO.

### Plan Headers

Remote plans use these headers:

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
| `**add-marketplaces:**` | No | Comma-separated list of Claude plugin marketplace repos to install |
| `**add-plugins:**` | No | Comma-separated list of Claude plugins to install (`name@marketplace`) |

The `andreas-pohl-parloa/my-coding` marketplace and `plan-executor@my-coding` plugin are always installed automatically.

### How It Works

1. `plan-executor execute <plan>` detects `**execution:** remote`
2. Pushes the plan + metadata to the execution repo on an `exec/` branch
3. Creates a PR which triggers the GitHub Actions workflow
4. The workflow clones the target repo, installs tools, runs the plan via Claude
5. Posts a result comment, closes the PR, and adds a `succeeded`/`failed` label
6. The local daemon monitors the PR and updates the plan status to `COMPLETED` or `FAILED`

## Configuration

Config lives at `~/.plan-executor/config.json`:

```json
{
  "watch_dirs": ["~/workspace/code", "~/tools"],
  "plan_patterns": ["**/.my/plans/*.md"],
  "auto_execute": false,
  "remote_repo": "your-org/plan-executions"
}
```

## License

Internal use only.
