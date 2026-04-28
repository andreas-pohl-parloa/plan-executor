//! Supervisor wiring layer: per-step state + single-turn classifier.
//!
//! Phase B2.1 (wiring-only). Provides the `SupervisorState` retry budget,
//! the `SupervisorAction` daemon-action enum, and `observe_turn`, the pure
//! function that runs the detector against a stream-json turn and advances
//! state. Phase D will plug this into the daemon's parser; rollback is
//! Phase B2.2.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::job::recovery::RecoveryPolicy;
use crate::supervisor::detector::detect;
use crate::supervisor::prompts::corrective_for;
use crate::supervisor::violation::ProtocolViolation;

/// Per-step supervisor state. Persisted under the step's current attempt
/// directory so a daemon restart can resume the retry budget where it
/// left off.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SupervisorState {
    /// Maximum number of corrective re-prompts allowed for this step.
    /// Default per the plan: 3.
    pub max_attempts: u8,
    /// Number of corrective re-prompts already issued. Each Recover
    /// action increments this. Compared against `max_attempts` to decide
    /// when to surface `Exhausted`.
    pub attempts_used: u8,
    /// Append-only history of every violation observed on this step,
    /// across all attempts.
    pub history: Vec<ObservedViolation>,
}

/// One violation entry in `SupervisorState::history`, tagged with the
/// 1-indexed attempt number it was observed on.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ObservedViolation {
    /// Attempt number this violation was observed on (1-indexed).
    pub attempt: u8,
    /// The protocol violation observed.
    pub violation: ProtocolViolation,
}

impl SupervisorState {
    /// New state with the given retry budget. Phase B default: 3.
    #[must_use]
    pub fn new(max_attempts: u8) -> Self {
        Self {
            max_attempts,
            attempts_used: 0,
            history: Vec::new(),
        }
    }

    /// True when the retry budget has been spent.
    #[must_use]
    pub fn is_exhausted(&self) -> bool {
        self.attempts_used >= self.max_attempts
    }

    /// Returns the attempt number whose `attempts/<n>/checkpoint.json`
    /// the daemon should restore on rollback, or `None` if no attempt
    /// has run yet (and therefore no checkpoint exists).
    ///
    /// Returns `Some(self.attempts_used)` rather than `attempts_used - 1`
    /// because the snapshot for attempt n is taken BEFORE attempt n
    /// starts and lives in the dir for attempt `attempts_used`.
    #[must_use]
    pub fn previous_attempt(&self) -> Option<u8> {
        if self.attempts_used >= 1 {
            Some(self.attempts_used)
        } else {
            None
        }
    }
}

/// What the daemon should do when the retry budget is exhausted.
///
/// `observe_turn` always returns `ExhaustedNext::Fail` for the
/// `Exhausted` action because it has no `RecoveryPolicy` in scope; the
/// daemon overrides via [`classify_exhaustion`] using the configured
/// policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExhaustedNext {
    /// Apply rollback before continuing. `to_attempt` selects which
    /// attempt directory's `checkpoint.json` to restore; `then` is the
    /// recovery policy to apply after the rollback completes.
    Rollback {
        /// 1-indexed attempt whose checkpoint should be restored.
        to_attempt: u8,
        /// Recovery policy applied after the rollback.
        then: RecoveryPolicy,
    },
    /// No applicable rollback configured; fail the step with
    /// `AttemptOutcome::ProtocolViolation`.
    Fail,
}

/// What the daemon should do after observing a turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisorAction {
    /// No violations detected; let the orchestrator continue.
    Continue,
    /// One or more violations detected and the retry budget has not been
    /// exhausted. The daemon should kill the current claude session and
    /// re-spawn with the supplied corrective prompt as the resume input.
    /// `attempt` is the new attempt number (1-indexed) to record under
    /// the step's `attempts/<attempt>/` directory.
    Recover {
        /// The corrective re-prompt template targeting `violation`.
        corrective: &'static str,
        /// 1-indexed attempt number for the step's `attempts/<attempt>/` dir.
        attempt: u8,
        /// The first violation observed on this turn.
        violation: ProtocolViolation,
    },
    /// Retry budget exhausted. The daemon should consult `next_step` to
    /// decide whether to roll back or fail the step.
    Exhausted {
        /// The first violation observed on the exhausting turn.
        last_violation: ProtocolViolation,
        /// What the daemon should do next: roll back or fail.
        next_step: ExhaustedNext,
    },
}

