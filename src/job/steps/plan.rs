//! `Step` implementations for `JobKind::Plan`.
//!
//! - [`PreflightStep`] — placeholder owned by the orchestrator skill today;
//!   real preflight logic lands in a later phase.
//! - [`WaveExecutionStep`] — D3.1 — drives wave traversal via
//!   [`crate::scheduler::run_wave_execution`].
//! - [`IntegrationTestingStep`] — D3.3 — runs `cargo test --workspace` (or
//!   the configured integration-test command) inside `ctx.workdir`, capturing
//!   stdout/stderr to `attempts/<n>/integration-tests.log`.
//! - [`CodeReviewStep`] — D3.2 — invokes the reviewer-team helper, runs the
//!   triage helper on `fix_required`, builds and appends a fix wave via the
//!   `compile-fix-waves` CLI in APPEND mode, and re-enters the scheduler for
//!   that wave; loops until success or the per-step retry budget is exhausted.
//! - [`ValidationStep`] — D3.2 — same pattern as [`CodeReviewStep`] driven by
//!   the validator helper, with `validation_fix` waves.
//! - [`PrCreationStep`] — D3.3 — opens the plan's PR via `gh pr create`,
//!   short-circuits if a PR already exists for the current branch, and
//!   persists the PR URL under `<job_dir>/pr-url` for downstream steps.
//! - [`PrFinalizeStep`] — placeholder; the dedicated `JobKind::PrFinalize`
//!   pipeline owns the production finalize flow.
//! - [`SummaryStep`] — D3.3 — invokes
//!   [`crate::helper::invoke_pr_finalize`] when `flags.skip_pr` is `false`,
//!   otherwise writes `<job_dir>/.tmp-execution-summary.md` directly.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::json;

use crate::finding::{Finding, Severity};
use crate::helper::{
    invoke_helper, invoke_pr_finalize, HelperError, HelperOutput, HelperSkill, HelperStatus,
    PrFinalizeInput, PrFinalizeMergeMode, ReviewTeamInput, ReviewTriageInput, ValidatorInput,
};
use crate::job::recovery::{Backoff, CorrectivePromptKey, RecoveryPolicy};
use crate::job::step::{Step, StepContext};
use crate::job::types::AttemptOutcome;
use crate::scheduler::{self, Manifest, SchedulerError};

/// Maximum number of inner fix-loop iterations attempted within a single step
/// attempt. Each iteration runs (helper → triage → compile-fix-waves → wave
/// re-execution → re-invoke helper). Beyond this cap the step gives up and
/// surfaces a `SemanticMistake` outcome so the registry-level retry/abort
/// machinery can take over.
const MAX_FIX_LOOP_ITERATIONS: u32 = 3;

/// Wall-clock budget for a single `run_helper_fix_loop` invocation.
///
/// Complements [`MAX_FIX_LOOP_ITERATIONS`]: each iteration may take 30+
/// minutes (helper timeout + wave dispatch), so an iteration cap alone does
/// not bound total wall-clock time. When this budget is exceeded the loop
/// returns [`AttemptOutcome::TransientInfra`] so the registry-level recovery
/// policy can decide whether to retry the step.
const MAX_FIX_LOOP_BUDGET: Duration = Duration::from_secs(2 * 60 * 60);

/// Stub. Real preflight logic lands in a later D-phase task.
///
/// Today the orchestrator skill performs preflight checks; this shell exists
/// so the registry has an 8-element vector for `JobKind::Plan`.
/// Preflight: prepares the working tree for the plan.
///
/// When `plan.flags.no_worktree` is `false` (the default), creates a fresh
/// `git worktree` rooted at `<source-repo>/../.my/worktrees/<repo>-<plan-stem>[-<jira>]`
/// and checks out the manifest's `target_branch` (or a derived
/// `<type>/<plan-stem>` when omitted). On success, mutates `ctx.workdir`
/// so every subsequent step (`wave_execution`, `code_review`,
/// `pr_creation`, …) runs inside the new worktree instead of the source
/// repo's main checkout.
///
/// When `plan.flags.no_worktree` is `true`, the step is a no-op — useful
/// for one-shot manifests where the caller has already arranged the
/// working tree.
///
/// Idempotent: when the target worktree path already exists and resolves
/// to the requested branch, the step short-circuits to
/// [`AttemptOutcome::Success`] and updates `ctx.workdir` so re-runs land
/// in the same place. When the path exists but points at a different
/// branch, the step surfaces a `ProtocolViolation` rather than risk
/// silently overwriting in-progress work.
#[derive(Debug, Clone)]
pub struct PreflightStep {
    /// Absolute path to the compiled `tasks.json` manifest. Read for
    /// `plan.flags`, `plan.target_branch`, `plan.target_repo`, `plan.type`
    /// during worktree setup.
    pub manifest_path: PathBuf,
}

#[async_trait]
impl Step for PreflightStep {
    fn name(&self) -> &'static str {
        "preflight"
    }
    fn idempotent(&self) -> bool {
        true
    }
    fn recovery_policy(&self) -> RecoveryPolicy {
        RecoveryPolicy::None
    }
    async fn run(&self, ctx: &mut StepContext) -> AttemptOutcome {
        run_preflight(ctx, &self.manifest_path)
    }
}

/// Implementation of [`PreflightStep::run`]; split out as a free function
/// for unit testability.
fn run_preflight(ctx: &mut StepContext, manifest_path: &Path) -> AttemptOutcome {
    let plan_meta = match load_plan_meta(manifest_path) {
        Ok(m) => m,
        Err(outcome) => return outcome,
    };

    if plan_meta.flags.no_worktree {
        tracing::info!(
            "preflight: skipped worktree setup (plan.flags.no_worktree=true)"
        );
        return AttemptOutcome::Success;
    }

    let source_repo = match find_source_repo(ctx, manifest_path) {
        Some(p) => p,
        None => {
            return AttemptOutcome::ProtocolViolation {
                category: "preflight_no_source_repo".to_string(),
                detail: format!(
                    "could not find a `.git` directory at or above ctx.workdir `{}` or manifest path `{}`",
                    ctx.workdir.display(),
                    manifest_path.display()
                ),
            };
        }
    };

    let plan_stem = plan_stem_from_manifest(manifest_path);
    let worktree_path = compute_worktree_path(&source_repo, &plan_stem, &plan_meta);
    let branch = derive_branch_name(&plan_meta, &plan_stem);

    if let Err(detail) = ensure_plan_executor_excluded(&source_repo) {
        // The exclude write is best-effort; surfacing the failure as a
        // protocol_violation makes it visible in `plan-executor jobs`
        // without losing the diagnostic.
        return AttemptOutcome::ProtocolViolation {
            category: "preflight_exclude_write_failed".to_string(),
            detail,
        };
    }

    match ensure_worktree(&source_repo, &worktree_path, &branch) {
        Ok(()) => {
            tracing::info!(
                worktree = %worktree_path.display(),
                branch = %branch,
                "preflight: worktree ready",
            );
            ctx.workdir = worktree_path;
            AttemptOutcome::Success
        }
        Err(detail) => AttemptOutcome::ProtocolViolation {
            category: "preflight_worktree_failed".to_string(),
            detail,
        },
    }
}

/// Resolves the source repository root for worktree creation. Prefers
/// `ctx.workdir` (the daemon sets this from `find_repo_root(plan)`), and
/// falls back to walking up from `manifest_path` for foreground / test
/// callers that may not have a workdir-resolved-to-repo invariant.
fn find_source_repo(ctx: &StepContext, manifest_path: &Path) -> Option<PathBuf> {
    if ctx.workdir.join(".git").exists() {
        return Some(ctx.workdir.clone());
    }
    let mut dir = manifest_path.parent()?.to_path_buf();
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        dir = dir.parent()?.to_path_buf();
    }
}

/// Plan-file stem (filename minus extension), used as the slug component
/// of both the worktree path and the derived branch name. Falls back to
/// `"plan"` when the manifest path has no usable parent directory name.
fn plan_stem_from_manifest(manifest_path: &Path) -> String {
    // tasks.json lives at `<plan-dir>/<plan-stem>/tasks.json`; the
    // plan-stem is the parent directory's name.
    manifest_path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| "plan".to_string())
}

/// Subdirectory of the source repo that holds plan-executor worktrees.
/// Kept out of `git status` via a `.git/info/exclude` entry written
/// during preflight (see [`ensure_plan_executor_excluded`]).
const WORKTREE_DIR_NAME: &str = ".plan-executor";

/// Computes the target worktree path per the convention
/// `<source-repo>/.plan-executor/<plan-stem>[-<jira>]`. Keeping the
/// worktree inside the source repo means the entire plan run lives next
/// to the code it operates on, and cleanup at the end of the pipeline
/// (see [`cleanup_worktree`]) reaches a single well-known location.
fn compute_worktree_path(
    source_repo: &Path,
    plan_stem: &str,
    plan_meta: &PlanMeta,
) -> PathBuf {
    let mut name = plan_stem.to_string();
    if let Some(jira) = plan_meta.jira.as_deref() {
        if !jira.is_empty() {
            name.push('-');
            name.push_str(jira);
        }
    }
    source_repo.join(WORKTREE_DIR_NAME).join(name)
}

/// Appends `.plan-executor/` to `<source-repo>/.git/info/exclude` when
/// the file does not already mention it. `.git/info/exclude` is the
/// per-clone gitignore, so this keeps the convention dir out of
/// `git status` without committing changes to the user's `.gitignore`.
/// Idempotent — repeated calls are a no-op once the entry is present.
fn ensure_plan_executor_excluded(source_repo: &Path) -> Result<(), String> {
    let exclude_path = source_repo.join(".git").join("info").join("exclude");
    let entry_line = format!("{WORKTREE_DIR_NAME}/");
    let existing = match std::fs::read_to_string(&exclude_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(format!(
                "read {}: {e}",
                exclude_path.display()
            ))
        }
    };
    if existing.lines().any(|l| l.trim() == entry_line) {
        return Ok(());
    }
    if let Some(parent) = exclude_path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return Err(format!("create {}: {e}", parent.display()));
        }
    }
    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str("# plan-executor: per-plan worktrees (auto-managed)\n");
    updated.push_str(&entry_line);
    updated.push('\n');
    std::fs::write(&exclude_path, updated)
        .map_err(|e| format!("write {}: {e}", exclude_path.display()))
}

/// Common base-branch names that callers sometimes drop into
/// `plan.target_branch` because they describe the PR target rather than a
/// dedicated feature branch. Honoring those would force the worktree onto
/// the same branch the source repo already has checked out, which fails
/// every time. Treat them as an unset override.
const BASE_BRANCH_NAMES: &[&str] = &["main", "master", "develop", "trunk", "HEAD"];

/// Derives the worktree branch name. Honors `plan.target_branch` when set
/// to a non-empty, non-base-branch string; otherwise generates
/// `<type>/<plan-stem>` so the branch is human-readable and matches the
/// project's conventional commit prefix.
///
/// `target_branch` semantically describes the PR target (where the
/// eventual PR will merge into), so a value like `main` must NOT be used
/// as the worktree branch — the source repo already has it checked out
/// and `git worktree add` would fail with a branch-collision error.
fn derive_branch_name(plan_meta: &PlanMeta, plan_stem: &str) -> String {
    if let Some(b) = plan_meta.target_branch.as_deref() {
        if !b.is_empty() && !BASE_BRANCH_NAMES.iter().any(|name| name.eq_ignore_ascii_case(b)) {
            return b.to_string();
        }
    }
    let prefix = match plan_meta.plan_type.as_str() {
        "feature" => "feat",
        "bug" => "fix",
        // refactor / chore / docs / infra all map to themselves
        other => other,
    };
    format!("{prefix}/{plan_stem}")
}

