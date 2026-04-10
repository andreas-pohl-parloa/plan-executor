# Design: Remote Execution via GitHub Actions

**Date:** 2026-04-08
**Status:** Approved
**Scope:** Add remote plan execution via GitHub Actions using a PR-per-execution model in a dedicated execution repo, a foreground execution mode, and a setup wizard.

---

## Problem

plan-executor currently runs plans locally via a background daemon. There is no way to offload execution to a remote environment (CI, cloud runner). This limits execution to machines where the daemon is running and blocks use cases like unattended batch runs, execution from machines without GPU/resources, and shared team execution infrastructure.

---

## Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Execution target | Plan header field (`**execution:** remote`) | Plan declares its own execution mode; `execute` routes accordingly |
| Target repo | Inferred from `git remote get-url origin` | No manual specification needed; natural for repo-scoped plans |
| Execution repo config | `remote_repo` in `config.json` | Infrastructure concern, not plan-specific |
| Trigger mechanism | PR-per-execution in execution repo | Audit trail, notifications, natural status model (open=running, closed=done) |
| Branch convention | `exec/<timestamp>-<plan-filename>` | Unique, sortable, descriptive |
| Remote handoff handling | Reuse plan-executor via foreground mode (`-f`) | 100% code reuse, no shell-script reimplementation |
| Result reporting | PR comments for summary, workflow UI for detailed logs | Simple, no artifact management needed |
| Auth for target repo cloning | Fine-grained PAT stored as repo secret | Execution repo's `GITHUB_TOKEN` is scoped to its own repo only |
| Agent auth | Secrets in execution repo (via `gh secret set`) | No sensitive files in git |
| Codex OAuth refresh | Idempotent `gh secret set` on every remote execute | Token always fresh without manual intervention |

---

## Section 1: Plan Header Changes

New optional header field in plan markdown files:

```markdown
**execution:** remote
```

Values: `local` (default if omitted) or `remote`.

### Changes to `src/plan.rs`

New enum and parser:

```rust
pub enum ExecutionMode {
    Local,
    Remote,
}

pub fn parse_execution_mode(path: &Path) -> ExecutionMode
```

Scans for `**execution:**` line, same pattern as `parse_plan_status()`. Returns `ExecutionMode::Local` when the field is absent or unrecognized.

Both the daemon's auto-execute path and the `execute` CLI command check execution mode before routing to local or remote execution.

---

## Section 2: Config Changes

New optional field in `~/.plan-executor/config.json`:

```json
{
  "remote_repo": "owner/plan-executions",
  "watch_dirs": ["..."],
  "plan_patterns": ["..."],
  "auto_execute": false,
  "agents": { "..." }
}
```

### Changes to `src/config.rs`

- New field: `remote_repo: Option<String>` on `Config`, defaults to `None`
- When a plan has `**execution:** remote` and `remote_repo` is `None`, execution fails with: `"remote execution requires 'remote_repo' in config — run 'plan-executor remote-setup'"`

---

## Section 3: Foreground Execution Mode (`execute -f`)

New `-f` / `--foreground` flag on the `Execute` CLI command.

When set, plan-executor runs the full execution loop in-process without requiring the daemon:

1. Resolve plan path, find repo root
2. Call `spawn_execution()` — same function the daemon uses
3. Process `ExecEvent` stream directly:
   - `DisplayLine` → print to stdout (with color formatting via `print_display_line`)
   - `HandoffRequired` → load state file, `dispatch_all()` sub-agents, `resume_execution()` — same handoff module
   - `Finished` → print final status, exit
4. Exit code: 0 = success, 1 = failure/killed

This reuses all existing execution and handoff logic from `executor.rs` and `handoff.rs`. No daemon, no Unix socket, no TUI.

### Changes to `src/cli.rs`

```rust
Commands::Execute {
    plan: String,
    #[arg(short = 'f', long)]
    foreground: bool,
}
```

New `execute_foreground()` async function that implements the self-contained event loop.

The daemon's `run_exec_event_loop` and the new foreground loop share the same handoff dispatch logic. Where the daemon writes to shared state and broadcasts events, the foreground loop prints directly and manages a single job.

---

## Section 4: Remote Execution Trigger

