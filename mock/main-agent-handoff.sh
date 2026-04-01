#!/usr/bin/env bash
# Mock main agent — triggers a single Wave 1 handoff, then resumes and completes.
# Handles both initial call and --resume continuation.
set -euo pipefail

# ── resume path ──────────────────────────────────────────────────────────────
if [[ "$*" == *"--resume"* ]]; then
    SESSION_ID="mock-resumed-session-$RANDOM"
    printf '{"type":"system","subtype":"init","model":"mock-claude-sonnet","session_id":"%s","tools":[],"mcp_servers":[],"slash_commands":[],"output_style":"auto","skills":[],"plugins":[],"apiKeySource":"env","cwd":"/tmp","permissionMode":"bypassPermissions","claude_code_version":"1.0"}\n' "$SESSION_ID"
    sleep 0.1
    printf '{"type":"assistant","message":{"content":[{"type":"text","text":"Received sub-agent outputs. Verifying results..."}],"usage":{"input_tokens":400,"output_tokens":12}}}\n'
    sleep 0.2
    printf '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t3","name":"Bash","input":{"command":"cat /tmp/mock-subtask-output.txt 2>/dev/null || echo (not found)"}}],"usage":{}}}\n'
    sleep 0.3
    printf '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t3","content":"Mock subtask output from sub-agent\n"}]}}\n'
    sleep 0.1
    printf '{"type":"assistant","message":{"content":[{"type":"text","text":"All waves complete. Plan executed successfully."}],"usage":{"input_tokens":450,"output_tokens":10}}}\n'
    sleep 0.05
    printf '{"type":"result","subtype":"success","total_cost_usd":0.0021,"duration_ms":3200,"usage":{"input_tokens":850,"output_tokens":46,"cache_creation_input_tokens":0,"cache_read_input_tokens":0},"session_id":"%s"}\n' "$SESSION_ID"
    exit 0
fi

# ── initial execution path ───────────────────────────────────────────────────

# Extract plan path from args (the last argument after -p)
PLAN_PATH=""
PREV=""
for arg in "$@"; do
    if [[ "$PREV" == "-p" ]]; then
        # Strip the "/my:execute-plan-non-interactive " prefix and quotes
        PLAN_PATH="${arg#/my:execute-plan-non-interactive }"
        PLAN_PATH="${PLAN_PATH//\"/}"
    fi
    PREV="$arg"
done

# Find execution root (nearest .git ancestor of plan path)
EXEC_ROOT="$(pwd)"
if [[ -n "$PLAN_PATH" && -e "$PLAN_PATH" ]]; then
    DIR="$(cd "$(dirname "$PLAN_PATH")" && pwd)"
    while [[ "$DIR" != "/" ]]; do
        if [[ -d "$DIR/.git" ]]; then
            EXEC_ROOT="$DIR"
            break
        fi
        DIR="$(dirname "$DIR")"
    done
fi

SESSION_ID="mock-handoff-session-$RANDOM$RANDOM"

printf '{"type":"system","subtype":"init","model":"mock-claude-sonnet","session_id":"%s","tools":[],"mcp_servers":[],"slash_commands":[],"output_style":"auto","skills":[],"plugins":[],"apiKeySource":"env","cwd":"/tmp","permissionMode":"bypassPermissions","claude_code_version":"1.0"}\n' "$SESSION_ID"
sleep 0.1

printf '{"type":"assistant","message":{"content":[{"type":"text","text":"Starting Wave 1 execution. Decomposing tasks..."}],"usage":{"input_tokens":200,"output_tokens":12}}}\n'
sleep 0.2

printf '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Write","input":{"file_path":"/tmp/mock-subtask.md","content":"Write mock output to /tmp/mock-subtask-output.txt"}}],"usage":{}}}\n'
sleep 0.15
printf '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"File written"}]}}\n'
sleep 0.05

# Create prompt file for sub-agent
PROMPT_FILE="$EXEC_ROOT/.tmp-subtask-wave-1-batch-1-1.md"
cat > "$PROMPT_FILE" << 'PROMPT'
You are a focused implementation sub-agent.

Task: Write the text "Mock subtask output from sub-agent" to /tmp/mock-subtask-output.txt

Report: file written and content verified.
PROMPT

printf '{"type":"assistant","message":{"content":[{"type":"text","text":"State persisted. Emitting Wave 1 batch:"}],"usage":{"input_tokens":250,"output_tokens":10}}}\n'

# Write handoff state
cat > "$EXEC_ROOT/.tmp-execute-plan-state.json" << STATE
{
  "phase": "wave_execution",
  "current_phase": 3,
  "current_wave": 1,
  "current_batch": 1,
  "attempt": 1,
  "expected_handoffs": [
    {"batch_id": "wave-1-batch-1", "index": 1, "prompt_file": "$PROMPT_FILE"}
  ],
  "waves": [],
  "changed_files": [],
  "review_state": null,
  "validation_state": null
}
STATE

# Print the handoff lines (informational, not parsed by daemon)
echo ""
echo "call sub-agent 1 (agent-type: claude): $PROMPT_FILE"
echo ""
echo "# output sub-agent 1:"
# Exit here — daemon detects state file and runs handoff protocol