/// Removes the worktree at `worktree_path` (best-effort). Idempotent: a
/// missing worktree path is treated as success because the cleanup ran
/// already on a prior summary attempt or the path was never created
/// (e.g. `plan.flags.no_worktree=true`). Any failure to remove is
/// surfaced as `Err` so the caller can decide whether to log it or
/// abort — the summary step downgrades it to a warning so a half-baked
/// cleanup never poisons an otherwise-successful run.
fn cleanup_worktree(source_repo: &Path, worktree_path: &Path) -> Result<(), String> {
    if !worktree_path.exists() {
        return Ok(());
    }
    let output = std::process::Command::new("git")
        .args(["-C"])
        .arg(source_repo)
        .args(["worktree", "remove", "--force"])
        .arg(worktree_path)
        .output()
        .map_err(|e| format!("git worktree remove (spawn): {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "git worktree remove {} failed: {}",
        worktree_path.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

/// Idempotent worktree setup. When the target path already exists and
/// resolves to the requested branch, returns `Ok(())`. When it exists but
/// points at a different branch, returns `Err` with a precise diagnostic
/// rather than risk overwriting in-progress work.
fn ensure_worktree(
    source_repo: &Path,
    worktree_path: &Path,
    branch: &str,
) -> Result<(), String> {
    if worktree_path.exists() {
        // Verify the branch matches; if so, treat as already-set-up.
        let existing = std::process::Command::new("git")
            .args(["-C"])
            .arg(worktree_path)
            .args(["symbolic-ref", "--short", "HEAD"])
            .output()
            .map_err(|e| format!("git symbolic-ref at existing worktree: {e}"))?;
        if !existing.status.success() {
            return Err(format!(
                "existing worktree at {} is not a valid git checkout",
                worktree_path.display()
            ));
        }
        let existing_branch = String::from_utf8_lossy(&existing.stdout).trim().to_string();
        if existing_branch == branch {
            return Ok(());
        }
        return Err(format!(
            "existing worktree at {} is on branch `{existing_branch}`, not the requested `{branch}`",
            worktree_path.display()
        ));
    }

    if let Some(parent) = worktree_path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return Err(format!(
                "create worktree parent dir {}: {e}",
                parent.display()
            ));
        }
    }

    // Try `git worktree add -b <branch> <path>` first; on failure
    // (likely "branch already exists"), retry without `-b` so the existing
    // branch is checked out into the new worktree.
    let new_branch_attempt = std::process::Command::new("git")
        .args(["-C"])
        .arg(source_repo)
        .args(["worktree", "add", "-b", branch])
        .arg(worktree_path)
        .output()
        .map_err(|e| format!("git worktree add (new branch): {e}"))?;
    if new_branch_attempt.status.success() {
        return Ok(());
    }

    let existing_branch_attempt = std::process::Command::new("git")
        .args(["-C"])
        .arg(source_repo)
        .args(["worktree", "add"])
        .arg(worktree_path)
        .arg(branch)
        .output()
        .map_err(|e| format!("git worktree add (existing branch): {e}"))?;
    if existing_branch_attempt.status.success() {
        return Ok(());
    }

    let stderr_new = String::from_utf8_lossy(&new_branch_attempt.stderr).into_owned();
    let stderr_existing = String::from_utf8_lossy(&existing_branch_attempt.stderr).into_owned();
    Err(format!(
        "git worktree add failed:\n  new-branch attempt: {}\n  existing-branch attempt: {}",
        stderr_new.trim(),
        stderr_existing.trim()
    ))
}

/// Real wave-traversal step for `JobKind::Plan`.
///
/// Loads the compiled manifest from `manifest_path`, then drives every wave
/// through [`scheduler::run_wave_execution`]. Sub-agent dispatch flows through
/// [`crate::handoff::dispatch_all`] — no orchestrator subprocess is spawned;
/// the previous LLM-driven slash-command flow has been removed in favor of
/// Rust-side wave traversal.
#[derive(Debug, Clone)]
pub struct WaveExecutionStep {
    /// Absolute path to the compiled `tasks.json` manifest.
    pub manifest_path: PathBuf,
}

#[async_trait]
impl Step for WaveExecutionStep {
    fn name(&self) -> &'static str {
        "wave_execution"
    }
    fn idempotent(&self) -> bool {
        false
    }
    fn recovery_policy(&self) -> RecoveryPolicy {
        RecoveryPolicy::None
    }
    async fn run(&self, ctx: &mut StepContext) -> AttemptOutcome {
        let manifest = match scheduler::load_manifest(&self.manifest_path) {
            Ok(m) => m,
            Err(e) => {
                tracing::error!(
                    path = %self.manifest_path.display(),
                    error = %e,
                    "wave_execution: manifest load failed",
                );
                return AttemptOutcome::HardInfra {
                    error: format!("manifest load failed: {e}"),
                };
            }
        };
        let manifest_dir = match self.manifest_path.parent() {
            Some(p) => p.to_path_buf(),
            None => {
                return AttemptOutcome::HardInfra {
                    error: format!(
                        "manifest_path `{}` has no parent directory",
                        self.manifest_path.display()
                    ),
                };
            }
        };
        scheduler::run_wave_execution(ctx, &manifest, &manifest_dir).await
    }
}

/// Real integration-testing step (D3.3).
///
/// Runs `cargo test --workspace` inside `ctx.workdir`, streaming combined
/// stdout / stderr to
/// `<job_dir>/steps/<NNN-integration_testing>/attempts/<n>/integration-tests.log`.
/// On a non-zero exit the stderr tail is inspected for transient signals
/// (network blips, sccache timeouts, file-system races); transient hits
/// surface [`AttemptOutcome::TransientInfra`] so the registry-level
/// `RetryTransient { max: 1 }` can re-try once. Anything else is treated as
/// a semantic mistake on the implementation side and surfaced as
/// [`AttemptOutcome::SemanticMistake`] so the prior code-review / validation
/// steps see it on the next pass through the fix-loop.
///
/// The struct is field-less today; integration-test command override (e.g.
/// `--skip-integration-tests` or a project-specific runner) will be wired in
/// by a future task once a flag for it lives in the manifest schema.
#[derive(Debug, Clone, Default)]
pub struct IntegrationTestingStep;

/// Default integration-test command. Mirrors the existing skill's behavior
/// so the Rust side does not silently change semantics.
const INTEGRATION_TEST_PROGRAM: &str = "cargo";
const INTEGRATION_TEST_ARGS: &[&str] = &["test", "--workspace"];

#[async_trait]
impl Step for IntegrationTestingStep {
    fn name(&self) -> &'static str {
        "integration_testing"
    }

    fn idempotent(&self) -> bool {
        true
    }

    fn recovery_policy(&self) -> RecoveryPolicy {
        RecoveryPolicy::RetryTransient {
            max: 1,
            backoff: Backoff::Fixed { ms: 0 },
        }
    }

    async fn run(&self, ctx: &mut StepContext) -> AttemptOutcome {
        run_integration_tests(ctx)
    }
}

/// Real code-review step (D3.2).
///
/// Invokes the `run-reviewer-team-non-interactive` helper, and on
/// `fix_required` triggers an inner fix-loop:
///
/// 1. Call [`crate::helper::invoke_review_triage`] to obtain a fix-wave id.
/// 2. Shell out to `plan-executor compile-fix-waves` with the helper's
///    findings file, which APPEND-mode-mutates the same `tasks.json`.
/// 3. Re-enter [`scheduler::run_wave_execution`] for the freshly-appended
///    wave.
/// 4. Re-invoke the reviewer team and continue until status=success or the
///    per-step iteration cap is reached.
///
/// Each fix-loop iteration is a self-contained corrective action inside the
/// caller's [`crate::job::types::StepAttempt`]; the registry-level retry
/// budget covers infrastructure / protocol failures, while the inner loop
/// handles the "fix and re-review" semantics.
#[derive(Debug, Clone)]
pub struct CodeReviewStep {
    /// Absolute path to the compiled `tasks.json` manifest. Required so the
    /// step can resolve plan path / execution-root for `compile-fix-waves`
    /// and re-load the manifest after APPEND mutates it.
    pub manifest_path: PathBuf,
}

#[async_trait]
impl Step for CodeReviewStep {
    fn name(&self) -> &'static str {
        "code_review"
    }

    fn idempotent(&self) -> bool {
        true
    }

    fn recovery_policy(&self) -> RecoveryPolicy {
        helper_compose_policy("code_review_protocol")
    }

    async fn run(&self, ctx: &mut StepContext) -> AttemptOutcome {
        // Manifest may opt out of review entirely (e.g. one-shot tasks).
        // The cleaner mechanism is to omit `code_review` from
        // `plan.pipeline.steps`, but `flags.skip_code_review` is honored
        // here too for legacy manifests that still set the flag.
        match load_plan_meta(&self.manifest_path) {
            Ok(meta) if meta.flags.skip_code_review => {
                tracing::info!(
                    "code_review: skipped (plan.flags.skip_code_review=true)"
                );
                return AttemptOutcome::Success;
            }
            Ok(_) => {}
            // Manifest read failures fall through to the helper loop, which
            // will surface the same problem with full context.
            Err(_) => {}
        }
        run_helper_fix_loop(
            ctx,
            &self.manifest_path,
            HelperLoopKind::CodeReview,
            MAX_FIX_LOOP_ITERATIONS,
        )
        .await
    }
}

/// Real validation step (D3.2).
///
/// Mirrors [`CodeReviewStep`] using the `validate-execution-plan-non-interactive`
/// helper. Outstanding plan-vs-output gaps are converted into
/// [`Finding`]s and fed to `compile-fix-waves`, which produces
/// `validation_fix`-kind waves the scheduler runs before re-invoking the
/// validator.
#[derive(Debug, Clone)]
pub struct ValidationStep {
    /// Absolute path to the compiled `tasks.json` manifest.
    pub manifest_path: PathBuf,
}

#[async_trait]
impl Step for ValidationStep {
    fn name(&self) -> &'static str {
        "validation"
    }

    fn idempotent(&self) -> bool {
        true
    }

    fn recovery_policy(&self) -> RecoveryPolicy {
        helper_compose_policy("validation_protocol")
    }

    async fn run(&self, ctx: &mut StepContext) -> AttemptOutcome {
        run_helper_fix_loop(
            ctx,
            &self.manifest_path,
            HelperLoopKind::Validation,
            MAX_FIX_LOOP_ITERATIONS,
        )
        .await
    }
}

/// Real PR-creation step (D3.3).
///
/// Opens the plan's PR via `gh pr create`. Idempotent: when a PR already
/// exists for the current branch (`gh pr view --json url`), the step short
/// circuits to [`AttemptOutcome::Success`] and reuses the existing URL.
///
/// On success the resulting URL is persisted to
/// `<job_dir>/pr-url`, which downstream steps (`SummaryStep`, the
/// dedicated PR-finalize pipeline) read to address the right PR without
/// re-deriving it from the branch.
///
/// `manifest_path` is required so the step can read `plan.flags.draft_pr`
/// from the manifest and pass it to `gh pr create --draft`.
#[derive(Debug, Clone)]
pub struct PrCreationStep {
    /// Absolute path to the compiled `tasks.json` manifest. Used to read
    /// `plan.path`, plan goal/title metadata, and the `draft_pr` flag.
    pub manifest_path: PathBuf,
}

#[async_trait]
impl Step for PrCreationStep {
    fn name(&self) -> &'static str {
        "pr_creation"
    }

    fn idempotent(&self) -> bool {
        true
    }

    fn recovery_policy(&self) -> RecoveryPolicy {
        RecoveryPolicy::RetryTransient {
            max: 3,
            backoff: Backoff::Exponential {
                initial_ms: 500,
                max_ms: 8_000,
                factor: 2.0,
            },
        }
    }

    async fn run(&self, ctx: &mut StepContext) -> AttemptOutcome {
        run_pr_creation(ctx, &self.manifest_path)
    }
}

/// Stub. Phase D3.3 delegates to PR finalize.
pub struct PrFinalizeStep;

#[async_trait]
impl Step for PrFinalizeStep {
    fn name(&self) -> &'static str {
        "pr_finalize"
    }
    fn idempotent(&self) -> bool {
        true
    }
    fn recovery_policy(&self) -> RecoveryPolicy {
        RecoveryPolicy::None
    }
    async fn run(&self, _ctx: &mut StepContext) -> AttemptOutcome {
        AttemptOutcome::Pending
    }
}

/// Real summary step (D3.3).
///
/// When `plan.flags.skip_pr` is `false`, the step delegates to
/// [`crate::helper::invoke_pr_finalize`] (the helper writes its own summary
/// alongside the bug-bot triage output).
///
/// When `skip_pr` is `true` (or no PR was opened), the step writes a
/// best-effort markdown summary directly to
/// `<job_dir>/.tmp-execution-summary.md` listing the PR URL (if known), the
/// wave count, and the source manifest path.
///
/// Re-running the step rewrites the summary file in place, so the step is
/// idempotent. Failures are best-effort and the step has
/// [`RecoveryPolicy::None`] — a broken summary should not abort the entire
/// plan run.
#[derive(Debug, Clone)]
pub struct SummaryStep {
    /// Absolute path to the compiled `tasks.json` manifest. Used to read
    /// the `plan.flags.skip_pr` flag and to count waves for the summary.
    pub manifest_path: PathBuf,
}

#[async_trait]
impl Step for SummaryStep {
    fn name(&self) -> &'static str {
        "summary"
    }

    fn idempotent(&self) -> bool {
        true
    }

    fn recovery_policy(&self) -> RecoveryPolicy {
        RecoveryPolicy::None
    }

    async fn run(&self, ctx: &mut StepContext) -> AttemptOutcome {
        run_summary(ctx, &self.manifest_path)
    }
}

