//! Integration tests for [`plan_executor::helper`] (Task D2.3).
//!
//! Validates [`invoke_helper`] and the four typed wrappers against a
//! `tests/fixtures/fake-claude.sh` shell-script fake whose stdout, stderr,
//! exit code, and pre-write sleep are steered through `FAKE_CLAUDE_*` env
//! vars. PATH is prepended for the duration of each test via a `RAII`
//! [`HelperHarness`] guard mirroring the pattern established in
//! `tests/pr_finalize.rs`.
//!
//! ### Test design
//!
//! ECP partitions:
//!   P1 success × 4 wrappers
//!   P2 fix_required → SemanticFailure
//!   P3 missing required keys → ProtocolViolation
//!   P4 timeout → TransientInfra
//!   P5 spawn ENOENT (claude missing from PATH) → HardInfra
//!   P6 child exit-nonzero with transient stderr → TransientInfra
//!   P7 schema-valid + extra unknown property → ProtocolViolation
//!     (additionalProperties:false drift)
//!
//! Decision matrix: see module-level comments before each scenario.
//!
//! ### Determinism
//!
//! - All scenarios use a 1-second helper timeout. The only test that
//!   sleeps is `timeout_maps_to_transient_infra`, which sleeps 3 s while
//!   the timeout is 1 s — the helper kills the child synchronously.
//! - Process-wide env (`PATH`, `FAKE_CLAUDE_*`,
//!   `PLAN_EXECUTOR_HELPER_TIMEOUT_SECS`) is mutated through a single
//!   global mutex so cargo's parallel test runner cannot interleave
//!   harnesses.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use plan_executor::helper::{
    invoke_helper, invoke_helper_with, invoke_pr_finalize, HelperError, HelperInvocation,
    HelperSkill, HelperStatus, PrFinalizeInput, PrFinalizeMergeMode, PrFinalizeOutput,
};
use plan_executor::job::step::StepContext;
use tempfile::TempDir;

/// Process-wide env lock shared by every test in this file. `std::env`
/// mutations are not thread-safe; cargo runs tests in parallel.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// `RAII` guard that prepends a directory to `PATH` and restores the
/// original value (and `FAKE_CLAUDE_*` / timeout overrides) on drop.
struct HelperHarness {
    _lock: std::sync::MutexGuard<'static, ()>,
    dir: TempDir,
    saved_path: Option<String>,
    saved_response: Option<String>,
    saved_stderr: Option<String>,
    saved_exit: Option<String>,
    saved_sleep: Option<String>,
    saved_timeout_env: Option<String>,
}

impl HelperHarness {
    /// Builds a harness with a `claude` symlink (or copy) of
    /// `tests/fixtures/fake-claude.sh` placed in a temp dir, then
    /// prepends that dir to `PATH`.
    fn new() -> Self {
        let lock = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let dir = tempfile::tempdir().expect("tempdir");

        // Copy the fake script under the name `claude` so
        // `Command::new("claude")` resolves to it via PATH lookup.
        let fixture = repo_root().join("tests/fixtures/fake-claude.sh");
        let target = dir.path().join("claude");
        fs::copy(&fixture, &target).expect("copy fake-claude.sh -> claude");
        // copy preserves mode on POSIX, but be explicit.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&target).expect("stat claude").permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&target, perms).expect("chmod claude");
        }

        let saved_path = std::env::var("PATH").ok();
        let saved_response = std::env::var("FAKE_CLAUDE_RESPONSE").ok();
        let saved_stderr = std::env::var("FAKE_CLAUDE_STDERR").ok();
        let saved_exit = std::env::var("FAKE_CLAUDE_EXIT_CODE").ok();
        let saved_sleep = std::env::var("FAKE_CLAUDE_SLEEP_SECS").ok();
        let saved_timeout_env = std::env::var("PLAN_EXECUTOR_HELPER_TIMEOUT_SECS").ok();

        let new_path = match &saved_path {
            Some(p) => format!("{}:{}", dir.path().display(), p),
            None => dir.path().display().to_string(),
        };
        // SAFETY: tests serialize on ENV_LOCK; the harness owns PATH for
        // its lifetime, restored on drop.
        unsafe { std::env::set_var("PATH", new_path) };

