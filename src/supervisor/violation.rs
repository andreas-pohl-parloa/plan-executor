//! Categorized LLM-protocol violations recognised by the supervisor.
//!
//! Each variant carries the minimal context needed to render a corrective
//! prompt in `crate::supervisor::prompts`. Some variants are emitted by the
//! pure turn detector (`crate::supervisor::detector::detect`); others require
//! state-file or phase context produced by an out-of-turn inspector that
//! lives in B2.1.

use serde::{Deserialize, Serialize};

/// Categorized LLM-protocol violations the supervisor knows how to recover
/// from. Detected from stream-json turns; recovered via the corresponding
/// corrective template under `src/supervisor/prompts/`.
///
/// Phase B1.1: detection only; Phase B2.1 wires this into the daemon's
/// re-prompt loop.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ProtocolViolation {
    /// `.tmp-execute-plan-state.json` exists but `handoffs` is an empty array.
    HandoffsArrayEmpty { state_path: String },
    /// `.tmp-execute-plan-state.json` exists but `handoffs` is missing or null.
    HandoffsArrayMissing { state_path: String },
    /// A `tool_use` block appears in the assistant turn AFTER the last
    /// `call sub-agent` line — orchestrator was supposed to end the turn.
    PostHandoffToolUse {
        tool_name: String,
        after_handoff_index: u32,
    },
    /// The assistant called a tool not on the allowlist for non-interactive
    /// orchestrator runs (e.g., `Agent`, `Task`, `WebFetch`).
    ForbiddenTool { tool_name: String, context: String },
    /// `.tmp-execute-plan-state.json` exists but cannot be parsed as JSON
    /// or does not match the expected shape.
    StateFileMalformed { path: String, error: String },
    /// Free-form narration (non-empty `text` block) appears AFTER a
    /// `call sub-agent` line in the same turn.
    ///
    /// `byte_offset` is the byte offset within the *text block* that contains
    /// the trailing narration (NOT a turn-global offset). Phase D consumers
    /// should treat it as a hint for log rendering, not as an absolute pointer.
    DanglingNarration { sample: String, byte_offset: usize },
    /// `ScheduleWakeup` was invoked in a non-interactive run, where it is
    /// unsupported.
    ScheduleWakeupInNonInteractive,
    /// The assistant invoked a Skill outside the orchestrator's allowed
    /// boundary (e.g., calling a reviewer skill while still in Phase 3).
    SkillBoundaryCrossed {
        from_skill: String,
        to_skill: String,
        expected_phase: String,
    },
    /// The orchestrator emitted a tight poll loop (e.g., `until ... sleep`)
    /// without an upper bound — would burn the cache and run the wallet.
    UnboundedPollEmitted { command_excerpt: String },
}