// ---------------------------------------------------------------------------
// Shared helper-fix-loop machinery
// ---------------------------------------------------------------------------

/// Discriminates between the two helper-driven steps that share the
/// fix-loop control flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HelperLoopKind {
    /// `code_review` step: reviewer team → triage → fix wave (kind=`fix`).
    CodeReview,
    /// `validation` step: validator → fix wave (kind=`validation_fix`).
    Validation,
}

impl HelperLoopKind {
    /// Short label used as the `kind` argument to the daemon's
    /// `dispatching N sub-agent(s) (kind: …)` display line. Distinct from
    /// the `Wave::kind` shape so per-step dispatches don't collide with
    /// implementation-wave naming.
    fn display_kind(self) -> &'static str {
        match self {
            HelperLoopKind::CodeReview => "review",
            HelperLoopKind::Validation => "validation",
        }
    }
}

/// Builds the documented `Compose([RetryTransient, RetryProtocol])` policy
/// shared by the two helper-driven steps.
fn helper_compose_policy(corrective_key: &str) -> RecoveryPolicy {
    RecoveryPolicy::Compose {
        policies: vec![
            RecoveryPolicy::RetryTransient {
                max: 3,
                backoff: Backoff::Exponential {
                    initial_ms: 500,
                    max_ms: 8_000,
                    factor: 2.0,
                },
            },
            RecoveryPolicy::RetryProtocol {
                max: 1,
                corrective: CorrectivePromptKey(corrective_key.to_string()),
            },
        ],
    }
}

/// Drives the helper → fix-wave → re-invoke loop for [`CodeReviewStep`] and
/// [`ValidationStep`].
///
/// The loop body:
///
/// 1. Invoke the helper (reviewer team or validator).
/// 2. On `Success` → [`AttemptOutcome::Success`].
/// 3. On `SemanticFailure { FixRequired }` → triage (code-review only),
///    APPEND a fix wave to the manifest via `compile-fix-waves`, re-enter
///    [`scheduler::run_wave_execution`] for the new wave, then loop.
/// 4. On `SemanticFailure { Blocked | Abort }` → fail the step with the
///    helper's notes surfaced to the user.
/// 5. On `ProtocolViolation` / `TransientInfra` / `HardInfra` → return the
///    matching [`AttemptOutcome`] variant; the registry-level recovery
///    policy decides what to do next.
async fn run_helper_fix_loop(
    ctx: &mut StepContext,
    manifest_path: &Path,
    kind: HelperLoopKind,
    max_iterations: u32,
) -> AttemptOutcome {
    let mut iteration: u32 = 0;
    let loop_started = Instant::now();
    loop {
        iteration += 1;
        // Enforce wall-clock budget on top of the iteration cap.
        // Each iteration can take 30+ minutes, so without a wall-clock guard
        // the loop can far exceed any reasonable step timeout.
        if loop_started.elapsed() > MAX_FIX_LOOP_BUDGET {
            tracing::warn!(
                kind = ?kind,
                iteration,
                elapsed_s = loop_started.elapsed().as_secs(),
                budget_s = MAX_FIX_LOOP_BUDGET.as_secs(),
                "fix-loop wall-clock budget exceeded; surfacing transient infra",
            );
            return AttemptOutcome::TransientInfra {
                error: "fix-loop wall-clock budget (2h) exceeded".into(),
            };
        }
        let started = Instant::now();
        let helper_result = invoke_loop_helper(ctx, manifest_path, kind, iteration);
        tracing::info!(
            kind = ?kind,
            iteration,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "helper invocation finished",
        );

        let helper_output = match helper_result {
            Ok(out) => out,
            Err(HelperError::SemanticFailure {
                status,
                notes,
                state_updates,
            }) => match status {
                HelperStatus::FixRequired => {
                    if iteration > max_iterations {
                        tracing::warn!(
                            kind = ?kind,
                            iteration,
                            "fix-loop budget exhausted; surfacing semantic mistake",
                        );
                        return AttemptOutcome::SemanticMistake {
                            fix_loop_round: iteration,
                        };
                    }
                    // Build and dispatch the fix wave; on any sub-failure we
                    // surface that as the attempt outcome and let the
                    // registry decide whether to retry or abort. The
                    // `state_updates` payload from the helper's SemanticFailure
                    // is forwarded so the validation branch can decode `gaps`
                    // without re-invoking the helper.
                    let dispatch =
                        dispatch_fix_wave(ctx, manifest_path, kind, iteration, &state_updates)
                            .await;
                    match dispatch {
                        FixWaveOutcome::Continue => continue,
                        FixWaveOutcome::Outcome(outcome) => return outcome,
                    }
                }
                HelperStatus::WaitingForHandoffs => {
                    // Skill emitted prompt files and is asking us to dispatch.
                    // Run the handoffs through `handoff::dispatch_all`, then
                    // re-invoke the helper with the captured outputs so it
                    // enters triage mode and returns a final envelope. Any
                    // dispatch failure surfaces as the attempt outcome — the
                    // step's RecoveryPolicy decides whether to retry.
                    match dispatch_handoffs_and_resume(
                        ctx,
                        manifest_path,
                        kind,
                        iteration,
                        &state_updates,
                    )
                    .await
                    {
                        DispatchOutcome::Continue => continue,
                        DispatchOutcome::Outcome(outcome) => return outcome,
                    }
                }
                HelperStatus::Blocked | HelperStatus::Abort => {
                    tracing::warn!(
                        kind = ?kind,
                        ?status,
                        notes = %notes,
                        "helper reported blocking semantic failure",
                    );
                    return AttemptOutcome::SpecDrift { gap: notes };
                }
                HelperStatus::Success => {
                    // `parse_and_validate_output` only emits `SemanticFailure`
                    // for non-success statuses; this arm is unreachable in
                    // practice but is left explicit so the match is total.
                    return AttemptOutcome::Success;
                }
            },
            Err(HelperError::ProtocolViolation { category, detail }) => {
                return AttemptOutcome::ProtocolViolation { category, detail };
            }
            Err(HelperError::TransientInfra(msg)) => {
                return AttemptOutcome::TransientInfra { error: msg };
            }
            Err(HelperError::HardInfra(msg)) => {
                return AttemptOutcome::HardInfra { error: msg };
            }
        };

        // Helper returned status=success (semantic pass).
        match helper_output.status {
            HelperStatus::Success => {
                tracing::info!(
                    kind = ?kind,
                    iteration,
                    "helper reported success; step complete",
                );
                return AttemptOutcome::Success;
            }
            // Defensive — `invoke_helper` already converts non-success into
            // an `Err`, so the value here is always `Success`.
            other => {
                return AttemptOutcome::ProtocolViolation {
                    category: "status_enum".to_string(),
                    detail: format!(
                        "helper returned non-success status `{other:?}` outside the error path",
                    ),
                };
            }
        }
    }
}

/// Result of [`dispatch_fix_wave`] — either continue the loop or surface a
/// definitive [`AttemptOutcome`] to the caller.
enum FixWaveOutcome {
    /// Fix wave dispatched and completed; loop body should re-invoke the helper.
    Continue,
    /// Definitive outcome to return from the step (failure on any rung of
    /// the fix-wave pipeline).
    Outcome(AttemptOutcome),
}

/// Result of [`dispatch_handoffs_and_resume`] — same shape as
/// [`FixWaveOutcome`] but specific to the dispatch→triage round-trip the
/// `WaitingForHandoffs` status triggers.
enum DispatchOutcome {
    /// Handoffs dispatched, outputs collected, sidecar staged. The loop body
    /// re-invokes the helper which then enters triage mode.
    Continue,
    /// Definitive outcome from a malformed handoff list, dispatch failure,
    /// or sidecar IO error.
    Outcome(AttemptOutcome),
}

/// Parses the `state_updates.handoffs` payload, dispatches the listed
/// sub-agents via `handoff::dispatch_all`, persists the collected outputs as
/// a sidecar JSON file the next helper invocation reads, and returns control
/// to the loop body so it re-invokes the helper in triage mode.
///
/// Sidecar shape (consumed by the run-reviewer-team-non-interactive skill):
///
/// ```json
/// [
///   { "index": 1, "exit_code": 0, "output": "...stdout..." },
///   { "index": 2, "exit_code": 0, "output": "..." }
/// ]
/// ```
///
/// The sidecar path is `<execution_root>/.tmp-helper-handoff-outputs-attempt-<N>.json`
/// — the next helper invocation passes its absolute path to the skill via the
/// `prior_handoff_outputs_path` input field. The skill enters triage mode
/// when that field is non-empty.
async fn dispatch_handoffs_and_resume(
    ctx: &mut StepContext,
    manifest_path: &Path,
    kind: HelperLoopKind,
    iteration: u32,
    state_updates: &serde_json::Value,
) -> DispatchOutcome {
    use crate::handoff::{dispatch_all, AgentType, Handoff};
    use crate::config::Config;

    // Step 1 — parse handoffs from state_updates. Any shape error becomes a
    // protocol violation so the step's RecoveryPolicy retries with the
    // corrective prompt.
    let raw = match state_updates.get("handoffs") {
        Some(v) => v,
        None => {
            return DispatchOutcome::Outcome(AttemptOutcome::ProtocolViolation {
                category: "handoffs_missing".to_string(),
                detail: "state_updates lacks `handoffs` array required by waiting_for_handoffs"
                    .to_string(),
            });
        }
    };
    let parsed: Vec<HandoffEntry> = match serde_json::from_value(raw.clone()) {
        Ok(v) => v,
        Err(e) => {
            return DispatchOutcome::Outcome(AttemptOutcome::ProtocolViolation {
                category: "handoffs_shape".to_string(),
                detail: format!("decode handoffs: {e}"),
            });
        }
    };
    if parsed.is_empty() {
        return DispatchOutcome::Outcome(AttemptOutcome::ProtocolViolation {
            category: "handoffs_empty".to_string(),
            detail: "handoffs array is empty".to_string(),
        });
    }

    // Step 2 — translate to the dispatcher's `Handoff` type. Reject paths
    // outside the workdir to mirror the existing fix-wave findings-path
    // canonicalize check.
    let canonical_workdir = match std::fs::canonicalize(&ctx.workdir) {
        Ok(p) => p,
        Err(e) => {
            return DispatchOutcome::Outcome(AttemptOutcome::HardInfra {
                error: format!("workdir canonicalize: {e}"),
            });
        }
    };
    let mut handoffs: Vec<Handoff> = Vec::with_capacity(parsed.len());
    for entry in &parsed {
        let agent_type = match entry.agent_type.as_str() {
            "claude" => AgentType::Claude,
            "codex" => AgentType::Codex,
            "gemini" => AgentType::Gemini,
            "bash" => AgentType::Bash,
            other => {
                return DispatchOutcome::Outcome(AttemptOutcome::ProtocolViolation {
                    category: "handoffs_agent_type".to_string(),
                    detail: format!("unknown agent_type `{other}` (index {})", entry.index),
                });
            }
        };
        let prompt_path = std::path::PathBuf::from(&entry.prompt_file);
        let canonical_prompt = match std::fs::canonicalize(&prompt_path) {
            Ok(p) => p,
            Err(e) => {
                return DispatchOutcome::Outcome(AttemptOutcome::ProtocolViolation {
                    category: "handoffs_prompt_missing".to_string(),
                    detail: format!("prompt file `{}`: {e}", prompt_path.display()),
                });
            }
        };
        if !canonical_prompt.starts_with(&canonical_workdir) {
            return DispatchOutcome::Outcome(AttemptOutcome::ProtocolViolation {
                category: "handoffs_prompt_escape".to_string(),
                detail: format!(
                    "prompt file `{}` escapes execution_root `{}`",
                    canonical_prompt.display(),
                    canonical_workdir.display()
                ),
            });
        }
        handoffs.push(Handoff {
            index: entry.index,
            agent_type,
            prompt_file: canonical_prompt,
            can_fail: entry.can_fail.unwrap_or(false),
        });
    }

    // Step 3 — dispatch via the same `dispatch_all` the wave executor uses,
    // wiring SchedulerHooks (when present) so output-tail and KillJob keep
    // working for the review wave too.
    let config = match Config::load(None) {
        Ok(c) => c,
        Err(e) => {
            return DispatchOutcome::Outcome(AttemptOutcome::HardInfra {
                error: format!("config load failed: {e}"),
            });
        }
    };
    let (subagent_tx, pgid_tx) = match ctx.daemon_hooks.as_ref() {
        Some(hooks) => {
            let dispatch_num = hooks
                .announce_wave_dispatch(handoffs.len(), kind.display_kind())
                .await;
            (
                Some(hooks.spawn_subagent_writer(dispatch_num)),
                Some(hooks.spawn_pgid_registrar()),
            )
        }
        None => (None, None),
    };
    let (results, _pgids) = dispatch_all(
        handoffs,
        &config.agents.claude,
        &config.agents.codex,
        &config.agents.gemini,
        &config.agents.bash,
        None,
        pgid_tx,
        subagent_tx,
    )
    .await;
    if let Some(hooks) = ctx.daemon_hooks.as_ref() {
        for r in &results {
            hooks.announce_subagent_done(r.index, r.success, r.can_fail, r.stdout.len(), &r.stderr);
        }
    }

    // Step 4 — fail fast if any required reviewer failed. The skill's
    // contract makes index 1 (Claude) and index 4 (Security) required;
    // codex/gemini are can_fail. We honor the per-handoff can_fail flag the
    // skill set rather than hard-coding indices.
    let any_required_failed = results
        .iter()
        .any(|r| !r.success && !r.can_fail);
    if any_required_failed {
        return DispatchOutcome::Outcome(AttemptOutcome::SemanticMistake {
            fix_loop_round: iteration,
        });
    }

    // Step 5 — persist the outputs as a sidecar the next helper invocation
    // can read, then continue the loop body so it re-invokes the helper.
    let sidecar_path = ctx
        .workdir
        .join(format!(".tmp-helper-handoff-outputs-attempt-{iteration}.json"));
    let sidecar_payload: Vec<HandoffOutputEntry> = results
        .iter()
        .map(|r| HandoffOutputEntry {
            index: r.index,
            exit_code: i32::from(r.success),
            output: r.stdout.clone(),
            stderr: r.stderr.clone(),
        })
        .collect();
    let body = match serde_json::to_string_pretty(&sidecar_payload) {
        Ok(s) => s,
        Err(e) => {
            return DispatchOutcome::Outcome(AttemptOutcome::HardInfra {
                error: format!("serialize handoff outputs: {e}"),
            });
        }
    };
    if let Err(e) = std::fs::write(&sidecar_path, body) {
        return DispatchOutcome::Outcome(AttemptOutcome::HardInfra {
            error: format!("write handoff outputs sidecar: {e}"),
        });
    }
    // Stash the sidecar path on the context so the next loop iteration's
    // input builder picks it up.
    ctx.workdir = ctx.workdir.clone(); // no-op; kept for symmetry. The
    // sidecar path is recomputed by build_review_team_input on next iteration
    // using `iteration`; we don't mutate ctx.
    let _ = manifest_path;

    DispatchOutcome::Continue
}

