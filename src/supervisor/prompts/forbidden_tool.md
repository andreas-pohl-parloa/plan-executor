[PROTOCOL VIOLATION DETECTED]

You invoked a tool that is not on the orchestrator's non-interactive
allowlist. Forbidden in this context: `Agent`, `Task`, `WebFetch`,
`WebSearch`, the `mcp__plugin_playwright_*` family, and any tool
that opens a nested interactive session.

Why this matters: the orchestrator runs head-less under the daemon.
Tools like `Agent` and `Task` spawn sub-sessions the daemon cannot
supervise — they bypass the wave/handoff protocol, the state-file
contract, and the recovery loop. `WebFetch` and `WebSearch` are
blocked because the orchestrator must not introduce non-determinism
into a wave's input set; sub-agents that genuinely need web access
are spawned with the right environment, the orchestrator is not.

Legal tool surface for the orchestrator's non-interactive turns:

- `Read`, `Write`, `Edit` — file I/O.
- `Bash` — shell commands (with the bounded-poll rules elsewhere
  in this catalog).
- `Glob`, `Grep` — file search.
- `Skill` — calling allowed skills inside the current phase.
- `TaskCreate`, `TaskUpdate`, `TaskList`, `TaskGet`, `TaskStop`
  — the harness's task list (NOT the forbidden `Task` tool).
- `ScheduleWakeup` — only in interactive runs; see the
  schedule_wakeup template.

Required correction:

1. Identify what you were trying to accomplish with the forbidden
   tool.

2. If the goal was "spawn an autonomous worker": that work belongs
   in a sub-agent. Write a prompt file under `.tmp-subtask-N.md`,
   add a corresponding entry to `.tmp-execute-plan-state.json`
   `handoffs[]`, and emit a
   `call sub-agent N (agent-type: T): <path>` line. Then end the
   turn.

3. If the goal was "fetch a web resource": stop. Pin the dependency
   in the plan, add the URL to the sub-agent's prompt as documented
   context, or fetch the resource via a sub-agent that has web
   access in its environment.

4. If the goal was "browse the UI" (Playwright): the orchestrator
   never drives a browser. That belongs in an integration-test
   sub-agent.

5. Redo the failed step using only the legal tool surface. Do not
   retry the forbidden tool — the daemon will block it again and
   you will burn another turn.

Do NOT acknowledge this message in prose. Do NOT apologise. Emit
the corrected step immediately using a tool from the legal list.
