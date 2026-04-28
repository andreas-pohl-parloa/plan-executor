//! Pure stream-json turn detector.
//!
//! Given a single parsed assistant turn (the JSON object the daemon's
//! stream-json parser already deserializes), returns the list of violations
//! observed in that turn. Empty Vec means the turn looks clean.
//!
//! Phase B1.1 is conservative: detect only the patterns documented below.
//! Phase F mines production logs to expand coverage.
//!
//! # Variants NOT emitted by `detect`
//!
//! Some `ProtocolViolation` variants require knowledge the detector cannot
//! derive from a single assistant turn. These are produced by the state-file
//! and phase inspectors that live in B2.1; the variants exist here so B2.1
//! can construct them, but `detect` never emits them:
//!
//! - `HandoffsArrayEmpty` / `HandoffsArrayMissing` — require reading
//!   `.tmp-execute-plan-state.json`.
//! - `StateFileMalformed` — same.
//! - `SkillBoundaryCrossed` — requires the orchestrator's current phase.
//!
//! # The five turn-derivable detectors
//!
//! - `detect_forbidden_tool` — flags tool_use blocks for the non-interactive
//!   forbidden list (`Agent`, `Task`, `WebFetch`, `WebSearch`).
//! - `detect_schedule_wakeup` — flags `ScheduleWakeup` invocations.
//! - `detect_post_handoff_tool_use` — flags any `tool_use` block emitted
//!   AFTER the last `call sub-agent` text block in the same turn.
//! - `detect_dangling_narration` — flags trailing prose in the same text
//!   block, after the final `call sub-agent` line.
//! - `detect_unbounded_poll` — flags Bash commands containing
//!   `while`/`until` + `sleep` with no obvious break/exit/`&&`.

use serde_json::Value;

use crate::supervisor::violation::ProtocolViolation;

/// Pure detector: given a single parsed stream-json turn, returns the list
/// of violations observed in that turn. Empty Vec means the turn looks
/// clean to the supervisor.
///
/// **Detection order** (the order matters because `observe_turn` recovers
/// using the FIRST returned violation, even when several are detected in
/// the same turn):
///
/// 1. `ForbiddenTool` — most actionable correction; the orchestrator must
///    redo the work via the legal tool surface.
/// 2. `ScheduleWakeupInNonInteractive` — easy to detect, surface early so
///    the orchestrator stops scheduling a no-op.
/// 3. `PostHandoffToolUse` — turn-shape violation; the daemon will SIGKILL
///    the session if it repeats.
/// 4. `DanglingNarration` — same shape class but text-only.
/// 5. `UnboundedPollEmitted` — heuristic; lowest priority because false
///    positives are non-zero (see `detect_unbounded_poll` doc-comment).
///
/// Phase B1.1 is conservative: detect only the patterns documented below.
/// Phase F mines production logs to expand coverage.
///
/// `HandoffsArrayEmpty`, `HandoffsArrayMissing`, `StateFileMalformed`, and
/// `SkillBoundaryCrossed` require knowledge the detector doesn't have from
/// a single turn (state-file contents, phase context). These are produced
/// by a separate state-file inspector (Phase B2.1+ daemon-side wiring), not
/// by this function.
#[must_use]
pub fn detect(turn: &Value) -> Vec<ProtocolViolation> {
    let mut out = Vec::new();
    detect_forbidden_tool(turn, &mut out);
    detect_schedule_wakeup(turn, &mut out);
    detect_post_handoff_tool_use(turn, &mut out);
    detect_dangling_narration(turn, &mut out);
    detect_unbounded_poll(turn, &mut out);
    out
}

/// Forbidden in the orchestrator's non-interactive context. Real call sites
/// (sub-agents, etc.) are filtered upstream by the daemon's allowlist; the
/// detector trusts whatever it is given.
const FORBIDDEN_TOOLS: &[&str] = &["Agent", "Task", "WebFetch", "WebSearch"];

fn detect_forbidden_tool(turn: &Value, out: &mut Vec<ProtocolViolation>) {
    for tu in tool_uses(turn) {
        if let Some(name) = tu.get("name").and_then(Value::as_str) {
            if FORBIDDEN_TOOLS.contains(&name) {
                out.push(ProtocolViolation::ForbiddenTool {
                    tool_name: name.to_string(),
                    context: "orchestrator non-interactive turn".to_string(),
                });
            }
        }
    }
}