/// Single entry parsed out of `state_updates.handoffs`. Mirrors the
/// `handoffs.schema.json` shape; serde rejects unknown fields so a skill
/// emitting a typo gets a precise protocol violation rather than a silent
/// drop.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct HandoffEntry {
    index: usize,
    agent_type: String,
    #[serde(default)]
    can_fail: Option<bool>,
    prompt_file: String,
}

/// Single entry written into the handoff-outputs sidecar the helper reads
/// in triage mode.
#[derive(Debug, serde::Serialize)]
struct HandoffOutputEntry {
    index: usize,
    exit_code: i32,
    output: String,
    stderr: String,
}

/// Executes the fix-loop sub-pipeline: load manifest → triage (review only) →
/// compile-fix-waves → re-enter scheduler for the appended wave.
///
/// On any error returns [`FixWaveOutcome::Outcome`] with the attempt outcome
/// the caller should surface; otherwise returns [`FixWaveOutcome::Continue`]
/// so the helper-loop body re-invokes the helper.
async fn dispatch_fix_wave(
    ctx: &mut StepContext,
    manifest_path: &Path,
    kind: HelperLoopKind,
    iteration: u32,
    state_updates: &serde_json::Value,
) -> FixWaveOutcome {
    let manifest_before = match scheduler::load_manifest(manifest_path) {
        Ok(m) => m,
        Err(e) => return FixWaveOutcome::Outcome(scheduler_load_outcome(e)),
    };
    let plan_path = PathBuf::from(&manifest_before.plan.path);
    let execution_root = match manifest_path.parent() {
        Some(p) => p.to_path_buf(),
        None => {
            return FixWaveOutcome::Outcome(AttemptOutcome::HardInfra {
                error: format!(
                    "manifest_path `{}` has no parent directory",
                    manifest_path.display()
                ),
            });
        }
    };

    // Step 1 — produce the findings JSON file the CLI consumes. For the
    // code-review path this is a triage call into the review-execution-output
    // helper; for the validation path the validator's own gaps array is
    // already structured findings (read directly from the SemanticFailure
    // `state_updates`, no helper re-invocation).
    let findings_path = match build_findings_for_fix_wave(
        ctx,
        manifest_path,
        &execution_root,
        &plan_path,
        kind,
        iteration,
        state_updates,
    ) {
        Ok(p) => p,
        Err(outcome) => return FixWaveOutcome::Outcome(outcome),
    };

    // Step 2 — invoke the existing `compile-fix-waves` CLI. Reuses the
    // production binary so we do not duplicate compile-plan logic; the CLI
    // mutates `tasks.json` in place via APPEND mode. Canonicalize the
    // findings path and confirm it stays within the workdir before passing
    // it into the child process — defends against helper-supplied paths
    // that escape the workdir via traversal or symlinks.
    let canonical_findings = match std::fs::canonicalize(&findings_path) {
        Ok(p) => p,
        Err(e) => {
            return FixWaveOutcome::Outcome(AttemptOutcome::ProtocolViolation {
                category: "findings_path_canonicalize".into(),
                detail: format!("{}: {e}", findings_path.display()),
            });
        }
    };
    let canonical_workdir = match std::fs::canonicalize(&ctx.workdir) {
        Ok(p) => p,
        Err(e) => {
            return FixWaveOutcome::Outcome(AttemptOutcome::HardInfra {
                error: format!("workdir canonicalize: {e}"),
            });
        }
    };
    if !canonical_findings.starts_with(&canonical_workdir) {
        return FixWaveOutcome::Outcome(AttemptOutcome::ProtocolViolation {
            category: "findings_path_escape".into(),
            detail: format!(
                "{} not under {}",
                canonical_findings.display(),
                canonical_workdir.display()
            ),
        });
    }
    let appended_path =
        match invoke_compile_fix_waves_cli(&plan_path, &execution_root, &canonical_findings) {
            Ok(path) => path,
            Err(outcome) => return FixWaveOutcome::Outcome(outcome),
        };
    tracing::info!(
        path = %appended_path.display(),
        "compile-fix-waves CLI returned updated manifest path",
    );

    // Step 3 — reload the now-mutated manifest and dispatch only the new
    // waves (those not present before APPEND).
    let manifest_after = match scheduler::load_manifest(manifest_path) {
        Ok(m) => m,
        Err(e) => return FixWaveOutcome::Outcome(scheduler_load_outcome(e)),
    };
    let new_waves = waves_added_by_append(&manifest_before, &manifest_after);
    if new_waves.is_empty() {
        return FixWaveOutcome::Outcome(AttemptOutcome::ProtocolViolation {
            category: "fix_wave_missing".to_string(),
            detail: "compile-fix-waves did not append any new wave".to_string(),
        });
    }

    let scoped = match manifest_scoped_to_waves(&manifest_after, &new_waves) {
        Ok(m) => m,
        Err(outcome) => return FixWaveOutcome::Outcome(outcome),
    };
    let scheduler_outcome =
        scheduler::run_wave_execution(ctx, &scoped, &execution_root).await;
    match scheduler_outcome {
        AttemptOutcome::Success => FixWaveOutcome::Continue,
        other => FixWaveOutcome::Outcome(other),
    }
}

/// Maps a [`SchedulerError`] from `load_manifest` onto the
/// caller-facing [`AttemptOutcome`] variant.
fn scheduler_load_outcome(err: SchedulerError) -> AttemptOutcome {
    match err {
        SchedulerError::ManifestRead { .. } => AttemptOutcome::HardInfra {
            error: err.to_string(),
        },
        SchedulerError::ManifestParse { .. } | SchedulerError::Invariant(_) => {
            AttemptOutcome::ProtocolViolation {
                category: "manifest_invariant".to_string(),
                detail: err.to_string(),
            }
        }
    }
}

/// Invokes the helper for the current loop iteration and returns the raw
/// [`HelperOutput`] envelope (status/next_step/notes/state_updates).
///
/// Uses [`invoke_helper`] directly rather than the typed wrappers so the
/// `state_updates` payload remains accessible on `fix_required` outcomes —
/// the typed wrappers discard it via [`HelperError::SemanticFailure`].
fn invoke_loop_helper(
    ctx: &StepContext,
    manifest_path: &Path,
    kind: HelperLoopKind,
    iteration: u32,
) -> Result<HelperOutput, HelperError> {
    match kind {
        HelperLoopKind::CodeReview => {
            let input = build_review_team_input(ctx, manifest_path, iteration)?;
            let envelope =
                serde_json::to_value(&input).map_err(|e| HelperError::ProtocolViolation {
                    category: "input_serialize".to_string(),
                    detail: format!("serialize ReviewTeamInput failed: {e}"),
                })?;
            invoke_helper(HelperSkill::RunReviewerTeam, envelope, ctx)
        }
        HelperLoopKind::Validation => {
            let input = build_validator_input(ctx, manifest_path, iteration)?;
            let envelope =
                serde_json::to_value(&input).map_err(|e| HelperError::ProtocolViolation {
                    category: "input_serialize".to_string(),
                    detail: format!("serialize ValidatorInput failed: {e}"),
                })?;
            invoke_helper(HelperSkill::ValidateExecutionPlan, envelope, ctx)
        }
    }
}

/// Produces a [`ReviewTeamInput`] from the per-step context.
///
/// Plan path is sourced from the manifest. `changed_files` is populated from
/// the helper-state file if available; otherwise an empty list (the helper
/// will fall back to its own discovery). `recipe_list` defaults to the empty
/// vec — the helper consults the workdir for project-specific recipes.
fn build_review_team_input(
    ctx: &StepContext,
    manifest_path: &Path,
    iteration: u32,
) -> Result<ReviewTeamInput, HelperError> {
    let manifest = scheduler::load_manifest(manifest_path)
        .map_err(|e| HelperError::HardInfra(format!("load manifest for review-team input: {e}")))?;
    // Re-entry sidecar from the previous loop iteration's
    // `dispatch_handoffs_and_resume` call, when present. We probe rather than
    // tracking attempt-N→iteration mapping in `ctx`: the path is fully
    // recoverable from `iteration - 1`, and absence simply means the prior
    // iteration was a fresh dispatch (or this is the first invocation).
    let prior_outputs = if iteration > 1 {
        let candidate = ctx
            .workdir
            .join(format!(".tmp-helper-handoff-outputs-attempt-{}.json", iteration - 1));
        if candidate.is_file() {
            candidate.to_string_lossy().into_owned()
        } else {
            String::new()
        }
    } else {
        String::new()
    };
    Ok(ReviewTeamInput {
        plan_context: manifest.plan.path.clone(),
        execution_outputs: format!(
            "wave_execution iteration {iteration}; manifest at {}",
            manifest_path.display()
        ),
        changed_files: Vec::new(),
        language: detect_language(&ctx.workdir),
        recipe_list: Vec::new(),
        prior_review_context: json!({}),
        execution_root: ctx.workdir.clone(),
        attempt: iteration,
        prior_handoff_outputs_path: prior_outputs,
    })
}

/// Produces a [`ValidatorInput`] from the per-step context.
fn build_validator_input(
    ctx: &StepContext,
    manifest_path: &Path,
    iteration: u32,
) -> Result<ValidatorInput, HelperError> {
    let manifest = scheduler::load_manifest(manifest_path)
        .map_err(|e| HelperError::HardInfra(format!("load manifest for validator input: {e}")))?;
    let state_file_path = ctx.workdir.join(".tmp-execute-plan-state.json");
    // Re-entry sidecar from the previous loop iteration's
    // `dispatch_handoffs_and_resume` call, when present. Same probe pattern
    // as `build_review_team_input` so the validator skill enters triage
    // mode automatically on the second invocation.
    let prior_outputs = if iteration > 1 {
        let candidate = ctx
            .workdir
            .join(format!(".tmp-helper-handoff-outputs-attempt-{}.json", iteration - 1));
        if candidate.is_file() {
            candidate.to_string_lossy().into_owned()
        } else {
            String::new()
        }
    } else {
        String::new()
    };
    Ok(ValidatorInput {
        plan_path: PathBuf::from(&manifest.plan.path),
        execution_root: ctx.workdir.clone(),
        changed_files: Vec::new(),
        language: detect_language(&ctx.workdir),
        recipe_list: Vec::new(),
        skip_code_review: false,
        state_file_path,
        execution_state: json!({}),
        validation_state: json!({}),
        validation_state_path: None,
        current_validation_attempt: iteration,
        prior_validation_notes: json!({}),
        prior_helper_outcomes: json!({}),
        prior_handoff_outputs_path: prior_outputs,
    })
}

