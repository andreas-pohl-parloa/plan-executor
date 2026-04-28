//! Core Job/Step type definitions used by the Job framework.
//!
//! Pure data types only — no behavior, no I/O, no execution logic. These types
//! are designed to be serializable for persistence and transport, and to
//! support pattern matching across the Job framework's state machine.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Unique identifier for a Job.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize, Serialize)]
pub struct JobId(pub String);

/// Wire-format merge mode carried inside [`JobKind::PrFinalize`].
///
/// Mirrors the runtime `crate::job::steps::pr_finalize::MergeMode` but adds
/// `Serialize`/`Deserialize` so the merge intent persists across daemon
/// restarts and `job.json` round-trips. Translation to the runtime type
/// happens in `crate::job::registry::steps_for` when the registry hydrates
/// the `MergeStep` from the persisted `JobKind`.
///
/// Defined here (rather than alongside the step impls) so the `pr_finalize`
/// step module remains free of serde derives — only wire-format types in
/// the `types` module carry that obligation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeMode {
    /// Do not attempt a merge; the merge step short-circuits to `Success`.
    #[default]
    None,
    /// Run `gh pr merge` with the standard squash-and-delete flags.
    Merge,
    /// Run `gh pr merge --admin` (bypasses required-reviewer checks).
    MergeAdmin,
}

/// Kind of work a Job performs, with kind-specific parameters.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobKind {
    /// Execute a plan manifest end-to-end.
    Plan { manifest_path: PathBuf },
    /// Finalize a pull request (rebase, checks, merge gating).
    ///
    /// `merge_mode` carries the user's `--merge` / `--merge-admin` intent
    /// from the CLI surface through `JobStore::create` so it persists in
    /// `job.json` and survives a daemon restart. `MergeMode::None` (the
    /// default) means "monitor only, no merge".
    PrFinalize {
        owner: String,
        repo: String,
        pr: u32,
        #[serde(default)]
        merge_mode: MergeMode,
    },
    /// Run a code review job for a branch against a base.
    Review { branch: String, base: String },
    /// Validate a plan manifest without executing it.
    Validate { manifest_path: PathBuf },
    /// Run compile + fix wave loops over a manifest using prior findings.
    CompileFixWaves {
        manifest_path: PathBuf,
        findings_path: PathBuf,
    },
}

/// Lifecycle state of a Job.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// Submitted but not yet started.
    Pending,
    /// Currently running.
    Running,
    /// Paused awaiting external input or operator action.
    Suspended { reason: String },
    /// Terminated successfully.
    Succeeded,
    /// Terminated unsuccessfully; `recoverable` indicates whether retry is meaningful.
    Failed { reason: String, recoverable: bool },
}

/// A submitted Job along with its current state and step plan.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Job {
    /// Unique identifier of the Job.
    pub id: JobId,
    /// Kind of work and its parameters.
    pub kind: JobKind,
    /// Current lifecycle state.
    pub state: JobState,
    /// ISO 8601 UTC timestamp of creation.
    pub created_at: String,
    /// Ordered step instances, populated at submission time.
    pub steps: Vec<StepInstance>,
}

/// A concrete instance of a step within a Job, with attempt history.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct StepInstance {
    /// 1-based sequence number within the Job's step list.
    pub seq: u32,
    /// Human-readable step name (matches the static step kind).
    pub name: String,
    /// Current status of this step.
    pub status: StepStatus,
    /// History of attempts made for this step.
    pub attempts: Vec<StepAttempt>,
    /// Whether this step is safe to retry without external compensation.
    pub idempotent: bool,
}

/// Status of a single step within a Job.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    /// Not yet started.
    Pending,
    /// Currently executing.
    Running,
    /// Completed successfully.
    Succeeded,
    /// Failed; `recoverable` indicates whether retry is meaningful.
    Failed { reason: String, recoverable: bool },
    /// Skipped because precondition rendered the step unnecessary.
    SkippedNotRequired,
}

/// One attempt at executing a step.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct StepAttempt {
    /// 1-based attempt number for this step.
    pub n: u32,
    /// ISO 8601 UTC timestamp when the attempt started.
    pub started_at: String,
    /// ISO 8601 UTC timestamp when the attempt finished, if finished.
    pub finished_at: Option<String>,
    /// Outcome classification used to drive recovery decisions.
    pub outcome: AttemptOutcome,
    /// Identifier of any recovery policy applied prior to this attempt.
    pub recovery_applied: Option<String>,
}