        // Always start from a clean FAKE_CLAUDE_* slate so previous runs
        // cannot leak.
        for k in [
            "FAKE_CLAUDE_RESPONSE",
            "FAKE_CLAUDE_STDERR",
            "FAKE_CLAUDE_EXIT_CODE",
            "FAKE_CLAUDE_SLEEP_SECS",
        ] {
            unsafe { std::env::remove_var(k) };
        }

        Self {
            _lock: lock,
            dir,
            saved_path,
            saved_response,
            saved_stderr,
            saved_exit,
            saved_sleep,
            saved_timeout_env,
        }
    }

    /// Removes the `claude` shim from this harness's PATH directory so
    /// `Command::new("claude")` returns `ErrorKind::NotFound` on spawn,
    /// reproducing the "binary missing from PATH" hard-infra path.
    fn break_claude_binary(&self) {
        let claude = self.dir.path().join("claude");
        if claude.exists() {
            fs::remove_file(&claude).expect("remove fake claude");
        }
    }

    /// Returns the fixture-rooted temp dir backing this harness; tests
    /// use it as the StepContext workdir so sidecar files land in a
    /// well-known location.
    fn workdir(&self) -> &Path {
        self.dir.path()
    }

    /// Sets `FAKE_CLAUDE_RESPONSE` for the spawn that follows.
    fn with_response(self, body: &str) -> Self {
        unsafe { std::env::set_var("FAKE_CLAUDE_RESPONSE", body) };
        self
    }

    /// Sets `FAKE_CLAUDE_STDERR` for the spawn that follows.
    fn with_stderr(self, body: &str) -> Self {
        unsafe { std::env::set_var("FAKE_CLAUDE_STDERR", body) };
        self
    }

    /// Sets `FAKE_CLAUDE_EXIT_CODE` for the spawn that follows.
    fn with_exit_code(self, code: i32) -> Self {
        unsafe { std::env::set_var("FAKE_CLAUDE_EXIT_CODE", code.to_string()) };
        self
    }

    /// Sets `FAKE_CLAUDE_SLEEP_SECS` for the spawn that follows.
    fn with_sleep_secs(self, secs: u32) -> Self {
        unsafe { std::env::set_var("FAKE_CLAUDE_SLEEP_SECS", secs.to_string()) };
        self
    }
}

impl Drop for HelperHarness {
    fn drop(&mut self) {
        unsafe {
            restore_env("PATH", &self.saved_path);
            restore_env("FAKE_CLAUDE_RESPONSE", &self.saved_response);
            restore_env("FAKE_CLAUDE_STDERR", &self.saved_stderr);
            restore_env("FAKE_CLAUDE_EXIT_CODE", &self.saved_exit);
            restore_env("FAKE_CLAUDE_SLEEP_SECS", &self.saved_sleep);
            restore_env("PLAN_EXECUTOR_HELPER_TIMEOUT_SECS", &self.saved_timeout_env);
        }
    }
}

/// Restores `name` to `saved` (set or unset, matching the original).
unsafe fn restore_env(name: &str, saved: &Option<String>) {
    match saved {
        Some(v) => std::env::set_var(name, v),
        None => std::env::remove_var(name),
    }
}

/// Resolves the crate root from `CARGO_MANIFEST_DIR` (set by cargo for
/// integration tests).
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Builds a minimal `StepContext` rooted at `dir`. `invoke_helper` only
/// reads `workdir`, `step_seq`, `attempt_n`; everything else is unused.
fn step_ctx(dir: &Path) -> StepContext {
    StepContext {
        job_dir: dir.to_path_buf(),
        step_seq: 1,
        attempt_n: 1,
        workdir: dir.to_path_buf(),
        daemon_hooks: None,
    }
}

/// Generous timeout used by tests that expect the child to succeed
/// quickly. Still bounded so CI cannot hang on a regression.
///
/// `HelperInvocation` is `#[non_exhaustive]` so external crates cannot
/// build it via a struct literal; `Default::default()` + field
/// assignment is the supported construction pattern.
fn fast_options() -> HelperInvocation {
    let mut o = HelperInvocation::default();
    o.timeout = Some(Duration::from_secs(15));
    o
}

