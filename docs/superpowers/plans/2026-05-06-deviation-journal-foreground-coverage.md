# Deviation Journal — Foreground Coverage Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Pin foreground-mode coverage of the deviation-journal protocol with a focused integration test.

**Architecture:** Add one integration test that exercises the prompt-injection helper directly. No production code change is required if the test surface from the original deviation-journal plan (Task 4) is `pub` enough to be reachable from `tests/`.

**Tech Stack:** Rust, `tempfile`, `cargo test`.

**Prerequisite:** Tasks 1–9 of `docs/superpowers/plans/2026-05-06-deviation-journal.md` have landed. In particular Task 4 must have shipped `ensure_deviation_block_in_prompt`, `DeviationContext`, and `DEVIATION_MARKER` (or an equivalent public marker) under `crate::handoff`.

---

## File Structure

**Create:**
- `tests/deviation_journal_foreground.rs` — integration test that invokes the public prompt-injection helper.

**Modify:**
- `src/handoff.rs` (only if `ensure_deviation_block_in_prompt`, `DeviationContext`, or `DEVIATION_MARKER` are not already `pub` after Task 4). Expose them as `pub` with no other behavior change.

---

### Task 1: Verify prerequisites

**Files:**
- Read-only check.

- [ ] **Step 1: Confirm Task 4 of the original deviation-journal plan is merged**

Run:

```bash
grep -n 'ensure_deviation_block_in_prompt\|DeviationContext\|DEVIATION_MARKER' src/handoff.rs
```

Expected output: matches for all three names. If any are missing, stop and execute the relevant steps of the original plan first; this follow-up plan depends on them.

- [ ] **Step 2: Confirm the names are publicly reachable**

Run:

```bash
grep -nE 'pub (struct DeviationContext|fn ensure_deviation_block_in_prompt|const DEVIATION_MARKER)' src/handoff.rs
```

Expected: all three are `pub`. If any are private, proceed to Task 2 to expose them; otherwise skip Task 2.

---

### Task 2: Expose prompt-injection helper to integration tests (only if needed)

**Files:**
- Modify: `src/handoff.rs`

Skip this task entirely if the prerequisite check in Task 1 already shows `pub` visibility for all three names.

- [ ] **Step 1: Make symbols public without changing behavior**

In `src/handoff.rs`, change visibility:

```rust
// before
const DEVIATION_MARKER: &str = "Deviation journal (plan-executor enforced";

// after
pub const DEVIATION_MARKER: &str = "Deviation journal (plan-executor enforced";
```

```rust
// before
struct DeviationContext { ... }
fn ensure_deviation_block_in_prompt(path: &Path, ctx: &DeviationContext) { ... }

// after
pub struct DeviationContext { ... }
pub fn ensure_deviation_block_in_prompt(path: &Path, ctx: &DeviationContext) { ... }
```

Each `DeviationContext` field that the test reads must also be `pub`. Mark them all `pub`:

```rust
pub struct DeviationContext {
    pub journal_path: PathBuf,
    pub job_id: String,
    pub phase: String,
    pub wave_id: Option<u32>,
    pub task_id: Option<String>,
    pub agent_index: usize,
    pub prior_digest: Option<String>,
}
```

- [ ] **Step 2: Run focused unit tests**

```bash
cargo test --lib handoff::
```

Expected: all existing handoff tests pass.

- [ ] **Step 3: Commit**

```bash
git add src/handoff.rs
git commit -m "refactor(handoff): expose deviation prompt helpers to integration tests"
```

---

### Task 3: Add the foreground integration test

**Files:**
- Create: `tests/deviation_journal_foreground.rs`

- [ ] **Step 1: Write the failing test**

Create `tests/deviation_journal_foreground.rs`:

