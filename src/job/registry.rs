//! Registry mapping `JobKind` to its ordered list of `Step` shells.
//!
//! Phase A only implements `JobKind::Plan`. Phase C adds `JobKind::PrFinalize`.
//! Other variants panic via `unimplemented!()` and will be populated in later
//! phases.

use crate::job::step::Step;
use crate::job::steps;
use crate::job::steps::pr_finalize::MergeMode as RuntimeMergeMode;
use crate::job::types::{JobKind, MergeMode as WireMergeMode};

/// Translate the wire-format [`WireMergeMode`] (carried in `JobKind`) into
/// the runtime [`RuntimeMergeMode`] used by `MergeStep`. Kept as a private
/// helper so the registry is the only seam between the two enums.
fn merge_mode_to_runtime(wire: WireMergeMode) -> RuntimeMergeMode {
    match wire {
        WireMergeMode::None => RuntimeMergeMode::None,
        WireMergeMode::Merge => RuntimeMergeMode::Merge,
        WireMergeMode::MergeAdmin => RuntimeMergeMode::MergeAdmin,
    }
}

/// Map a `JobKind` to its ordered list of steps.
///
/// For `JobKind::PrFinalize` the registry emits a fixed 5-step sequence:
/// `pr_lookup`, `mark_ready`, `monitor`, `merge`, `report`. The merge step
/// is always present; its runtime `mode` is derived from
/// `JobKind::PrFinalize::merge_mode` (the CLI surface in Phase C1.2 sets
/// this to `MergeMode::None | Merge | MergeAdmin` based on the user's
/// `--merge` / `--merge-admin` flags). When `MergeMode::None`, `MergeStep`
/// short-circuits to `AttemptOutcome::Success`.
///
/// # Panics
///
/// Panics with `unimplemented!("populated in later phases")` for
/// `JobKind::Review`, `JobKind::Validate`, and `JobKind::CompileFixWaves`
/// until those job kinds are populated.
#[must_use]
pub fn steps_for(kind: &JobKind) -> Vec<Box<dyn Step>> {
    match kind {
        JobKind::Plan { manifest_path } => vec![
            Box::new(steps::plan::PreflightStep),
            Box::new(steps::plan::WaveExecutionStep {
                manifest_path: manifest_path.clone(),
            }),
            Box::new(steps::plan::IntegrationTestingStep),
            Box::new(steps::plan::CodeReviewStep {
                manifest_path: manifest_path.clone(),
            }),
            Box::new(steps::plan::ValidationStep {
                manifest_path: manifest_path.clone(),
            }),
            Box::new(steps::plan::PrCreationStep {
                manifest_path: manifest_path.clone(),
            }),
            Box::new(steps::plan::PrFinalizeStep),
            Box::new(steps::plan::SummaryStep {
                manifest_path: manifest_path.clone(),
            }),
        ],
        JobKind::PrFinalize {
            owner,
            repo,
            pr,
            merge_mode,
        } => vec![
            Box::new(steps::pr_finalize::PrLookupStep {
                owner: owner.clone(),
                repo: repo.clone(),
                pr: *pr,
            }),
            Box::new(steps::pr_finalize::MarkReadyStep {
                owner: owner.clone(),
                repo: repo.clone(),
                pr: *pr,
            }),
            Box::new(steps::pr_finalize::MonitorStep {
                owner: owner.clone(),
                repo: repo.clone(),
                pr: *pr,
                // `None` defers resolution to runtime — the step looks at
                // `PLAN_EXECUTOR_PR_MONITOR_SCRIPT`, the binary's sibling
                // dir, then the plan-executor plugin install location.
                script_path: None,
            }),
            Box::new(steps::pr_finalize::MergeStep {
                owner: owner.clone(),
                repo: repo.clone(),
                pr: *pr,
                mode: merge_mode_to_runtime(*merge_mode),
            }),
            Box::new(steps::pr_finalize::ReportStep {
                owner: owner.clone(),
                repo: repo.clone(),
                pr: *pr,
            }),
        ],
        JobKind::Review { .. } | JobKind::Validate { .. } | JobKind::CompileFixWaves { .. } => {
            unimplemented!("populated in later phases")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::recovery::RecoveryPolicy;
    use std::path::PathBuf;

    #[test]
    fn steps_for_plan_returns_eight_steps_in_expected_order() {
        let kind = JobKind::Plan {
            manifest_path: PathBuf::from("/tmp/x"),
        };
        let steps = steps_for(&kind);
        assert_eq!(steps.len(), 8);
        let names: Vec<_> = steps.iter().map(|s| s.name()).collect();
        assert_eq!(
            names,
            vec![
                "preflight",
                "wave_execution",
                "integration_testing",
                "code_review",
                "validation",
                "pr_creation",
                "pr_finalize",
                "summary",
            ]
        );
    }

    #[test]
    fn steps_for_plan_idempotency_flags_match_expectations() {
        let kind = JobKind::Plan {
            manifest_path: PathBuf::from("/tmp/x"),
        };
        let steps = steps_for(&kind);
        let flags: Vec<(&'static str, bool)> =
            steps.iter().map(|s| (s.name(), s.idempotent())).collect();
        assert_eq!(
            flags,
            vec![
                ("preflight", true),
                ("wave_execution", false),
                // D3.3: integration_testing became idempotent — re-running
                // `cargo test --workspace` is safe.
                ("integration_testing", true),
                // D3.2: code_review and validation became idempotent — the
                // helper-driven steps re-issue the same helper invocations
                // on retry without external compensation.
                ("code_review", true),
                ("validation", true),
                // D3.3: pr_creation became idempotent — `gh pr view` short
                // circuits to the existing PR URL when one already exists.
                ("pr_creation", true),
                ("pr_finalize", true),
                ("summary", true),
            ]
        );
    }

    #[test]
    fn steps_for_plan_recovery_policies_match_documentation() {
        use crate::job::recovery::{Backoff, CorrectivePromptKey};
        let kind = JobKind::Plan {
            manifest_path: PathBuf::from("/tmp/x"),
        };
        let steps = steps_for(&kind);
        let policies: Vec<RecoveryPolicy> = steps.iter().map(|s| s.recovery_policy()).collect();
        let helper_compose = |corrective: &str| RecoveryPolicy::Compose {
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
                    corrective: CorrectivePromptKey(corrective.to_string()),
                },
            ],
        };
        let expected = vec![
            RecoveryPolicy::None, // preflight
            RecoveryPolicy::None, // wave_execution
            // D3.3: integration_testing surfaces TransientInfra on
            // retry-able test failures; one retry gives flaky / network-
            // bound suites a single chance to recover.
            RecoveryPolicy::RetryTransient {
                max: 1,
                backoff: Backoff::Fixed { ms: 0 },
            },
            helper_compose("code_review_protocol"), // code_review (D3.2)
            helper_compose("validation_protocol"),  // validation (D3.2)
            // D3.3: pr_creation runs `gh pr create`; exponential backoff
            // matches the cadence already used elsewhere for `gh` API
            // hiccups (see pr_finalize::default_backoff).
            RecoveryPolicy::RetryTransient {
                max: 3,
                backoff: Backoff::Exponential {
                    initial_ms: 500,
                    max_ms: 8_000,
                    factor: 2.0,
                },
            },
            RecoveryPolicy::None, // pr_finalize (placeholder; the dedicated
            // JobKind::PrFinalize pipeline owns finalize)
            RecoveryPolicy::None, // summary (D3.3 — best-effort)
        ];
        assert_eq!(policies, expected);
    }

    #[test]
    fn steps_for_pr_finalize_returns_five_steps_in_expected_order() {
        let kind = JobKind::PrFinalize {
            owner: "octo".to_string(),
            repo: "demo".to_string(),
            pr: 1,
            merge_mode: WireMergeMode::None,
        };
        let steps = steps_for(&kind);
        let names: Vec<&'static str> = steps.iter().map(|s| s.name()).collect();
        assert_eq!(
            names,
            vec!["pr_lookup", "mark_ready", "monitor", "merge", "report"]
        );
    }

    #[test]
    fn steps_for_pr_finalize_idempotency_flags_match_expectations() {
        let kind = JobKind::PrFinalize {
            owner: "octo".to_string(),
            repo: "demo".to_string(),
            pr: 1,
            merge_mode: WireMergeMode::None,
        };
        let steps = steps_for(&kind);
        let flags: Vec<(&'static str, bool)> =
            steps.iter().map(|s| (s.name(), s.idempotent())).collect();
        assert_eq!(
            flags,
            vec![
                ("pr_lookup", true),
                ("mark_ready", true),
                ("monitor", true),
                ("merge", false),
                ("report", true),
            ]
        );
    }

    #[test]
    fn steps_for_pr_finalize_recovery_policies_match_documentation() {
        use crate::job::recovery::Backoff;
        let kind = JobKind::PrFinalize {
            owner: "octo".to_string(),
            repo: "demo".to_string(),
            pr: 1,
            merge_mode: WireMergeMode::None,
        };
        let steps = steps_for(&kind);
        let policies: Vec<RecoveryPolicy> = steps.iter().map(|s| s.recovery_policy()).collect();
        let expected = vec![
            RecoveryPolicy::RetryTransient {
                max: 3,
                backoff: Backoff::Exponential {
                    initial_ms: 500,
                    max_ms: 8_000,
                    factor: 2.0,
                },
            },
            RecoveryPolicy::RetryTransient {
                max: 3,
                backoff: Backoff::Exponential {
                    initial_ms: 500,
                    max_ms: 8_000,
                    factor: 2.0,
                },
            },
            RecoveryPolicy::RetryTransient {
                max: 1,
                backoff: Backoff::Fixed { ms: 0 },
            },
            RecoveryPolicy::None,
            RecoveryPolicy::None,
        ];
        assert_eq!(policies, expected);
    }

    #[test]
    fn merge_mode_to_runtime_translates_each_variant() {
        let translated = (
            merge_mode_to_runtime(WireMergeMode::None),
            merge_mode_to_runtime(WireMergeMode::Merge),
            merge_mode_to_runtime(WireMergeMode::MergeAdmin),
        );
        let expected = (
            RuntimeMergeMode::None,
            RuntimeMergeMode::Merge,
            RuntimeMergeMode::MergeAdmin,
        );
        assert_eq!(translated, expected);
    }

    #[test]
    #[should_panic(expected = "populated in later phases")]
    fn steps_for_review_panics() {
        let kind = JobKind::Review {
            branch: "feat/x".to_string(),
            base: "main".to_string(),
        };
        let _ = steps_for(&kind);
    }

    #[test]
    #[should_panic(expected = "populated in later phases")]
    fn steps_for_validate_panics() {
        let kind = JobKind::Validate {
            manifest_path: PathBuf::from("/tmp/x"),
        };
        let _ = steps_for(&kind);
    }

    #[test]
    #[should_panic(expected = "populated in later phases")]
    fn steps_for_compile_fix_waves_panics() {
        let kind = JobKind::CompileFixWaves {
            manifest_path: PathBuf::from("/tmp/x"),
            findings_path: PathBuf::from("/tmp/f"),
        };
        let _ = steps_for(&kind);
    }
}
