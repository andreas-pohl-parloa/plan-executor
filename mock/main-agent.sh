#!/usr/bin/env bash
# Mock main agent — complete single-turn execution, no handoff.
# Produces stream-json output compatible with sjv.
set -euo pipefail

SESSION_ID="mock-session-$RANDOM$RANDOM"

# system/init
printf '{"type":"system","subtype":"init","model":"mock-claude-sonnet","session_id":"%s","tools":[],"mcp_servers":[],"slash_commands":[],"output_style":"auto","skills":[],"plugins":[],"apiKeySource":"env","cwd":"/tmp","permissionMode":"bypassPermissions","claude_code_version":"1.0"}\n' "$SESSION_ID"
sleep 0.05

# assistant text
printf '{"type":"assistant","message":{"content":[{"type":"text","text":"Reading the plan file to understand requirements..."}],"usage":{"input_tokens":150,"output_tokens":12}}}\n'
sleep 0.1

# tool: Read
printf '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"/tmp/mock-plan.md"}}],"usage":{}}}\n'
sleep 0.2
printf '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"# Mock Plan\n**Status:** READY\n\nTask 1: Create output file\n"}]}}\n'
sleep 0.1

# assistant text
printf '{"type":"assistant","message":{"content":[{"type":"text","text":"Implementing Task 1..."}],"usage":{"input_tokens":200,"output_tokens":8}}}\n'
sleep 0.05

# tool: Bash
printf '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t2","name":"Bash","input":{"command":"echo mock-output > /tmp/mock-plan-result.txt && echo done"}}],"usage":{}}}\n'
sleep 0.3
printf '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t2","content":"done\n"}]}}\n'
sleep 0.1

# tool progress
printf '{"type":"tool_progress","tool_name":"Bash","tool_use_id":"t2","elapsed_time_seconds":0.3}\n'
sleep 0.2

# final assistant message
printf '{"type":"assistant","message":{"content":[{"type":"text","text":"All tasks complete. Created /tmp/mock-plan-result.txt."}],"usage":{"input_tokens":280,"output_tokens":14}}}\n'
sleep 0.05

# result
printf '{"type":"result","subtype":"success","total_cost_usd":0.0014,"duration_ms":1850,"usage":{"input_tokens":630,"output_tokens":34,"cache_creation_input_tokens":0,"cache_read_input_tokens":0},"session_id":"%s"}\n' "$SESSION_ID"
