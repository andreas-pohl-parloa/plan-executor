//! Per-job metrics: counters maintained over a job's lifetime and persisted
//! to `~/.plan-executor/jobs/<id>/metrics.json` on terminal state.
//!
//! Counters are incremented incrementally as attempts complete via
//! [`JobMetrics::record_attempt`], and a final snapshot is written to disk
//! when the job reaches a terminal state. Writing happens through
//! [`crate::job::storage::JobDir::write_metrics`].
//!
//! Wire format invariants (consumed by the upcoming `plan-executor jobs
//! metrics` aggregator in F2.2):
//!
//! - `recoveries_by_kind` keys are `RecoveryKind` snake_case variants.
//! - `outcomes_by_kind` keys are `AttemptOutcomeKind` snake_case variants.
//! - `started_at` and `finished_at` are RFC 3339 / ISO 8601 UTC strings.

use std::collections::HashMap;

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::job::recovery::RecoveryPolicy;
use crate::job::types::{AttemptOutcome, JobId};

/// Kind tag for the recovery policy applied around a step attempt.
///
/// `None` represents the absence of a recovery policy and is the default
/// for steps that succeed on the first try.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryKind {
    /// No recovery policy applied.
    None,
    /// Retry policy targeting transient infrastructure failures.
    RetryTransient,
    /// Retry policy targeting protocol violations with a corrective prompt.
    RetryProtocol,
    /// Rollback to a checkpoint and resume.
    Rollback,
    /// Sequence of policies composed together.
    Compose,
    /// Cap reached; awaiting operator decision.
    OperatorDecision,
}

/// Kind tag for the outcome of a step attempt, normalized for aggregation.
///
/// Mirrors the variants of [`AttemptOutcome`] but discards payload to make
/// the value usable as a `HashMap` key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptOutcomeKind {
    /// Attempt succeeded.
    Success,
    /// Non-transient infrastructure failure.
    HardInfra,
    /// Transient infrastructure failure.
    TransientInfra,
    /// Agent violated the protocol contract.
    ProtocolViolation,
    /// Agent produced semantically wrong output.
    SemanticMistake,
    /// Spec drift detected.
    SpecDrift,
    /// Outcome not yet determined.
    Pending,
}

/// Per-job metrics snapshot persisted to `metrics.json`.
///
/// Counters are updated incrementally via [`JobMetrics::record_attempt`]
/// and the `finished_at` timestamp is set via [`JobMetrics::finalize`] when
/// the job reaches a terminal state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JobMetrics {
    /// Owning Job's identifier.
    pub job_id: JobId,
    /// Total number of distinct steps observed by this metrics writer.
    pub step_count: u32,
    /// Total number of attempts recorded across all steps.
    pub attempts_total: u32,
    /// Counts of recovery policies applied, keyed by recovery kind.
    pub recoveries_by_kind: HashMap<RecoveryKind, u32>,
    /// Counts of attempt outcomes, keyed by outcome kind.
    pub outcomes_by_kind: HashMap<AttemptOutcomeKind, u32>,
    /// ISO 8601 UTC timestamp of metrics initialization.
    pub started_at: String,
    /// ISO 8601 UTC timestamp set by [`JobMetrics::finalize`]; `None` until
    /// the job reaches a terminal state.
    pub finished_at: Option<String>,
}

/// `Default` for `JobId` is implemented locally so that `JobMetrics` can
/// derive `Default`. The empty string is a sentinel that callers must
/// overwrite by constructing via [`JobMetrics::new`].
impl Default for JobId {
    fn default() -> Self {
        JobId(String::new())
    }
}

impl JobMetrics {
    /// Initialize a fresh metrics snapshot with all counters at zero.
    ///
    /// `started_at` is set to the current UTC time in RFC 3339 format.
    #[must_use]
    pub fn new(job_id: JobId) -> Self {
        Self {
            job_id,
            step_count: 0,
            attempts_total: 0,
            recoveries_by_kind: HashMap::new(),
            outcomes_by_kind: HashMap::new(),
            started_at: Utc::now().to_rfc3339(),
            finished_at: None,
        }
    }

    /// Record a single step attempt's outcome and any recovery policy that
    /// preceded it.
    ///
    /// Increments `attempts_total`, the relevant `outcomes_by_kind` bucket,
    /// and (when `recovery_applied` is `Some`) the relevant
    /// `recoveries_by_kind` bucket.
    pub fn record_attempt(
        &mut self,
        outcome: &AttemptOutcome,
        recovery_applied: Option<&RecoveryPolicy>,
    ) {
        self.attempts_total = self.attempts_total.saturating_add(1);
        let outcome_kind = AttemptOutcomeKind::from(outcome);
        *self.outcomes_by_kind.entry(outcome_kind).or_insert(0) += 1;
        if let Some(policy) = recovery_applied {
            let recovery_kind = RecoveryKind::from(policy);
            *self.recoveries_by_kind.entry(recovery_kind).or_insert(0) += 1;
        }
    }

    /// Increment the `step_count` counter.
    ///
    /// Callers invoke this once per step boundary (typically when a step
    /// transitions from `Pending` to `Running`). Kept separate from
    /// [`Self::record_attempt`] because a single step may produce many
    /// attempts.
    pub fn record_step(&mut self) {
        self.step_count = self.step_count.saturating_add(1);
    }

    /// Set `finished_at` to the current UTC time.
    ///
    /// Idempotent: subsequent calls overwrite the previous timestamp.
    pub fn finalize(&mut self) {
        self.finished_at = Some(Utc::now().to_rfc3339());
    }
}

impl From<&AttemptOutcome> for AttemptOutcomeKind {
    fn from(outcome: &AttemptOutcome) -> Self {
        match outcome {
            AttemptOutcome::Success => Self::Success,
            AttemptOutcome::HardInfra { .. } => Self::HardInfra,
            AttemptOutcome::TransientInfra { .. } => Self::TransientInfra,
            AttemptOutcome::ProtocolViolation { .. } => Self::ProtocolViolation,
            AttemptOutcome::SemanticMistake { .. } => Self::SemanticMistake,
            AttemptOutcome::SpecDrift { .. } => Self::SpecDrift,
            AttemptOutcome::Pending => Self::Pending,
        }
    }
}

impl From<&RecoveryPolicy> for RecoveryKind {
    fn from(policy: &RecoveryPolicy) -> Self {
        match policy {
            RecoveryPolicy::None => Self::None,
            RecoveryPolicy::RetryTransient { .. } => Self::RetryTransient,
            RecoveryPolicy::RetryProtocol { .. } => Self::RetryProtocol,
            RecoveryPolicy::Rollback { .. } => Self::Rollback,
            RecoveryPolicy::Compose { .. } => Self::Compose,
            RecoveryPolicy::OperatorDecision { .. } => Self::OperatorDecision,
        }
    }
}