fn detect_schedule_wakeup(turn: &Value, out: &mut Vec<ProtocolViolation>) {
    for tu in tool_uses(turn) {
        if tu.get("name").and_then(Value::as_str) == Some("ScheduleWakeup") {
            out.push(ProtocolViolation::ScheduleWakeupInNonInteractive);
        }
    }
}

fn detect_post_handoff_tool_use(turn: &Value, out: &mut Vec<ProtocolViolation>) {
    let blocks: Vec<&Value> = content_blocks(turn).collect();
    let mut last_handoff_index: Option<usize> = None;
    for (idx, block) in blocks.iter().enumerate() {
        if let Some(text) = block.get("text").and_then(Value::as_str) {
            if text.contains("call sub-agent") {
                last_handoff_index = Some(idx);
            }
        }
    }
    let Some(handoff_idx) = last_handoff_index else {
        return;
    };
    for (idx, block) in blocks.iter().enumerate() {
        if idx <= handoff_idx {
            continue;
        }
        if block.get("type").and_then(Value::as_str) == Some("tool_use") {
            let tool_name = block
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("<unknown>")
                .to_string();
            out.push(ProtocolViolation::PostHandoffToolUse {
                tool_name,
                after_handoff_index: u32::try_from(handoff_idx).unwrap_or(u32::MAX),
            });
        }
    }
}

fn detect_dangling_narration(turn: &Value, out: &mut Vec<ProtocolViolation>) {
    let blocks: Vec<&Value> = content_blocks(turn).collect();
    let mut last_handoff_byte: Option<(usize, &str)> = None;
    for block in &blocks {
        if let Some(text) = block.get("text").and_then(Value::as_str) {
            if let Some(off) = text.rfind("call sub-agent") {
                last_handoff_byte = Some((off, text));
            }
        }
    }
    let Some((off, text)) = last_handoff_byte else {
        return;
    };
    let after = &text[off..];
    let line_end = after.find('\n').map_or(after.len(), |n| n + 1);
    let trailing = &after[line_end..];
    let trailing_trimmed = trailing.trim();
    if !trailing_trimmed.is_empty() {
        let sample: String = trailing_trimmed.chars().take(160).collect();
        out.push(ProtocolViolation::DanglingNarration {
            sample,
            byte_offset: off + line_end,
        });
    }
}

