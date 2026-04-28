//! `Step` implementations for `JobKind::Plan`.
//!
//! `WaveExecutionStep` is the only step with real wave-traversal logic in
//! Phase D3.1; it loads the compiled manifest from disk and delegates to
//! [`crate::scheduler::run_wave_execution`]. The remaining steps are still
//! shells and will receive bodies in D3.2 / D3.3.

use std::path::PathBuf;

use async_trait::async_trait;

use crate::job::recovery::RecoveryPolicy;
use crate::job::step::{Step, StepContext};
use crate::job::types::AttemptOutcome;
use crate::scheduler;

/// Stub. Real preflight logic lands in Phase D (D3).
///
/// Today the orchestrator skill performs preflight checks; this shell exists
/// so the registry has an 8-element vector for `JobKind::Plan`.
pub struct PreflightStep;

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
    async fn run(&self, _ctx: &mut StepContext) -> AttemptOutcome {
        AttemptOutcome::Pending
    }
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
        scheduler::run_wave_execution(ctx, &manifest).await
    }
}

/// Stub. Phase A2.2 delegates to the integration testing flow.
pub struct IntegrationTestingStep;

#[async_trait]
impl Step for IntegrationTestingStep {
    fn name(&self) -> &'static str {
        "integration_testing"
    }
    fn idempotent(&self) -> bool {
        false
    }
    fn recovery_policy(&self) -> RecoveryPolicy {
        RecoveryPolicy::None
    }
    async fn run(&self, _ctx: &mut StepContext) -> AttemptOutcome {
        AttemptOutcome::Pending
    }
}

/// Stub. Phase A2.2 delegates to the code review flow.
pub struct CodeReviewStep;

#[async_trait]
impl Step for CodeReviewStep {
    fn name(&self) -> &'static str {
        "code_review"
    }
    fn idempotent(&self) -> bool {
        false
    }
    fn recovery_policy(&self) -> RecoveryPolicy {
        RecoveryPolicy::None
    }
    async fn run(&self, _ctx: &mut StepContext) -> AttemptOutcome {
        AttemptOutcome::Pending
    }
}

/// Stub. Phase A2.2 delegates to the validation flow.
pub struct ValidationStep;

#[async_trait]
impl Step for ValidationStep {
    fn name(&self) -> &'static str {
        "validation"
    }
    fn idempotent(&self) -> bool {
        false
    }
    fn recovery_policy(&self) -> RecoveryPolicy {
        RecoveryPolicy::None
    }
    async fn run(&self, _ctx: &mut StepContext) -> AttemptOutcome {
        AttemptOutcome::Pending
    }
}

/// Stub. Phase A2.2 delegates to PR creation.
pub struct PrCreationStep;

#[async_trait]
impl Step for PrCreationStep {
    fn name(&self) -> &'static str {
        "pr_creation"
    }
    fn idempotent(&self) -> bool {
        false
    }
    fn recovery_policy(&self) -> RecoveryPolicy {
        RecoveryPolicy::None
    }
    async fn run(&self, _ctx: &mut StepContext) -> AttemptOutcome {
        AttemptOutcome::Pending
    }
}

/// Stub. Phase A2.2 delegates to PR finalize.
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

/// Stub. Phase A2.2 delegates to the execution-summary writer.
pub struct SummaryStep;

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
    async fn run(&self, _ctx: &mut StepContext) -> AttemptOutcome {
        AttemptOutcome::Pending
    }
}
