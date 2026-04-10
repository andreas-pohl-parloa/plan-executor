# Add `jobs` subcommand

**Goal:** Add a `plan-executor jobs` subcommand that lists completed and running jobs from `~/.plan-executor/jobs/`, showing plan name, status, duration, and cost per job.
**Type:** Feature
**JIRA:** none
**Tech Stack:** Rust (edition 2024)
**Code Standards:** rust-services:production-code-recipe, rust-services:test-code-recipe
**Status:** COMPLETED
**no-worktree:** [x]
**no-pr:** [x]

---

## Context

The daemon stores job metadata at `~/.plan-executor/jobs/<uuid>/metadata.json`.
`JobMetadata::load_all()` already loads and sorts them descending by `started_at`.

The new subcommand should print a table to stdout:

```
ID       PLAN                          STATUS   DURATION  COST
──────────────────────────────────────────────────────────────
a1b2c3   plan-foo.md                   success  142s      $0.8421
d4e5f6   plan-bar.md                   failed   23s       -
```

Columns:
- **ID** — first 6 chars of UUID
- **PLAN** — filename only (not full path)
- **STATUS** — `success`, `failed`, `killed`, `running`
- **DURATION** — `duration_ms / 1000` + "s", or `-` if missing
- **COST** — `$N.NNNN` from `cost_usd`, or `-` if missing

Print "No jobs found." when `~/.plan-executor/jobs/` is empty or absent.

---

## Acceptance Criteria

- [ ] `plan-executor jobs` prints the header and one row per job
- [ ] Jobs are listed newest-first (already guaranteed by `load_all`)
- [ ] Empty state prints "No jobs found." and exits 0
- [ ] Column widths are padded so columns align
- [ ] Compiles with zero warnings

---

### Task 1: Add `Jobs` variant to `Commands` enum and implement handler

**Files:**
- Modify: `src/cli.rs`

**Step 1: Add variant**

In `src/cli.rs`, add to the `Commands` enum:
```rust
/// List job history
Jobs,
```

**Step 2: Handle in `run()`**

Add to the `match cli.command` block **before** the Tokio runtime is created (it is synchronous):

```rust
if matches!(cli.command, Commands::Jobs) {
    list_jobs();
    return;
}
```

**Step 3: Implement `list_jobs()`**

Add the following function to `src/cli.rs`:

```rust
fn list_jobs() {
    use crate::jobs::{JobMetadata, JobStatus};

    let jobs = JobMetadata::load_all();
    if jobs.is_empty() {
        println!("No jobs found.");
        return;
    }

    // Column widths
    let id_w = 8;
    let plan_w = 34;
    let status_w = 9;
    let dur_w = 10;
    let cost_w = 8;

    println!(
        "{:<id_w$}  {:<plan_w$}  {:<status_w$}  {:>dur_w$}  {:>cost_w$}",
        "ID", "PLAN", "STATUS", "DURATION", "COST",
        id_w = id_w, plan_w = plan_w, status_w = status_w,
        dur_w = dur_w, cost_w = cost_w,
    );
    println!("{}", "─".repeat(id_w + 2 + plan_w + 2 + status_w + 2 + dur_w + 2 + cost_w));

    for job in &jobs {
        let id = &job.id[..job.id.len().min(6)];

        let plan = job.plan_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?");
        let plan_truncated = if plan.len() > plan_w {
            format!("{}…", &plan[..plan_w - 1])
        } else {
            plan.to_string()
        };

        let status = match job.status {
            JobStatus::Success => "success",
            JobStatus::Failed  => "failed",
            JobStatus::Killed  => "killed",
            JobStatus::Running => "running",
        };

        let duration = job.duration_ms
            .map(|ms| format!("{}s", ms / 1000))
            .unwrap_or_else(|| "-".to_string());

        let cost = job.cost_usd
            .map(|c| format!("${:.4}", c))
            .unwrap_or_else(|| "-".to_string());

        println!(
            "{:<id_w$}  {:<plan_w$}  {:<status_w$}  {:>dur_w$}  {:>cost_w$}",
            id, plan_truncated, status, duration, cost,
            id_w = id_w, plan_w = plan_w, status_w = status_w,
            dur_w = dur_w, cost_w = cost_w,
        );
    }
}
```

**Step 4: Wire the unreachable arm**

Add `Commands::Jobs => unreachable!()` to the `match cli.command` block that dispatches async commands (after the `return` guard handles it synchronously).

**Step 5: Verify**

```bash
cd /Users/andreas.pohl/workspace/code/plan-executor && cargo build 2>&1
```

Expected: zero errors, zero warnings.

Smoke test:
```bash
./target/debug/plan-executor jobs
```

Expected: either "No jobs found." or a populated table.

**Step 6: Commit**
```
feat(VC-0): add jobs subcommand to list execution history
```

---

### Task 2: Unit test for `list_jobs` output format

**Files:**
- Create: `tests/jobs_subcommand_test.rs`

**Step 1: Test the formatting helpers**

The column-alignment logic in `list_jobs` is not easily unit-testable without extracting helpers. Instead, test the `JobMetadata::load_all()` empty-directory path and verify the binary output via a simple integration approach.

Create `tests/jobs_subcommand_test.rs`:

```rust
use std::process::Command;

/// Smoke test: `plan-executor jobs` exits 0 and prints a header or empty message.
#[test]
fn jobs_subcommand_exits_zero() {
    let bin = env!("CARGO_BIN_EXE_plan-executor");
    let output = Command::new(bin)
        .arg("jobs")
        .output()
        .expect("failed to run plan-executor");

    assert!(
        output.status.success(),
        "plan-executor jobs exited non-zero: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("No jobs found.") || stdout.contains("ID"),
        "unexpected output: {}", stdout
    );
}
```

**Step 2: Verify**

```bash
cd /Users/andreas.pohl/workspace/code/plan-executor && cargo test jobs_subcommand 2>&1
```

Expected: 1 test passes.

**Step 3: Commit**
```
test(VC-0): add smoke test for jobs subcommand
```

---

## Task Dependency Graph

```
Task 1 (add Jobs subcommand)
  └─> Task 2 (test)
```