When `plan-executor execute <plan>` detects `**execution:** remote` (with or without `-f`):

### Step 1: Gather context

- Read plan file content
- `git remote get-url origin` → extract `owner/repo` as target repo
- `git rev-parse HEAD` → capture SHA as checkout ref
- `git rev-parse --abbrev-ref HEAD` → capture branch name
- Scan for co-located `.tmp-subtask-*.md` prompt files in the plan's directory

### Step 2: Push auth tokens (idempotent)

- If `~/.codex/auth.json` exists: `gh secret set CODEX_AUTH --repo <execution-repo> < ~/.codex/auth.json`
- This ensures the OAuth token is always fresh

### Step 3: Create branch in execution repo

Branch name: `exec/<YYYYMMDD-HHMMSS>-<plan-filename-without-extension>`

Files pushed to branch:
- `plan.md` — the plan file content
- `prompt-files/<name>.md` — any `.tmp-subtask-*.md` files (preserving names)
- `execution.json` — metadata:

```json
{
  "target_repo": "owner/repo",
  "target_ref": "<sha>",
  "target_branch": "<branch>",
  "plan_filename": "<original-filename.md>",
  "started_at": "<ISO 8601>"
}
```

### Step 4: Create PR

- `gh pr create` against `main` of the execution repo
- Title: `exec: <plan-filename> @ owner/repo`
- Body: target repo, ref, plan filename, link to source commit
- Labels: `execution` (if label exists)

### Step 5: Report to user

```
Remote execution triggered.
PR: https://github.com/owner/plan-executions/pull/42
```

### Dependencies

- `gh` CLI authenticated and on PATH
- `remote_repo` configured in `config.json`

### Daemon integration

When the daemon detects a READY plan with `**execution:** remote`, it routes through the same remote trigger path instead of local `spawn_execution`. The daemon does not track remote job state — that lives in the execution repo's PRs.

---

## Section 5: GitHub Actions Workflow

The execution repo contains `.github/workflows/execute-plan.yml`.

### Trigger

```yaml
on:
  pull_request:
    types: [opened]
    branches: ['exec/**']
```

### Secrets required

| Secret | Source | Purpose |
|--------|--------|---------|
| `TARGET_REPO_TOKEN` | Fine-grained PAT (org, Contents:Read) | Clone target repos |
| `ANTHROPIC_API_KEY` | Anthropic API key | Claude agent |
| `CODEX_AUTH` | `~/.codex/auth.json` content | Codex OAuth (pushed on each remote execute) |
| `OPENAI_API_KEY` | OpenAI API key (optional, alternative to CODEX_AUTH) | Codex API key auth |
| `GEMINI_API_KEY` | Google AI API key | Gemini agent |

### Workflow steps

```yaml
jobs:
  execute:
    runs-on: ubuntu-latest
    steps:
      - name: Checkout execution repo
        uses: actions/checkout@v4

      - name: Parse execution metadata
        id: meta
        run: |
          # Read execution.json, export target_repo, target_ref, plan_filename

      - name: Clone target repo
        run: |
          git clone https://x-access-token:${{ secrets.TARGET_REPO_TOKEN }}@github.com/${{ steps.meta.outputs.target_repo }}.git workspace
          cd workspace && git checkout ${{ steps.meta.outputs.target_ref }}

      - name: Install tooling
        run: |
          # Install claude CLI
          # Install codex CLI
          # Install gemini-cli
          # Install my-coding plugin (includes plan-executor):
          bash -c "$(gh api 'repos/andreas-pohl-parloa/my-coding/contents/install.sh' \
            --header 'Accept: application/vnd.github.raw')"

      - name: Restore agent auth
        run: |
          # Codex OAuth
          if [ -n "${{ secrets.CODEX_AUTH }}" ]; then
            mkdir -p ~/.codex
            echo '${{ secrets.CODEX_AUTH }}' > ~/.codex/auth.json
          fi
        env:
          ANTHROPIC_API_KEY: ${{ secrets.ANTHROPIC_API_KEY }}
          OPENAI_API_KEY: ${{ secrets.OPENAI_API_KEY }}
          GEMINI_API_KEY: ${{ secrets.GEMINI_API_KEY }}

      - name: Copy plan into target repo
        run: |
          cp plan.md workspace/.my/plans/${{ steps.meta.outputs.plan_filename }}
          cp prompt-files/* workspace/.my/plans/ 2>/dev/null || true

      - name: Execute plan
        run: |
          cd workspace
          plan-executor execute -f .my/plans/${{ steps.meta.outputs.plan_filename }}
        env:
          ANTHROPIC_API_KEY: ${{ secrets.ANTHROPIC_API_KEY }}
          OPENAI_API_KEY: ${{ secrets.OPENAI_API_KEY }}
          GEMINI_API_KEY: ${{ secrets.GEMINI_API_KEY }}

      - name: Post result
        if: always()
        run: |
          STATUS=$( [ ${{ steps.execute.outcome }} = "success" ] && echo "succeeded" || echo "failed" )
          gh pr comment ${{ github.event.pull_request.number }} --body "## Execution Complete
          **Status:** ${STATUS}
          **Target:** ${{ steps.meta.outputs.target_repo }}@${{ steps.meta.outputs.target_ref }}"
          gh pr close ${{ github.event.pull_request.number }}
          gh pr edit ${{ github.event.pull_request.number }} --add-label "${STATUS}" || true
        env:
          GH_TOKEN: ${{ github.token }}
```

