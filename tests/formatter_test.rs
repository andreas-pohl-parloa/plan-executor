use plan_executor::formatter::format_stream_line;

#[test]
fn test_format_text_message() {
    let line = r#"{"type":"assistant","uuid":"x","session_id":"s","message":{"content":[{"type":"text","text":"Hello world"}],"usage":{}}}"#;
    let lines = format_stream_line(line);
    assert_eq!(lines, vec!["[Claude] Hello world"]);
}

#[test]
fn test_format_tool_use_bash() {
    let line = r#"{"type":"assistant","uuid":"x","session_id":"s","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls -la"}}],"usage":{}}}"#;
    let lines = format_stream_line(line);
    assert_eq!(lines, vec!["[Tool: Bash] ls -la"]);
}

#[test]
fn test_format_tool_result() {
    let line = r#"{"type":"user","uuid":"x","session_id":"s","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"file1.rs\nfile2.rs\n"}]}}"#;
    let lines = format_stream_line(line);
    assert!(lines.iter().any(|l| l.contains("file1.rs")));
}

#[test]
fn test_format_result_success() {
    let line = r#"{"type":"result","subtype":"success","uuid":"x","session_id":"s","total_cost_usd":0.05,"duration_ms":45000,"usage":{"input_tokens":10000,"output_tokens":5000}}"#;
    let lines = format_stream_line(line);
    assert_eq!(lines.len(), 1);
    assert!(lines[0].starts_with("[OK]"));
    assert!(lines[0].contains("45s"));
    assert!(lines[0].contains("$0.0500"));
}

#[test]
fn test_format_system_init() {
    let line = r#"{"type":"system","subtype":"init","uuid":"x","session_id":"s","model":"claude-sonnet-4-6","tools":[],"mcp_servers":[],"slash_commands":[],"output_style":"auto","skills":[],"plugins":[],"apiKeySource":"env","cwd":"/tmp","permissionMode":"bypassPermissions","claude_code_version":"1.0"}"#;
    let lines = format_stream_line(line);
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("claude-sonnet-4-6"));
}

#[test]
fn test_suppress_unknown_types() {
    let line = r#"{"type":"tool_use_summary","summary":"Did stuff","preceding_tool_use_ids":[],"uuid":"x","session_id":"s"}"#;
    let lines = format_stream_line(line);
    assert!(lines.is_empty());
}
