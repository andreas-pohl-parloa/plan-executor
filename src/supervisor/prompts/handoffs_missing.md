[PROTOCOL VIOLATION DETECTED]

Your last turn ended without writing a non-empty `handoffs[]` array
to `.tmp-execute-plan-state.json`. The executor cannot dispatch
sub-agents without it.

The wave you just emitted contains
`call sub-agent N (agent-type: T): <path>` lines, but the state
file either has no `handoffs` key, has `"handoffs": null`, or has
`"handoffs": []`. The runtime requires one entry per `call sub-agent`
line — `{"index": <N>, "agentType": "<type>", "promptFile": "<path>",
"canFail": <bool>}`.

Required correction:

1. Re-emit the same batch of prompt files and
   `call sub-agent N (agent-type: T): <path>` lines, in the same
   order, with the same agent types and prompt-file paths. Do not
   change the indices, agent types, or paths from the previous turn
   — the runtime correlates them across the corrected batch.

2. BEFORE the first `call sub-agent` line in the corrected batch,
   write `.tmp-execute-plan-state.json` with one `handoffs` entry
   per emitted prompt file. Use the same `index`, `agentType`, and
   `promptFile` values that appear on the `call sub-agent` lines.
   Set `canFail` per the original sub-task; if you do not remember,
   default to `false` for implementation tasks and `true` for
   review/validation tasks.

3. After the last `call sub-agent` line, end the turn. Do NOT add
   narration after the line. Do NOT call any tool after the line.
   Do NOT acknowledge that a correction is in progress.

4. The state file is the single source of truth for the executor.
   The `call sub-agent` lines are advisory text the executor uses
   for log correlation only. If the two disagree, the runtime trusts
   the state file. So keep them in lockstep.

5. If you cannot recover (the prompt-file paths are no longer valid,
   the sub-tasks were already started elsewhere, etc.), abort the
   turn by writing `.tmp-execute-plan-state.json` with
   `"abort": true` and a `"reason"` string. The runtime will surface
   this to the operator and stop the wave.

6. Do not add any keys beyond `handoffs`, `wave`, `phase`, `notes`,
   `abort`, and `reason`. Unknown keys are silently ignored today
   but may become an error later. Stay on the documented schema.

Do NOT acknowledge this message in prose. Do NOT explain the failure.
Do NOT apologise. Emit the corrected batch immediately. The runtime
is watching for the corrected `handoffs` array on the next turn —
anything else extends the failure window and burns the cache.
