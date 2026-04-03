#!/usr/bin/env bash
# Mock main agent — triggers a single Wave 1 handoff, then resumes and completes.
# Handles both initial call and --resume continuation.
set -euo pipefail

# ── resume path ──────────────────────────────────────────────────────────────
if [[ "$*" == *"--resume"* ]]; then
    SESSION_ID="mock-resumed-session-$RANDOM"
    printf '{"type":"system","subtype":"init","model":"mock-claude-sonnet","session_id":"%s","tools":[],"mcp_servers":[],"slash_commands":[],"output_style":"auto","skills":[],"plugins":[],"apiKeySource":"env","cwd":"/tmp","permissionMode":"bypassPermissions","claude_code_version":"1.0"}\n' "$SESSION_ID"
    sleep 2
    printf '{"type":"assistant","message":{"content":[{"type":"text","text":"Received sub-agent outputs. Verifying results..."}],"usage":{"input_tokens":400,"output_tokens":12}}}\n'
    sleep 5
    printf '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t3","name":"Bash","input":{"command":"cat /tmp/mock-subtask-output.txt 2>/dev/null || echo (not found)"}}],"usage":{}}}\n'
    sleep 3
    printf '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t3","content":"Mock subtask output from sub-agent\n"}]}}\n'
    sleep 4
    printf '{"type":"result","subtype":"error_during_execution","errors":["Some mock error"],"session_id":"%s","duration_ms":1500,"usage":{"input_tokens":100,"output_tokens":5}}\n' "$SESSION_ID"
    exit 1
fi

# ── initial execution path ───────────────────────────────────────────────────

PLAN_PATH=""
PREV=""
for arg in "$@"; do
    if [[ "$PREV" == "-p" ]]; then
        PLAN_PATH="${arg#/my:execute-plan-non-interactive }"
        PLAN_PATH="${PLAN_PATH//\"/}"
    fi
    PREV="$arg"
done

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
sleep 2

printf '{"type":"assistant","message":{"content":[{"type":"text","text":"Starting Wave 1 execution. Decomposing tasks..."}],"usage":{"input_tokens":200,"output_tokens":12}}}\n'
sleep 3

printf '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Write","input":{"file_path":"/tmp/mock-subtask.md","content":"Write mock output to /tmp/mock-subtask-output.txt"}}],"usage":{}}}\n'
sleep 2
printf '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"File written"}]}}\n'
sleep 5

# Create prompt file for sub-agent — use printf, not heredoc, to avoid
# background feeder processes that hold stdout (the executor pipe) open.
PROMPT_FILE="$EXEC_ROOT/.tmp-subtask-wave-1-batch-1-1.md"
printf 'You are a focused implementation sub-agent.\n\nTask: Write the text "Mock subtask output from sub-agent" to /tmp/mock-subtask-output.txt\n\nReport: file written and content verified.\n' > "$PROMPT_FILE"

printf '{"type":"assistant","message":{"content":[{"type":"text","text":"State persisted. Emitting Wave 1 batch:"}],"usage":{"input_tokens":250,"output_tokens":10}}}\n'

# Write handoff state — use printf, not heredoc (same reason as above).
printf '{\n  "phase": "wave_execution",\n  "current_phase": 3,\n  "current_wave": 1,\n  "current_batch": 1,\n  "attempt": 1,\n  "expected_handoffs": [\n    {"batch_id": "wave-1-batch-1", "index": 1, "prompt_file": "%s"}\n  ],\n  "waves": [],\n  "changed_files": [],\n  "review_state": null,\n  "validation_state": null\n}\n' "$PROMPT_FILE" > "$EXEC_ROOT/.tmp-execute-plan-state.json"

echo ""
echo "call sub-agent 1 (agent-type: claude): $PROMPT_FILE"
echo ""
echo "# output sub-agent 1:"
