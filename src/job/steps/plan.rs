//! Stub `Step` implementations for `JobKind::Plan`.
//!
//! Each step is a unit struct with a trivial `Step` impl that returns
//! `AttemptOutcome::Pending`. Real bodies arrive in Phase A2.2 and Phase D.

use async_trait::async_trait;

use crate::job::recovery::RecoveryPolicy;
use crate::job::step::{Step, StepContext};
use crate::job::types::AttemptOutcome;

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

/// Stub. Phase A2.2 wires this to `executor::run_compiled_manifest`.
pub struct WaveExecutionStep;

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
    async fn run(&self, _ctx: &mut StepContext) -> AttemptOutcome {
        AttemptOutcome::Pending
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
