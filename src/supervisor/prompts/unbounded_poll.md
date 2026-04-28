[PROTOCOL VIOLATION DETECTED]

You emitted a tight poll loop with no upper bound. Patterns like `while ... ; do sleep N; done` or `until ... ; do sleep N; done` without a break condition will burn the prompt cache and the user's wallet — every iteration that fires inside the same Bash invocation is fine, but every loop body that re-checks state across many minutes risks compounding latency and, in interactive mode, fires the model once per check.

Why this matters: the daemon supervises long-running Bash. An unbounded poll has no failure mode short of process supervision intervening. The operator pays for both the wall-clock and any model invocations triggered by the loop's output. Even in headless runs the loop can outlive the wave the orchestrator was meant to drive.

Required correction. Pick ONE of the three legal patterns below:

Pattern A — bounded synchronous poll inside `Bash`:

```bash
for i in $(seq 1 30); do
  <check> && break
  sleep 5
done
```

This caps the wait at 30 × 5 s = 150 s. Choose the bound to match the expected duration. Always include the `break` on success.

Pattern B — `Monitor` against a backgrounded process:

```bash
# Start the long task in the background.
<long_task> &
# Then call the Monitor tool with a wait condition or use the
# run_in_background=true Bash option and follow up with a single wait.
```

The harness will resume you when the background task signals completion. No tight loop needed.

Pattern C — synchronous wait with no loop:

If the operation is "wait for command X to finish", just run X synchronously. Bash will block until X exits. No loop, no sleep, no special-casing.

Required correction:

1. Identify the loop you emitted. Note the check condition and the expected duration.

2. Replace the loop with whichever of the three patterns fits. If the duration is bounded and short, prefer Pattern A. If you need the model out of the loop entirely, prefer Pattern B. If the operation is naturally synchronous, prefer Pattern C.

3. Drop the original unbounded loop. Do not attempt to add a bound after the fact and re-issue the same pattern; the supervisor flagged it for a reason.

4. If you genuinely need to wait longer than what Pattern A bounds support, end the wave and let the daemon's next iteration drive the recheck. Time-spanning waits are not the orchestrator's job.

Do NOT acknowledge this message in prose. Do NOT apologise. Emit the corrected step immediately using one of the three legal patterns.
