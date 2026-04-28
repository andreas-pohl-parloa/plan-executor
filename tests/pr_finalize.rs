//! Integration tests for [`plan_executor::job::steps::pr_finalize`].
//!
//! Tests construct each step struct directly and drive it via the
//! [`Step::run`] async method. External tools (`gh`, `pr-monitor.sh`) are
//! replaced by tiny shell-script fakes written into per-test temp dirs;
//! `PATH` is prepended for the duration of the test so the production
//! step code spawns the fake instead of the real binary.
//!
//! ### Test design
//!
//! ECP partitions (one test per partition) cover every documented
//! `AttemptOutcome` arm reachable through the public step API. The
//! `MergeStep` decision table is split across two named tests
//! (`merge_step_skipped_when_mode_is_none` and
//! `merge_step_invokes_gh_with_admin_flag`).
//!
//! ### Determinism
//!
//! All scripts are pure shell, exit synchronously, and never sleep. Tests
//! capture per-invocation argv into a counter file so assertions can
//! verify both call count AND the exact argv pattern.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use plan_executor::job::step::{Step, StepContext};
use plan_executor::job::steps::pr_finalize::{
    MarkReadyStep, MergeMode, MergeStep, MonitorStep, PrLookupStep, ReportStep,
};
use plan_executor::job::types::AttemptOutcome;
use tempfile::TempDir;

/// Process-wide PATH lock. `std::env::set_var` is not thread-safe; cargo
/// test runs tests in parallel, so any test that mutates PATH must hold
/// this guard for the full setup-run-teardown window.
static PATH_LOCK: Mutex<()> = Mutex::new(());

/// Holds an exclusive PATH override for the duration of a single test.
/// `Drop` restores the original PATH so the next test starts clean.
struct PathGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    original: Option<String>,
}

impl PathGuard {
    fn new(prepend: &Path) -> Self {
        let lock = PATH_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let original = std::env::var("PATH").ok();
        let new_path = match &original {
            Some(p) => format!("{}:{}", prepend.display(), p),
            None => prepend.display().to_string(),
        };
        // SAFETY: tests serialize on PATH_LOCK; no other thread reads PATH
        // through libc concurrently for the duration of this guard.
        unsafe { std::env::set_var("PATH", new_path) };
        Self {
            _lock: lock,
            original,
        }
    }
}

impl Drop for PathGuard {
    fn drop(&mut self) {
        match &self.original {
            // SAFETY: tests serialize on PATH_LOCK.
            Some(p) => unsafe { std::env::set_var("PATH", p) },
            None => unsafe { std::env::remove_var("PATH") },
        }
    }
}

/// Writes `body` to `path` and marks it executable (0o755).
fn write_script(path: &Path, body: &str) {
    fs::write(path, body).expect("write script");
    let mut perms = fs::metadata(path).expect("stat script").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).expect("chmod script");
}

/// Returns a `StepContext` rooted at `dir`. The context is the minimum
/// needed to satisfy the trait; only `job_dir` is read by `ReportStep`.
fn step_ctx(dir: &Path) -> StepContext {
    StepContext {
        job_dir: dir.to_path_buf(),
        step_seq: 1,
        attempt_n: 1,
        workdir: dir.to_path_buf(),
    }
}

/// Builds a fake `gh` script that records its argv to `counter_path`
/// and returns `body`'s exit semantics. `body` runs after the argv
/// recording so it can `echo` JSON to stdout or `>&2` to stderr.
fn fake_gh_body(counter_path: &Path, body: &str) -> String {
    format!(
        "#!/bin/sh\necho \"$@\" >> \"{counter}\"\n{body}\n",
        counter = counter_path.display()
    )
}

/// Counts how many times the fake `gh` (or fake monitor) recorded an
/// invocation by reading line count of `counter_path`.
fn invocation_count(counter_path: &Path) -> usize {
    fs::read_to_string(counter_path)
        .map(|s| s.lines().filter(|l| !l.is_empty()).count())
        .unwrap_or(0)
}

/// Reads the recorded argv of the Nth invocation (1-based) of the fake.
fn nth_invocation_args(counter_path: &Path, n: usize) -> Option<String> {
    fs::read_to_string(counter_path)
        .ok()?
        .lines()
        .filter(|l| !l.is_empty())
        .nth(n.saturating_sub(1))
        .map(str::to_owned)
}

/// Sets up a temp dir + counter + fake gh on PATH, returning the guard
/// that owns the PATH override.
struct GhHarness {
    dir: TempDir,
    counter: PathBuf,
    _path_guard: PathGuard,
}

impl GhHarness {
    fn new(gh_body: &str) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let counter = dir.path().join("gh.calls");
        fs::write(&counter, "").expect("init counter");
        let gh = dir.path().join("gh");
        let body = fake_gh_body(&counter, gh_body);
        write_script(&gh, &body);
        let path_guard = PathGuard::new(dir.path());
        Self {
            dir,
            counter,
            _path_guard: path_guard,
        }
    }

    fn counter(&self) -> &Path {
        &self.counter
    }

    fn dir(&self) -> &Path {
        self.dir.path()
    }
}

