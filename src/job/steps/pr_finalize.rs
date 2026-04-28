//! `Step` implementations for `JobKind::PrFinalize`.
//!
//! The `PrFinalize` job has 5 steps:
//!
//! 1. `PrLookup` — `gh pr view`, capture HEAD SHA, owner, repo, draft state
//! 2. `MarkReady` — `gh pr ready` (skipped if PR is already ready)
//! 3. `Monitor` — invoke `pr-monitor.sh` with bounded timeout
//! 4. `Merge` — only when `--merge` or `--merge-admin` requested; `gh pr merge`
//! 5. `Report` — write summary to `.tmp-execution-summary.md`
//!
//! ### Conditional `MergeStep`
//!
//! `MergeStep` is always included in the registry-emitted step vector and
//! short-circuits internally to `AttemptOutcome::Success` when its `mode`
//! field is `MergeMode::None`. This avoids extending `JobKind::PrFinalize`
//! (a wire-format change) for Task C1.1; the CLI surface in Task C1.2 is
//! responsible for threading the user's merge intent into the field.

use async_trait::async_trait;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::job::recovery::{Backoff, RecoveryPolicy};
use crate::job::step::{Step, StepContext};
use crate::job::types::AttemptOutcome;

/// Default exponential backoff used by transient-retry policies in this
/// module. Initial 500 ms, capped at 8 s, factor 2.0 — matches the cadence
/// the supervisor expects for `gh` API hiccups.
fn default_backoff() -> Backoff {
    Backoff::Exponential {
        initial_ms: 500,
        max_ms: 8_000,
        factor: 2.0,
    }
}

/// Maximum wall-clock for a single `pr-monitor.sh` invocation. The script
/// has its own internal retry/poll loop; this guard prevents a runaway
/// child from blocking the whole job indefinitely.
const MONITOR_TIMEOUT: Duration = Duration::from_secs(45 * 60);

/// Whether to attempt a merge after monitor completes.
///
/// `MergeStep::run` reads this field to decide between short-circuiting
/// to `Success` (the common case where the user did not pass `--merge`)
/// and shelling out to `gh pr merge`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeMode {
    /// Do not attempt a merge; the step short-circuits to `Success`.
    None,
    /// Run `gh pr merge` with the standard squash-and-delete flags.
    Merge,
    /// Run `gh pr merge --admin` (bypasses required-reviewer checks).
    MergeAdmin,
}

/// Step 1: resolve the PR's head SHA and draft state via `gh pr view`.
#[derive(Debug, Clone)]
pub struct PrLookupStep {
    /// PR owner (e.g. `parloa`).
    pub owner: String,
    /// PR repository name (e.g. `plan-executor`).
    pub repo: String,
    /// PR number.
    pub pr: u32,
}

#[async_trait]
impl Step for PrLookupStep {
    fn name(&self) -> &'static str {
        "pr_lookup"
    }

    fn idempotent(&self) -> bool {
        true
    }

    fn recovery_policy(&self) -> RecoveryPolicy {
        RecoveryPolicy::RetryTransient {
            max: 3,
            backoff: default_backoff(),
        }
    }

    async fn run(&self, _ctx: &mut StepContext) -> AttemptOutcome {
        let pr_arg = self.pr.to_string();
        let repo_arg = format!("{}/{}", self.owner, self.repo);
        let args = [
            "pr",
            "view",
            pr_arg.as_str(),
            "--repo",
            repo_arg.as_str(),
            "--json",
            "headRefOid,isDraft,number,baseRefName,headRefName",
        ];
        run_gh_capture(&args).map_or_else(
            |GhError { kind, error }| match kind {
                GhFailureKind::SpawnFailed | GhFailureKind::Transient => {
                    AttemptOutcome::TransientInfra { error }
                }
                GhFailureKind::Hard => AttemptOutcome::HardInfra { error },
            },
            |_stdout| AttemptOutcome::Success,
        )
    }
}

