#!/bin/sh
# Fake `claude` CLI used by D3.4 end-to-end scheduler tests.
#
# Logs every spawn's argv to FAKE_CLAUDE_SPAWN_LOG (one record per line)
# so tests can grep for `execute-plan-non-interactive` etc.
#
# Decides what to print on stdout by inspecting the `-p` prompt argv:
#   - `/plan-executor:run-reviewer-team-non-interactive ...` -> reviewer envelope
#   - `/plan-executor:review-execution-output-non-interactive ...` -> triage envelope
#   - `/plan-executor:validate-execution-plan-non-interactive ...` -> validator envelope
#   - `/plan-executor:pr-finalize ...` -> pr-finalize envelope
#   - everything else (sub-agent prompt-file framing) -> exit 0 with empty stdout
#
# Per-helper response bodies are read from files set in env vars
# (FAKE_CLAUDE_REVIEW_RESPONSE_FILE, FAKE_CLAUDE_TRIAGE_RESPONSE_FILE,
# FAKE_CLAUDE_VALIDATOR_RESPONSE_FILE, FAKE_CLAUDE_PR_FINALIZE_RESPONSE_FILE).
# This indirection keeps argv off the command line so a test can stage
# multi-line JSON responses without shell-escaping headaches.
#
# Multi-call sequencing for the fix-loop test:
# FAKE_CLAUDE_REVIEW_RESPONSE_SEQUENCE_FILE points at a directory of files
# named 1, 2, 3, ...; the script consumes them in order using a counter
# stored in FAKE_CLAUDE_COUNTER_DIR/review_count.

set -u

LOG="${FAKE_CLAUDE_SPAWN_LOG:-/dev/null}"
# Record this invocation's argv (one line per spawn).
{
    printf 'argv:'
    for a in "$@"; do
        printf ' %s' "$a"
    done
    printf '\n'
} >> "$LOG"

# Find the value following `-p`.
prompt=""
while [ $# -gt 0 ]; do
    case "$1" in
        -p)
            shift
            prompt="${1:-}"
            ;;
    esac
    shift || true
done

emit_file() {
    if [ -n "${1:-}" ] && [ -f "$1" ]; then
        cat "$1"
    fi
}

case "$prompt" in
    "/plan-executor:run-reviewer-team-non-interactive"*)
        if [ -n "${FAKE_CLAUDE_REVIEW_RESPONSE_SEQUENCE_DIR:-}" ]; then
            counter_file="${FAKE_CLAUDE_COUNTER_DIR:-/tmp}/review_count"
            n=$(cat "$counter_file" 2>/dev/null || printf '0')
            n=$((n + 1))
            printf '%s' "$n" > "$counter_file"
            seq_file="${FAKE_CLAUDE_REVIEW_RESPONSE_SEQUENCE_DIR}/$n"
            if [ -f "$seq_file" ]; then
                emit_file "$seq_file"
            else
                # Fall back to last file if we ran out
                last=$(ls "${FAKE_CLAUDE_REVIEW_RESPONSE_SEQUENCE_DIR}" | sort -n | tail -1)
                emit_file "${FAKE_CLAUDE_REVIEW_RESPONSE_SEQUENCE_DIR}/${last}"
            fi
        else
            emit_file "${FAKE_CLAUDE_REVIEW_RESPONSE_FILE:-}"
        fi
        ;;
    "/plan-executor:review-execution-output-non-interactive"*)
        emit_file "${FAKE_CLAUDE_TRIAGE_RESPONSE_FILE:-}"
        ;;
    "/plan-executor:validate-execution-plan-non-interactive"*)
        emit_file "${FAKE_CLAUDE_VALIDATOR_RESPONSE_FILE:-}"
        ;;
    "/plan-executor:pr-finalize"*)
        emit_file "${FAKE_CLAUDE_PR_FINALIZE_RESPONSE_FILE:-}"
        ;;
    *)
        # Sub-agent prompt-file framing or anything else: succeed silently.
        # Emit a minimal stream-json terminal `result` line so the
        # sub-agent dispatch parser does not flag the run as failed.
        printf '{"type":"result","subtype":"success","result":"sub-agent done","duration_ms":1}\n'
        ;;
esac

exit "${FAKE_CLAUDE_EXIT_CODE:-0}"