// =====================================================================
// Scenario 1: happy path — all 5 steps reach Succeeded.
// =====================================================================

#[tokio::test]
async fn happy_path_all_five_steps_reach_success() {
    // gh body branches on the subcommand to satisfy each step.
    let body = r#"
case "$1 $2" in
  "pr view")
    echo '{"headRefOid":"abc123","isDraft":false,"number":42,"baseRefName":"main","headRefName":"feat/x"}'
    exit 0
    ;;
  "pr ready")
    exit 0
    ;;
  "pr merge")
    exit 0
    ;;
  *)
    echo "unexpected gh args: $@" >&2
    exit 99
    ;;
esac
"#;
    let h = GhHarness::new(body);

    // pr-monitor.sh fake — exits 0 immediately.
    let monitor = h.dir().join("pr-monitor.sh");
    write_script(&monitor, "#!/bin/sh\nexit 0\n");

    let owner = "octo".to_string();
    let repo = "demo".to_string();
    let pr = 42u32;

    let mut ctx = step_ctx(h.dir());

    let outcomes = vec![
        PrLookupStep {
            owner: owner.clone(),
            repo: repo.clone(),
            pr,
        }
        .run(&mut ctx)
        .await,
        MarkReadyStep {
            owner: owner.clone(),
            repo: repo.clone(),
            pr,
        }
        .run(&mut ctx)
        .await,
        MonitorStep {
            owner: owner.clone(),
            repo: repo.clone(),
            pr,
            script_path: Some(monitor),
        }
        .run(&mut ctx)
        .await,
        MergeStep {
            owner: owner.clone(),
            repo: repo.clone(),
            pr,
            mode: MergeMode::Merge,
        }
        .run(&mut ctx)
        .await,
        ReportStep { owner, repo, pr }.run(&mut ctx).await,
    ];

    let expected = vec![
        AttemptOutcome::Success,
        AttemptOutcome::Success,
        AttemptOutcome::Success,
        AttemptOutcome::Success,
        AttemptOutcome::Success,
    ];
    assert_eq!(outcomes, expected);
}

// =====================================================================
// Scenario 2: PrLookup transient retry — three attempts, last succeeds.
// =====================================================================
//
// `Step::run` is one attempt; the supervisor owns retry orchestration.
// This test validates that:
//   (a) the first two invocations of the underlying gh return a
//       transient-classified outcome, and
//   (b) the third returns Success,
//   (c) a counter file shows three total spawns.
// Attempts are dispatched manually by the test (mimicking what the
// supervisor would do under `RetryTransient { max: 3 }`).

#[tokio::test]
async fn pr_lookup_transient_then_success_records_three_attempts() {
    // Per-attempt behavior: the fake gh writes an attempt counter and
    // exits non-zero with "HTTP 502" for the first two calls, then
    // emits valid JSON on the third.
    let body = r#"
state="$0.state"
n=$(cat "$state" 2>/dev/null || echo 0)
n=$((n+1))
echo "$n" > "$state"
if [ "$n" -lt 3 ]; then
  echo "HTTP 502: bad gateway" >&2
  exit 1
fi
echo '{"headRefOid":"abc","isDraft":false,"number":1,"baseRefName":"main","headRefName":"x"}'
exit 0
"#;
    let h = GhHarness::new(body);
    let mut ctx = step_ctx(h.dir());

    let step = PrLookupStep {
        owner: "octo".to_string(),
        repo: "demo".to_string(),
        pr: 1,
    };

    let attempt1 = step.run(&mut ctx).await;
    let attempt2 = step.run(&mut ctx).await;
    let attempt3 = step.run(&mut ctx).await;

    // assert each attempt classified correctly; transient errors carry
    // an `error` payload — match on the discriminant only by mapping.
    let kinds: Vec<&str> = vec![&attempt1, &attempt2, &attempt3]
        .into_iter()
        .map(outcome_kind)
        .collect();
    assert_eq!(kinds, vec!["transient_infra", "transient_infra", "success"]);

    let calls = invocation_count(h.counter());
    assert_eq!(calls, 3);
}

// =====================================================================
// Scenario 3: MarkReady idempotent skip when PR is already ready.
// =====================================================================
//
// Production code recognizes "not in draft state" / "already ready" in
// stderr and short-circuits to Success without further action.