/// Heuristic detector for unbounded poll loops in Bash tool calls.
///
/// Flags Bash commands that contain `while`/`until` AND `sleep` AND no
/// obvious break condition (`break`, `exit`, `&&`).
///
/// **Known false positives** (will be calibrated in Phase F log mining):
///   * `while read line; do ...; sleep 0.1; done < input.txt` — bounded
///     by EOF on stdin; no break/exit/`&&` substring.
///   * `while sleep 1; do cmd_that_eventually_returns_nonzero; done` —
///     bounded by command exit code in the loop condition.
///   * `until pgrep foo > /dev/null 2>&1; do sleep 1; done` — strictly
///     unbounded but a frequent legit "wait until process appears" idiom.
///
/// **Known false negatives:**
///   * Loops that include the literal substring `&&` *anywhere* are not
///     flagged, even if the `&&` is unrelated to break logic
///     (e.g., `until [ -f done ]; do sleep 1 && echo waiting; done`).
///   * Loops with break logic in named functions (e.g.,
///     `break_after_10_iterations`) are flagged because the substring
///     `break` is matched but not the actual semantic break.
///
/// Phase F log mining will tighten these heuristics against real
/// production transcripts.
fn detect_unbounded_poll(turn: &Value, out: &mut Vec<ProtocolViolation>) {
    for tu in tool_uses(turn) {
        if tu.get("name").and_then(Value::as_str) != Some("Bash") {
            continue;
        }
        let Some(cmd) = tu
            .get("input")
            .and_then(|i| i.get("command"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        let lower = cmd.to_lowercase();
        let has_loop = lower.contains("while ") || lower.contains("until ");
        let has_sleep = lower.contains("sleep ");
        let has_break = lower.contains("break") || lower.contains("exit ") || lower.contains("&&");
        if has_loop && has_sleep && !has_break {
            let excerpt: String = cmd.chars().take(200).collect();
            out.push(ProtocolViolation::UnboundedPollEmitted {
                command_excerpt: excerpt,
            });
        }
    }
}

fn tool_uses(turn: &Value) -> impl Iterator<Item = &Value> {
    content_blocks(turn).filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
}

fn content_blocks(turn: &Value) -> impl Iterator<Item = &Value> {
    turn.get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn turn_with_blocks(blocks: serde_json::Value) -> Value {
        json!({ "message": { "content": blocks } })
    }

    #[test]
    fn detect_clean_turn_returns_empty_vec() {
        let turn = turn_with_blocks(json!([
            { "type": "text", "text": "Hello world" },
            { "type": "tool_use", "name": "Read", "input": { "file_path": "/tmp/x" } }
        ]));
        assert_eq!(detect(&turn), Vec::<ProtocolViolation>::new());
    }

    #[test]
    fn detect_empty_turn_returns_empty_vec() {
        let turn = json!({});
        assert_eq!(detect(&turn), Vec::<ProtocolViolation>::new());
    }

    #[test]
    fn detect_forbidden_tool_agent() {
        let turn = turn_with_blocks(json!([
            { "type": "tool_use", "name": "Agent", "input": {} }
        ]));
        assert_eq!(
            detect(&turn),
            vec![ProtocolViolation::ForbiddenTool {
                tool_name: "Agent".to_string(),
                context: "orchestrator non-interactive turn".to_string(),
            }]
        );
    }

    #[test]
    fn detect_forbidden_tool_task() {
        let turn = turn_with_blocks(json!([
            { "type": "tool_use", "name": "Task", "input": {} }
        ]));
        assert_eq!(
            detect(&turn),
            vec![ProtocolViolation::ForbiddenTool {
                tool_name: "Task".to_string(),
                context: "orchestrator non-interactive turn".to_string(),
            }]
        );
    }

    #[test]
    fn detect_forbidden_tool_web_fetch() {
        let turn = turn_with_blocks(json!([
            { "type": "tool_use", "name": "WebFetch", "input": {} }
        ]));
        assert_eq!(
            detect(&turn),
            vec![ProtocolViolation::ForbiddenTool {
                tool_name: "WebFetch".to_string(),
                context: "orchestrator non-interactive turn".to_string(),
            }]
        );
    }

    #[test]
    fn detect_forbidden_tool_web_search() {
        let turn = turn_with_blocks(json!([
            { "type": "tool_use", "name": "WebSearch", "input": {} }
        ]));
        assert_eq!(
            detect(&turn),
            vec![ProtocolViolation::ForbiddenTool {
                tool_name: "WebSearch".to_string(),
                context: "orchestrator non-interactive turn".to_string(),
            }]
        );
    }

    #[test]
    fn detect_allowed_tool_returns_empty_vec() {
        let turn = turn_with_blocks(json!([
            { "type": "tool_use", "name": "Read", "input": {} },
            { "type": "tool_use", "name": "Edit", "input": {} },
            { "type": "tool_use", "name": "Bash", "input": { "command": "ls" } }
        ]));
        assert_eq!(detect(&turn), Vec::<ProtocolViolation>::new());
    }

    #[test]
    fn detect_schedule_wakeup_emits_variant() {
        let turn = turn_with_blocks(json!([
            { "type": "tool_use", "name": "ScheduleWakeup", "input": { "delaySeconds": 60 } }
        ]));
        assert_eq!(
            detect(&turn),
            vec![ProtocolViolation::ScheduleWakeupInNonInteractive]
        );
    }

    #[test]
    fn detect_post_handoff_tool_use_emits_variant() {
        let turn = turn_with_blocks(json!([
            { "type": "text", "text": "ready\n\ncall sub-agent 1 (agent-type: implementer): /tmp/p" },
            { "type": "tool_use", "name": "Bash", "input": { "command": "ls" } }
        ]));
        assert_eq!(
            detect(&turn),
            vec![ProtocolViolation::PostHandoffToolUse {
                tool_name: "Bash".to_string(),
                after_handoff_index: 0,
            }]
        );
    }

    #[test]
    fn detect_tool_use_before_handoff_returns_empty_vec() {
        let turn = turn_with_blocks(json!([
            { "type": "tool_use", "name": "Read", "input": {} },
            { "type": "text", "text": "now: call sub-agent 1 (agent-type: implementer): /tmp/p" }
        ]));
        assert_eq!(detect(&turn), Vec::<ProtocolViolation>::new());
    }

    #[test]
    fn detect_dangling_narration_in_same_block_emits_variant() {
        let turn = turn_with_blocks(json!([
            {
                "type": "text",
                "text": "call sub-agent 1 (agent-type: implementer): /tmp/p\nbut wait, let me explain"
            }
        ]));
        assert_eq!(
            detect(&turn),
            vec![ProtocolViolation::DanglingNarration {
                sample: "but wait, let me explain".to_string(),
                byte_offset: 51,
            }]
        );
    }

    #[test]
    fn detect_clean_handoff_line_returns_empty_vec() {
        let turn = turn_with_blocks(json!([
            { "type": "text", "text": "call sub-agent 1 (agent-type: implementer): /tmp/p\n" }
        ]));
        assert_eq!(detect(&turn), Vec::<ProtocolViolation>::new());
    }

    #[test]
    fn detect_unbounded_poll_with_until_loop_emits_variant() {
        let turn = turn_with_blocks(json!([
            {
                "type": "tool_use",
                "name": "Bash",
                "input": { "command": "until pgrep foo; do sleep 1; done" }
            }
        ]));
        assert_eq!(
            detect(&turn),
            vec![ProtocolViolation::UnboundedPollEmitted {
                command_excerpt: "until pgrep foo; do sleep 1; done".to_string(),
            }]
        );
    }

    #[test]
    fn detect_unbounded_poll_with_while_loop_emits_variant() {
        let turn = turn_with_blocks(json!([
            {
                "type": "tool_use",
                "name": "Bash",
                "input": { "command": "while ! test -f /tmp/done; do sleep 5; done" }
            }
        ]));
        assert_eq!(
            detect(&turn),
            vec![ProtocolViolation::UnboundedPollEmitted {
                command_excerpt: "while ! test -f /tmp/done; do sleep 5; done".to_string(),
            }]
        );
    }

    #[test]
    fn detect_bounded_poll_with_break_returns_empty_vec() {
        let turn = turn_with_blocks(json!([
            {
                "type": "tool_use",
                "name": "Bash",
                "input": { "command": "while true; do sleep 1; break; done" }
            }
        ]));
        assert_eq!(detect(&turn), Vec::<ProtocolViolation>::new());
    }

    #[test]
    fn detect_poll_without_loop_returns_empty_vec() {
        let turn = turn_with_blocks(json!([
            {
                "type": "tool_use",
                "name": "Bash",
                "input": { "command": "sleep 5 && echo done" }
            }
        ]));
        assert_eq!(detect(&turn), Vec::<ProtocolViolation>::new());
    }

    #[test]
    fn detect_handoff_only_emits_no_state_variant() {
        let turn = turn_with_blocks(json!([
            { "type": "text", "text": "call sub-agent 1 (agent-type: x): /tmp/p\n" }
        ]));
        let violations = detect(&turn);
        let any_state = violations.iter().any(|v| {
            matches!(
                v,
                ProtocolViolation::HandoffsArrayEmpty { .. }
                    | ProtocolViolation::HandoffsArrayMissing { .. }
                    | ProtocolViolation::StateFileMalformed { .. }
                    | ProtocolViolation::SkillBoundaryCrossed { .. }
            )
        });
        assert_eq!(any_state, false);
    }

    #[test]
    fn unbounded_poll_flags_known_false_positive_while_read() {
        // Locks the current FP behavior so a future Phase F change has to
        // explicitly update this test.
        let turn = serde_json::json!({
            "message": {
                "content": [
                    {
                        "type": "tool_use",
                        "name": "Bash",
                        "input": {
                            "command": "while read line; do echo $line; sleep 0.1; done < input.txt"
                        }
                    }
                ]
            }
        });
        let v = detect(&turn);
        // Currently a false positive: the heuristic flags this as unbounded.
        assert_eq!(
            v.len(),
            1,
            "FP class: while-read-EOF-bounded loops are currently flagged. \
             If Phase F tightens the detector, update this test."
        );
    }

    #[test]
    fn detect_combined_violations_emits_all() {
        let turn = turn_with_blocks(json!([
            { "type": "text", "text": "call sub-agent 1 (agent-type: x): /tmp/p" },
            { "type": "tool_use", "name": "Agent", "input": {} }
        ]));
        let violations = detect(&turn);
        let has_forbidden = violations
            .iter()
            .any(|v| matches!(v, ProtocolViolation::ForbiddenTool { .. }));
        let has_post_handoff = violations
            .iter()
            .any(|v| matches!(v, ProtocolViolation::PostHandoffToolUse { .. }));
        assert_eq!(
            (has_forbidden, has_post_handoff, violations.len()),
            (true, true, 2)
        );
    }
}
