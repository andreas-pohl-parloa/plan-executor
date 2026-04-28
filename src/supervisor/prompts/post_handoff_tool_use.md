[PROTOCOL VIOLATION DETECTED]

A `tool_use` block appeared in your last turn AFTER the final
`call sub-agent` line. The handoff line is a hard turn boundary —
the runtime treats it as the orchestrator yielding to the executor.
Any content after that line, including tool calls, breaks the
protocol.

Why this matters: the daemon parses the assistant turn looking for
the last `call sub-agent N (...)` text block. Everything after it
is rejected because the executor has already taken control of the
wave. If your tool call hit the wire, the daemon will SIGKILL the
session on the next repeat to avoid double-dispatch and undefined
state.

Required correction:

1. Drop whatever the post-handoff tool call was trying to accomplish
   from THIS turn. End the turn at the handoff line.

2. If the tool call was setup work the sub-agents need (file edits,
   branch creation, etc.), move it to BEFORE the first
   `call sub-agent` line in the corrected batch. Do all setup, write
   `.tmp-execute-plan-state.json`, then emit the `call sub-agent`
   lines, then stop.

3. If the tool call was follow-up work (validation, summarisation,
   cleanup), defer it to the NEXT turn — the turn after the
   sub-agents return. Do not try to do it in the same turn as the
   handoff.

4. If the tool call was a Read/Grep/Glob to gather context, decide
   whether the sub-agent prompts need that context. If yes, do the
   read BEFORE the handoff and inline the result into the prompt
   files. If no, drop it.

5. Re-emit the corrected batch: setup tools (if any) →
   `.tmp-execute-plan-state.json` write → `call sub-agent` lines →
   end of turn. No narration after the last handoff line. No tool
   calls after the last handoff line. No exceptions.

6. The legal post-handoff content is exactly: nothing. Not a
   newline, not a closing remark, not a status update. The handoff
   line ends the turn.

Do NOT acknowledge this message in prose. Do NOT apologise. Emit
the corrected batch immediately. If this violation repeats on the
next turn the daemon will SIGKILL the session and surface the
failure to the operator.
