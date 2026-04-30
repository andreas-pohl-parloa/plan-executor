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

/// Default plan pipeline emitted by `compile-plan` when the manifest does
/// not specify `plan.pipeline.steps`. The full 8-step sequence is the
/// canonical default — review and validation are essential, not optional.
/// One-shot manifests can drop steps explicitly via `plan.pipeline.steps`.
pub const DEFAULT_PLAN_STEPS: &[&str] = &[
    "preflight",
    "wave_execution",
    "integration_testing",
    "code_review",
    "validation",
    "pr_creation",
    "pr_finalize",
    "summary",
];

/// Every plan step the registry can construct. `steps_for_plan_filtered`
/// validates manifest-supplied step lists against this set so an unknown
/// name fails fast at job submission instead of silently dropping.
pub const KNOWN_PLAN_STEPS: &[&str] = &[
    "preflight",
    "wave_execution",
    "integration_testing",
    "code_review",
    "validation",
    "pr_creation",
    "pr_finalize",
    "summary",
];

/// Map a `JobKind` to its ordered list of steps.
///
/// For `JobKind::Plan` the step list defaults to [`DEFAULT_PLAN_STEPS`].
/// Callers that have a parsed manifest in hand should prefer
/// [`steps_for_plan_filtered`] so a manifest-supplied
/// `plan.pipeline.steps` override is honored.
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
        JobKind::Plan { manifest_path } => steps_for_plan_filtered(manifest_path, None),
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

/// Plan-pipeline step builder honoring an optional manifest-supplied step
/// list. When `override_steps` is `None`, builds [`DEFAULT_PLAN_STEPS`].
/// When `Some`, builds the named steps in the order given (any name not in
/// [`KNOWN_PLAN_STEPS`] panics — the manifest schema rejects those names
/// before the daemon would call this).
#[must_use]
pub fn steps_for_plan_filtered(
    manifest_path: &std::path::Path,
    override_steps: Option<&[String]>,
) -> Vec<Box<dyn Step>> {
    let owned_default: Vec<String> = DEFAULT_PLAN_STEPS.iter().map(|s| (*s).to_string()).collect();
    let names: &[String] = match override_steps {
        Some(s) if !s.is_empty() => s,
        _ => owned_default.as_slice(),
    };
    names
        .iter()
        .map(|name| build_plan_step(name.as_str(), manifest_path))
        .collect()
}

