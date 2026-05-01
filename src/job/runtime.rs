//! Shared plan-execution engine.
//!
//! Both the daemon's job loop and the foreground / GHA-runner CLI go
//! through [`run_pipeline`] so the real business logic — step
//! execution, recovery / retry, [`JobMetrics`] tracking, manifest
//! status flips, terminal `metrics.json` persistence — runs the same
//! way regardless of how the job was kicked off.
//!
//! What the two surfaces still differ on is folded into a
//! [`PipelineObserver`]: where display lines go (broadcast bus vs
//! `eprintln`), whether external kill is observed, and the
//! `SchedulerHooks` that step impls call into for sub-agent
//! announcements. The engine itself is observer-agnostic.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::job::metrics::JobMetrics;
use crate::job::recovery::{Backoff, RecoveryPolicy};
use crate::job::step::{Step, StepContext};
use crate::job::storage::JobStore;
use crate::job::types::{AttemptOutcome, JobId};

/// Forward-declares the daemon's `SchedulerHooks` so step impls that
/// need sub-agent announcements (wave dispatch, summary lines, …)
/// can reach it via [`PipelineObserver::daemon_hooks`]. Foreground
/// observers return `None`.
pub use crate::daemon::SchedulerHooks;

/// Hooks the orchestrator (daemon vs foreground) plugs into the
/// shared engine. Default impls cover the foreground "no-op"
/// behavior — concrete daemon impls override what they need.
#[async_trait]
pub trait PipelineObserver: Send + Sync {
    /// Job id this pipeline is running under. Used by the engine to
    /// build a [`JobId`] for [`JobMetrics`] and to address
    /// `metrics.json` writes.
    fn job_id(&self) -> &str;

    /// Announce that a step is about to start.
    fn on_step_start(&self, _seq: u32, _name: &str) {}

    /// Announce a step's final outcome (after retries).
    /// `summary` is a one-line human-readable rendering of `outcome`.
    fn on_step_end(&self, _seq: u32, _name: &str, _outcome: &AttemptOutcome, _summary: &str) {}

    /// Announce that a step is being retried after a transient or
    /// protocol failure. `total_budget` is the total of all retry
    /// budgets reachable from the step's recovery policy.
    fn on_step_retry(
        &self,
        _seq: u32,
        _name: &str,
        _next_attempt: u32,
        _total_budget: u32,
        _reason: &str,
        _detail: &str,
    ) {
    }

    /// Has this job been killed externally? Daemon: KillJob signal /
    /// watchdog. Foreground: never. The engine checks before each
    /// step and aborts cleanly when this returns `true`.
    async fn is_killed(&self) -> bool {
        false
    }

    /// Per-line liveness heartbeat. Daemon ticks
    /// `job_last_activity` so the watchdog stays satisfied across
    /// long-running steps. Foreground: no-op.
    fn touch_activity(&self) {}

    /// Optional [`SchedulerHooks`] for step impls that broadcast
    /// sub-agent announcements (wave dispatch, summary lines).
    /// Foreground returns `None`; the step impls already gate on
    /// `ctx.daemon_hooks.is_some()`.
    fn daemon_hooks(&self) -> Option<Arc<SchedulerHooks>> {
        None
    }
}

/// Result of a single pipeline run. Carries the (possibly partial)
/// metrics so the caller can persist a final `metrics.json` and
/// inspect counters without reading the file back from disk.
pub struct PipelineRun {
    pub success: bool,
    pub metrics: JobMetrics,
}