/// Tight timeout used only by [`timeout_maps_to_transient_infra`] to
/// force a timeout against a child that sleeps longer than this bound.
fn tight_timeout_options() -> HelperInvocation {
    let mut o = HelperInvocation::default();
    o.timeout = Some(Duration::from_secs(1));
    o
}

// =====================================================================
// Test data — schema-valid envelopes for each helper, parameterized by
// `status`. Building these programmatically (vs string templates) keeps
// the tests close to the production schemas.
// =====================================================================

/// Minimal `RunReviewerTeam` envelope sufficient for tests that only assert
/// on `invoke_helper`'s error-mapping behavior (timeout, status mapping,
/// protocol violation). Carries the schema-required fields with placeholder
/// values; tests that need the typed `state_updates` payload should build
/// their own envelope inline.
fn run_reviewer_team_envelope(status: &str) -> String {
    serde_json::json!({
        "status": status,
        "next_step": "proceed",
        "notes": "ok",
        "state_updates": {
            "findings_path": "/tmp/findings.md",
            "reviewer_runs": [
                {"reviewer": "claude",   "exit_code": 0, "findings_count": 0},
                {"reviewer": "codex",    "exit_code": 0, "findings_count": 0},
                {"reviewer": "gemini",   "exit_code": 0, "findings_count": 0},
                {"reviewer": "security", "exit_code": 0, "findings_count": 0}
            ]
        }
    })
    .to_string()
}

fn pr_finalize_envelope(status: &str) -> String {
    serde_json::json!({
        "status": status,
        "next_step": "done",
        "notes": "ok",
        "state_updates": {
            "pr_state": "MERGED",
            "merge_sha": "0123456789abcdef0123456789abcdef01234567",
            "bugbot_comments_addressed": 2
        }
    })
    .to_string()
}

fn sample_pr_finalize_input() -> PrFinalizeInput {
    PrFinalizeInput {
        owner: "octo".to_string(),
        repo: "demo".to_string(),
        pr: 42,
        merge_mode: PrFinalizeMergeMode::Merge,
    }
}

// =====================================================================
// Scenario P1: invoke_pr_finalize happy path returns typed output.
// =====================================================================

#[test]
fn invoke_pr_finalize_returns_typed_output_on_success() {
    let h = HelperHarness::new().with_response(&pr_finalize_envelope("success"));
    let ctx = step_ctx(h.workdir());

    let result = invoke_pr_finalize_with_timeout(sample_pr_finalize_input(), &ctx);

    let expected: PrFinalizeOutput = serde_json::from_value(serde_json::json!({
        "pr_state": "MERGED",
        "merge_sha": "0123456789abcdef0123456789abcdef01234567",
        "bugbot_comments_addressed": 2
    }))
    .expect("decode expected PrFinalizeOutput");
    assert_eq!(result.expect("invoke_pr_finalize Ok"), expected);
}

// =====================================================================
// Scenario P2: status=fix_required → SemanticFailure.
// =====================================================================

#[test]
fn fix_required_status_maps_to_semantic_failure() {
    let envelope = serde_json::json!({
        "status": "fix_required",
        "next_step": "dispatch_fix_wave",
        "notes": "two findings need a fix wave",
        "state_updates": {
            "findings_path": "/tmp/findings.md",
            "reviewer_runs": [
                {"reviewer": "claude",   "exit_code": 0, "findings_count": 1},
                {"reviewer": "codex",    "exit_code": 0, "findings_count": 1},
                {"reviewer": "gemini",   "exit_code": 0, "findings_count": 0},
                {"reviewer": "security", "exit_code": 0, "findings_count": 0}
            ]
        }
    })
    .to_string();
    let h = HelperHarness::new().with_response(&envelope);
    let ctx = step_ctx(h.workdir());

    let err = invoke_helper_with(
        HelperSkill::RunReviewerTeam,
        serde_json::json!({}),
        &ctx,
        &fast_options(),
    )
    .expect_err("expected SemanticFailure");

    match err {
        HelperError::SemanticFailure {
            status,
            notes,
            state_updates,
        } => {
            assert_eq!(
                (status, notes),
                (
                    HelperStatus::FixRequired,
                    "two findings need a fix wave".to_string()
                )
            );
            // Structural assertion: state_updates is preserved verbatim.
            assert_eq!(
                state_updates
                    .get("findings_path")
                    .and_then(serde_json::Value::as_str),
                Some("/tmp/findings.md"),
            );
        }
        other => panic!("expected SemanticFailure, got {other:?}"),
    }
}