/// Constructs a single plan step by name. Panics on unknown names; callers
/// must validate against [`KNOWN_PLAN_STEPS`] (the manifest schema does so
/// at validate time).
fn build_plan_step(name: &str, manifest_path: &std::path::Path) -> Box<dyn Step> {
    match name {
        "preflight" => Box::new(steps::plan::PreflightStep),
        "wave_execution" => Box::new(steps::plan::WaveExecutionStep {
            manifest_path: manifest_path.to_path_buf(),
        }),
        "integration_testing" => Box::new(steps::plan::IntegrationTestingStep),
        "code_review" => Box::new(steps::plan::CodeReviewStep {
            manifest_path: manifest_path.to_path_buf(),
        }),
        "validation" => Box::new(steps::plan::ValidationStep {
            manifest_path: manifest_path.to_path_buf(),
        }),
        "pr_creation" => Box::new(steps::plan::PrCreationStep {
            manifest_path: manifest_path.to_path_buf(),
        }),
        "pr_finalize" => Box::new(steps::plan::PrFinalizeStep),
        "summary" => Box::new(steps::plan::SummaryStep {
            manifest_path: manifest_path.to_path_buf(),
        }),
        other => panic!("unknown plan step `{other}` (manifest schema should have rejected this)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::recovery::RecoveryPolicy;
    use std::path::PathBuf;

    #[test]
    fn default_plan_pipeline_includes_full_eight_steps() {
        let kind = JobKind::Plan {
            manifest_path: PathBuf::from("/tmp/x"),
        };
        let steps = steps_for(&kind);
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
    fn steps_for_plan_filtered_respects_override() {
        let path = PathBuf::from("/tmp/x");
        let override_steps = vec![
            "preflight".to_string(),
            "wave_execution".to_string(),
            "summary".to_string(),
        ];
        let steps = steps_for_plan_filtered(&path, Some(&override_steps));
        let names: Vec<_> = steps.iter().map(|s| s.name()).collect();
        assert_eq!(names, vec!["preflight", "wave_execution", "summary"]);
    }

    #[test]
    fn steps_for_plan_filtered_includes_code_review_when_listed() {
        let path = PathBuf::from("/tmp/x");
        let override_steps = vec![
            "wave_execution".to_string(),
            "code_review".to_string(),
            "validation".to_string(),
        ];
        let steps = steps_for_plan_filtered(&path, Some(&override_steps));
        let names: Vec<_> = steps.iter().map(|s| s.name()).collect();
        assert_eq!(names, vec!["wave_execution", "code_review", "validation"]);
    }

    #[test]
    fn steps_for_plan_filtered_falls_back_to_default_when_override_is_empty() {
        let path = PathBuf::from("/tmp/x");
        let empty: Vec<String> = Vec::new();
        let steps = steps_for_plan_filtered(&path, Some(&empty));
        assert_eq!(steps.len(), DEFAULT_PLAN_STEPS.len());
    }

    #[test]
    #[should_panic(expected = "unknown plan step")]
    fn steps_for_plan_filtered_panics_on_unknown_step() {
        let path = PathBuf::from("/tmp/x");
        let bad = vec!["does-not-exist".to_string()];
        let _ = steps_for_plan_filtered(&path, Some(&bad));
    }

    #[test]
    fn known_plan_steps_covers_default_and_code_review() {
        for step in DEFAULT_PLAN_STEPS {
            assert!(
                KNOWN_PLAN_STEPS.contains(step),
                "default step `{step}` missing from KNOWN_PLAN_STEPS",
            );
        }
        assert!(
            KNOWN_PLAN_STEPS.contains(&"code_review"),
            "code_review must remain a known step name even when not default",
        );
    }

    #[test]
    fn steps_for_plan_idempotency_flags_match_expectations() {
        let override_steps: Vec<String> = KNOWN_PLAN_STEPS.iter().map(|s| (*s).to_string()).collect();
        let steps = steps_for_plan_filtered(
            std::path::Path::new("/tmp/x"),
            Some(&override_steps),
        );
        let flags: Vec<(&'static str, bool)> =
            steps.iter().map(|s| (s.name(), s.idempotent())).collect();
        assert_eq!(
            flags,
            vec![
                ("preflight", true),
                ("wave_execution", false),
                ("integration_testing", true),
                ("code_review", true),
                ("validation", true),
                ("pr_creation", true),
                ("pr_finalize", true),
                ("summary", true),
            ]
        );
    }

    #[test]
    fn steps_for_plan_recovery_policies_match_documentation() {
        use crate::job::recovery::{Backoff, CorrectivePromptKey};
        // Exercise via the override path so this test can re-include
        // `code_review` regardless of whether the default omits it.
        let override_steps: Vec<String> = KNOWN_PLAN_STEPS.iter().map(|s| (*s).to_string()).collect();
        let steps = steps_for_plan_filtered(
            std::path::Path::new("/tmp/x"),
            Some(&override_steps),
        );
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
            RecoveryPolicy::RetryTransient {
                max: 1,
                backoff: Backoff::Fixed { ms: 0 },
            },
            helper_compose("code_review_protocol"),
            helper_compose("validation_protocol"),
            RecoveryPolicy::RetryTransient {
                max: 3,
                backoff: Backoff::Exponential {
                    initial_ms: 500,
                    max_ms: 8_000,
                    factor: 2.0,
                },
            },
            RecoveryPolicy::None,
            RecoveryPolicy::None,
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