/// Heuristic language detector for helper inputs. Looks for canonical project
/// markers; defaults to `"rust"` to match the host project.
fn detect_language(workdir: &Path) -> String {
    if workdir.join("Cargo.toml").is_file() {
        return "rust".to_string();
    }
    if workdir.join("package.json").is_file() {
        return "typescript".to_string();
    }
    if workdir.join("pyproject.toml").is_file() || workdir.join("setup.py").is_file() {
        return "python".to_string();
    }
    if workdir.join("go.mod").is_file() {
        return "go".to_string();
    }
    "rust".to_string()
}

/// Generates the `findings.json` file consumed by the `compile-fix-waves`
/// CLI. The shape depends on the loop variant:
///
/// - For [`HelperLoopKind::CodeReview`], invokes the review-triage helper
///   to obtain `wave_id_for_fix` plus the consolidated findings file path,
///   then re-uses the helper-produced file directly when it is already in
///   the findings.json shape; otherwise wraps the helper-supplied
///   `findings_path` in a synthetic findings document. The triage-supplied
///   path is canonicalized and confirmed to live under `ctx.workdir` before
///   it is returned.
/// - For [`HelperLoopKind::Validation`], reads the validator's `gaps` array
///   from the helper's `state_updates` payload (no re-invocation — the
///   outer loop already paid the cost), converts each gap into a
///   [`Finding`] with `category="validation_gap"` and severity `major`, and
///   writes them under `<workdir>/.plan-executor/fix-loop/`.
///
/// On any failure returns the [`AttemptOutcome`] the caller should surface.
fn build_findings_for_fix_wave(
    ctx: &StepContext,
    _manifest_path: &Path,
    _execution_root: &Path,
    plan_path: &Path,
    kind: HelperLoopKind,
    iteration: u32,
    state_updates: &serde_json::Value,
) -> Result<PathBuf, AttemptOutcome> {
    match kind {
        HelperLoopKind::CodeReview => {
            // The fix-wave triage helper is invoked synchronously after a
            // SemanticFailure; if the helper requests `waiting_for_handoffs`
            // it has its own dispatch round-trip handled by run_helper_fix_loop.
            // We don't pre-populate `prior_handoff_outputs_path` here (the
            // first call into the triage skill is always dispatch mode).
            let triage_input = ReviewTriageInput {
                plan_path: plan_path.to_path_buf(),
                execution_root: ctx.workdir.clone(),
                changed_files: Vec::new(),
                language: detect_language(&ctx.workdir),
                recipe_list: Vec::new(),
                skip_code_review: false,
                state_file_path: ctx.workdir.join(".tmp-execute-plan-state.json"),
                execution_state: json!({}),
                review_state: json!({}),
                review_state_path: None,
                prior_review_notes: json!({}),
                prior_handoff_outputs_path: String::new(),
            };
            let envelope = serde_json::to_value(&triage_input).map_err(|e| {
                AttemptOutcome::ProtocolViolation {
                    category: "input_serialize".to_string(),
                    detail: format!("serialize ReviewTriageInput failed: {e}"),
                }
            })?;
            let raw = invoke_helper(HelperSkill::ReviewExecutionOutput, envelope, ctx)
                .map_err(helper_error_to_outcome)?;
            // Triage state_updates carries `triaged_findings_path` and
            // optionally `wave_id_for_fix`. The triage helper guarantees the
            // findings file is in `findings.json` shape (the file the CLI
            // consumes directly). The reviewer-team `findings_path` (when
            // present) and the validator `validation_report_path` are also
            // helper-supplied paths; we canonicalize each one and require
            // it to remain under `ctx.workdir` before trusting it.
            let triaged_findings_path = raw
                .state_updates
                .get("triaged_findings_path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| AttemptOutcome::ProtocolViolation {
                    category: "triage_missing_path".to_string(),
                    detail: "triage state_updates missing triaged_findings_path".to_string(),
                })?;
            let triaged_buf = PathBuf::from(triaged_findings_path);
            let canonical =
                ensure_path_under_workdir(&triaged_buf, &ctx.workdir, "triaged_findings_path")?;
            // If the reviewer team also reported its raw `findings_path`,
            // confirm it is contained too. We do not consume that path here
            // (the triaged file is what the CLI ingests), but a missing
            // containment check would still let an attacker influence
            // downstream consumers via the same payload.
            if let Some(findings_path_str) = raw
                .state_updates
                .get("findings_path")
                .and_then(|v| v.as_str())
            {
                ensure_path_under_workdir(
                    Path::new(findings_path_str),
                    &ctx.workdir,
                    "findings_path",
                )?;
            }
            Ok(canonical)
        }
        HelperLoopKind::Validation => {
            // The validator's `state_updates` payload was forwarded from
            // `HelperError::SemanticFailure` so we do not re-invoke the
            // helper. Decode `gaps` into `Vec<ValidationGap>` directly.
            // Optionally containment-check `validation_report_path` when the
            // helper supplied one.
            if let Some(report_path_str) = state_updates
                .get("validation_report_path")
                .and_then(|v| v.as_str())
            {
                ensure_path_under_workdir(
                    Path::new(report_path_str),
                    &ctx.workdir,
                    "validation_report_path",
                )?;
            }
            let gaps_value = state_updates
                .get("gaps")
                .cloned()
                .unwrap_or(serde_json::Value::Array(Vec::new()));
            let gaps: Vec<crate::helper::ValidationGap> = serde_json::from_value(gaps_value)
                .map_err(|e| AttemptOutcome::ProtocolViolation {
                    category: "validation_gaps_shape".into(),
                    detail: format!("decode validator gaps from state_updates failed: {e}"),
                })?;
            let findings: Vec<Finding> = gaps
                .iter()
                .enumerate()
                .map(|(idx, gap)| Finding {
                    id: format!("V{:03}", idx + 1),
                    severity: Severity::Major,
                    category: "validation_gap".to_string(),
                    description: format!("{}: {}", gap.goal, gap.missing_evidence),
                    files: Vec::new(),
                    suggested_fix: None,
                })
                .collect();
            if findings.is_empty() {
                return Err(AttemptOutcome::ProtocolViolation {
                    category: "validator_no_gaps".to_string(),
                    detail: "validator reported fix_required without gaps".to_string(),
                });
            }
            write_findings_file(ctx, kind, iteration, &findings)
        }
    }
}

/// Canonicalizes `path` and confirms it is a descendant of `workdir`
/// (also canonicalized). Returns [`AttemptOutcome::ProtocolViolation`] with a
/// `<field>_canonicalize` or `<field>_escape` category when the path cannot
/// be canonicalized or escapes the workdir. Defends helper-supplied paths
/// against directory-traversal and symlink escapes.
fn ensure_path_under_workdir(
    path: &Path,
    workdir: &Path,
    field: &str,
) -> Result<PathBuf, AttemptOutcome> {
    let canonical_path =
        std::fs::canonicalize(path).map_err(|e| AttemptOutcome::ProtocolViolation {
            category: format!("{field}_canonicalize"),
            detail: format!("{}: {e}", path.display()),
        })?;
    let canonical_workdir =
        std::fs::canonicalize(workdir).map_err(|e| AttemptOutcome::HardInfra {
            error: format!("workdir canonicalize: {e}"),
        })?;
    if !canonical_path.starts_with(&canonical_workdir) {
        return Err(AttemptOutcome::ProtocolViolation {
            category: format!("{field}_escape"),
            detail: format!(
                "{} not under {}",
                canonical_path.display(),
                canonical_workdir.display()
            ),
        });
    }
    Ok(canonical_path)
}

/// Converts a [`HelperError`] into the matching [`AttemptOutcome`] variant.
fn helper_error_to_outcome(err: HelperError) -> AttemptOutcome {
    match err {
        HelperError::HardInfra(msg) => AttemptOutcome::HardInfra { error: msg },
        HelperError::TransientInfra(msg) => AttemptOutcome::TransientInfra { error: msg },
        HelperError::ProtocolViolation { category, detail } => {
            AttemptOutcome::ProtocolViolation { category, detail }
        }
        HelperError::SemanticFailure {
            status,
            notes,
            state_updates: _,
        } => match status {
            HelperStatus::FixRequired => AttemptOutcome::SemanticMistake { fix_loop_round: 0 },
            HelperStatus::Blocked | HelperStatus::Abort => AttemptOutcome::SpecDrift { gap: notes },
            HelperStatus::Success => AttemptOutcome::Success,
            // The triage helper is invoked from the fix-wave path which
            // doesn't have a dispatcher set up; surface as a protocol
            // violation so the operator sees the misuse rather than a
            // confusing drop. The helper-loop driver above handles the
            // dispatch path explicitly.
            HelperStatus::WaitingForHandoffs => AttemptOutcome::ProtocolViolation {
                category: "waiting_for_handoffs_unexpected".to_string(),
                detail: "triage helper requested handoffs from a context that cannot dispatch them".to_string(),
            },
        },
    }
}

/// Serializes `findings` to a fresh JSON file under
/// `<workdir>/.plan-executor/fix-loop/<seq>-<attempt>-<iter>-<kind>.findings.json`.
fn write_findings_file(
    ctx: &StepContext,
    kind: HelperLoopKind,
    iteration: u32,
    findings: &[Finding],
) -> Result<PathBuf, AttemptOutcome> {
    let dir = ctx.workdir.join(".plan-executor").join("fix-loop");
    std::fs::create_dir_all(&dir).map_err(|e| AttemptOutcome::HardInfra {
        error: format!("create fix-loop dir {} failed: {e}", dir.display()),
    })?;
    let kind_tag = match kind {
        HelperLoopKind::CodeReview => "review",
        HelperLoopKind::Validation => "validation",
    };
    let path = dir.join(format!(
        "{:03}-{:03}-{:02}-{kind_tag}.findings.json",
        ctx.step_seq, ctx.attempt_n, iteration
    ));
    let payload = json!({ "findings": findings });
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&payload).map_err(|e| AttemptOutcome::HardInfra {
            error: format!("serialize findings: {e}"),
        })?,
    )
    .map_err(|e| AttemptOutcome::HardInfra {
        error: format!("write findings file {} failed: {e}", path.display()),
    })?;
    Ok(path)
}