/// Step 2: ensure the PR is marked ready (no-op if already ready).
#[derive(Debug, Clone)]
pub struct MarkReadyStep {
    /// PR owner.
    pub owner: String,
    /// PR repository name.
    pub repo: String,
    /// PR number.
    pub pr: u32,
}

#[async_trait]
impl Step for MarkReadyStep {
    fn name(&self) -> &'static str {
        "mark_ready"
    }

    fn idempotent(&self) -> bool {
        true
    }

    fn recovery_policy(&self) -> RecoveryPolicy {
        RecoveryPolicy::RetryTransient {
            max: 3,
            backoff: default_backoff(),
        }
    }

    async fn run(&self, _ctx: &mut StepContext) -> AttemptOutcome {
        let pr_arg = self.pr.to_string();
        let repo_arg = format!("{}/{}", self.owner, self.repo);
        let args = ["pr", "ready", pr_arg.as_str(), "--repo", repo_arg.as_str()];
        match run_gh_capture(&args) {
            Ok(_) => AttemptOutcome::Success,
            Err(GhError { kind, error }) => {
                if is_already_ready(&error) {
                    return AttemptOutcome::Success;
                }
                match kind {
                    GhFailureKind::SpawnFailed | GhFailureKind::Transient => {
                        AttemptOutcome::TransientInfra { error }
                    }
                    GhFailureKind::Hard => AttemptOutcome::HardInfra { error },
                }
            }
        }
    }
}

/// Step 3: run `pr-monitor.sh`, which polls CI / Bugbot / `SonarCloud` and
/// only exits when the PR is in a mergeable state (or the bounded timeout
/// expires).
#[derive(Debug, Clone)]
pub struct MonitorStep {
    /// PR owner.
    pub owner: String,
    /// PR repository name.
    pub repo: String,
    /// PR number.
    pub pr: u32,
    /// Absolute path to `pr-monitor.sh` (resolved by the CLI/registry).
    pub script_path: PathBuf,
}

#[async_trait]
impl Step for MonitorStep {
    fn name(&self) -> &'static str {
        "monitor"
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

    async fn run(&self, _ctx: &mut StepContext) -> AttemptOutcome {
        if !self.script_path.is_file() {
            return AttemptOutcome::HardInfra {
                error: format!("pr-monitor.sh not found at {}", self.script_path.display()),
            };
        }
        let pr_arg = self.pr.to_string();
        let repo_arg = format!("{}/{}", self.owner, self.repo);
        let mut cmd = Command::new(self.script_path.as_os_str());
        cmd.arg("--repo")
            .arg(repo_arg.as_str())
            .arg("--pr")
            .arg(pr_arg.as_str())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return AttemptOutcome::TransientInfra {
                    error: format!("failed to spawn pr-monitor.sh: {e}"),
                };
            }
        };
        let result = match wait_timeout::ChildExt::wait_timeout(&mut child, MONITOR_TIMEOUT) {
            Ok(Some(status)) => status,
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return AttemptOutcome::TransientInfra {
                    error: format!(
                        "pr-monitor.sh exceeded {} s timeout",
                        MONITOR_TIMEOUT.as_secs()
                    ),
                };
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return AttemptOutcome::TransientInfra {
                    error: format!("pr-monitor.sh wait failed: {e}"),
                };
            }
        };
        if result.success() {
            AttemptOutcome::Success
        } else {
            AttemptOutcome::TransientInfra {
                error: format!("pr-monitor.sh exited with code {:?}", result.code()),
            }
        }
    }
}

/// Step 4: optionally merge the PR via `gh pr merge`.
#[derive(Debug, Clone)]
pub struct MergeStep {
    /// PR owner.
    pub owner: String,
    /// PR repository name.
    pub repo: String,
    /// PR number.
    pub pr: u32,
    /// Whether (and how) to attempt the merge.
    pub mode: MergeMode,
}