/// Run the detector on a single parsed stream-json turn and advance the
/// supervisor state. Returns the action the daemon should take.
///
/// If multiple violations are detected in the same turn, the FIRST one is
/// the one the corrective targets; the rest are recorded in `history` but
/// do not produce additional `Recover` actions (one corrective per turn
/// keeps the re-prompt loop simple).
pub fn observe_turn(turn: &Value, state: &mut SupervisorState) -> SupervisorAction {
    let violations = detect(turn);
    if violations.is_empty() {
        return SupervisorAction::Continue;
    }
    let first = violations
        .first()
        .expect("non-empty (just checked)")
        .clone();

    let recording_attempt = state.attempts_used.saturating_add(1);
    for v in violations {
        state.history.push(ObservedViolation {
            attempt: recording_attempt,
            violation: v,
        });
    }

    if state.attempts_used >= state.max_attempts {
        return SupervisorAction::Exhausted {
            last_violation: first,
            next_step: ExhaustedNext::Fail,
        };
    }
    state.attempts_used = recording_attempt;
    let corrective = corrective_for(&first);
    SupervisorAction::Recover {
        corrective,
        attempt: state.attempts_used,
        violation: first,
    }
}

/// Decides the daemon's next step on retry-budget exhaustion given the
/// configured `RecoveryPolicy`.
///
/// Returns [`ExhaustedNext::Rollback`] when the policy is a
/// `RecoveryPolicy::Rollback` whose target resolves for the current
/// attempt; otherwise [`ExhaustedNext::Fail`].
#[must_use]
pub fn classify_exhaustion(
    state: &SupervisorState,
    policy: &RecoveryPolicy,
    _last_violation: &ProtocolViolation,
) -> ExhaustedNext {
    let Some(attempt) = state.previous_attempt() else {
        return ExhaustedNext::Fail;
    };
    match crate::supervisor::rollback::resolve_rollback_target(policy, attempt) {
        Some(target) => ExhaustedNext::Rollback {
            to_attempt: target.attempt,
            then: target.then,
        },
        None => ExhaustedNext::Fail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn clean_turn() -> Value {
        json!({
            "message": {
                "content": [
                    { "type": "text", "text": "I will dispatch sub-agents now." },
                    { "type": "text", "text": "call sub-agent 1 (agent-type: implementer): /tmp/p1" }
                ]
            }
        })
    }

    fn schedule_wakeup_turn() -> Value {
        json!({
            "message": {
                "content": [
                    { "type": "tool_use", "name": "ScheduleWakeup", "input": {"delaySeconds": 60} }
                ]
            }
        })
    }

    fn forbidden_tool_turn() -> Value {
        json!({
            "message": {
                "content": [
                    { "type": "tool_use", "name": "Agent", "input": {} }
                ]
            }
        })
    }

    #[test]
    fn clean_turn_returns_continue_and_does_not_advance_state() {
        let mut s = SupervisorState::new(3);
        let action = observe_turn(&clean_turn(), &mut s);
        assert_eq!(action, SupervisorAction::Continue);
        assert_eq!(s.attempts_used, 0);
        assert_eq!(s.history.len(), 0);
        assert!(!s.is_exhausted());
    }

    #[test]
    fn first_violation_recovers_and_advances_attempt() {
        let mut s = SupervisorState::new(3);
        let action = observe_turn(&schedule_wakeup_turn(), &mut s);
        match action {
            SupervisorAction::Recover {
                corrective,
                attempt,
                violation,
            } => {
                assert!(corrective.starts_with("[PROTOCOL VIOLATION DETECTED]"));
                assert_eq!(attempt, 1);
                assert_eq!(violation, ProtocolViolation::ScheduleWakeupInNonInteractive);
            }
            _ => panic!("expected Recover, got {action:?}"),
        }
        assert_eq!(s.attempts_used, 1);
        assert_eq!(s.history.len(), 1);
        assert_eq!(s.history[0].attempt, 1);
    }

    #[test]
    fn budget_exhaustion_returns_exhausted_without_advancing_attempt() {
        let mut s = SupervisorState::new(2);
        let _ = observe_turn(&schedule_wakeup_turn(), &mut s);
        let _ = observe_turn(&schedule_wakeup_turn(), &mut s);
        assert!(s.is_exhausted());
        let action = observe_turn(&schedule_wakeup_turn(), &mut s);
        match action {
            SupervisorAction::Exhausted {
                last_violation,
                next_step,
            } => {
                assert_eq!(
                    last_violation,
                    ProtocolViolation::ScheduleWakeupInNonInteractive
                );
                assert_eq!(next_step, ExhaustedNext::Fail);
            }
            _ => panic!("expected Exhausted, got {action:?}"),
        }
        assert_eq!(s.attempts_used, 2);
        assert_eq!(s.history.len(), 3);
    }

    #[test]
    fn multiple_violations_in_one_turn_record_all_but_recover_targets_first() {
        let mut s = SupervisorState::new(3);
        let combined = json!({
            "message": {
                "content": [
                    { "type": "tool_use", "name": "Agent", "input": {} },
                    { "type": "tool_use", "name": "ScheduleWakeup", "input": {"delaySeconds": 60} }
                ]
            }
        });
        let action = observe_turn(&combined, &mut s);
        match action {
            SupervisorAction::Recover {
                violation, attempt, ..
            } => {
                assert!(matches!(violation, ProtocolViolation::ForbiddenTool { .. }));
                assert_eq!(attempt, 1);
            }
            _ => panic!("expected Recover, got {action:?}"),
        }
        assert_eq!(s.history.len(), 2);
        assert!(matches!(
            s.history[0].violation,
            ProtocolViolation::ForbiddenTool { .. }
        ));
        assert_eq!(
            s.history[1].violation,
            ProtocolViolation::ScheduleWakeupInNonInteractive
        );
    }

    #[test]
    fn supervisor_state_serde_roundtrip() {
        let mut s = SupervisorState::new(3);
        let _ = observe_turn(&forbidden_tool_turn(), &mut s);
        let json = serde_json::to_string(&s).expect("serialize");
        let back: SupervisorState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, s);
    }

    #[test]
    fn new_state_is_not_exhausted() {
        let s = SupervisorState::new(1);
        assert!(!s.is_exhausted());
    }

    #[test]
    fn zero_max_attempts_state_is_immediately_exhausted() {
        let s = SupervisorState::new(0);
        assert!(s.is_exhausted());
    }

    #[test]
    fn observe_turn_with_zero_max_attempts_returns_exhausted_immediately() {
        let mut s = SupervisorState::new(0);
        let action = observe_turn(&forbidden_tool_turn(), &mut s);
        match action {
            SupervisorAction::Exhausted {
                next_step: ExhaustedNext::Fail,
                ..
            } => {}
            other => panic!("expected Exhausted/Fail, got {other:?}"),
        }
        assert_eq!(s.attempts_used, 0);
    }

    #[test]
    fn previous_attempt_is_none_for_fresh_state() {
        let s = SupervisorState::new(3);
        assert!(s.previous_attempt().is_none());
    }

    #[test]
    fn previous_attempt_returns_attempts_used_after_recover() {
        let mut s = SupervisorState::new(3);
        let _ = observe_turn(&schedule_wakeup_turn(), &mut s);
        assert_eq!(s.previous_attempt(), Some(1));
    }

    #[test]
    fn classify_exhaustion_returns_fail_for_fresh_state() {
        let s = SupervisorState::new(3);
        let next = classify_exhaustion(
            &s,
            &RecoveryPolicy::None,
            &ProtocolViolation::ScheduleWakeupInNonInteractive,
        );
        assert_eq!(next, ExhaustedNext::Fail);
    }

    #[test]
    fn classify_exhaustion_returns_fail_when_policy_is_not_rollback() {
        let mut s = SupervisorState::new(3);
        let _ = observe_turn(&schedule_wakeup_turn(), &mut s);
        let next = classify_exhaustion(
            &s,
            &RecoveryPolicy::None,
            &ProtocolViolation::ScheduleWakeupInNonInteractive,
        );
        assert_eq!(next, ExhaustedNext::Fail);
    }

    #[test]
    fn classify_exhaustion_returns_rollback_for_applicable_policy() {
        use crate::job::recovery::CheckpointTarget;
        let mut s = SupervisorState::new(3);
        let _ = observe_turn(&schedule_wakeup_turn(), &mut s);
        let _ = observe_turn(&schedule_wakeup_turn(), &mut s);
        let policy = RecoveryPolicy::Rollback {
            to: CheckpointTarget::PreviousAttempt,
            then: Box::new(RecoveryPolicy::None),
        };
        let next = classify_exhaustion(
            &s,
            &policy,
            &ProtocolViolation::ScheduleWakeupInNonInteractive,
        );
        assert_eq!(
            next,
            ExhaustedNext::Rollback {
                to_attempt: 1,
                then: RecoveryPolicy::None,
            }
        );
    }

    #[test]
    fn classify_exhaustion_returns_fail_for_rollback_when_no_previous_attempt() {
        use crate::job::recovery::CheckpointTarget;
        let mut s = SupervisorState::new(3);
        let _ = observe_turn(&schedule_wakeup_turn(), &mut s);
        let policy = RecoveryPolicy::Rollback {
            to: CheckpointTarget::PreviousAttempt,
            then: Box::new(RecoveryPolicy::None),
        };
        let next = classify_exhaustion(
            &s,
            &policy,
            &ProtocolViolation::ScheduleWakeupInNonInteractive,
        );
        assert_eq!(next, ExhaustedNext::Fail);
    }
}