/// Shells out to `plan-executor compile-fix-waves` reusing the binary that is
/// running this step — we do not re-implement compile-plan logic in-process.
///
/// On success returns the `PathBuf` printed on stdout (the updated manifest
/// path); on any failure returns the [`AttemptOutcome`] the caller should
/// surface.
fn invoke_compile_fix_waves_cli(
    plan_path: &Path,
    execution_root: &Path,
    findings_path: &Path,
) -> Result<PathBuf, AttemptOutcome> {
    let exe = std::env::current_exe().map_err(|e| AttemptOutcome::HardInfra {
        error: format!("resolve current exe for compile-fix-waves: {e}"),
    })?;
    let output = Command::new(&exe)
        .arg("compile-fix-waves")
        .arg("--plan")
        .arg(plan_path)
        .arg("--execution-root")
        .arg(execution_root)
        .arg("--findings-json")
        .arg(findings_path)
        .output()
        .map_err(|e| AttemptOutcome::HardInfra {
            error: format!("spawn compile-fix-waves CLI: {e}"),
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        return Err(AttemptOutcome::ProtocolViolation {
            category: "compile_fix_waves_failed".to_string(),
            detail: format!(
                "compile-fix-waves exited {:?}: {}",
                output.status.code(),
                stderr.trim(),
            ),
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        return Err(AttemptOutcome::ProtocolViolation {
            category: "compile_fix_waves_empty_stdout".to_string(),
            detail: "compile-fix-waves produced no manifest path on stdout".to_string(),
        });
    }
    Ok(PathBuf::from(stdout))
}

/// Returns the wave ids present in `after` but not in `before`. Maintains
/// numeric order so the scheduler's topological pass dispatches the
/// freshly-appended waves in the order the compiler emitted them.
fn waves_added_by_append(before: &Manifest, after: &Manifest) -> Vec<u32> {
    let known: std::collections::HashSet<u32> = before.waves.iter().map(|w| w.id).collect();
    let mut new: Vec<u32> = after
        .waves
        .iter()
        .filter(|w| !known.contains(&w.id))
        .map(|w| w.id)
        .collect();
    new.sort_unstable();
    new
}

/// Builds a [`Manifest`] copy that contains only the listed waves and the
/// task entries those waves reference, so the scheduler treats the
/// freshly-appended fix waves as the only work to do.
fn manifest_scoped_to_waves(full: &Manifest, wave_ids: &[u32]) -> Result<Manifest, AttemptOutcome> {
    let id_set: std::collections::HashSet<u32> = wave_ids.iter().copied().collect();
    let waves: Vec<scheduler::Wave> = full
        .waves
        .iter()
        .filter(|w| id_set.contains(&w.id))
        // Strip cross-manifest dependencies that point at completed
        // implementation waves; the scoped manifest only contains fix waves.
        .map(|w| scheduler::Wave {
            id: w.id,
            task_ids: w.task_ids.clone(),
            depends_on: w
                .depends_on
                .iter()
                .copied()
                .filter(|d| id_set.contains(d))
                .collect(),
            kind: w.kind.clone(),
        })
        .collect();

    if waves.is_empty() {
        return Err(AttemptOutcome::ProtocolViolation {
            category: "fix_wave_scope_empty".to_string(),
            detail: "scoped manifest had no waves".to_string(),
        });
    }

    let mut tasks = std::collections::HashMap::new();
    for wave in &waves {
        for tid in &wave.task_ids {
            let spec = full
                .tasks
                .get(tid)
                .ok_or_else(|| AttemptOutcome::ProtocolViolation {
                    category: "fix_wave_missing_task".to_string(),
                    detail: format!("scoped manifest references unknown task `{tid}`"),
                })?;
            tasks.insert(tid.clone(), spec.clone());
        }
    }

    Ok(Manifest {
        version: full.version,
        plan: full.plan.clone(),
        waves,
        tasks,
    })
}

// ---------------------------------------------------------------------------
// D3.3 — IntegrationTestingStep helpers
// ---------------------------------------------------------------------------

/// Per-step attempt directory built from the [`StepContext`] using the same
/// layout as [`crate::job::storage`]:
/// `<job_dir>/steps/<NNN-<name>>/attempts/<n>/`.
fn attempt_dir(ctx: &StepContext, step_name: &str) -> PathBuf {
    ctx.job_dir
        .join("steps")
        .join(format!("{:03}-{step_name}", ctx.step_seq))
        .join("attempts")
        .join(ctx.attempt_n.to_string())
}

/// Runs the integration-test command and writes captured output to
/// `attempts/<n>/integration-tests.log`.
///
/// Returns a [`AttemptOutcome::Success`] on a clean exit;
/// [`AttemptOutcome::TransientInfra`] when stderr matches a transient
/// signal (so `RetryTransient { max: 1 }` can re-run);
/// [`AttemptOutcome::SemanticMistake`] when the failure looks like a real
/// test bug;
/// [`AttemptOutcome::HardInfra`] when the attempt-dir cannot be prepared or
/// the test binary cannot be spawned.
fn run_integration_tests(ctx: &StepContext) -> AttemptOutcome {
    let dir = attempt_dir(ctx, "integration_testing");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return AttemptOutcome::HardInfra {
            error: format!("create attempt dir {} failed: {e}", dir.display()),
        };
    }
    let log_path = dir.join("integration-tests.log");

    let started = Instant::now();
    let output = match Command::new(INTEGRATION_TEST_PROGRAM)
        .args(INTEGRATION_TEST_ARGS)
        .current_dir(&ctx.workdir)
        .stdin(Stdio::null())
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            return AttemptOutcome::HardInfra {
                error: format!(
                    "spawn `{} {}` in {}: {e}",
                    INTEGRATION_TEST_PROGRAM,
                    INTEGRATION_TEST_ARGS.join(" "),
                    ctx.workdir.display()
                ),
            };
        }
    };
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

    let mut combined = Vec::with_capacity(output.stdout.len() + output.stderr.len() + 256);
    combined.extend_from_slice(b"# integration-tests\n# command: ");
    combined.extend_from_slice(INTEGRATION_TEST_PROGRAM.as_bytes());
    for arg in INTEGRATION_TEST_ARGS {
        combined.push(b' ');
        combined.extend_from_slice(arg.as_bytes());
    }
    combined.push(b'\n');
    combined.extend_from_slice(b"# workdir: ");
    combined.extend_from_slice(ctx.workdir.display().to_string().as_bytes());
    combined.push(b'\n');
    combined.extend_from_slice(b"\n## stdout\n");
    combined.extend_from_slice(&output.stdout);
    combined.extend_from_slice(b"\n## stderr\n");
    combined.extend_from_slice(&output.stderr);
    if let Err(e) = std::fs::write(&log_path, &combined) {
        return AttemptOutcome::HardInfra {
            error: format!("write integration-tests.log to {}: {e}", log_path.display()),
        };
    }

    if output.status.success() {
        tracing::info!(
            elapsed_ms,
            log = %log_path.display(),
            "integration tests passed",
        );
        return AttemptOutcome::Success;
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let exit_code = output.status.code();
    tracing::warn!(
        ?exit_code,
        elapsed_ms,
        log = %log_path.display(),
        "integration tests failed",
    );
    if is_transient_test_error(&stderr) {
        AttemptOutcome::TransientInfra {
            error: format!(
                "integration tests failed transiently (exit {exit_code:?}); see {}",
                log_path.display()
            ),
        }
    } else {
        AttemptOutcome::SemanticMistake { fix_loop_round: 0 }
    }
}

/// Detects transient test-runner errors that warrant `RetryTransient` rather
/// than a `SemanticMistake`. Conservative — pattern set mirrors
/// [`crate::job::steps::pr_finalize::is_transient_gh_error`] but tuned for
/// `cargo test` output.
///
/// Single-word markers (`timeout`) are matched against tokens
/// rather than substrings to avoid false positives when the marker appears
/// inside an unrelated identifier (e.g. `MyTimeoutError`). Multi-word
/// markers (`connection reset`, `network is unreachable`) are still matched
/// as substrings because their phrasing is specific enough that incidental
/// matches are negligible.
fn is_transient_test_error(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    if has_transient_token(&lower, &["timeout"]) {
        return true;
    }
    lower.contains("connection reset")
        || lower.contains("network is unreachable")
        || lower.contains("timed out")
        || lower.contains("temporary failure")
        || lower.contains("rate limit")
        || lower.contains("could not download")
        || lower.contains("text file busy")
        || lower.contains("resource temporarily unavailable")
}

/// Returns `true` when any of `targets` appears as a whole token in `lower`.
///
/// Tokens are runs of ASCII alphanumerics; everything else (whitespace,
/// punctuation, brackets, quotes, slashes, etc.) is treated as a separator.
/// Used by [`is_transient_test_error`] and [`is_transient_gh_error`] to
/// avoid substring false positives. `lower` is expected to be
/// pre-lowercased by the caller; `targets` MUST also be lowercase.
fn has_transient_token(lower: &str, targets: &[&str]) -> bool {
    lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|tok| targets.iter().any(|t| tok == *t))
}

// ---------------------------------------------------------------------------
// D3.3 — PrCreationStep helpers
// ---------------------------------------------------------------------------

/// Plan-block fields the PR-creation and summary steps consult. Decoded from
/// the raw manifest JSON because [`scheduler::PlanBlock`] only exposes the
/// fields the scheduler itself uses.
#[derive(Debug, Clone)]
struct PlanMeta {
    /// Plan goal (used as the PR title prefix).
    goal: String,
    /// Plan kind (`feature` / `bug` / `refactor` / `chore` / `docs` / `infra`).
    plan_type: String,
    /// JIRA ticket id, when present.
    jira: Option<String>,
    /// `owner/repo` slug, when present (`null` for local-only runs).
    target_repo: Option<String>,
    /// Branch name to push / open the PR from.
    target_branch: Option<String>,
    /// Manifest plan flags.
    flags: PlanFlags,
}

/// Plan-flag block in the manifest. Field-for-field mirror of the schema's
/// `plan.flags` object. Each flag's runtime semantics are documented on its
/// reader rather than here so the contract stays close to the consumer.
#[derive(Debug, Clone, Copy, Default)]
struct PlanFlags {
    /// Honored by `summary` step's pr-finalize delegation: when true, skips
    /// the helper invocation and writes a local summary instead.
    skip_pr: bool,
    /// Honored by `pr_creation`: when true, runs `gh pr create --draft`.
    draft_pr: bool,
    /// Honored by `summary` step's pr-finalize delegation: when true, the
    /// `PrFinalizeInput.merge_mode` is set to `Merge` so the helper runs
    /// `gh pr merge --merge` after finalization.
    merge: bool,
    /// Honored by `summary` step's pr-finalize delegation: when true, the
    /// `PrFinalizeInput.merge_mode` is set to `MergeAdmin` so the helper
    /// runs `gh pr merge --merge --admin`. Wins over `merge` when both are
    /// set (admin merges bypass branch protections, which the plain
    /// `--merge` cannot).
    merge_admin: bool,
    /// Honored by `code_review` step: when true, the step short-circuits
    /// to `Success` without invoking the reviewer team.
    skip_code_review: bool,
    /// Honored by `preflight` step: when true, the step skips worktree +
    /// branch creation. The plan runs in the source repo's existing
    /// checkout.
    no_worktree: bool,
}

/// Loads `plan.{goal,type,jira,target_repo,target_branch,flags}` from the
/// manifest JSON. Returns a [`AttemptOutcome::HardInfra`] when the file
/// cannot be read and [`AttemptOutcome::ProtocolViolation`] when its shape
/// disagrees with the schema.
fn load_plan_meta(manifest_path: &Path) -> Result<PlanMeta, AttemptOutcome> {
    let raw = std::fs::read_to_string(manifest_path).map_err(|e| AttemptOutcome::HardInfra {
        error: format!("read manifest {}: {e}", manifest_path.display()),
    })?;
    let value: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| AttemptOutcome::ProtocolViolation {
            category: "manifest_invariant".to_string(),
            detail: format!("parse manifest {}: {e}", manifest_path.display()),
        })?;
    let plan = value
        .get("plan")
        .ok_or_else(|| AttemptOutcome::ProtocolViolation {
            category: "manifest_invariant".to_string(),
            detail: "manifest missing top-level `plan` block".to_string(),
        })?;
    let goal = plan
        .get("goal")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AttemptOutcome::ProtocolViolation {
            category: "manifest_invariant".to_string(),
            detail: "manifest plan.goal missing or not a string".to_string(),
        })?
        .to_string();
    let plan_type = plan
        .get("type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AttemptOutcome::ProtocolViolation {
            category: "manifest_invariant".to_string(),
            detail: "manifest plan.type missing or not a string".to_string(),
        })?
        .to_string();
    let jira = plan
        .get("jira")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let target_repo = plan
        .get("target_repo")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let target_branch = plan
        .get("target_branch")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let flags_value = plan
        .get("flags")
        .ok_or_else(|| AttemptOutcome::ProtocolViolation {
            category: "manifest_invariant".to_string(),
            detail: "manifest plan.flags missing".to_string(),
        })?;
    let flag_bool = |key: &str| -> bool {
        flags_value
            .get(key)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    };
    let flags = PlanFlags {
        skip_pr: flag_bool("skip_pr"),
        draft_pr: flag_bool("draft_pr"),
        merge: flag_bool("merge"),
        merge_admin: flag_bool("merge_admin"),
        skip_code_review: flag_bool("skip_code_review"),
        no_worktree: flag_bool("no_worktree"),
    };
    Ok(PlanMeta {
        goal,
        plan_type,
        jira,
        target_repo,
        target_branch,
        flags,
    })
}

/// Persisted PR-URL filename under [`StepContext::job_dir`].
const PR_URL_FILE: &str = "pr-url";

