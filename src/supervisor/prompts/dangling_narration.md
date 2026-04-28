[PROTOCOL VIOLATION DETECTED]

Your last turn contained free-form prose AFTER the final
`call sub-agent` line in the same `assistant` event. The handoff
line is a hard turn boundary — anything after it (including a
closing remark, a status update, or even a single trailing newline
of commentary) breaks the protocol.

Why this matters: the daemon scans each assistant turn for the LAST
`call sub-agent N (agent-type: T): <path>` line and treats
everything after it as a protocol error. The executor has already
taken control of the wave by the time it sees the handoff line;
trailing narration introduces ambiguity about whether the
orchestrator finished or wanted to do more. The daemon resolves
the ambiguity by killing the session.

Required correction. Choose ONE of the two strategies below — do
not mix:

Strategy A — narrate BEFORE the handoff lines:

1. Move all narration (the "what I am about to do" framing, status,
   summary, etc.) to BEFORE the first `call sub-agent` line.

2. After the narration, do any setup tool calls (Read, Edit, Write,
   Bash) and write `.tmp-execute-plan-state.json`.

3. Then emit the `call sub-agent` lines back-to-back.

4. End the turn at the last handoff line. No newline-with-text
   after it. No "I will now wait..." remark. Nothing.

Strategy B — defer narration to the NEXT turn:

1. Drop the dangling narration from this turn entirely.

2. End the turn at the last handoff line.

3. When the sub-agents return and the daemon re-prompts you, put
   the narration in that next turn — at the top, before any further
   tool calls or handoffs.

Both strategies are equally acceptable. Pick whichever is closer to
your intent. Do not invent a third option (e.g., "narrate inside
the prompt file") — sub-agent prompt files are independent contexts
and have their own narration conventions.

Do NOT acknowledge this message in prose in this turn. Do NOT
apologise. Re-emit the corrected turn immediately. If the
dangling-narration pattern repeats, the daemon will SIGKILL the
session and surface the failure to the operator.
