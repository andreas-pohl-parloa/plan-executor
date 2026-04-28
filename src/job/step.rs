//! `Step` trait and `StepContext` for the Job framework.
//!
//! Phase A introduces the trait and registry only. The concrete step impls for
//! `JobKind::Plan` arrive in Phase A2.2 and Phase D.

use async_trait::async_trait;
use std::path::PathBuf;

use crate::job::recovery::RecoveryPolicy;
use crate::job::types::AttemptOutcome;

/// A unit of work inside a Job.
///
/// Each step has its own recovery policy and runs against a `StepContext` that
/// exposes per-step state and workdir.
#[async_trait]
pub trait Step: Send + Sync {
    /// Stable, lower_snake_case identifier of the step.
    fn name(&self) -> &'static str;

    /// Whether this step is safe to retry without external compensation.
    fn idempotent(&self) -> bool;

    /// Recovery policy applied when this step's attempt outcome warrants it.
    fn recovery_policy(&self) -> RecoveryPolicy;

    /// Whether the orchestrator should checkpoint state before running.
    fn checkpoint_before(&self) -> bool {
        true
    }

    /// Run the step against the given context, returning the attempt outcome.
    async fn run(&self, ctx: &mut StepContext) -> AttemptOutcome;
}

/// Per-step execution context.
///
/// Phase A keeps it minimal; later phases will add accessors for prior step
/// outputs, persisted state, claude-invoker configuration, etc.
#[derive(Debug, Clone)]
pub struct StepContext {
    /// Job-scoped working directory for state and artifacts.
    pub job_dir: PathBuf,
    /// 1-based sequence number of the step within its Job.
    pub step_seq: u32,
    /// 1-based attempt number for this step run.
    pub attempt_n: u32,
    /// Repository workdir the step operates on.
    pub workdir: PathBuf,
}
