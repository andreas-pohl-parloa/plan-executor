use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecoveryPolicy {
    None,
    RetryTransient {
        max: u8,
        backoff: Backoff,
    },
    RetryProtocol {
        max: u8,
        corrective: CorrectivePromptKey,
    },
    Rollback {
        to: CheckpointTarget,
        then: Box<RecoveryPolicy>,
    },
    Compose {
        policies: Vec<RecoveryPolicy>,
    },
    /// Cap reached; require operator decision before continuing.
    OperatorDecision {
        decision_key: String,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Backoff {
    Fixed {
        ms: u64,
    },
    Exponential {
        initial_ms: u64,
        max_ms: u64,
        factor: f32,
    },
}

// Manual Eq because f32 does not implement Eq. Acceptable: we never compare
// Backoff for equality in production code; tests use specific factor values.
impl Eq for Backoff {}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointTarget {
    PreviousAttempt,
    PreviousStep,
    PreviousPhase,
    Named(String),
}

/// Identifier for a corrective-prompt template stored in the protocol-recovery
/// catalog. The actual prompt text lives in src/supervisor/prompts/<key>.md and
/// is loaded via include_str! at compile time (Phase B1.2).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct CorrectivePromptKey(pub String);

impl Backoff {
    pub fn delay(&self, attempt: u32) -> Duration {
        match self {
            Backoff::Fixed { ms } => Duration::from_millis(*ms),
            Backoff::Exponential {
                initial_ms,
                max_ms,
                factor,
            } => {
                let raw = (*initial_ms as f64) * (*factor as f64).powi(attempt as i32 - 1);
                Duration::from_millis((raw as u64).min(*max_ms))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn backoff_fixed_returns_constant_delay() {
        let b = Backoff::Fixed { ms: 100 };
        assert_eq!(b.delay(1), Duration::from_millis(100));
    }

    #[test]
    fn backoff_fixed_ignores_attempt_count() {
        let b = Backoff::Fixed { ms: 100 };
        assert_eq!(b.delay(5), Duration::from_millis(100));
    }

    #[test]
    fn backoff_exponential_attempt_one_returns_initial() {
        let b = Backoff::Exponential {
            initial_ms: 100,
            max_ms: 10_000,
            factor: 2.0,
        };
        assert_eq!(b.delay(1), Duration::from_millis(100));
    }

    #[test]
    fn backoff_exponential_attempt_two_doubles() {
        let b = Backoff::Exponential {
            initial_ms: 100,
            max_ms: 10_000,
            factor: 2.0,
        };
        assert_eq!(b.delay(2), Duration::from_millis(200));
    }

    #[test]
    fn backoff_exponential_attempt_three_quadruples() {
        let b = Backoff::Exponential {
            initial_ms: 100,
            max_ms: 10_000,
            factor: 2.0,
        };
        assert_eq!(b.delay(3), Duration::from_millis(400));
    }

    #[test]
    fn backoff_exponential_attempt_four() {
        let b = Backoff::Exponential {
            initial_ms: 100,
            max_ms: 10_000,
            factor: 2.0,
        };
        assert_eq!(b.delay(4), Duration::from_millis(800));
    }

    #[test]
    fn backoff_exponential_saturates_at_max() {
        let b = Backoff::Exponential {
            initial_ms: 100,
            max_ms: 500,
            factor: 2.0,
        };
        assert_eq!(b.delay(10), Duration::from_millis(500));
    }

    #[test]
    fn recovery_policy_none_serde_roundtrip() {
        let p = RecoveryPolicy::None;
        let json = serde_json::to_string(&p).expect("serialize");
        assert_eq!(json, r#"{"kind":"none"}"#);
        let back: RecoveryPolicy = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, p);
    }

    #[test]
    fn recovery_policy_retry_transient_with_fixed_backoff_roundtrip() {
        let p = RecoveryPolicy::RetryTransient {
            max: 3,
            backoff: Backoff::Fixed { ms: 250 },
        };
        let json = serde_json::to_string(&p).expect("serialize");
        let back: RecoveryPolicy = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, p);
    }

    #[test]
    fn recovery_policy_retry_protocol_roundtrip() {
        let p = RecoveryPolicy::RetryProtocol {
            max: 2,
            corrective: CorrectivePromptKey("missing_handoffs".to_string()),
        };
        let json = serde_json::to_string(&p).expect("serialize");
        let back: RecoveryPolicy = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, p);
    }

    #[test]
    fn recovery_policy_rollback_with_retry_protocol_roundtrip() {
        let p = RecoveryPolicy::Rollback {
            to: CheckpointTarget::PreviousAttempt,
            then: Box::new(RecoveryPolicy::RetryProtocol {
                max: 1,
                corrective: CorrectivePromptKey("retry_protocol".to_string()),
            }),
        };
        let json = serde_json::to_string(&p).expect("serialize");
        let back: RecoveryPolicy = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, p);
    }

    #[test]
    fn recovery_policy_rollback_with_compose_roundtrip() {
        let p = RecoveryPolicy::Rollback {
            to: CheckpointTarget::PreviousStep,
            then: Box::new(RecoveryPolicy::Compose {
                policies: vec![
                    RecoveryPolicy::None,
                    RecoveryPolicy::RetryTransient {
                        max: 2,
                        backoff: Backoff::Fixed { ms: 50 },
                    },
                ],
            }),
        };
        let json = serde_json::to_string(&p).expect("serialize");
        let back: RecoveryPolicy = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, p);
    }

    #[test]
    fn recovery_policy_compose_with_mixed_children_roundtrip() {
        let p = RecoveryPolicy::Compose {
            policies: vec![
                RecoveryPolicy::RetryTransient {
                    max: 3,
                    backoff: Backoff::Exponential {
                        initial_ms: 100,
                        max_ms: 1000,
                        factor: 2.0,
                    },
                },
                RecoveryPolicy::RetryProtocol {
                    max: 1,
                    corrective: CorrectivePromptKey("fix_format".to_string()),
                },
                RecoveryPolicy::OperatorDecision {
                    decision_key: "manual".to_string(),
                },
            ],
        };
        let json = serde_json::to_string(&p).expect("serialize");
        let back: RecoveryPolicy = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, p);
    }

    #[test]
    fn recovery_policy_operator_decision_roundtrip() {
        let p = RecoveryPolicy::OperatorDecision {
            decision_key: "phase6_cap".to_string(),
        };
        let json = serde_json::to_string(&p).expect("serialize");
        let back: RecoveryPolicy = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, p);
    }

    #[test]
    fn checkpoint_target_named_roundtrip() {
        let t = CheckpointTarget::Named("foo".to_string());
        let json = serde_json::to_string(&t).expect("serialize");
        assert_eq!(json, r#"{"named":"foo"}"#);
        let back: CheckpointTarget = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, t);
    }

    #[test]
    fn corrective_prompt_key_equality_and_hashing() {
        let k1 = CorrectivePromptKey("missing_handoffs".to_string());
        let k2 = CorrectivePromptKey("missing_handoffs".to_string());
        assert_eq!(k1, k2);
        let mut set: HashSet<CorrectivePromptKey> = HashSet::new();
        set.insert(k1);
        let inserted_again = set.insert(k2);
        assert_eq!(inserted_again, false);
    }
}