impl ProtocolViolation {
    /// Stable key used by `corrective_for` and `RecoveryPolicy::RetryProtocol`.
    /// Matches the file stem under `src/supervisor/prompts/`.
    #[must_use]
    pub fn template_key(&self) -> &'static str {
        match self {
            ProtocolViolation::HandoffsArrayEmpty { .. }
            | ProtocolViolation::HandoffsArrayMissing { .. } => "handoffs_missing",
            ProtocolViolation::PostHandoffToolUse { .. } => "post_handoff_tool_use",
            ProtocolViolation::ForbiddenTool { .. } => "forbidden_tool",
            ProtocolViolation::StateFileMalformed { .. } => "state_malformed",
            ProtocolViolation::DanglingNarration { .. } => "dangling_narration",
            ProtocolViolation::ScheduleWakeupInNonInteractive => "schedule_wakeup",
            ProtocolViolation::SkillBoundaryCrossed { .. } => "skill_boundary",
            ProtocolViolation::UnboundedPollEmitted { .. } => "unbounded_poll",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn all_variants() -> Vec<ProtocolViolation> {
        vec![
            ProtocolViolation::HandoffsArrayEmpty {
                state_path: ".tmp-execute-plan-state.json".to_string(),
            },
            ProtocolViolation::HandoffsArrayMissing {
                state_path: ".tmp-execute-plan-state.json".to_string(),
            },
            ProtocolViolation::PostHandoffToolUse {
                tool_name: "Bash".to_string(),
                after_handoff_index: 4,
            },
            ProtocolViolation::ForbiddenTool {
                tool_name: "Agent".to_string(),
                context: "orchestrator non-interactive turn".to_string(),
            },
            ProtocolViolation::StateFileMalformed {
                path: ".tmp-execute-plan-state.json".to_string(),
                error: "expected object, got array".to_string(),
            },
            ProtocolViolation::DanglingNarration {
                sample: "and now I will reflect...".to_string(),
                byte_offset: 142,
            },
            ProtocolViolation::ScheduleWakeupInNonInteractive,
            ProtocolViolation::SkillBoundaryCrossed {
                from_skill: "executor".to_string(),
                to_skill: "reviewer".to_string(),
                expected_phase: "phase-3".to_string(),
            },
            ProtocolViolation::UnboundedPollEmitted {
                command_excerpt: "until pgrep foo; do sleep 1; done".to_string(),
            },
        ]
    }

    #[test]
    fn template_key_handoffs_array_empty_is_handoffs_missing() {
        let v = ProtocolViolation::HandoffsArrayEmpty {
            state_path: "p".to_string(),
        };
        assert_eq!(v.template_key(), "handoffs_missing");
    }

    #[test]
    fn template_key_handoffs_array_missing_is_handoffs_missing() {
        let v = ProtocolViolation::HandoffsArrayMissing {
            state_path: "p".to_string(),
        };
        assert_eq!(v.template_key(), "handoffs_missing");
    }

    #[test]
    fn template_key_post_handoff_tool_use_is_post_handoff_tool_use() {
        let v = ProtocolViolation::PostHandoffToolUse {
            tool_name: "Bash".to_string(),
            after_handoff_index: 0,
        };
        assert_eq!(v.template_key(), "post_handoff_tool_use");
    }

    #[test]
    fn template_key_forbidden_tool_is_forbidden_tool() {
        let v = ProtocolViolation::ForbiddenTool {
            tool_name: "Agent".to_string(),
            context: "ctx".to_string(),
        };
        assert_eq!(v.template_key(), "forbidden_tool");
    }

    #[test]
    fn template_key_state_malformed_is_state_malformed() {
        let v = ProtocolViolation::StateFileMalformed {
            path: "p".to_string(),
            error: "e".to_string(),
        };
        assert_eq!(v.template_key(), "state_malformed");
    }

    #[test]
    fn template_key_dangling_narration_is_dangling_narration() {
        let v = ProtocolViolation::DanglingNarration {
            sample: "s".to_string(),
            byte_offset: 0,
        };
        assert_eq!(v.template_key(), "dangling_narration");
    }

    #[test]
    fn template_key_schedule_wakeup_is_schedule_wakeup() {
        let v = ProtocolViolation::ScheduleWakeupInNonInteractive;
        assert_eq!(v.template_key(), "schedule_wakeup");
    }

    #[test]
    fn template_key_skill_boundary_is_skill_boundary() {
        let v = ProtocolViolation::SkillBoundaryCrossed {
            from_skill: "a".to_string(),
            to_skill: "b".to_string(),
            expected_phase: "p".to_string(),
        };
        assert_eq!(v.template_key(), "skill_boundary");
    }

    #[test]
    fn template_key_unbounded_poll_is_unbounded_poll() {
        let v = ProtocolViolation::UnboundedPollEmitted {
            command_excerpt: "x".to_string(),
        };
        assert_eq!(v.template_key(), "unbounded_poll");
    }

    #[test]
    fn template_key_all_variants_return_non_empty_static_str() {
        let all_non_empty = all_variants().iter().all(|v| !v.template_key().is_empty());
        assert_eq!(all_non_empty, true);
    }

    #[test]
    fn template_key_nine_variants_fold_to_eight_distinct_keys() {
        let variants = all_variants();
        assert_eq!(variants.len(), 9);
        let keys: HashSet<&'static str> = variants
            .iter()
            .map(ProtocolViolation::template_key)
            .collect();
        assert_eq!(keys.len(), 8);
    }

    #[test]
    fn handoffs_empty_and_missing_share_template_key() {
        let a = ProtocolViolation::HandoffsArrayEmpty {
            state_path: "p".to_string(),
        };
        let b = ProtocolViolation::HandoffsArrayMissing {
            state_path: "p".to_string(),
        };
        assert_eq!(a.template_key(), b.template_key());
    }

    #[test]
    fn serde_roundtrip_handoffs_array_empty() {
        let v = ProtocolViolation::HandoffsArrayEmpty {
            state_path: ".tmp-execute-plan-state.json".to_string(),
        };
        let json = serde_json::to_string(&v).expect("serialize");
        let back: ProtocolViolation = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, v);
    }