/// Drives the PR-creation step.
///
/// 1. Resolves the current branch in `ctx.workdir`.
/// 2. Calls `gh pr view --head <branch> --json url --jq .url`; if a non-empty
///    URL comes back, persists it to `<job_dir>/pr-url` and returns
///    [`AttemptOutcome::Success`].
/// 3. Otherwise calls `gh pr create` with title/body derived from the
///    manifest plan goal/type and the `--draft` flag honoured.
/// 4. Persists the resulting URL and returns [`AttemptOutcome::Success`].
///
/// Errors are mapped to [`AttemptOutcome::TransientInfra`] for known
/// retryable signatures, and [`AttemptOutcome::HardInfra`] otherwise.
fn run_pr_creation(ctx: &StepContext, manifest_path: &Path) -> AttemptOutcome {
    let plan_meta = match load_plan_meta(manifest_path) {
        Ok(m) => m,
        Err(outcome) => return outcome,
    };

    let branch = match resolve_current_branch(&ctx.workdir) {
        Ok(b) => b,
        Err(outcome) => return outcome,
    };

    // Idempotency: short-circuit if a PR for this branch already exists.
    if let Some(url) = lookup_existing_pr(&branch, plan_meta.target_repo.as_deref()) {
        tracing::info!(branch = %branch, %url, "pr_creation: reusing existing PR");
        if let Err(outcome) = persist_pr_url(ctx, &url) {
            return outcome;
        }
        return AttemptOutcome::Success;
    }

    let title = pr_title(&plan_meta);
    let body = pr_body(&plan_meta);

    // Wave sub-agents edit files but do not commit. Stage + commit any
    // outstanding worktree changes here, using the PR title as the
    // commit message, then push the branch. Without this `gh pr create`
    // returns "No commits between main and <branch>" and the pipeline
    // dies one step short of an opened PR.
    if let Err(outcome) = commit_worktree_changes(&ctx.workdir, &title) {
        return outcome;
    }
    if let Err(outcome) = push_branch(&ctx.workdir, &branch) {
        return outcome;
    }

    let mut args: Vec<String> = vec![
        "pr".to_string(),
        "create".to_string(),
        "--head".to_string(),
        branch.clone(),
        "--title".to_string(),
        title,
        "--body".to_string(),
        body,
    ];
    if let Some(repo) = plan_meta.target_repo.as_deref() {
        args.push("--repo".to_string());
        args.push(repo.to_string());
    }
    if plan_meta.flags.draft_pr {
        args.push("--draft".to_string());
    }

    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let outcome = run_gh_in(&ctx.workdir, &arg_refs);
    match outcome {
        Ok(stdout) => {
            let url = stdout.trim().to_string();
            if url.is_empty() {
                return AttemptOutcome::ProtocolViolation {
                    category: "pr_creation_empty_url".to_string(),
                    detail: "gh pr create returned empty stdout".to_string(),
                };
            }
            tracing::info!(branch = %branch, %url, "pr_creation: opened PR");
            if let Err(outcome) = persist_pr_url(ctx, &url) {
                return outcome;
            }
            AttemptOutcome::Success
        }
        Err(GhError { kind, error }) => {
            // gh sometimes errors with "a pull request for branch <x> already exists"
            // even on the create-path; treat that as a soft idempotent success
            // by re-querying for the existing URL.
            if error.to_ascii_lowercase().contains("already exists") {
                if let Some(url) = lookup_existing_pr(&branch, plan_meta.target_repo.as_deref()) {
                    tracing::info!(branch = %branch, %url, "pr_creation: PR already exists");
                    if let Err(outcome) = persist_pr_url(ctx, &url) {
                        return outcome;
                    }
                    return AttemptOutcome::Success;
                }
            }
            match kind {
                GhFailureKind::Transient | GhFailureKind::SpawnFailed => {
                    AttemptOutcome::TransientInfra { error }
                }
                GhFailureKind::Hard => AttemptOutcome::HardInfra { error },
            }
        }
    }
}

/// Stages every working-tree change and commits with `message`. No-op
/// when `git status --porcelain` is clean (i.e. wave sub-agents already
/// committed, or a previous run reached this point and committed
/// already). Returns an [`AttemptOutcome`] tagged for the caller to
/// surface verbatim.
///
/// The commit is signed off only when the host's `commit.gpgsign` is
/// already configured; we do not pass `--no-gpg-sign` because the
/// daemon should never silently bypass operator signing policy.
fn commit_worktree_changes(workdir: &Path, message: &str) -> Result<(), AttemptOutcome> {
    let dirty = match git_workdir_is_dirty(workdir) {
        Ok(d) => d,
        Err(e) => {
            return Err(AttemptOutcome::HardInfra {
                error: format!("git status check failed: {e}"),
            })
        }
    };
    if !dirty {
        tracing::info!("pr_creation: worktree clean, skipping commit");
        return Ok(());
    }
    let add = Command::new("git")
        .args(["-C"])
        .arg(workdir)
        .args(["add", "-A"])
        .output()
        .map_err(|e| AttemptOutcome::HardInfra {
            error: format!("spawn `git add` failed: {e}"),
        })?;
    if !add.status.success() {
        return Err(AttemptOutcome::HardInfra {
            error: format!(
                "git add -A failed (status {:?}): {}",
                add.status.code(),
                String::from_utf8_lossy(&add.stderr).trim()
            ),
        });
    }
    let commit = Command::new("git")
        .args(["-C"])
        .arg(workdir)
        .args(["commit", "-m", message])
        .output()
        .map_err(|e| AttemptOutcome::HardInfra {
            error: format!("spawn `git commit` failed: {e}"),
        })?;
    if !commit.status.success() {
        return Err(AttemptOutcome::HardInfra {
            error: format!(
                "git commit failed (status {:?}): {}",
                commit.status.code(),
                String::from_utf8_lossy(&commit.stderr).trim()
            ),
        });
    }
    Ok(())
}

/// Pushes `branch` to `origin`, setting upstream so subsequent runs
/// don't have to re-pass `-u`. Idempotent when the upstream already
/// tracks the branch — the second invocation is a fast-forward push
/// or no-op.
fn push_branch(workdir: &Path, branch: &str) -> Result<(), AttemptOutcome> {
    let push = Command::new("git")
        .args(["-C"])
        .arg(workdir)
        .args(["push", "-u", "origin", branch])
        .output()
        .map_err(|e| AttemptOutcome::HardInfra {
            error: format!("spawn `git push` failed: {e}"),
        })?;
    if !push.status.success() {
        let stderr = String::from_utf8_lossy(&push.stderr).into_owned();
        // Network blips, auth retries, or the rare "everything up-to-date"
        // edge case (which exits non-zero on some git builds) should be
        // re-tried once at the next attempt rather than failing the
        // pipeline outright.
        let is_transient = is_transient_gh_error(&stderr);
        let error = format!(
            "git push -u origin {branch} failed (status {:?}): {}",
            push.status.code(),
            stderr.trim()
        );
        return Err(if is_transient {
            AttemptOutcome::TransientInfra { error }
        } else {
            AttemptOutcome::HardInfra { error }
        });
    }
    Ok(())
}

/// `git status --porcelain` shorthand: returns true when there's any
/// uncommitted change in the worktree (staged, unstaged, or untracked).
fn git_workdir_is_dirty(workdir: &Path) -> std::io::Result<bool> {
    let output = Command::new("git")
        .args(["-C"])
        .arg(workdir)
        .args(["status", "--porcelain"])
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "git status --porcelain exited {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(!output.stdout.is_empty())
}

/// Builds the conventional-commits PR title from the plan metadata, falling
/// back to a generic prefix when the manifest does not provide a `type`.
fn pr_title(plan: &PlanMeta) -> String {
    let prefix = match plan.plan_type.as_str() {
        "bug" => "fix".to_string(),
        "feature" => "feat".to_string(),
        "refactor" | "chore" | "docs" | "infra" => plan.plan_type.clone(),
        _ => "chore".to_string(),
    };
    let scope = plan.jira.clone().unwrap_or_default();
    let scope_segment = if scope.is_empty() {
        String::new()
    } else {
        format!("({scope})")
    };
    format!("{prefix}{scope_segment}: {}", plan.goal)
}

/// Builds the PR body. Plain text — the orchestrator skill writes the
/// review/validation summaries separately into `.tmp-execution-summary.md`.
fn pr_body(plan: &PlanMeta) -> String {
    let mut body = String::with_capacity(plan.goal.len() + 256);
    body.push_str("Automated PR opened by `plan-executor`.\n\n");
    body.push_str(&format!("- Goal: {}\n", plan.goal));
    body.push_str(&format!("- Type: {}\n", plan.plan_type));
    if let Some(jira) = &plan.jira {
        body.push_str(&format!("- JIRA: {jira}\n"));
    }
    if let Some(branch) = &plan.target_branch {
        body.push_str(&format!("- Branch: {branch}\n"));
    }
    body
}

/// Resolves the current branch in `repo_dir`. Maps git errors onto the
/// caller-facing [`AttemptOutcome`].
fn resolve_current_branch(repo_dir: &Path) -> Result<String, AttemptOutcome> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .map_err(|e| AttemptOutcome::HardInfra {
            error: format!("spawn git rev-parse: {e}"),
        })?;
    if !output.status.success() {
        return Err(AttemptOutcome::HardInfra {
            error: format!(
                "git rev-parse --abbrev-ref HEAD failed in {}: {}",
                repo_dir.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if branch.is_empty() || branch == "HEAD" {
        return Err(AttemptOutcome::HardInfra {
            error: format!("git in {} is detached or empty branch", repo_dir.display()),
        });
    }
    Ok(branch)
}

/// Looks up an existing PR URL for `branch`. Returns `None` when no PR
/// matches or when the lookup itself fails (lookups are best-effort; the
/// caller falls through to `gh pr create`).
fn lookup_existing_pr(branch: &str, repo: Option<&str>) -> Option<String> {
    let mut args: Vec<&str> = vec!["pr", "view", branch, "--json", "url", "--jq", ".url"];
    if let Some(r) = repo {
        args.push("--repo");
        args.push(r);
    }
    match Command::new("gh").args(&args).output() {
        Ok(out) if out.status.success() => {
            let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if url.is_empty() {
                None
            } else {
                Some(url)
            }
        }
        _ => None,
    }
}

/// Persists `url` to `<job_dir>/pr-url`. Errors map to
/// [`AttemptOutcome::HardInfra`] since the file-system fault is not
/// retryable without operator intervention.
fn persist_pr_url(ctx: &StepContext, url: &str) -> Result<(), AttemptOutcome> {
    let path = ctx.job_dir.join(PR_URL_FILE);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| AttemptOutcome::HardInfra {
            error: format!("create job dir {}: {e}", parent.display()),
        })?;
    }
    std::fs::write(&path, format!("{}\n", url.trim())).map_err(|e| AttemptOutcome::HardInfra {
        error: format!("write pr-url to {}: {e}", path.display()),
    })?;
    Ok(())
}

// --- gh shellout -----------------------------------------------------------

/// Categorical failure mode for a `gh` invocation, mirroring the equivalent
/// enum in `pr_finalize.rs`.
enum GhFailureKind {
    SpawnFailed,
    Transient,
    Hard,
}

struct GhError {
    kind: GhFailureKind,
    error: String,
}

/// Runs `gh <args>` with `cwd = repo_dir`, capturing stdout. Errors are
/// classified for the caller so each step can map them to an
/// [`AttemptOutcome`].
fn run_gh_in(repo_dir: &Path, args: &[&str]) -> Result<String, GhError> {
    let output = Command::new("gh")
        .args(args)
        .current_dir(repo_dir)
        .output()
        .map_err(|e| GhError {
            kind: GhFailureKind::SpawnFailed,
            error: format!("failed to spawn gh: {e}"),
        })?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let kind = if is_transient_gh_error(&stderr) {
        GhFailureKind::Transient
    } else {
        GhFailureKind::Hard
    };
    Err(GhError {
        kind,
        error: format!(
            "gh {} failed (status {:?}): {}",
            args.join(" "),
            output.status.code(),
            stderr.trim()
        ),
    })
}

/// Returns `true` when stderr indicates a transient gh API hiccup.
/// Pattern list mirrors [`crate::job::steps::pr_finalize`] so behavior stays
/// uniform across PR-touching steps.
///
/// HTTP status codes (`502`/`503`/`504`) and the `timeout` keyword are
/// matched as whole tokens via [`has_transient_token`] so unrelated
/// identifiers or numeric noise (e.g. a path containing `503`) cannot
/// classify a hard failure as transient.
fn is_transient_gh_error(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    if has_transient_token(&lower, &["502", "503", "504", "timeout"]) {
        return true;
    }
    lower.contains("rate limit")
        || lower.contains("connection reset")
        || lower.contains("timed out")
        || lower.contains("temporary failure")
        || lower.contains("network is unreachable")
}

// ---------------------------------------------------------------------------
// D3.3 — SummaryStep helpers
// ---------------------------------------------------------------------------

/// Filename the summary step writes when running locally (skip_pr=true).
const SUMMARY_FILE: &str = ".tmp-execution-summary.md";

/// Drives the summary step.
///
/// When `flags.skip_pr` is `false` and a PR URL is on disk, calls
/// [`invoke_pr_finalize`] to delegate finalize / Bugbot triage to the
/// shared helper. Otherwise writes a best-effort markdown summary to
/// `<job_dir>/.tmp-execution-summary.md`.
fn run_summary(ctx: &StepContext, manifest_path: &Path) -> AttemptOutcome {
    let plan_meta = match load_plan_meta(manifest_path) {
        Ok(m) => m,
        Err(outcome) => return outcome,
    };

    let pr_url = read_pr_url(ctx);

    let outcome = if !plan_meta.flags.skip_pr && pr_url.is_some() {
        delegate_to_pr_finalize(ctx, pr_url.as_deref().expect("checked"), plan_meta.flags)
    } else {
        if !plan_meta.flags.skip_pr {
            // skip_pr=false but no PR URL on disk; fall through to the
            // local summary so the run still produces an artifact.
            tracing::warn!(
                "summary: skip_pr=false but no pr-url file present; writing local summary",
            );
        }
        write_local_summary(ctx, manifest_path, &plan_meta, pr_url.as_deref())
    };

    if matches!(outcome, AttemptOutcome::Success) && !plan_meta.flags.no_worktree {
        cleanup_worktree_after_summary(ctx, manifest_path, &plan_meta);
    }
    outcome
}

/// Removes the per-plan worktree once summary completes successfully.
/// The source repo is rediscovered from the manifest path (preflight's
/// `find_source_repo` walks `ctx.workdir`, but by summary time
/// `ctx.workdir` has been redirected to the worktree itself, so the
/// manifest is the only stable anchor). Failures are logged and
/// swallowed — a stale worktree on disk is preferable to flipping a
/// successful run into a failure on a cosmetic teardown step.
fn cleanup_worktree_after_summary(
    ctx: &StepContext,
    manifest_path: &Path,
    plan_meta: &PlanMeta,
) {
    let Some(source_repo) = find_source_repo_for_cleanup(ctx, manifest_path) else {
        tracing::warn!("summary: cleanup skipped — could not resolve source repo");
        return;
    };
    let plan_stem = plan_stem_from_manifest(manifest_path);
    let worktree_path = compute_worktree_path(&source_repo, &plan_stem, plan_meta);
    if let Err(e) = cleanup_worktree(&source_repo, &worktree_path) {
        tracing::warn!(
            worktree = %worktree_path.display(),
            error = %e,
            "summary: worktree cleanup failed (best-effort)",
        );
    } else {
        tracing::info!(
            worktree = %worktree_path.display(),
            "summary: worktree cleaned up",
        );
    }
}

/// Walks parents of either `ctx.workdir` (when it is itself the worktree
/// inside `<repo>/.plan-executor/<name>`, two levels up resolves to the
/// source repo) or `manifest_path` to find a directory containing a
/// `.git` entry. `find_source_repo` is preflight-only because it
/// short-circuits when `ctx.workdir/.git` exists; by summary time
/// `ctx.workdir` has been redirected to the worktree, whose `.git` is a
/// file pointer rather than a directory, so a dedicated lookup is
/// clearer than overloading the existing helper.
fn find_source_repo_for_cleanup(
    ctx: &StepContext,
    manifest_path: &Path,
) -> Option<PathBuf> {
    let candidates: [Option<&Path>; 2] = [Some(ctx.workdir.as_path()), manifest_path.parent()];
    for start in candidates.into_iter().flatten() {
        let mut dir = start.to_path_buf();
        loop {
            if dir.join(".git").is_dir() {
                return Some(dir);
            }
            match dir.parent() {
                Some(parent) if parent != dir => dir = parent.to_path_buf(),
                _ => break,
            }
        }
    }
    None
}

/// Reads `<job_dir>/pr-url` if it exists, returning the trimmed URL or
/// `None`. Errors are swallowed because the summary step is best-effort.
fn read_pr_url(ctx: &StepContext) -> Option<String> {
    let path = ctx.job_dir.join(PR_URL_FILE);
    match std::fs::read_to_string(&path) {
        Ok(s) => {
            let trimmed = s.trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        }
        Err(_) => None,
    }
}

/// Calls [`invoke_pr_finalize`] for `url`. Maps helper failures onto the
/// matching [`AttemptOutcome`] using [`helper_error_to_outcome`].
///
/// Honors the manifest's `plan.flags.merge` / `plan.flags.merge_admin`:
/// when `merge_admin` is set the helper merges with `gh pr merge --merge
/// --admin` (bypassing branch protections); when only `merge` is set the
/// helper merges with `gh pr merge --merge`; otherwise the helper monitors
/// the PR but does not merge.
fn delegate_to_pr_finalize(
    ctx: &StepContext,
    url: &str,
    flags: PlanFlags,
) -> AttemptOutcome {
    let (owner, repo, pr) = match parse_pr_url(url) {
        Some(parts) => parts,
        None => {
            return AttemptOutcome::ProtocolViolation {
                category: "pr_url_unparseable".to_string(),
                detail: format!("could not parse owner/repo/pr from `{url}`"),
            };
        }
    };
    let merge_mode = if flags.merge_admin {
        PrFinalizeMergeMode::MergeAdmin
    } else if flags.merge {
        PrFinalizeMergeMode::Merge
    } else {
        PrFinalizeMergeMode::None
    };
    let input = PrFinalizeInput {
        owner,
        repo,
        pr,
        merge_mode,
    };
    match invoke_pr_finalize(input, ctx) {
        Ok(_) => AttemptOutcome::Success,
        Err(e) => helper_error_to_outcome(e),
    }
}

/// Parses `https://github.com/<owner>/<repo>/pull/<n>` into its three
/// segments. Returns `None` for any URL that doesn't match. The host is
/// pinned to `github.com` over `https`; non-`https` schemes, alternative
/// hosts, the misspelled `pulls` segment, and missing/non-numeric PR ids
/// all fail.
fn parse_pr_url(url: &str) -> Option<(String, String, u32)> {
    const PREFIX: &str = "https://github.com/";
    let trimmed = url.trim();
    let rest = trimmed.strip_prefix(PREFIX)?;
    let mut parts = rest.split('/');
    let owner = parts.next()?.to_string();
    let repo = parts.next()?.to_string();
    if parts.next()? != "pull" {
        return None;
    }
    let pr: u32 = parts.next()?.parse().ok()?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner, repo, pr))
}