/// Runs every step in `steps` against a fresh [`StepContext`] anchored
/// at `job_dir` and `workdir`. Carries `ctx.workdir` mutations forward
/// across steps so a `PreflightStep`'s worktree redirect propagates to
/// every later step. Persists `metrics.json` after each step + once
/// more at the end so partial runs leave usable observability data on
/// disk.
///
/// Bails on the first non-`Success` / non-`Pending` outcome — recovery
/// per step is owned by [`run_step_with_retries`] under the recovery
/// policy declared on the step itself.
pub async fn run_pipeline(
    steps: Vec<Box<dyn Step>>,
    job_dir: PathBuf,
    workdir: PathBuf,
    observer: Arc<dyn PipelineObserver>,
) -> PipelineRun {
    let job_id_owned = JobId(observer.job_id().to_string());
    let mut metrics = JobMetrics::new(job_id_owned.clone());
    let store_for_metrics = JobStore::new().ok();
    let dir_for_metrics = store_for_metrics
        .as_ref()
        .and_then(|s| s.open(&job_id_owned).ok());
    let persist_metrics = |m: &JobMetrics| {
        if let Some(dir) = dir_for_metrics.as_ref() {
            let _ = dir.write_metrics(m);
        }
    };

    let mut current_workdir = workdir;
    let mut all_ok = true;
    let hooks = observer.daemon_hooks();

    for (idx, step) in steps.iter().enumerate() {
        let seq = u32::try_from(idx + 1).unwrap_or(u32::MAX);

        if observer.is_killed().await {
            tracing::info!(job = %observer.job_id(), seq, "pipeline aborted: job killed");
            all_ok = false;
            break;
        }

        let mut ctx = StepContext {
            job_dir: job_dir.clone(),
            step_seq: seq,
            attempt_n: 1,
            workdir: current_workdir.clone(),
            daemon_hooks: hooks.clone(),
        };

        observer.on_step_start(seq, step.name());
        observer.touch_activity();

        metrics.record_step();
        let policy = step.recovery_policy();
        let outcome = run_step_with_retries(
            step.as_ref(),
            &policy,
            &mut ctx,
            &mut metrics,
            seq,
            observer.as_ref(),
        )
        .await;

        // Carry workdir mutations (preflight redirects into the
        // worktree) forward to the next step.
        current_workdir = ctx.workdir.clone();

        let summary = render_outcome(&outcome);
        observer.on_step_end(seq, step.name(), &outcome, &summary);
        observer.touch_activity();

        // Per-step metrics snapshot so SummaryStep can read live
        // counts from `metrics.json`.
        persist_metrics(&metrics);

        let ok = matches!(outcome, AttemptOutcome::Success | AttemptOutcome::Pending);
        if !ok {
            tracing::error!(
                step = step.name(),
                seq,
                outcome = ?outcome,
                "step failed; aborting pipeline",
            );
            all_ok = false;
            break;
        }
    }

    metrics.finalize();
    persist_metrics(&metrics);

    PipelineRun {
        success: all_ok,
        metrics,
    }
}

/// Renders an `AttemptOutcome` as a short human-readable line. Used
/// by both the foreground CLI's eprintln output and the daemon's
/// broadcast display lines so they stay byte-identical.
pub fn render_outcome(outcome: &AttemptOutcome) -> String {
    match outcome {
        AttemptOutcome::Success => "success".to_string(),
        AttemptOutcome::Pending => "pending (placeholder)".to_string(),
        AttemptOutcome::TransientInfra { error } => format!("transient_infra: {error}"),
        AttemptOutcome::HardInfra { error } => format!("hard_infra: {error}"),
        AttemptOutcome::ProtocolViolation { category, detail } => {
            format!("protocol_violation [{category}]: {detail}")
        }
        AttemptOutcome::SemanticMistake { fix_loop_round } => {
            format!("semantic_mistake (round {fix_loop_round})")
        }
        AttemptOutcome::SpecDrift { gap } => format!("spec_drift: {gap}"),
    }
}

/// Runs a single step under its recovery policy. Tracks attempts in
/// `metrics`, announces retries via `observer`, and returns the final
/// outcome (after retries are exhausted or a non-retryable outcome is
/// observed).
async fn run_step_with_retries(
    step: &dyn Step,
    policy: &RecoveryPolicy,
    ctx: &mut StepContext,
    metrics: &mut JobMetrics,
    seq: u32,
    observer: &dyn PipelineObserver,
) -> AttemptOutcome {
    let (transient_budget, protocol_budget) = collect_retry_budgets(policy);
    let mut transient_used: u32 = 0;
    let mut protocol_used: u32 = 0;
    let mut attempt: u32 = 1;

    loop {
        ctx.attempt_n = attempt;
        tracing::info!(step = step.name(), seq, attempt, "running step");
        let outcome = step.run(ctx).await;
        metrics.record_attempt(&outcome, Some(policy));
        observer.touch_activity();

        let retry_decision = match &outcome {
            AttemptOutcome::TransientInfra { .. } => transient_budget
                .as_ref()
                .filter(|b| transient_used < u32::from(b.max))
                .map(|b| ("transient_infra", b.backoff.delay(attempt))),
            AttemptOutcome::ProtocolViolation { .. } => protocol_budget
                .as_ref()
                .filter(|b| protocol_used < u32::from(b.max))
                .map(|_| ("protocol_violation", Duration::ZERO)),
            _ => None,
        };

        let Some((reason_kind, delay)) = retry_decision else {
            return outcome;
        };

        let detail = match &outcome {
            AttemptOutcome::TransientInfra { error } => error.clone(),
            AttemptOutcome::ProtocolViolation { category, detail } => {
                format!("[{category}] {detail}")
            }
            _ => unreachable!("retry_decision matched non-retryable outcome"),
        };
        let next_attempt = attempt + 1;
        let total_budget = u32::from(transient_budget.as_ref().map(|b| b.max).unwrap_or(0))
            + u32::from(protocol_budget.as_ref().map(|b| b.max).unwrap_or(0));

        observer.on_step_retry(
            seq,
            step.name(),
            next_attempt,
            total_budget + 1,
            reason_kind,
            &truncate_ellipsis(&detail, 160),
        );

        match reason_kind {
            "transient_infra" => transient_used += 1,
            "protocol_violation" => protocol_used += 1,
            _ => unreachable!(),
        }

        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        attempt = next_attempt;
    }
}