    #[test]
    fn serde_roundtrip_handoffs_array_missing() {
        let v = ProtocolViolation::HandoffsArrayMissing {
            state_path: ".tmp-execute-plan-state.json".to_string(),
        };
        let json = serde_json::to_string(&v).expect("serialize");
        let back: ProtocolViolation = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, v);
    }

    #[test]
    fn serde_roundtrip_post_handoff_tool_use() {
        let v = ProtocolViolation::PostHandoffToolUse {
            tool_name: "Bash".to_string(),
            after_handoff_index: 7,
        };
        let json = serde_json::to_string(&v).expect("serialize");
        let back: ProtocolViolation = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, v);
    }

    #[test]
    fn serde_roundtrip_forbidden_tool() {
        let v = ProtocolViolation::ForbiddenTool {
            tool_name: "Agent".to_string(),
            context: "ctx".to_string(),
        };
        let json = serde_json::to_string(&v).expect("serialize");
        let back: ProtocolViolation = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, v);
    }

    #[test]
    fn serde_roundtrip_state_file_malformed() {
        let v = ProtocolViolation::StateFileMalformed {
            path: ".tmp-execute-plan-state.json".to_string(),
            error: "expected object".to_string(),
        };
        let json = serde_json::to_string(&v).expect("serialize");
        let back: ProtocolViolation = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, v);
    }

    #[test]
    fn serde_roundtrip_dangling_narration() {
        let v = ProtocolViolation::DanglingNarration {
            sample: "trailing words".to_string(),
            byte_offset: 256,
        };
        let json = serde_json::to_string(&v).expect("serialize");
        let back: ProtocolViolation = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, v);
    }

    #[test]
    fn serde_roundtrip_schedule_wakeup_in_non_interactive() {
        let v = ProtocolViolation::ScheduleWakeupInNonInteractive;
        let json = serde_json::to_string(&v).expect("serialize");
        let back: ProtocolViolation = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, v);
    }

    #[test]
    fn serde_roundtrip_skill_boundary_crossed() {
        let v = ProtocolViolation::SkillBoundaryCrossed {
            from_skill: "executor".to_string(),
            to_skill: "reviewer".to_string(),
            expected_phase: "phase-3".to_string(),
        };
        let json = serde_json::to_string(&v).expect("serialize");
        let back: ProtocolViolation = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, v);
    }

    #[test]
    fn serde_roundtrip_unbounded_poll_emitted() {
        let v = ProtocolViolation::UnboundedPollEmitted {
            command_excerpt: "while true; do sleep 1; done".to_string(),
        };
        let json = serde_json::to_string(&v).expect("serialize");
        let back: ProtocolViolation = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, v);
    }
}
