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
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use wait_timeout::ChildExt;

use crate::compile::{join_drainer, spawn_drainer};
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

/// Default wall-clock cap for a single `pr-monitor.sh` invocation. The
/// script has its own internal retry/poll loop (up to MAX_FIX_SESSIONS=20
/// fix sessions × 30 min each = ~10h theoretical worst case); this is a
/// kill-switch for runaways, not a budget for normal work. 4 hours sits
/// well under GHA's 6-hour per-job limit while leaving room for PRs that
/// need 4-6 fix rounds. Override via env `PLAN_EXECUTOR_MONITOR_TIMEOUT_SECS`.
const MONITOR_TIMEOUT_DEFAULT: Duration = Duration::from_secs(4 * 60 * 60);
const MONITOR_TIMEOUT_ENV: &str = "PLAN_EXECUTOR_MONITOR_TIMEOUT_SECS";

fn monitor_timeout() -> Duration {
    std::env::var(MONITOR_TIMEOUT_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .map(Duration::from_secs)
        .unwrap_or(MONITOR_TIMEOUT_DEFAULT)
}

/// Maximum wall-clock for a single `gh` invocation. The supervisor's
/// retry policy handles per-call transients; this hard ceiling protects
/// against `gh` hanging on a credential prompt or network stall.
const GH_TIMEOUT: Duration = Duration::from_secs(60);

/// Maximum bytes captured from each subprocess stream after a timeout
/// or exit. Mirrors `compile::SUBPROCESS_STREAM_CAP_BYTES` so error
/// messages stay bounded even when a child writes pathological output.
const SUBPROCESS_STREAM_CAP_BYTES: u64 = 16 * 1024 * 1024;

/// Operator override for the absolute path to `pr-monitor.sh`. Read by
/// [`resolve_monitor_script`]; takes priority over every fallback.
const MONITOR_SCRIPT_ENV: &str = "PLAN_EXECUTOR_PR_MONITOR_SCRIPT";

/// Resolves the absolute path to `pr-monitor.sh` at runtime.
///
/// Lookup order:
///
/// 1. `PLAN_EXECUTOR_PR_MONITOR_SCRIPT` env var (operator override).
/// 2. The directory containing the running `plan-executor` binary,
///    plus a sibling `share/plan-executor/` lookup.
/// 3. The plan-executor plugin install location under
///    `~/.claude/plugins/cache/plan-executor/plan-executor/<sha>/skills/pr-finalize/pr-monitor.sh`.
///
/// Returns the first candidate that exists as a regular file. `None`
/// when no candidate is present.
fn resolve_monitor_script() -> Option<PathBuf> {
    if let Ok(value) = std::env::var(MONITOR_SCRIPT_ENV) {
        let candidate = PathBuf::from(value);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let sibling = parent.join("pr-monitor.sh");
            if sibling.is_file() {
                return Some(sibling);
            }
            let share = parent.join("share/plan-executor/pr-monitor.sh");
            if share.is_file() {
                return Some(share);
            }
            // Common cargo layout: `target/{debug,release}/plan-executor`;
            // walk one level up so `share/plan-executor/...` next to the
            // crate root is also discoverable.
            if let Some(grand) = parent.parent() {
                let share2 = grand.join("share/plan-executor/pr-monitor.sh");
                if share2.is_file() {
                    return Some(share2);
                }
            }
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let plugin_root =
            Path::new(&home).join(".claude/plugins/cache/plan-executor/plan-executor");
        if let Ok(entries) = std::fs::read_dir(&plugin_root) {
            for entry in entries.flatten() {
                let candidate = entry.path().join("skills/pr-finalize/pr-monitor.sh");
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
        // Marketplaces install path (no sha intermediate dir).
        let marketplace = Path::new(&home).join(
            ".claude/plugins/marketplaces/plan-executor/plugins/plan-executor/skills/pr-finalize/pr-monitor.sh",
        );
        if marketplace.is_file() {
            return Some(marketplace);
        }
    }
    None
}

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
    /// Optional absolute path to `pr-monitor.sh`. When `None`, the step
    /// resolves the script at runtime via [`resolve_monitor_script`]
    /// (env override → binary sibling → plugin install).
    pub script_path: Option<PathBuf>,
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

    async fn run(&self, ctx: &mut StepContext) -> AttemptOutcome {
        let resolved = self
            .script_path
            .as_ref()
            .filter(|p| p.is_file())
            .cloned()
            .or_else(resolve_monitor_script);
        let script_path = match resolved {
            Some(p) => p,
            None => {
                return AttemptOutcome::HardInfra {
                    error: format!(
                        "pr-monitor.sh not found (set {MONITOR_SCRIPT_ENV} or install plan-executor plugin)"
                    ),
                };
            }
        };

        // pr-monitor.sh requires 8 args: --owner --repo --pr --head-sha
        // --push-time --workdir --summary-file --log-file. Capture HEAD SHA
        // via gh, derive push-time from now, and place per-attempt summary
        // + log files under the step's attempt directory.
        let pr_str = self.pr.to_string();
        let repo_slug = format!("{}/{}", self.owner, self.repo);
        let head_sha = match run_gh_capture(&[
            "pr",
            "view",
            pr_str.as_str(),
            "--repo",
            repo_slug.as_str(),
            "--json",
            "headRefOid",
            "--jq",
            ".headRefOid",
        ]) {
            Ok(stdout) => stdout.trim().to_string(),
            Err(GhError { kind, error }) => match kind {
                GhFailureKind::SpawnFailed | GhFailureKind::Transient => {
                    return AttemptOutcome::TransientInfra { error };
                }
                GhFailureKind::Hard => {
                    return AttemptOutcome::HardInfra { error };
                }
            },
        };
        if head_sha.is_empty() {
            return AttemptOutcome::TransientInfra {
                error: "gh pr view returned empty headRefOid".into(),
            };
        }

        let push_time = chrono::Utc::now().to_rfc3339();
        let attempt_dir = ctx
            .job_dir
            .join("steps")
            .join(format!("{:03}-{}", ctx.step_seq, self.name()))
            .join("attempts")
            .join(ctx.attempt_n.to_string());
        if let Err(e) = std::fs::create_dir_all(&attempt_dir) {
            return AttemptOutcome::HardInfra {
                error: format!(
                    "failed to create monitor attempt dir {}: {e}",
                    attempt_dir.display()
                ),
            };
        }
        let summary_file = attempt_dir.join("monitor-summary.md");
        let log_file = attempt_dir.join("monitor.log");

        let pr_arg = self.pr.to_string();
        let mut cmd = Command::new(script_path.as_os_str());
        cmd.arg("--owner")
            .arg(self.owner.as_str())
            .arg("--repo")
            .arg(self.repo.as_str())
            .arg("--pr")
            .arg(pr_arg.as_str())
            .arg("--head-sha")
            .arg(head_sha.as_str())
            .arg("--push-time")
            .arg(push_time.as_str())
            .arg("--workdir")
            .arg(ctx.workdir.as_os_str())
            .arg("--summary-file")
            .arg(summary_file.as_os_str())
            .arg("--log-file")
            .arg(log_file.as_os_str())
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return AttemptOutcome::TransientInfra {
                    error: format!("failed to spawn pr-monitor.sh: {e}"),
                };
            }
        };

        let timeout = monitor_timeout();
        let wait_result = child.wait_timeout(timeout);
        let needs_kill = matches!(wait_result, Ok(None) | Err(_));
        if needs_kill {
            let _ = child.kill();
            let _ = child.wait();
        }

        match wait_result {
            Ok(Some(status)) if status.success() => AttemptOutcome::Success,
            Ok(Some(status)) => AttemptOutcome::TransientInfra {
                error: format!("pr-monitor.sh exited with code {:?}", status.code()),
            },
            Ok(None) => AttemptOutcome::TransientInfra {
                error: format!("pr-monitor.sh exceeded {} s timeout", timeout.as_secs()),
            },
            Err(e) => AttemptOutcome::TransientInfra {
                error: format!("pr-monitor.sh wait failed: {e}"),
            },
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
///
/// Subprocess hygiene:
///   - `stdin` is nulled so `gh` cannot block on a credential prompt.
///   - stdout/stderr are drained in background threads to avoid pipe-
///     buffer deadlocks on chatty subcommands.
///   - The wait is bounded by [`GH_TIMEOUT`]; on expiry the child is
///     killed and the call is reported as transient (the supervisor's
///     retry policy decides whether to attempt again).
fn run_gh_capture(args: &[&str]) -> Result<String, GhError> {
    let mut child = Command::new("gh")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| GhError {
            kind: GhFailureKind::SpawnFailed,
            error: format!("failed to spawn gh: {e}"),
        })?;

    let stdout_handle = child
        .stdout
        .take()
        .map(|s| spawn_drainer(s, SUBPROCESS_STREAM_CAP_BYTES));
    let stderr_handle = child
        .stderr
        .take()
        .map(|s| spawn_drainer(s, SUBPROCESS_STREAM_CAP_BYTES));

    let wait_result = child.wait_timeout(GH_TIMEOUT);
    let needs_kill = matches!(wait_result, Ok(None) | Err(_));
    if needs_kill {
        let _ = child.kill();
        let _ = child.wait();
    }
    let stdout = stdout_handle.map(join_drainer).unwrap_or_default();
    let stderr = stderr_handle.map(join_drainer).unwrap_or_default();

    let status = match wait_result {
        Ok(Some(status)) => status,
        Ok(None) => {
            return Err(GhError {
                kind: GhFailureKind::Transient,
                error: format!(
                    "gh {} timed out after {}s",
                    args.join(" "),
                    GH_TIMEOUT.as_secs()
                ),
            });
        }
        Err(e) => {
            return Err(GhError {
                kind: GhFailureKind::Transient,
                error: format!("gh {} wait failed: {e}", args.join(" ")),
            });
        }
    };

    if status.success() {
        return Ok(stdout);
    }
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
            status.code(),
            stderr.trim()
        ),
    })
}

/// Returns `true` when stderr indicates a transient gh API hiccup that the
/// supervisor's `RetryTransient` policy is meant to absorb.
///
/// HTTP status codes (`502`/`503`/`504`) and the `timeout` keyword are
/// matched as whole tokens (split on non-alphanumerics) rather than
/// substrings, so unrelated text containing those characters cannot
/// misclassify a hard failure as transient.
fn is_transient_gh_error(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    let has_token = lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|tok| matches!(tok, "502" | "503" | "504" | "timeout"));
    if has_token {
        return true;
    }
    lower.contains("rate limit")
        || lower.contains("connection reset")
        || lower.contains("timed out")
        || lower.contains("temporary failure")
        || lower.contains("network is unreachable")
}

/// Returns `true` when `gh pr ready` complains the PR is already ready;
/// treat that as success since `MarkReadyStep` is idempotent.
fn is_already_ready(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("not in draft state") || lower.contains("already ready")
}
