#!/bin/sh
# Fake `claude` CLI used by tests/helper.rs.
#
# Behaviour is steered by environment variables, not argv, so a single
# script can satisfy every scenario:
#
#   FAKE_CLAUDE_RESPONSE   raw stdout body (printed verbatim, no JSON
#                          assumptions). Default: empty.
#   FAKE_CLAUDE_STDERR     stderr body. Default: empty.
#   FAKE_CLAUDE_EXIT_CODE  numeric exit code. Default: 0.
#   FAKE_CLAUDE_SLEEP_SECS integer seconds to sleep BEFORE writing stdout.
#                          Default: 0. Used by the timeout test.
#
# The script intentionally ignores its argv (--allowed-tools, -p, etc.)
# because the tests target invoke_helper's output-handling code path,
# not the CLI surface itself.

if [ -n "${FAKE_CLAUDE_SLEEP_SECS:-}" ] && [ "${FAKE_CLAUDE_SLEEP_SECS}" -gt 0 ]; then
    sleep "${FAKE_CLAUDE_SLEEP_SECS}"
fi

if [ -n "${FAKE_CLAUDE_RESPONSE:-}" ]; then
    printf '%s' "${FAKE_CLAUDE_RESPONSE}"
fi

if [ -n "${FAKE_CLAUDE_STDERR:-}" ]; then
    printf '%s' "${FAKE_CLAUDE_STDERR}" >&2
fi

exit "${FAKE_CLAUDE_EXIT_CODE:-0}"