```rust
use std::fs;

use plan_executor::deviation_journal::journal_path;
use plan_executor::handoff::{ensure_deviation_block_in_prompt, DeviationContext, DEVIATION_MARKER};

#[test]
fn deviation_block_is_injected_for_foreground_handoff() {
    let dir = tempfile::tempdir().expect("tempdir");
    let prompt = dir.path().join("task-1.md");
    fs::write(&prompt, "# Task body\n").expect("write prompt");

    let ctx = DeviationContext {
        journal_path: journal_path(dir.path()),
        job_id: "test-job".into(),
        phase: "wave_execution".into(),
        wave_id: Some(1),
        task_id: Some("1".into()),
        agent_index: 1,
        prior_digest: None,
    };

    ensure_deviation_block_in_prompt(&prompt, &ctx);

    let body = fs::read_to_string(&prompt).expect("read prompt");
    assert!(
        body.contains(DEVIATION_MARKER),
        "prompt missing deviation marker: {body}"
    );
    assert!(
        body.contains(&format!("journal_path: `{}`", ctx.journal_path.display())),
        "prompt missing journal path: {body}"
    );
    assert!(
        body.contains("task_id: `1`"),
        "prompt missing task_id: {body}"
    );
    assert!(
        body.contains("job_id: `test-job`"),
        "prompt missing job_id: {body}"
    );
}

#[test]
fn deviation_block_injection_is_idempotent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let prompt = dir.path().join("task-2.md");
    fs::write(&prompt, "# Task body\n").expect("write prompt");

    let ctx = DeviationContext {
        journal_path: journal_path(dir.path()),
        job_id: "test-job".into(),
        phase: "wave_execution".into(),
        wave_id: Some(1),
        task_id: Some("2".into()),
        agent_index: 1,
        prior_digest: None,
    };

    ensure_deviation_block_in_prompt(&prompt, &ctx);
    let after_first = fs::read_to_string(&prompt).expect("read prompt");

    ensure_deviation_block_in_prompt(&prompt, &ctx);
    let after_second = fs::read_to_string(&prompt).expect("read prompt");

    assert_eq!(
        after_first, after_second,
        "second injection must be a no-op; got\n--- first ---\n{after_first}\n--- second ---\n{after_second}"
    );
}
```

- [ ] **Step 2: Run the test (expect failure if Task 4 has not landed)**

```bash
cargo test --test deviation_journal_foreground
```

Expected: 2/2 pass. If `ensure_deviation_block_in_prompt`, `DeviationContext`, or `DEVIATION_MARKER` are missing, the test will fail to compile — that is the prerequisite check; finish the original plan before proceeding.

- [ ] **Step 3: Run focused unit suites**

```bash
cargo test --lib deviation_journal:: handoff::
```

Expected: existing unit tests still pass.

- [ ] **Step 4: Commit**

```bash
git add tests/deviation_journal_foreground.rs
git commit -m "test(deviation-journal): assert foreground handoff prompt injection"
```

---

### Task 4: Confirm the test fails when injection regresses

**Files:**
- Read-only check on top of the merged test.

This task does not change source. It is a one-shot manual smoke check that the new test actually catches a regression. Run only locally; do not commit.

- [ ] **Step 1: Temporarily disable injection**

In `src/handoff.rs`, comment out the call to `ensure_deviation_block_in_prompt` inside `dispatch_agent` (or whichever function applies the helper). Save.

- [ ] **Step 2: Run only the new test**

```bash
cargo test --test deviation_journal_foreground
```

Expected: the test still passes because it calls `ensure_deviation_block_in_prompt` directly. That confirms it is exercising the helper, not the dispatch wrapper.

- [ ] **Step 3: Confirm catch by mutating the helper**

Change the helper's marker constant:

```rust
pub const DEVIATION_MARKER: &str = "ZZZ_BROKEN_MARKER";
```

Run:

```bash
cargo test --test deviation_journal_foreground
```

Expected: BOTH new tests fail with "prompt missing deviation marker" or related assertion failures.

- [ ] **Step 4: Restore the source**

```bash
git restore src/handoff.rs
```

Confirm clean tree:

```bash
git status --short
```

Expected: empty.

- [ ] **Step 5: No commit**

This is a manual smoke check only. Do not commit.
