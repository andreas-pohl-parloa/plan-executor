[PROTOCOL VIOLATION DETECTED]

You invoked `ScheduleWakeup` in a non-interactive run.
`ScheduleWakeup` is interactive-only — it asks the harness to
re-fire your prompt at a future time. The daemon does not implement
that contract for headless runs; calling it here will at best be a
no-op and at worst hang the daemon waiting for a wake-up that will
never arrive.

Why this matters: the orchestrator's run is bounded by the
wave-and-handoff protocol. There is no scheduler that re-enters
your conversation between waves. If you need to wait for something
— a long-running build, a remote check, a sub-agent that is still
working — the legal options are:

- Wait synchronously inside the current turn (bounded `Bash` poll,
  `Monitor` on a background process, `run_in_background=true` plus
  a follow-up wait).
- Finish the work in this turn and let the daemon's wave loop drive
  the next iteration.
- Emit a `pending_resume_at` field in
  `.tmp-execute-plan-state.json` and let the operator's external
  poller decide when to re-prompt — this is for daemon authors only
  and almost certainly not what you want today.

Required correction:

1. Identify what you were trying to defer. Most often the answer is
   "I wanted to check progress later" — that is just a bounded poll,
   not a schedule.

2. If the goal was "poll until X completes": use `Bash` with
   `for i in 1..30; do <check> && break; sleep 5; done` or `Monitor`
   on a background task. Do NOT use `ScheduleWakeup`. Do NOT use a
   tight `until` loop without a bound — see the unbounded_poll
   catalog entry.

3. If the goal was "wake me up in 20 minutes to retry": let the
   wave finish. The daemon will re-prompt the orchestrator after
   the next wave's sub-agents return. Time-based scheduling is the
   operator's responsibility, not the orchestrator's.

4. If the goal was "block until a sub-agent emits a signal":
   dispatch the sub-agent now and end the turn. The daemon already
   blocks the orchestrator until sub-agents return. You do not need
   to schedule anything.

5. Drop the `ScheduleWakeup` call. Re-emit the corrected step using
   one of the strategies above.

Do NOT acknowledge this message in prose. Do NOT apologise. Emit
the corrected step immediately. The daemon ignores `ScheduleWakeup`
calls in non-interactive runs, so retrying with the same call
wastes a turn.