/// Categorical outcome of a step attempt; drives recovery routing.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AttemptOutcome {
    /// Attempt succeeded.
    Success,
    /// Non-transient infrastructure failure (e.g., missing tool, auth denied).
    HardInfra { error: String },
    /// Transient infrastructure failure (e.g., network blip, rate limit).
    TransientInfra { error: String },
    /// Agent violated the protocol contract (malformed output, missing artifact).
    ProtocolViolation { category: String, detail: String },
    /// Agent produced semantically wrong output; awaiting fix-loop iteration.
    SemanticMistake { fix_loop_round: u32 },
    /// Spec drift detected; gap describes what diverged from the manifest.
    SpecDrift { gap: String },
    /// Outcome not yet determined.
    Pending,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn job_id_equality_and_hashing() {
        let a = JobId("job-1".to_string());
        let b = JobId("job-1".to_string());
        let c = JobId("job-2".to_string());
        assert_eq!(a, b);
        let mut set = HashSet::new();
        set.insert(a.clone());
        set.insert(b.clone());
        set.insert(c.clone());
        let expected: HashSet<JobId> = [a, c].into_iter().collect();
        assert_eq!(set, expected);
    }

    #[test]
    fn job_id_ordering() {
        let a = JobId("a".to_string());
        let b = JobId("b".to_string());
        let mut ids = vec![b.clone(), a.clone()];
        ids.sort();
        assert_eq!(ids, vec![a, b]);
    }

    #[test]
    fn job_kind_plan_roundtrip() {
        let value = JobKind::Plan {
            manifest_path: PathBuf::from("/tmp/plan.json"),
        };
        let json = serde_json::to_string(&value).expect("serialize");
        let back: JobKind = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, value);
    }

    #[test]
    fn job_kind_pr_finalize_roundtrip() {
        let value = JobKind::PrFinalize {
            owner: "octo".to_string(),
            repo: "demo".to_string(),
            pr: 42,
            merge_mode: MergeMode::None,
        };
        let json = serde_json::to_string(&value).expect("serialize");
        let back: JobKind = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, value);
    }

    #[test]
    fn job_kind_pr_finalize_merge_mode_roundtrip_for_each_variant() {
        for mode in [MergeMode::None, MergeMode::Merge, MergeMode::MergeAdmin] {
            let value = JobKind::PrFinalize {
                owner: "octo".to_string(),
                repo: "demo".to_string(),
                pr: 42,
                merge_mode: mode,
            };
            let json = serde_json::to_string(&value).expect("serialize");
            let back: JobKind = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, value);
        }
    }

    #[test]
    fn job_kind_pr_finalize_defaults_merge_mode_when_field_missing() {
        // Older `job.json` files written before C1.2 won't have a
        // `merge_mode` key. `#[serde(default)]` must fall back to
        // `MergeMode::None` so existing manifests still deserialize.
        let json = r#"{"kind":"pr_finalize","owner":"octo","repo":"demo","pr":42}"#;
        let parsed: JobKind = serde_json::from_str(json).expect("deserialize");
        let expected = JobKind::PrFinalize {
            owner: "octo".to_string(),
            repo: "demo".to_string(),
            pr: 42,
            merge_mode: MergeMode::None,
        };
        assert_eq!(parsed, expected);
    }

    #[test]
    fn job_kind_review_roundtrip() {
        let value = JobKind::Review {
            branch: "feat/x".to_string(),
            base: "main".to_string(),
        };
        let json = serde_json::to_string(&value).expect("serialize");
        let back: JobKind = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, value);
    }

    #[test]
    fn job_kind_validate_roundtrip() {
        let value = JobKind::Validate {
            manifest_path: PathBuf::from("/tmp/manifest.json"),
        };
        let json = serde_json::to_string(&value).expect("serialize");
        let back: JobKind = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, value);
    }

    #[test]
    fn job_kind_compile_fix_waves_roundtrip() {
        let value = JobKind::CompileFixWaves {
            manifest_path: PathBuf::from("/tmp/manifest.json"),
            findings_path: PathBuf::from("/tmp/findings.json"),
        };
        let json = serde_json::to_string(&value).expect("serialize");
        let back: JobKind = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, value);
    }

    #[test]
    fn job_kind_rejects_unknown_kind() {
        let parsed: Result<JobKind, _> = serde_json::from_str(r#"{"kind":"bogus"}"#);
        assert!(parsed.is_err());
    }

    #[test]
    fn job_state_pending_roundtrip() {
        let value = JobState::Pending;
        let json = serde_json::to_string(&value).expect("serialize");
        let back: JobState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, value);
    }

    #[test]
    fn job_state_running_roundtrip() {
        let value = JobState::Running;
        let json = serde_json::to_string(&value).expect("serialize");
        let back: JobState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, value);
    }

    #[test]
    fn job_state_suspended_roundtrip() {
        let value = JobState::Suspended {
            reason: "awaiting approval".to_string(),
        };
        let json = serde_json::to_string(&value).expect("serialize");
        let back: JobState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, value);
    }

    #[test]
    fn job_state_succeeded_roundtrip() {
        let value = JobState::Succeeded;
        let json = serde_json::to_string(&value).expect("serialize");
        let back: JobState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, value);
    }

    #[test]
    fn job_state_failed_roundtrip() {
        let value = JobState::Failed {
            reason: "boom".to_string(),
            recoverable: false,
        };
        let json = serde_json::to_string(&value).expect("serialize");
        let back: JobState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, value);
    }

    #[test]
    fn attempt_outcome_success_roundtrip() {
        let value = AttemptOutcome::Success;
        let json = serde_json::to_string(&value).expect("serialize");
        let back: AttemptOutcome = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, value);
    }

    #[test]
    fn attempt_outcome_hard_infra_roundtrip() {
        let value = AttemptOutcome::HardInfra {
            error: "tool missing".to_string(),
        };
        let json = serde_json::to_string(&value).expect("serialize");
        let back: AttemptOutcome = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, value);
    }

    #[test]
    fn attempt_outcome_transient_infra_roundtrip() {
        let value = AttemptOutcome::TransientInfra {
            error: "rate limited".to_string(),
        };
        let json = serde_json::to_string(&value).expect("serialize");
        let back: AttemptOutcome = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, value);
    }

    #[test]
    fn attempt_outcome_protocol_violation_roundtrip() {
        let value = AttemptOutcome::ProtocolViolation {
            category: "missing_artifact".to_string(),
            detail: "no findings.json".to_string(),
        };
        let json = serde_json::to_string(&value).expect("serialize");
        let back: AttemptOutcome = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, value);
    }

    #[test]
    fn attempt_outcome_semantic_mistake_roundtrip() {
        let value = AttemptOutcome::SemanticMistake { fix_loop_round: 2 };
        let json = serde_json::to_string(&value).expect("serialize");
        let back: AttemptOutcome = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, value);
    }

    #[test]
    fn attempt_outcome_spec_drift_roundtrip() {
        let value = AttemptOutcome::SpecDrift {
            gap: "missing acceptance check".to_string(),
        };
        let json = serde_json::to_string(&value).expect("serialize");
        let back: AttemptOutcome = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, value);
    }

    #[test]
    fn attempt_outcome_pending_roundtrip() {
        let value = AttemptOutcome::Pending;
        let json = serde_json::to_string(&value).expect("serialize");
        let back: AttemptOutcome = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, value);
    }

    #[test]
    fn step_status_pending_roundtrip() {
        let value = StepStatus::Pending;
        let json = serde_json::to_string(&value).expect("serialize");
        let back: StepStatus = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, value);
    }

    #[test]
    fn step_status_running_roundtrip() {
        let value = StepStatus::Running;
        let json = serde_json::to_string(&value).expect("serialize");
        let back: StepStatus = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, value);
    }

    #[test]
    fn step_status_succeeded_roundtrip() {
        let value = StepStatus::Succeeded;
        let json = serde_json::to_string(&value).expect("serialize");
        let back: StepStatus = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, value);
    }

    #[test]
    fn step_status_failed_roundtrip() {
        let value = StepStatus::Failed {
            reason: "step failed".to_string(),
            recoverable: true,
        };
        let json = serde_json::to_string(&value).expect("serialize");
        let back: StepStatus = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, value);
    }

    #[test]
    fn step_status_skipped_not_required_roundtrip() {
        let value = StepStatus::SkippedNotRequired;
        let json = serde_json::to_string(&value).expect("serialize");
        let back: StepStatus = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, value);
    }
}
