[PROTOCOL VIOLATION DETECTED]

`.tmp-execute-plan-state.json` exists but the executor cannot parse it. The file is either invalid JSON, has the wrong top-level shape, or is missing a required field. The daemon refuses to dispatch sub-agents from a malformed state file because the consequences of guessing are worse than stopping.

Required correction:

1. Do NOT attempt incremental edits on the malformed file. Overwrite it from scratch using a single `Write` call. Incremental edits compound the corruption — start over.

2. Use exactly this schema. The top-level value is a JSON object:

   ```json
   {
     "handoffs": [
       {
         "index": 1,
         "agentType": "implementer",
         "promptFile": "/abs/path/.tmp-subtask-1.md",
         "canFail": false
       }
     ],
     "wave": 3,
     "phase": "implementation",
     "notes": "optional human-readable string"
   }
   ```

3. `handoffs` is required and must be an array. One entry per `call sub-agent` line you intend to emit in this turn. The entries have:
   - `index` (positive integer, matches the N in `call sub-agent N`),
   - `agentType` (string, matches the `agent-type: T` value),
   - `promptFile` (absolute path to the prompt markdown),
   - `canFail` (boolean — `false` for blocking work, `true` for advisory work).

4. `wave`, `phase`, `notes` are optional. If you include them, they must be the right type (`wave` integer, `phase` string, `notes` string). Do NOT add fields outside this schema; unknown keys are ignored today but may become an error.

5. After overwriting the state file, re-emit the `call sub-agent` lines in the same order as the `handoffs[]` array, then end the turn.

6. If the malformation came from a previous wave's leftover content, simply replacing the file is sufficient. The runtime does not care about the prior contents — only the current parse.

Do NOT acknowledge this message in prose. Do NOT apologise. Overwrite the state file and re-emit the corrected batch immediately.
