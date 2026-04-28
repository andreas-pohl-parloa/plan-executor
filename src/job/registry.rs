//! Registry mapping `JobKind` to its ordered list of `Step` shells.
//!
//! Phase A only implements `JobKind::Plan`. Other variants panic via
//! `unimplemented!()` and will be populated in later phases.

use crate::job::step::Step;
use crate::job::steps;
use crate::job::types::JobKind;

/// Map a `JobKind` to its ordered list of steps.
///
/// # Panics
///
/// Panics with `unimplemented!("populated in later phases")` for
/// `JobKind::PrFinalize`, `JobKind::Review`, `JobKind::Validate`, and
/// `JobKind::CompileFixWaves` until those job kinds are populated.
#[must_use]
pub fn steps_for(kind: &JobKind) -> Vec<Box<dyn Step>> {
    match kind {
        JobKind::Plan { .. } => vec![
            Box::new(steps::plan::PreflightStep),
            Box::new(steps::plan::WaveExecutionStep),
            Box::new(steps::plan::IntegrationTestingStep),
            Box::new(steps::plan::CodeReviewStep),
            Box::new(steps::plan::ValidationStep),
            Box::new(steps::plan::PrCreationStep),
            Box::new(steps::plan::PrFinalizeStep),
            Box::new(steps::plan::SummaryStep),
        ],
        JobKind::PrFinalize { .. }
        | JobKind::Review { .. }
        | JobKind::Validate { .. }
        | JobKind::CompileFixWaves { .. } => unimplemented!("populated in later phases"),
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
                ("integration_testing", false),
                ("code_review", false),
                ("validation", false),
                ("pr_creation", false),
                ("pr_finalize", true),
                ("summary", true),
            ]
        );
    }

    #[test]
    fn steps_for_plan_all_recovery_policies_are_none() {
        let kind = JobKind::Plan {
            manifest_path: PathBuf::from("/tmp/x"),
        };
        let steps = steps_for(&kind);
        let policies: Vec<RecoveryPolicy> = steps.iter().map(|s| s.recovery_policy()).collect();
        assert_eq!(policies, vec![RecoveryPolicy::None; 8]);
    }

    #[test]
    #[should_panic(expected = "populated in later phases")]
    fn steps_for_pr_finalize_panics() {
        let kind = JobKind::PrFinalize {
            owner: "octo".to_string(),
            repo: "demo".to_string(),
            pr: 1,
        };
        let _ = steps_for(&kind);
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