#[async_trait]
impl Step for MergeStep {
    fn name(&self) -> &'static str {
        "merge"
    }

    fn idempotent(&self) -> bool {
        false
    }

    fn recovery_policy(&self) -> RecoveryPolicy {
        RecoveryPolicy::None
    }

    async fn run(&self, _ctx: &mut StepContext) -> AttemptOutcome {
        if matches!(self.mode, MergeMode::None) {
            return AttemptOutcome::Success;
        }
        let pr_arg = self.pr.to_string();
        let repo_arg = format!("{}/{}", self.owner, self.repo);
        let mut args: Vec<&str> = vec![
            "pr",
            "merge",
            pr_arg.as_str(),
            "--repo",
            repo_arg.as_str(),
            "--squash",
            "--delete-branch",
        ];
        if matches!(self.mode, MergeMode::MergeAdmin) {
            args.push("--admin");
        }
        run_gh_capture(&args).map_or_else(
            |GhError { kind, error }| match kind {
                GhFailureKind::SpawnFailed => AttemptOutcome::TransientInfra { error },
                GhFailureKind::Hard | GhFailureKind::Transient => {
                    AttemptOutcome::HardInfra { error }
                }
            },
            |_stdout| AttemptOutcome::Success,
        )
    }
}

/// Step 5: write a best-effort summary to `.tmp-execution-summary.md`.
#[derive(Debug, Clone)]
pub struct ReportStep {
    /// PR owner.
    pub owner: String,
    /// PR repository name.
    pub repo: String,
    /// PR number.
    pub pr: u32,
}

#[async_trait]
impl Step for ReportStep {
    fn name(&self) -> &'static str {
        "report"
    }

    fn idempotent(&self) -> bool {
        true
    }

    fn recovery_policy(&self) -> RecoveryPolicy {
        RecoveryPolicy::None
    }

    async fn run(&self, ctx: &mut StepContext) -> AttemptOutcome {
        let summary_path = ctx.job_dir.join(".tmp-execution-summary.md");
        let body = format!(
            "# pr-finalize summary\n\n- repo: {owner}/{repo}\n- pr: #{pr}\n- job_dir: {job_dir}\n",
            owner = self.owner,
            repo = self.repo,
            pr = self.pr,
            job_dir = ctx.job_dir.display(),
        );
        match std::fs::write(&summary_path, body) {
            Ok(()) => AttemptOutcome::Success,
            Err(e) => AttemptOutcome::HardInfra {
                error: format!("failed to write summary to {}: {e}", summary_path.display()),
            },
        }
    }
}

/// Categorical failure mode for a `gh` invocation.
enum GhFailureKind {
    /// The `gh` binary could not be spawned (PATH issue, missing tool).
    SpawnFailed,
    /// `gh` exited non-zero with stderr matching transient patterns.
    Transient,
    /// `gh` exited non-zero with stderr that needs operator attention.
    Hard,
}

struct GhError {
    kind: GhFailureKind,
    error: String,
}

/// Runs `gh <args>`, capturing stdout. Errors are categorized for the
/// caller so each step can map them to the right `AttemptOutcome`.
fn run_gh_capture(args: &[&str]) -> Result<String, GhError> {
    let output = Command::new("gh")
        .args(args)
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

/// Returns `true` when stderr indicates a transient gh API hiccup that the
/// supervisor's `RetryTransient` policy is meant to absorb.
fn is_transient_gh_error(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("rate limit")
        || lower.contains("connection reset")
        || lower.contains("timed out")
        || lower.contains("timeout")
        || lower.contains("temporary failure")
        || lower.contains("503")
        || lower.contains("502")
        || lower.contains("504")
        || lower.contains("network is unreachable")
}

/// Returns `true` when `gh pr ready` complains the PR is already ready;
/// treat that as success since `MarkReadyStep` is idempotent.
fn is_already_ready(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("not in draft state") || lower.contains("already ready")
}
