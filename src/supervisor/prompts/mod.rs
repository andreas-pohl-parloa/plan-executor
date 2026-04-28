//! Corrective-prompt template catalog.
//!
//! Each violation maps to one corrective markdown template, loaded at
//! compile time via `include_str!`. The `Handoffs*` variants share a single
//! template (`handoffs_missing.md`) — 9 variants → 8 templates.

use crate::supervisor::violation::ProtocolViolation;

pub const HANDOFFS_MISSING: &str = include_str!("handoffs_missing.md");
pub const POST_HANDOFF_TOOL_USE: &str = include_str!("post_handoff_tool_use.md");
pub const FORBIDDEN_TOOL: &str = include_str!("forbidden_tool.md");
pub const STATE_MALFORMED: &str = include_str!("state_malformed.md");
pub const DANGLING_NARRATION: &str = include_str!("dangling_narration.md");
pub const SCHEDULE_WAKEUP: &str = include_str!("schedule_wakeup.md");
pub const SKILL_BOUNDARY: &str = include_str!("skill_boundary.md");
pub const UNBOUNDED_POLL: &str = include_str!("unbounded_poll.md");

/// Returns the corrective template for the given violation. Two
/// `Handoffs*` variants share `HANDOFFS_MISSING`.
#[must_use]
pub fn corrective_for(v: &ProtocolViolation) -> &'static str {
    match v {
        ProtocolViolation::HandoffsArrayEmpty { .. }
        | ProtocolViolation::HandoffsArrayMissing { .. } => HANDOFFS_MISSING,
        ProtocolViolation::PostHandoffToolUse { .. } => POST_HANDOFF_TOOL_USE,
        ProtocolViolation::ForbiddenTool { .. } => FORBIDDEN_TOOL,
        ProtocolViolation::StateFileMalformed { .. } => STATE_MALFORMED,
        ProtocolViolation::DanglingNarration { .. } => DANGLING_NARRATION,
        ProtocolViolation::ScheduleWakeupInNonInteractive => SCHEDULE_WAKEUP,
        ProtocolViolation::SkillBoundaryCrossed { .. } => SKILL_BOUNDARY,
        ProtocolViolation::UnboundedPollEmitted { .. } => UNBOUNDED_POLL,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MARKER: &str = "[PROTOCOL VIOLATION DETECTED]";

    fn all_templates() -> [(&'static str, &'static str); 8] {
        [
            ("handoffs_missing", HANDOFFS_MISSING),
            ("post_handoff_tool_use", POST_HANDOFF_TOOL_USE),
            ("forbidden_tool", FORBIDDEN_TOOL),
            ("state_malformed", STATE_MALFORMED),
            ("dangling_narration", DANGLING_NARRATION),
            ("schedule_wakeup", SCHEDULE_WAKEUP),
            ("skill_boundary", SKILL_BOUNDARY),
            ("unbounded_poll", UNBOUNDED_POLL),
        ]
    }

    fn all_variants() -> Vec<ProtocolViolation> {
        vec![
            ProtocolViolation::HandoffsArrayEmpty {
                state_path: "p".to_string(),
            },
            ProtocolViolation::HandoffsArrayMissing {
                state_path: "p".to_string(),
            },
            ProtocolViolation::PostHandoffToolUse {
                tool_name: "Bash".to_string(),
                after_handoff_index: 0,
            },
            ProtocolViolation::ForbiddenTool {
                tool_name: "Agent".to_string(),
                context: "ctx".to_string(),
            },
            ProtocolViolation::StateFileMalformed {
                path: "p".to_string(),
                error: "e".to_string(),
            },
            ProtocolViolation::DanglingNarration {
                sample: "s".to_string(),
                byte_offset: 0,
            },
            ProtocolViolation::ScheduleWakeupInNonInteractive,
            ProtocolViolation::SkillBoundaryCrossed {
                from_skill: "a".to_string(),
                to_skill: "b".to_string(),
                expected_phase: "p".to_string(),
            },
            ProtocolViolation::UnboundedPollEmitted {
                command_excerpt: "x".to_string(),
            },
        ]
    }

    #[test]
    fn handoffs_missing_starts_with_marker() {
        assert_eq!(HANDOFFS_MISSING.starts_with(MARKER), true);
    }

    #[test]
    fn post_handoff_tool_use_starts_with_marker() {
        assert_eq!(POST_HANDOFF_TOOL_USE.starts_with(MARKER), true);
    }

    #[test]
    fn forbidden_tool_starts_with_marker() {
        assert_eq!(FORBIDDEN_TOOL.starts_with(MARKER), true);
    }

    #[test]
    fn state_malformed_starts_with_marker() {
        assert_eq!(STATE_MALFORMED.starts_with(MARKER), true);
    }

    #[test]
    fn dangling_narration_starts_with_marker() {
        assert_eq!(DANGLING_NARRATION.starts_with(MARKER), true);
    }

    #[test]
    fn schedule_wakeup_starts_with_marker() {
        assert_eq!(SCHEDULE_WAKEUP.starts_with(MARKER), true);
    }

    #[test]
    fn skill_boundary_starts_with_marker() {
        assert_eq!(SKILL_BOUNDARY.starts_with(MARKER), true);
    }

    #[test]
    fn unbounded_poll_starts_with_marker() {
        assert_eq!(UNBOUNDED_POLL.starts_with(MARKER), true);
    }

    #[test]
    fn all_templates_are_at_least_200_bytes() {
        let all_long_enough = all_templates().iter().all(|(_, body)| body.len() >= 200);
        assert_eq!(all_long_enough, true);
    }

    #[test]
    fn all_templates_start_with_marker() {
        let all_marked = all_templates()
            .iter()
            .all(|(_, body)| body.starts_with(MARKER));
        assert_eq!(all_marked, true);
    }

    #[test]
    fn corrective_for_handoffs_array_empty_returns_handoffs_missing() {
        let v = ProtocolViolation::HandoffsArrayEmpty {
            state_path: "p".to_string(),
        };
        assert_eq!(corrective_for(&v), HANDOFFS_MISSING);
    }

    #[test]
    fn corrective_for_handoffs_array_missing_returns_handoffs_missing() {
        let v = ProtocolViolation::HandoffsArrayMissing {
            state_path: "p".to_string(),
        };
        assert_eq!(corrective_for(&v), HANDOFFS_MISSING);
    }

    #[test]
    fn corrective_for_post_handoff_tool_use_returns_post_handoff_tool_use() {
        let v = ProtocolViolation::PostHandoffToolUse {
            tool_name: "Bash".to_string(),
            after_handoff_index: 0,
        };
        assert_eq!(corrective_for(&v), POST_HANDOFF_TOOL_USE);
    }

    #[test]
    fn corrective_for_forbidden_tool_returns_forbidden_tool() {
        let v = ProtocolViolation::ForbiddenTool {
            tool_name: "Agent".to_string(),
            context: "ctx".to_string(),
        };
        assert_eq!(corrective_for(&v), FORBIDDEN_TOOL);
    }

    #[test]
    fn corrective_for_state_malformed_returns_state_malformed() {
        let v = ProtocolViolation::StateFileMalformed {
            path: "p".to_string(),
            error: "e".to_string(),
        };
        assert_eq!(corrective_for(&v), STATE_MALFORMED);
    }

    #[test]
    fn corrective_for_dangling_narration_returns_dangling_narration() {
        let v = ProtocolViolation::DanglingNarration {
            sample: "s".to_string(),
            byte_offset: 0,
        };
        assert_eq!(corrective_for(&v), DANGLING_NARRATION);
    }

    #[test]
    fn corrective_for_schedule_wakeup_returns_schedule_wakeup() {
        let v = ProtocolViolation::ScheduleWakeupInNonInteractive;
        assert_eq!(corrective_for(&v), SCHEDULE_WAKEUP);
    }

    #[test]
    fn corrective_for_skill_boundary_returns_skill_boundary() {
        let v = ProtocolViolation::SkillBoundaryCrossed {
            from_skill: "a".to_string(),
            to_skill: "b".to_string(),
            expected_phase: "p".to_string(),
        };
        assert_eq!(corrective_for(&v), SKILL_BOUNDARY);
    }

    #[test]
    fn corrective_for_unbounded_poll_returns_unbounded_poll() {
        let v = ProtocolViolation::UnboundedPollEmitted {
            command_excerpt: "x".to_string(),
        };
        assert_eq!(corrective_for(&v), UNBOUNDED_POLL);
    }

    #[test]
    fn corrective_for_all_variants_return_non_empty_templates() {
        let all_non_empty = all_variants().iter().all(|v| !corrective_for(v).is_empty());
        assert_eq!(all_non_empty, true);
    }
}
