[PROTOCOL VIOLATION DETECTED]

You invoked a skill outside the phase that authorises it. The
plan-executor enforces phase-scoped skill boundaries:

- Phase 3 is implementation.
- Phase 4 is integration testing.
- Phase 5 is code review.
- Phase 6 is plan validation.
- Phase 7 is cleanup-and-PR.

Skills marked for a later phase must not be called while you are
still in an earlier phase, and skills marked for an earlier phase
must not be re-entered once the work has advanced.

Why this matters: skills carry phase-specific assumptions. A
reviewer skill called during implementation reviews half-finished
code and produces noise findings; a validator skill called before
review is clean validates a moving target. The boundary keeps each
skill operating on the artefact it was designed for.

Required correction:

1. Note the violation: which skill you called, which skill the
   previous turn was inside, and which phase the orchestrator
   currently believes it is in. The state-file's `phase` field is
   the source of truth — read it before deciding.

2. If the skill you called is for a LATER phase: revert. Do not
   call it. Continue the current phase's work until the wave-loop
   advances `phase` in the state file. Then the skill becomes
   legal.

3. If the skill you called is for an EARLIER phase: revert. The
   earlier phase's work is finished — going back to it now
   invalidates downstream work. If you genuinely need to revisit
   an earlier phase (e.g., a code-review finding requires a fresh
   implementation pass), update the state file's `phase` field
   explicitly, document the reason in `notes`, and let the wave-loop
   re-enter the earlier phase cleanly.

4. If the skill you called is at the wrong granularity (e.g., a
   service-wide skill called for a single-file change): pick the
   right-grained alternative or skip the skill entirely. Not every
   change needs a skill.

5. Re-emit the corrected step. Stay inside the current phase's
   allowed skill surface. Do not retry the boundary-crossing skill
   — the supervisor will block it again.

6. If you believe the boundary itself is wrong (the plan claims
   you are in Phase 3 but the work is genuinely Phase 5), abort
   the turn by writing `.tmp-execute-plan-state.json` with
   `"abort": true` and a `"reason"` string. The operator will
   reconcile the phase manually.

Do NOT acknowledge this message in prose. Do NOT apologise. Emit
the corrected step immediately, staying inside the current phase's
allowed skill surface.
