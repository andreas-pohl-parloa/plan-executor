use plan_executor::handoff::{build_continuation, load_state, AgentType, HandoffResult};
use std::io::Write;
use tempfile::NamedTempFile;

#[test]
fn build_continuation_orders_by_index() {
    let results = vec![
        HandoffResult { index: 2, stdout: "out2".to_string(), stderr: String::new(), success: true },
        HandoffResult { index: 1, stdout: "out1".to_string(), stderr: String::new(), success: true },
    ];
    let s = build_continuation(&results);
    assert!(s.find("# output sub-agent 1:").unwrap() < s.find("# output sub-agent 2:").unwrap());
    assert!(s.contains("out1"));
    assert!(s.contains("out2"));
}

#[test]
fn build_continuation_includes_empty_stdout_for_failed_agents() {
    let results = vec![
        HandoffResult { index: 1, stdout: String::new(), stderr: "error".to_string(), success: false },
    ];
    let s = build_continuation(&results);
    assert!(s.contains("# output sub-agent 1:"));
}

#[test]
fn load_state_parses_all_agent_types() {
    let json = r#"{
        "phase": "wave_execution",
        "wave": 1, "attempt": 1, "batch": 1,
        "handoffs": [
            {"index": 1, "agentType": "claude", "promptFile": "/tmp/p1.md"},
            {"index": 2, "agentType": "codex",  "promptFile": "/tmp/p2.md"},
            {"index": 3, "agentType": "gemini", "promptFile": "/tmp/p3.md"}
        ]
    }"#;
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(json.as_bytes()).unwrap();
    let state = load_state(f.path()).unwrap();
    assert_eq!(state.handoffs.len(), 3);
    assert!(matches!(state.handoffs[0].agent_type, AgentType::Claude));
    assert!(matches!(state.handoffs[1].agent_type, AgentType::Codex));
    assert!(matches!(state.handoffs[2].agent_type, AgentType::Gemini));
}