#[tokio::test]
async fn mark_ready_returns_success_when_pr_already_ready() {
    // Fake gh always exits 1 with the canonical "not in draft state"
    // stderr. The production step must classify this as Success.
    let body = r#"
echo "could not mark pull request as ready: PR is not in draft state" >&2
exit 1
"#;
    let h = GhHarness::new(body);
    let mut ctx = step_ctx(h.dir());

    let step = MarkReadyStep {
        owner: "octo".to_string(),
        repo: "demo".to_string(),
        pr: 7,
    };

    let outcome = step.run(&mut ctx).await;

    assert_eq!(outcome, AttemptOutcome::Success);
}

// =====================================================================
// Scenario 4: Monitor crash → TransientInfra (caller's RetryTransient
// { max: 1 } policy is what supplies the retry; the step itself runs
// once per call). We verify both the per-attempt outcome AND the
// declared recovery policy so the test catches accidental policy
// changes.
// =====================================================================

#[tokio::test]
async fn monitor_step_returns_transient_when_script_exits_nonzero() {
    // MonitorStep first calls `gh pr view` to capture HEAD SHA; shim it
    // alongside the monitor script. The script exits non-zero on purpose
    // — that's what this test verifies routes to TransientInfra.
    let body = r#"
case "$1 $2" in
  "pr view")
    echo '{"headRefOid":"deadbeef"}'
    exit 0
    ;;
  *)
    echo "unexpected gh args: $@" >&2
    exit 99
    ;;
esac
"#;
    let h = GhHarness::new(body);
    let monitor = h.dir.path().join("pr-monitor.sh");
    write_script(&monitor, "#!/bin/sh\necho boom >&2\nexit 1\n");

    let mut ctx = step_ctx(h.dir.path());
    let step = MonitorStep {
        owner: "octo".to_string(),
        repo: "demo".to_string(),
        pr: 1,
        script_path: Some(monitor),
    };

    let outcome = step.run(&mut ctx).await;
    drop(h);

    assert_eq!(outcome_kind(&outcome), "transient_infra");
}

#[tokio::test]
async fn monitor_step_recovery_policy_is_retry_transient_max_one() {
    use plan_executor::job::recovery::{Backoff, RecoveryPolicy};

    let step = MonitorStep {
        owner: "octo".to_string(),
        repo: "demo".to_string(),
        pr: 1,
        script_path: Some(PathBuf::from("/nonexistent/pr-monitor.sh")),
    };

    let policy = step.recovery_policy();

    let expected = RecoveryPolicy::RetryTransient {
        max: 1,
        backoff: Backoff::Fixed { ms: 0 },
    };
    assert_eq!(policy, expected);
}

// =====================================================================
// Scenario 5: MergeStep skipped when MergeMode::None — gh is NOT
// invoked. We assert by checking that the counter file (which the fake
// gh appends to on every invocation) stays empty.
// =====================================================================

#[tokio::test]
async fn merge_step_skipped_when_mode_is_none() {
    // The fake gh would record any invocation; we assert it records
    // zero. Body is unreachable in this scenario.
    let h = GhHarness::new("exit 99\n");
    let mut ctx = step_ctx(h.dir());

    let step = MergeStep {
        owner: "octo".to_string(),
        repo: "demo".to_string(),
        pr: 1,
        mode: MergeMode::None,
    };

    let outcome = step.run(&mut ctx).await;

    assert_eq!(outcome, AttemptOutcome::Success);
    assert_eq!(invocation_count(h.counter()), 0);
}

// =====================================================================
// Scenario 6: MergeStep with MergeMode::MergeAdmin invokes gh pr merge
// with the --admin flag. We validate the recorded argv contains the
// expected subcommand AND --admin token.
// =====================================================================

#[tokio::test]
async fn merge_step_invokes_gh_with_admin_flag() {
    let h = GhHarness::new("exit 0\n");
    let mut ctx = step_ctx(h.dir());

    let step = MergeStep {
        owner: "octo".to_string(),
        repo: "demo".to_string(),
        pr: 42,
        mode: MergeMode::MergeAdmin,
    };

    let outcome = step.run(&mut ctx).await;

    assert_eq!(outcome, AttemptOutcome::Success);
    assert_eq!(invocation_count(h.counter()), 1);
    let args = nth_invocation_args(h.counter(), 1).expect("first call recorded");
    let expected = "pr merge 42 --repo octo/demo --squash --delete-branch --admin".to_string();
    assert_eq!(args, expected);
}

// ---------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------

/// Returns a string discriminant for `AttemptOutcome` so tests can
/// compare the variant without caring about its `error` payload.
fn outcome_kind(o: &AttemptOutcome) -> &'static str {
    match o {
        AttemptOutcome::Success => "success",
        AttemptOutcome::HardInfra { .. } => "hard_infra",
        AttemptOutcome::TransientInfra { .. } => "transient_infra",
        AttemptOutcome::ProtocolViolation { .. } => "protocol_violation",
        AttemptOutcome::SemanticMistake { .. } => "semantic_mistake",
        AttemptOutcome::SpecDrift { .. } => "spec_drift",
        AttemptOutcome::Pending => "pending",
    }
}