/// Writes a best-effort markdown summary to
/// `<job_dir>/.tmp-execution-summary.md`. Returns
/// [`AttemptOutcome::HardInfra`] only when the file cannot be written.
fn write_local_summary(
    ctx: &StepContext,
    manifest_path: &Path,
    plan_meta: &PlanMeta,
    pr_url: Option<&str>,
) -> AttemptOutcome {
    let wave_count = match scheduler::load_manifest(manifest_path) {
        Ok(m) => m.waves.len(),
        Err(_) => 0,
    };
    let mut body = String::with_capacity(512);
    body.push_str("# plan-executor summary\n\n");
    body.push_str(&format!("- goal: {}\n", plan_meta.goal));
    body.push_str(&format!("- type: {}\n", plan_meta.plan_type));
    if let Some(jira) = &plan_meta.jira {
        body.push_str(&format!("- jira: {jira}\n"));
    }
    body.push_str(&format!("- waves: {wave_count}\n"));
    body.push_str(&format!("- skip_pr: {}\n", plan_meta.flags.skip_pr));
    if let Some(url) = pr_url {
        body.push_str(&format!("- pr_url: {url}\n"));
    } else {
        body.push_str("- pr_url: (none)\n");
    }
    body.push_str(&format!("- manifest: {}\n", manifest_path.display()));
    body.push_str(&format!("- job_dir: {}\n", ctx.job_dir.display()));

    let path = ctx.job_dir.join(SUMMARY_FILE);
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return AttemptOutcome::HardInfra {
                error: format!("create job dir {}: {e}", parent.display()),
            };
        }
    }
    if let Err(e) = std::fs::write(&path, body) {
        return AttemptOutcome::HardInfra {
            error: format!("write summary file {}: {e}", path.display()),
        };
    }
    AttemptOutcome::Success
}

#[cfg(test)]
mod preflight_tests {
    use super::*;

    fn meta_with(jira: Option<&str>, target_branch: Option<&str>, plan_type: &str) -> PlanMeta {
        PlanMeta {
            goal: "g".into(),
            plan_type: plan_type.into(),
            jira: jira.map(str::to_string),
            target_repo: None,
            target_branch: target_branch.map(str::to_string),
            flags: PlanFlags::default(),
        }
    }

    #[test]
    fn plan_stem_uses_manifest_parent_directory_name() {
        let manifest = std::path::Path::new(
            "/abs/repo/docs/superpowers/plans/2026-04-29-month-reporting-fix/tasks.json",
        );
        assert_eq!(plan_stem_from_manifest(manifest), "2026-04-29-month-reporting-fix");
    }

    #[test]
    fn plan_stem_falls_back_when_manifest_has_no_parent() {
        let manifest = std::path::Path::new("tasks.json");
        // No parent → "plan"
        assert_eq!(plan_stem_from_manifest(manifest), "plan");
    }

    #[test]
    fn worktree_path_lives_inside_source_repo_under_dot_plan_executor() {
        let repo = std::path::Path::new("/Users/me/code/my-repo");
        let meta = meta_with(None, None, "feature");
        let path = compute_worktree_path(repo, "month-reporting-fix", &meta);
        assert_eq!(
            path,
            std::path::PathBuf::from(
                "/Users/me/code/my-repo/.plan-executor/month-reporting-fix"
            )
        );
    }

    #[test]
    fn worktree_path_appends_jira_when_present() {
        let repo = std::path::Path::new("/Users/me/code/my-repo");
        let meta = meta_with(Some("CCP-123"), None, "feature");
        let path = compute_worktree_path(repo, "stem", &meta);
        assert_eq!(
            path,
            std::path::PathBuf::from("/Users/me/code/my-repo/.plan-executor/stem-CCP-123")
        );
    }

    #[test]
    fn worktree_path_omits_jira_when_empty_string() {
        let repo = std::path::Path::new("/Users/me/code/my-repo");
        let meta = meta_with(Some(""), None, "feature");
        let path = compute_worktree_path(repo, "stem", &meta);
        assert_eq!(
            path,
            std::path::PathBuf::from("/Users/me/code/my-repo/.plan-executor/stem")
        );
    }

    #[test]
    fn ensure_plan_executor_excluded_appends_when_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        std::fs::create_dir_all(repo.join(".git/info")).expect("git/info");
        std::fs::write(repo.join(".git/info/exclude"), "# preexisting\n").expect("seed");
        ensure_plan_executor_excluded(repo).expect("write");
        let body = std::fs::read_to_string(repo.join(".git/info/exclude")).expect("read");
        assert!(body.contains("# preexisting"), "preserved existing content: {body:?}");
        assert!(body.lines().any(|l| l.trim() == ".plan-executor/"), "appended entry: {body:?}");
    }

    #[test]
    fn ensure_plan_executor_excluded_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        std::fs::create_dir_all(repo.join(".git/info")).expect("git/info");
        ensure_plan_executor_excluded(repo).expect("write 1");
        let body1 = std::fs::read_to_string(repo.join(".git/info/exclude")).expect("read 1");
        ensure_plan_executor_excluded(repo).expect("write 2");
        let body2 = std::fs::read_to_string(repo.join(".git/info/exclude")).expect("read 2");
        assert_eq!(body1, body2, "second call must not duplicate the entry");
    }

    #[test]
    fn cleanup_worktree_is_a_noop_when_path_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join(".plan-executor").join("does-not-exist");
        cleanup_worktree(dir.path(), &missing).expect("noop on missing path");
    }

    #[test]
    fn derived_branch_uses_target_branch_when_set() {
        let meta = meta_with(None, Some("feat/CCP-123-month-reporting"), "feature");
        assert_eq!(
            derive_branch_name(&meta, "stem-ignored"),
            "feat/CCP-123-month-reporting"
        );
    }

    #[test]
    fn derived_branch_maps_feature_to_feat_prefix() {
        let meta = meta_with(None, None, "feature");
        assert_eq!(derive_branch_name(&meta, "month-reporting-fix"), "feat/month-reporting-fix");
    }

    #[test]
    fn derived_branch_maps_bug_to_fix_prefix() {
        let meta = meta_with(None, None, "bug");
        assert_eq!(derive_branch_name(&meta, "broken-thing"), "fix/broken-thing");
    }

    #[test]
    fn derived_branch_passes_through_other_types() {
        for t in ["refactor", "chore", "docs", "infra"] {
            let meta = meta_with(None, None, t);
            assert_eq!(
                derive_branch_name(&meta, "x"),
                format!("{t}/x"),
                "type {t} should map to itself",
            );
        }
    }

    #[test]
    fn empty_target_branch_falls_back_to_derived() {
        // An empty string in target_branch shouldn't win over the derived
        // default — it indicates an unset field, not a deliberate empty
        // branch name.
        let meta = meta_with(None, Some(""), "feature");
        assert_eq!(derive_branch_name(&meta, "stem"), "feat/stem");
    }

    #[test]
    fn base_branch_target_falls_back_to_derived() {
        // `target_branch: main` describes the PR target, not the worktree
        // branch. Forwarding it as the worktree branch collides with the
        // source repo's checkout and breaks `git worktree add`. Same for
        // master / develop / trunk / HEAD, case-insensitive.
        for base in ["main", "master", "develop", "trunk", "HEAD", "Main", "MASTER"] {
            let meta = meta_with(None, Some(base), "bug");
            assert_eq!(
                derive_branch_name(&meta, "month-fix"),
                "fix/month-fix",
                "base-branch `{base}` should not be used as worktree branch",
            );
        }
    }
}