---

## Section 6: Remote Job Status

When `remote_repo` is configured, `plan-executor jobs` appends remote execution status below the local jobs table.

### Display format

```
ID        PLAN                          STATUS     DURATION
──────────────────────────────────────────────────────────
a1b2c3    plan-foo.md                   success    142s

Remote (owner/plan-executions):
PR        PLAN                          STATUS     TARGET
──────────────────────────────────────────────────────────
#12       plan-baz.md                   running    owner/repo@abc123
#11       plan-qux.md                   succeeded  owner/repo@def456
```

### Implementation

- Query: `gh pr list --repo <remote_repo> --label execution --state all --limit 20 --json number,title,state,labels`
- Parse title (`exec: <plan-filename> @ owner/repo`) for plan name and target
- Status mapping: open PR = `running`, closed + `succeeded` label = `succeeded`, closed + `failed` label = `failed`
- Synchronous `gh` CLI call, no daemon involvement

---

## Section 7: `remote-setup` Command

Interactive CLI wizard for first-time configuration of the execution repo and its secrets.

### New CLI subcommand

```rust
Commands::RemoteSetup
```

### Flow

```
$ plan-executor remote-setup

Execution repo [owner/plan-executions]: andreas-pohl-parloa/plan-executions
  Saved to ~/.plan-executor/config.json

GitHub PAT for cloning org repos:
  Create one at: https://github.com/settings/personal-access-tokens/new
  Scope: your org, permission: Contents -> Read
  Paste token: ****
  Stored as TARGET_REPO_TOKEN

Anthropic API key: ****
  Stored as ANTHROPIC_API_KEY

Codex auth — (o)auth / (a)pi key / (s)kip: o
  Read ~/.codex/auth.json
  Stored as CODEX_AUTH

Gemini API key (enter to skip): ****
  Stored as GEMINI_API_KEY

Setup complete. Remote execution ready.
```

### Implementation

- Reads input from stdin (interactive terminal prompts)
- Each secret: `gh secret set <NAME> --repo <execution-repo> --body <value>`
- For Codex OAuth: reads `~/.codex/auth.json` file content
- Updates `config.json` with `remote_repo` if changed
- Idempotent — re-running overwrites existing secrets
- Validates `gh` CLI is available and authenticated before starting

---

## Out of Scope

- Remote kill/cancel (use GitHub Actions UI to cancel workflow)
- Remote output streaming (check workflow logs in GitHub UI)
- Remote pause/unpause
- Workflow for non-`my-coding` execution environments
- Multiple execution repos

---

## References

- `docs/superpowers/specs/2026-04-01-plan-executor-handoff-protocol-design.md` — handoff protocol design
- `~/workspace/code/my-coding/install.sh` — my-coding plugin installer (installs plan-executor)
- `~/workspace/code/my-coding/README.md` — plugin structure and skill list