/// Linearizes the retry budgets reachable from `policy`. Returns the
/// first `RetryTransient` and the first `RetryProtocol` encountered in
/// a depth-first walk, ignoring `None` / `Rollback` / `OperatorDecision`
/// children (those are no-op for retry budgeting today).
pub(crate) fn collect_retry_budgets(
    policy: &RecoveryPolicy,
) -> (Option<RetryTransientBudget>, Option<RetryProtocolBudget>) {
    let mut transient = None;
    let mut protocol = None;
    fn walk(
        p: &RecoveryPolicy,
        transient: &mut Option<RetryTransientBudget>,
        protocol: &mut Option<RetryProtocolBudget>,
    ) {
        match p {
            RecoveryPolicy::RetryTransient { max, backoff } => {
                if transient.is_none() {
                    *transient = Some(RetryTransientBudget {
                        max: *max,
                        backoff: backoff.clone(),
                    });
                }
            }
            RecoveryPolicy::RetryProtocol { max, .. } => {
                if protocol.is_none() {
                    *protocol = Some(RetryProtocolBudget { max: *max });
                }
            }
            RecoveryPolicy::Compose { policies } => {
                for child in policies {
                    walk(child, transient, protocol);
                }
            }
            RecoveryPolicy::Rollback { then, .. } => {
                walk(then, transient, protocol);
            }
            RecoveryPolicy::None | RecoveryPolicy::OperatorDecision { .. } => {}
        }
    }
    walk(policy, &mut transient, &mut protocol);
    (transient, protocol)
}

pub(crate) struct RetryTransientBudget {
    pub max: u8,
    pub backoff: Backoff,
}

pub(crate) struct RetryProtocolBudget {
    pub max: u8,
}

fn truncate_ellipsis(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::recovery::{Backoff, CorrectivePromptKey, RecoveryPolicy};

    #[test]
    fn collect_retry_budgets_unwraps_compose_pair() {
        // Mirrors the canonical helper_compose_policy shape used by
        // CodeReviewStep / ValidationStep: Compose([RetryTransient,
        // RetryProtocol]). Both budgets must be lifted out so retries fire
        // for either outcome kind.
        let policy = RecoveryPolicy::Compose {
            policies: vec![
                RecoveryPolicy::RetryTransient {
                    max: 3,
                    backoff: Backoff::Fixed { ms: 100 },
                },
                RecoveryPolicy::RetryProtocol {
                    max: 1,
                    corrective: CorrectivePromptKey("k".to_string()),
                },
            ],
        };
        let (t, p) = collect_retry_budgets(&policy);
        assert_eq!(t.as_ref().expect("transient").max, 3);
        assert_eq!(p.as_ref().expect("protocol").max, 1);
    }

    #[test]
    fn collect_retry_budgets_handles_bare_retry_transient() {
        let policy = RecoveryPolicy::RetryTransient {
            max: 2,
            backoff: Backoff::Fixed { ms: 0 },
        };
        let (t, p) = collect_retry_budgets(&policy);
        assert_eq!(t.expect("transient").max, 2);
        assert!(p.is_none());
    }

    #[test]
    fn collect_retry_budgets_returns_none_for_recovery_none() {
        let (t, p) = collect_retry_budgets(&RecoveryPolicy::None);
        assert!(t.is_none());
        assert!(p.is_none());
    }

    #[test]
    fn collect_retry_budgets_walks_nested_compose() {
        let policy = RecoveryPolicy::Compose {
            policies: vec![
                RecoveryPolicy::None,
                RecoveryPolicy::Compose {
                    policies: vec![RecoveryPolicy::RetryTransient {
                        max: 5,
                        backoff: Backoff::Fixed { ms: 0 },
                    }],
                },
                RecoveryPolicy::RetryProtocol {
                    max: 2,
                    corrective: CorrectivePromptKey("k".to_string()),
                },
            ],
        };
        let (t, p) = collect_retry_budgets(&policy);
        assert_eq!(t.expect("transient").max, 5);
        assert_eq!(p.expect("protocol").max, 2);
    }
}