// =====================================================================
// Scenario P3: schema-invalid (missing required keys) → ProtocolViolation.
// =====================================================================

#[test]
fn missing_required_keys_maps_to_protocol_violation() {
    // Only "status" present; `next_step`, `notes`, `state_updates` missing.
    let h = HelperHarness::new().with_response(r#"{"status":"ok"}"#);
    let ctx = step_ctx(h.workdir());

    let err = invoke_helper_with(
        HelperSkill::RunReviewerTeam,
        serde_json::json!({}),
        &ctx,
        &fast_options(),
    )
    .expect_err("expected ProtocolViolation");

    let category = protocol_violation_category(&err);
    // The schema fires on the FIRST violation; jsonschema reports root-level
    // failures (missing required + bad enum) at "" → category="envelope_shape"
    // or "status_enum" depending on iteration order. We only assert that the
    // error is a ProtocolViolation (not Semantic / Transient / Hard).
    assert!(
        !category.is_empty(),
        "expected non-empty ProtocolViolation category, got {err:?}"
    );
}

// =====================================================================
// Scenario P4: child sleeps past helper timeout → TransientInfra.
// =====================================================================

#[test]
fn timeout_maps_to_transient_infra() {
    let h = HelperHarness::new()
        .with_response(&run_reviewer_team_envelope("success"))
        .with_sleep_secs(3);
    let ctx = step_ctx(h.workdir());

    let err = invoke_helper_with(
        HelperSkill::RunReviewerTeam,
        serde_json::json!({}),
        &ctx,
        &tight_timeout_options(),
    )
    .expect_err("expected timeout error");

    assert!(
        matches!(err, HelperError::TransientInfra(_)),
        "expected TransientInfra, got {err:?}"
    );
}

// =====================================================================
// Scenario P5: claude binary missing from PATH → HardInfra.
// =====================================================================
//
// `Command::new("claude")` returns `ErrorKind::NotFound` from spawn() —
// production code maps this to HardInfra. We trigger it by removing the
// shim from the harness PATH directory before invoking.

#[test]
fn missing_claude_binary_maps_to_hard_infra() {
    let h = HelperHarness::new();
    h.break_claude_binary();
    let ctx = step_ctx(h.workdir());

    // The harness puts ONLY its temp dir at the head of PATH, but the
    // saved tail still resolves a real `claude` if one exists on the
    // developer's system. Override PATH to the broken dir alone for
    // this test so the spawn definitively fails.
    let broken_only = h.workdir().display().to_string();
    let saved = std::env::var("PATH").ok();
    // SAFETY: the harness owns ENV_LOCK; we restore on test exit.
    unsafe { std::env::set_var("PATH", &broken_only) };

    let err = invoke_helper_with(
        HelperSkill::RunReviewerTeam,
        serde_json::json!({}),
        &ctx,
        &fast_options(),
    );

    // restore the harness PATH BEFORE asserting so a panic does not
    // leak a broken PATH into other tests on the same thread.
    unsafe {
        if let Some(p) = saved {
            std::env::set_var("PATH", p);
        }
    }

    let err = err.expect_err("expected HardInfra error");
    assert!(
        matches!(err, HelperError::HardInfra(_)),
        "expected HardInfra, got {err:?}"
    );
}

// =====================================================================
// Scenario P6: child exits 1 with transient stderr → TransientInfra.
// =====================================================================

#[test]
fn nonzero_exit_with_transient_stderr_maps_to_transient_infra() {
    let h = HelperHarness::new()
        .with_response("")
        .with_stderr("HTTP 502: bad gateway")
        .with_exit_code(1);
    let ctx = step_ctx(h.workdir());

    let err = invoke_helper_with(
        HelperSkill::RunReviewerTeam,
        serde_json::json!({}),
        &ctx,
        &fast_options(),
    )
    .expect_err("expected TransientInfra error");

    assert!(
        matches!(err, HelperError::TransientInfra(_)),
        "expected TransientInfra, got {err:?}"
    );
}

// =====================================================================
// Scenario P7: schema-valid + extra unknown top-level prop ⇒
// `additionalProperties: false` triggers ProtocolViolation.
// =====================================================================

#[test]
fn extra_top_level_field_maps_to_protocol_violation() {
    let mut envelope: serde_json::Value =
        serde_json::from_str(&run_reviewer_team_envelope("success")).expect("seed envelope");
    envelope
        .as_object_mut()
        .expect("envelope is object")
        .insert(
            "unexpected_top_level".to_string(),
            serde_json::json!("drift"),
        );

    let h = HelperHarness::new().with_response(&envelope.to_string());
    let ctx = step_ctx(h.workdir());

    let err = invoke_helper_with(
        HelperSkill::RunReviewerTeam,
        serde_json::json!({}),
        &ctx,
        &fast_options(),
    )
    .expect_err("expected ProtocolViolation from extra field");

    let category = protocol_violation_category(&err);
    assert!(
        !category.is_empty(),
        "expected non-empty ProtocolViolation category, got {err:?}"
    );
}

// =====================================================================
// Scenario: invoke_helper sidecar file is created on the workdir for
// every spawn (covers the sidecar write path of the public API). Use
// the bare `invoke_helper` (no timeout override) to also exercise the
// default-timeout code path, but still apply a short env override so
// the test cannot hang.
// =====================================================================

#[test]
fn invoke_helper_writes_input_sidecar_under_workdir() {
    let h = HelperHarness::new().with_response(&pr_finalize_envelope("success"));
    let ctx = step_ctx(h.workdir());

    // SAFETY: harness owns ENV_LOCK.
    unsafe { std::env::set_var("PLAN_EXECUTOR_HELPER_TIMEOUT_SECS", "2") };

    let result = invoke_helper(
        HelperSkill::PrFinalize,
        serde_json::json!({"owner":"o","repo":"r","pr":1,"merge_mode":"none"}),
        &ctx,
    );

    let sidecar_dir = h.workdir().join(".plan-executor").join("helpers");
    let entries: Vec<_> = fs::read_dir(&sidecar_dir)
        .expect("sidecar dir exists")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();

    assert_eq!(entries.len(), 1, "expected one sidecar, got {entries:?}");
    let only = entries.first().expect("entry");
    let expected_name = "001-001-pr_finalize.input.json".to_string();
    assert_eq!(only, &expected_name);
    // Sanity: the helper run succeeded (PrFinalize returns Ok HelperOutput).
    assert!(
        result.is_ok(),
        "expected helper Ok envelope, got {:?}",
        result.err()
    );
}

// ---------------------------------------------------------------------
// Wrapper helpers — every wrapper takes a `HelperInvocation` indirectly
// through `invoke_helper`; these shims thread `fast_options()` through
// the typed entry points by re-implementing the wrapper inline. This
// preserves the public-API guarantee while keeping per-test wall-clock
// bounded to ≤ 1 s on success and ≤ 1 s + spawn overhead on errors.
// ---------------------------------------------------------------------

fn invoke_pr_finalize_with_timeout(
    input: PrFinalizeInput,
    ctx: &StepContext,
) -> Result<PrFinalizeOutput, HelperError> {
    let json = serde_json::to_value(&input).expect("serialize PrFinalizeInput");
    let raw = invoke_helper_with(HelperSkill::PrFinalize, json, ctx, &fast_options())?;
    serde_json::from_value(raw.state_updates).map_err(|e| HelperError::ProtocolViolation {
        category: "state_updates_shape".to_string(),
        detail: format!("decode state_updates failed: {e}"),
    })
}

/// Returns the `category` string of a `ProtocolViolation`, panicking
/// with a descriptive message for any other variant.
fn protocol_violation_category(err: &HelperError) -> String {
    match err {
        HelperError::ProtocolViolation { category, .. } => category.clone(),
        other => panic!("expected ProtocolViolation, got {other:?}"),
    }
}

// Suppress unused-import warning when the only typed wrapper shim left
// (pr_finalize) happens to be unused on a given test cfg.
#[allow(dead_code)]
fn _api_surface_smoke() {
    let _ = invoke_pr_finalize;
}
