#!/usr/bin/env bash
# Mock sub-agent — produces plain text output used as handoff continuation.
# Called as: ./mock/sub-agent.sh <prompt_file_path>
set -euo pipefail

sleep 5

PROMPT_FILE="${1:-}"

echo "=== Mock sub-agent output ==="
echo ""

if [[ -n "$PROMPT_FILE" && -f "$PROMPT_FILE" ]]; then
    echo "Prompt file: $PROMPT_FILE"
    echo "Prompt content:"
    cat "$PROMPT_FILE"
    echo ""
fi

echo "Writing mock output to /tmp/mock-subtask-output.txt..."
echo "Mock subtask output from sub-agent" > /tmp/mock-subtask-output.txt

echo ""
echo "Files created or modified:"
echo "  - /tmp/mock-subtask-output.txt"
echo ""
echo "Task complete."
