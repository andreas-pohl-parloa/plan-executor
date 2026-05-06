# Deviation Journal Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an evidence-gated deviation journal so plan-executor sub-agents can pass structured discoveries to later review, validation, fix, and summary stages.

**Architecture:** The Rust binary owns validation, digesting, and prompt injection. Sub-agents append validated JSONL entries under `<execution_root>/.plan-executor/deviations.jsonl`; later Rust steps read, validate, summarize, and inject bounded digests into helper inputs and sub-agent prompts. Journal entries are advisory until later stages re-verify their evidence.

**Tech Stack:** Rust, serde, jsonschema, existing `plan-executor validate --schema=...`, JSONL, Claude/Codex/Gemini sub-agent dispatch.

---

## File Structure

**Create:**
- `src/deviation_journal.rs` — Rust types, schema-facing validators, JSONL reader, digest builder, prompt block builder, artifact archiving.
- `src/schemas/deviation_journal_entry.schema.json` — JSON Schema for one entry; usable with stdin.
- `src/schemas/deviation_journal.schema.json` — JSON Schema for the whole file representation used by file validation.

**Modify:**
- `src/main.rs` — register `mod deviation_journal;`.
- `src/schema_registry.rs` — add schema ids `deviation-journal-entry` and `deviation-journal`.
- `src/cli.rs` — route `plan-executor validate --schema=deviation-journal <path>` through JSONL validation, keep `deviation-journal-entry` on generic JSON stdin path.
- `src/handoff.rs` — carry optional handoff context and inject the deviation-journal prompt block before dispatch.
- `src/scheduler.rs` — populate handoff context for wave execution; read/digest journal for later waves; archive journal into job artifacts best-effort.
- `src/helper.rs` — add optional `deviation_journal_path` and `deviation_digest` to `ReviewTeamInput` and `ValidatorInput`.
- `src/job/steps/plan.rs` — pass journal data to review/validation helpers; pass context for helper-dispatched reviewers/fixers; add final-summary/PR notable-deviation plumbing if present.
- `src/schemas/helpers/run_reviewer_team/input.schema.json` if present, or helper prompt/input schema sources if generated elsewhere — accept new optional journal fields.
- `src/schemas/helpers/validate_execution_plan/input.schema.json` if present, or helper prompt/input schema sources if generated elsewhere — accept new optional journal fields.

**Tests:**
- Unit tests in `src/deviation_journal.rs`.
- Existing schema registry tests in `src/schema_registry.rs` should cover schema compile/roundtrip.
- Existing handoff/scheduler tests extended where practical.

---

### Task 1: Add deviation-journal Rust types and validation helpers

**Files:**
- Create: `src/deviation_journal.rs`
- Modify: `src/main.rs`

- [ ] **Step 1: Create `src/deviation_journal.rs` with data types**

Create the file with this initial content:

```rust
//! Evidence-gated deviation journal for plan execution.
//!
//! Sub-agents append validated JSONL entries when they discover plan/code
//! mismatches. Later phases treat entries as hints and re-verify evidence.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const ENTRY_VERSION: u32 = 1;
pub const MAX_ENTRY_BYTES: usize = 16 * 1024;
pub const MAX_JOURNAL_BYTES: u64 = 256 * 1024;
pub const MAX_DIGEST_BYTES: usize = 32 * 1024;
pub const MAX_DIGEST_LINES: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviationCategory {
    Skip,
    Substitute,
    ScopeChange,
    Discovery,
    Blocker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviationSeverity {
    Info,
    Warning,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    FileLine,
    CommandLog,
    TestResult,
    Commit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviationEvidence {
    pub kind: EvidenceKind,
    pub path: Option<String>,
    pub lines: Option<String>,
    pub command: Option<String>,
    pub commit: Option<String>,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviationJournalEntry {
    pub version: u32,
    pub job_id: String,
    pub phase: String,
    pub wave_id: Option<u32>,
    pub task_id: Option<String>,
    pub agent_index: Option<usize>,
    pub category: DeviationCategory,
    pub severity: DeviationSeverity,
    pub claim: String,
    pub plan_anchor: String,
    pub evidence: Vec<DeviationEvidence>,
    pub impact: String,
    pub recommended_followup: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalWarning {
    pub line: usize,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DigestScope<'a> {
    All,
    ChangedFiles(&'a [PathBuf]),
    TaskDependencyContext(&'a [String]),
    FileSpecific(&'a Path),
}

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("entry exceeds {MAX_ENTRY_BYTES} bytes")]
    EntryTooLarge,
    #[error("entry is not valid JSON: {0}")]
    InvalidJson(serde_json::Error),
    #[error("entry semantic error: {0}")]
    Semantic(String),
    #[error("journal exceeds {MAX_JOURNAL_BYTES} bytes")]
    JournalTooLarge,
    #[error("journal read failed: {0}")]
    Read(std::io::Error),
}

pub fn journal_path(execution_root: &Path) -> PathBuf {
    execution_root.join(".plan-executor").join("deviations.jsonl")
}

pub fn validate_entry_bytes(bytes: &[u8]) -> Result<DeviationJournalEntry, JournalError> {
    if bytes.len() > MAX_ENTRY_BYTES {
        return Err(JournalError::EntryTooLarge);
    }
    let entry: DeviationJournalEntry = serde_json::from_slice(bytes).map_err(JournalError::InvalidJson)?;
    validate_entry_semantics(&entry)?;
    Ok(entry)
}

pub fn validate_entry_semantics(entry: &DeviationJournalEntry) -> Result<(), JournalError> {
    if entry.version != ENTRY_VERSION {
        return Err(JournalError::Semantic(format!(
            "version must be {ENTRY_VERSION}, got {}",
            entry.version
        )));
    }
    if entry.job_id.trim().is_empty() {
        return Err(JournalError::Semantic("job_id must be non-empty".into()));
    }
    if entry.phase.trim().is_empty() {
        return Err(JournalError::Semantic("phase must be non-empty".into()));
    }
    if entry.claim.trim().is_empty() {
        return Err(JournalError::Semantic("claim must be non-empty".into()));
    }
    if entry.plan_anchor.trim().is_empty() {
        return Err(JournalError::Semantic("plan_anchor must be non-empty".into()));
    }
    if entry.impact.trim().is_empty() {
        return Err(JournalError::Semantic("impact must be non-empty".into()));
    }
    chrono::DateTime::parse_from_rfc3339(&entry.created_at)
        .map_err(|e| JournalError::Semantic(format!("created_at must be RFC3339: {e}")))?;
    let evidence_required = matches!(
        entry.category,
        DeviationCategory::Skip | DeviationCategory::Substitute | DeviationCategory::ScopeChange
    );
    if evidence_required && entry.evidence.is_empty() {
        return Err(JournalError::Semantic(
            "skip/substitute/scope_change entries require at least one evidence item".into(),
        ));
    }
    for (idx, evidence) in entry.evidence.iter().enumerate() {
        if evidence.summary.trim().is_empty() {
            return Err(JournalError::Semantic(format!(
                "evidence[{idx}].summary must be non-empty"
            )));
        }
        match evidence.kind {
            EvidenceKind::FileLine => {
                if evidence.path.as_deref().unwrap_or_default().trim().is_empty() {
                    return Err(JournalError::Semantic(format!(
                        "evidence[{idx}] file_line requires path"
                    )));
                }
                if evidence.lines.as_deref().unwrap_or_default().trim().is_empty() {
                    return Err(JournalError::Semantic(format!(
                        "evidence[{idx}] file_line requires lines"
                    )));
                }
            }
            EvidenceKind::CommandLog | EvidenceKind::TestResult => {
                if evidence.path.as_deref().unwrap_or_default().trim().is_empty() {
                    return Err(JournalError::Semantic(format!(
                        "evidence[{idx}] {:?} requires path",
                        evidence.kind
                    )));
                }
            }
            EvidenceKind::Commit => {
                if evidence.commit.as_deref().unwrap_or_default().trim().is_empty() {
                    return Err(JournalError::Semantic(format!(
                        "evidence[{idx}] commit requires commit"
                    )));
                }
            }
        }
    }
    Ok(())
}
```

- [ ] **Step 2: Add the module to `src/main.rs`**

Add this line near the other modules:

```rust
mod deviation_journal;
```

- [ ] **Step 3: Add unit tests for semantic validation**

Append this test module to `src/deviation_journal.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn valid_entry(category: DeviationCategory) -> DeviationJournalEntry {
        DeviationJournalEntry {
            version: 1,
            job_id: "job-123".into(),
            phase: "wave_execution".into(),
            wave_id: Some(3),
            task_id: Some("18".into()),
            agent_index: Some(2),
            category,
            severity: DeviationSeverity::Warning,
            claim: "Task appears already implemented".into(),
            plan_anchor: "Task 18".into(),
            evidence: vec![DeviationEvidence {
                kind: EvidenceKind::FileLine,
                path: Some("plugins/foo/SKILL.md".into()),
                lines: Some("31-59".into()),
                command: None,
                commit: None,
                summary: "Loop already present".into(),
            }],
            impact: "Validator should verify the lines before failing".into(),
            recommended_followup: Some("No code change if evidence still matches".into()),
            created_at: "2026-05-06T15:30:00Z".into(),
        }
    }

    #[test]
    fn valid_skip_with_file_line_evidence_passes() {
        validate_entry_semantics(&valid_entry(DeviationCategory::Skip)).unwrap();
    }

    #[test]
    fn skip_without_evidence_fails() {
        let mut entry = valid_entry(DeviationCategory::Skip);
        entry.evidence.clear();
        let err = validate_entry_semantics(&entry).unwrap_err().to_string();
        assert!(err.contains("require at least one evidence"), "{err}");
    }

    #[test]
    fn discovery_without_evidence_passes() {
        let mut entry = valid_entry(DeviationCategory::Discovery);
        entry.evidence.clear();
        validate_entry_semantics(&entry).unwrap();
    }

    #[test]
    fn invalid_timestamp_fails() {
        let mut entry = valid_entry(DeviationCategory::Discovery);
        entry.created_at = "not-a-date".into();
        let err = validate_entry_semantics(&entry).unwrap_err().to_string();
        assert!(err.contains("RFC3339"), "{err}");
    }

    #[test]
    fn oversized_entry_fails() {
        let bytes = vec![b'x'; MAX_ENTRY_BYTES + 1];
        assert!(matches!(
            validate_entry_bytes(&bytes),
            Err(JournalError::EntryTooLarge)
        ));
    }
}
```

- [ ] **Step 4: Run focused tests**

Run:

```bash
cargo test --lib deviation_journal::
```

Expected: all deviation-journal tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/main.rs src/deviation_journal.rs
git commit -m "feat(deviation-journal): add entry types and semantic checks"
```

---

### Task 2: Add deviation-journal JSON Schemas and registry ids

**Files:**
- Create: `src/schemas/deviation_journal_entry.schema.json`
- Create: `src/schemas/deviation_journal.schema.json`
- Modify: `src/schema_registry.rs`

- [ ] **Step 1: Create entry schema**

Create `src/schemas/deviation_journal_entry.schema.json`:

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "$id": "https://parloa.dev/plan-executor/deviation_journal_entry.schema.json",
  "title": "Deviation journal entry",
  "type": "object",
  "additionalProperties": false,
  "required": [
    "version",
    "job_id",
    "phase",
    "category",
    "severity",
    "claim",
    "plan_anchor",
    "evidence",
    "impact",
    "created_at"
  ],
  "properties": {
    "version": { "type": "integer", "const": 1 },
    "job_id": { "type": "string", "minLength": 1, "maxLength": 128 },
    "phase": { "type": "string", "minLength": 1, "maxLength": 64 },
    "wave_id": { "type": ["integer", "null"], "minimum": 1 },
    "task_id": { "type": ["string", "null"], "minLength": 1, "maxLength": 128 },
    "agent_index": { "type": ["integer", "null"], "minimum": 1, "maximum": 64 },
    "category": {
      "type": "string",
      "enum": ["skip", "substitute", "scope_change", "discovery", "blocker"]
    },
    "severity": { "type": "string", "enum": ["info", "warning", "critical"] },
    "claim": { "type": "string", "minLength": 1, "maxLength": 2048 },
    "plan_anchor": { "type": "string", "minLength": 1, "maxLength": 1024 },
    "evidence": {
      "type": "array",
      "maxItems": 8,
      "items": {
        "type": "object",
        "additionalProperties": false,
        "required": ["kind", "summary"],
        "properties": {
          "kind": {
            "type": "string",
            "enum": ["file_line", "command_log", "test_result", "commit"]
          },
          "path": { "type": ["string", "null"], "minLength": 1, "maxLength": 1024 },
          "lines": { "type": ["string", "null"], "minLength": 1, "maxLength": 64 },
          "command": { "type": ["string", "null"], "minLength": 1, "maxLength": 2048 },
          "commit": { "type": ["string", "null"], "pattern": "^[0-9a-fA-F]{7,40}$" },
          "summary": { "type": "string", "minLength": 1, "maxLength": 2048 }
        }
      }
    },
    "impact": { "type": "string", "minLength": 1, "maxLength": 2048 },
    "recommended_followup": { "type": ["string", "null"], "maxLength": 2048 },
    "created_at": { "type": "string", "format": "date-time" }
  },
  "allOf": [
    {
      "if": { "properties": { "category": { "enum": ["skip", "substitute", "scope_change"] } } },
      "then": { "properties": { "evidence": { "minItems": 1 } } }
    }
  ]
}
```

- [ ] **Step 2: Create file schema**

Create `src/schemas/deviation_journal.schema.json`:

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "$id": "https://parloa.dev/plan-executor/deviation_journal.schema.json",
  "title": "Deviation journal file representation",
  "description": "Array representation used by the CLI after reading deviations.jsonl line-by-line.",
  "type": "array",
  "maxItems": 512,
  "items": { "$ref": "https://parloa.dev/plan-executor/deviation_journal_entry.schema.json" }
}
```

- [ ] **Step 3: Register schemas**

In `src/schema_registry.rs`, add constants after `HANDOFFS_SCHEMA`:

```rust
const DEVIATION_JOURNAL_ENTRY_SCHEMA: &str = include_str!("schemas/deviation_journal_entry.schema.json");
const DEVIATION_JOURNAL_SCHEMA: &str = include_str!("schemas/deviation_journal.schema.json");
```

Add variants to `SchemaId`:

```rust
DeviationJournalEntry,
DeviationJournal,
```

Update `as_str()`:

```rust
Self::DeviationJournalEntry => "deviation-journal-entry",
Self::DeviationJournal => "deviation-journal",
```

Update `embedded_text()`:

```rust
Self::DeviationJournalEntry => DEVIATION_JOURNAL_ENTRY_SCHEMA,
Self::DeviationJournal => DEVIATION_JOURNAL_SCHEMA,
```

Update `ALL_IDS`:

```rust
SchemaId::DeviationJournalEntry,
SchemaId::DeviationJournal,
```

Update `compiled()` with two `OnceLock`s:

```rust
static DEVIATION_JOURNAL_ENTRY: OnceLock<jsonschema::Validator> = OnceLock::new();
static DEVIATION_JOURNAL: OnceLock<jsonschema::Validator> = OnceLock::new();
```

and match arms:

```rust
SchemaId::DeviationJournalEntry => &DEVIATION_JOURNAL_ENTRY,
SchemaId::DeviationJournal => &DEVIATION_JOURNAL,
```

- [ ] **Step 4: Update unknown-id test**

In `schema_registry.rs` test `unknown_id_lists_known_ones`, add:

```rust
assert!(err.contains("deviation-journal-entry"), "{err}");
assert!(err.contains("deviation-journal"), "{err}");
```

- [ ] **Step 5: Run schema tests**

Run:

```bash
cargo test --lib schema_registry::
```

Expected: schema registry tests pass and compile every schema.

- [ ] **Step 6: Commit**

```bash
git add src/schema_registry.rs src/schemas/deviation_journal_entry.schema.json src/schemas/deviation_journal.schema.json
git commit -m "feat(deviation-journal): register validation schemas"
```

---

### Task 3: Wire `plan-executor validate` for entry stdin and JSONL file validation

**Files:**
- Modify: `src/cli.rs`
- Modify: `src/deviation_journal.rs`

- [ ] **Step 1: Add JSONL reader functions**

Append to `src/deviation_journal.rs`:

```rust
pub fn read_valid_entries(path: &Path) -> Result<(Vec<DeviationJournalEntry>, Vec<JournalWarning>), JournalError> {
    let metadata = std::fs::metadata(path).map_err(JournalError::Read)?;
    if metadata.len() > MAX_JOURNAL_BYTES {
        return Err(JournalError::JournalTooLarge);
    }
    let raw = std::fs::read_to_string(path).map_err(JournalError::Read)?;
    let mut entries = Vec::new();
    let mut warnings = Vec::new();
    for (idx, line) in raw.lines().enumerate() {
        let line_no = idx + 1;
        if line.trim().is_empty() {
            continue;
        }
        match validate_entry_bytes(line.as_bytes()) {
            Ok(entry) => entries.push(entry),
            Err(e) => warnings.push(JournalWarning {
                line: line_no,
                message: e.to_string(),
            }),
        }
    }
    Ok((entries, warnings))
}

pub fn validate_journal_file(path: &Path) -> Result<Vec<DeviationJournalEntry>, Vec<JournalWarning>> {
    match read_valid_entries(path) {
        Ok((entries, warnings)) if warnings.is_empty() => Ok(entries),
        Ok((_entries, warnings)) => Err(warnings),
        Err(e) => Err(vec![JournalWarning { line: 0, message: e.to_string() }]),
    }
}
```

- [ ] **Step 2: Route file validation in CLI**

In `src/cli.rs`, change `run_validate()` branch from:

```rust
if schema_id == "tasks" {
    run_validate_tasks_manifest(path);
} else {
    run_validate_against_schema(path, schema_id);
}
```

to:

```rust
if schema_id == "tasks" {
    run_validate_tasks_manifest(path);
} else if schema_id == "deviation-journal" {
    run_validate_deviation_journal(path);
} else {
    run_validate_against_schema(path, schema_id);
}
```

Add this function after `run_validate_tasks_manifest`:

```rust
fn run_validate_deviation_journal(path: &Path) {
    if path == Path::new("-") {
        eprintln!("ERROR: `--schema=deviation-journal` requires an on-disk JSONL file; use `--schema=deviation-journal-entry -` for stdin");
        std::process::exit(1);
    }
    match crate::deviation_journal::validate_journal_file(path) {
        Ok(entries) => {
            println!("VALID: {} (schema: deviation-journal, entries: {})", path.display(), entries.len());
        }
        Err(warnings) => {
            for warning in warnings {
                if warning.line == 0 {
                    eprintln!("ERROR: {}", warning.message);
                } else {
                    eprintln!("ERROR: line {}: {}", warning.line, warning.message);
                }
            }
            std::process::exit(1);
        }
    }
}
```

- [ ] **Step 3: Add CLI-facing tests**

Add tests to `src/deviation_journal.rs`:

```rust
#[test]
fn jsonl_reader_reports_malformed_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("deviations.jsonl");
    std::fs::write(&path, "not-json\n").unwrap();
    let err = validate_journal_file(&path).unwrap_err();
    assert_eq!(err[0].line, 1);
    assert!(err[0].message.contains("valid JSON"), "{}", err[0].message);
}

#[test]
fn jsonl_reader_accepts_valid_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("deviations.jsonl");
    let entry = serde_json::to_string(&valid_entry(DeviationCategory::Discovery)).unwrap();
    std::fs::write(&path, format!("{entry}\n")).unwrap();
    let entries = validate_journal_file(&path).unwrap();
    assert_eq!(entries.len(), 1);
}
```

`tempfile` is already present in `Cargo.toml`, so use `tempfile::tempdir()` exactly as shown.

- [ ] **Step 4: Run validator smoke checks**

Run:

```bash
printf '%s\n' '{"version":1,"job_id":"job","phase":"wave_execution","wave_id":1,"task_id":"1","agent_index":1,"category":"discovery","severity":"info","claim":"found context","plan_anchor":"Task 1","evidence":[],"impact":"later stages should know","recommended_followup":null,"created_at":"2026-05-06T15:30:00Z"}' | cargo run --quiet -- validate --schema=deviation-journal-entry -
```

Expected stdout contains:

```text
VALID: <stdin> (schema: deviation-journal-entry)
```

Run:

```bash
tmp=$(mktemp)
printf '%s\n' '{"version":1,"job_id":"job","phase":"wave_execution","wave_id":1,"task_id":"1","agent_index":1,"category":"discovery","severity":"info","claim":"found context","plan_anchor":"Task 1","evidence":[],"impact":"later stages should know","recommended_followup":null,"created_at":"2026-05-06T15:30:00Z"}' > "$tmp"
cargo run --quiet -- validate --schema=deviation-journal "$tmp"
rm -f "$tmp"
```

Expected stdout contains:

```text
VALID: <tmp-path> (schema: deviation-journal, entries: 1)
```

- [ ] **Step 5: Commit**

```bash
git add src/cli.rs src/deviation_journal.rs
git commit -m "feat(deviation-journal): validate JSONL journals"
```

---

### Task 4: Inject deviation-journal write instructions into wave sub-agent prompts

**Files:**
- Modify: `src/handoff.rs`
- Modify: `src/scheduler.rs`

- [ ] **Step 1: Extend `Handoff` metadata**

In `src/handoff.rs`, extend `Handoff`:

```rust
pub deviation_context: Option<DeviationContext>,
```

Add this struct near `Handoff`:

```rust
#[derive(Debug, Clone)]
pub struct DeviationContext {
    pub journal_path: PathBuf,
    pub job_id: String,
    pub phase: String,
    pub wave_id: Option<u32>,
    pub task_id: Option<String>,
    pub agent_index: usize,
    pub prior_digest: Option<String>,
}
```

Update every direct construction of `Handoff` in tests to set `deviation_context: None`.

- [ ] **Step 2: Add prompt-block builder**

In `src/handoff.rs`, add:

```rust
const DEVIATION_MARKER: &str = "Deviation journal (plan-executor enforced";

fn deviation_block(ctx: &DeviationContext) -> String {
    let wave_id = ctx.wave_id.map(|x| x.to_string()).unwrap_or_else(|| "null".into());
    let task_id = ctx.task_id.as_deref().unwrap_or("null");
    let mut block = format!(
        "> **Deviation journal (plan-executor enforced — do not remove):**\n\
         >\n\
         > If you discover a mismatch between this task and the codebase, or you intentionally skip/substitute/scope-change part of the task, write a validated journal entry.\n\
         >\n\
         > Constants for this task:\n\
         > - journal_path: `{}`\n\
         > - job_id: `{}`\n\
         > - phase: `{}`\n\
         > - wave_id: `{}`\n\
         > - task_id: `{}`\n\
         > - agent_index: `{}`\n\
         >\n\
         > Protocol:\n\
         > 1. Create one JSON object matching `plan-executor validate --schema=deviation-journal-entry`.\n\
         > 2. Validate it with `plan-executor validate --schema=deviation-journal-entry -`.\n\
         > 3. Append it as one line to `journal_path` only after validation passes.\n\
         > 4. Do not ask the user. Do not use the journal to justify incomplete work. If a required task cannot be completed, fail explicitly.\n",
        ctx.journal_path.display(),
        ctx.job_id,
        ctx.phase,
        wave_id,
        task_id,
        ctx.agent_index,
    );
    if let Some(digest) = &ctx.prior_digest {
        if !digest.trim().is_empty() {
            block.push_str(">\n> Prior deviation digest for context:\n");
            for line in digest.lines() {
                block.push_str("> ");
                block.push_str(line);
                block.push('\n');
            }
        }
    }
    block.push_str("\n---\n\n");
    block
}

fn ensure_deviation_block_in_prompt(path: &Path, ctx: &DeviationContext) {
    let Ok(original) = std::fs::read_to_string(path) else { return; };
    if original.contains(DEVIATION_MARKER) {
        return;
    }
    let block = deviation_block(ctx);
    if let Err(err) = std::fs::write(path, format!("{block}{original}")) {
        tracing::warn!(path = %path.display(), error = %err, "failed to prepend deviation journal block");
    }
}
```

- [ ] **Step 3: Call prompt-block injection before hygiene injection**

In `dispatch_agent()` after prompt-file existence and before `ensure_hygiene_in_prompt`, add:

```rust
if !is_bash {
    if let Some(ctx) = &handoff.deviation_context {
        ensure_deviation_block_in_prompt(&handoff.prompt_file, ctx);
    }
}
```

Keep existing hygiene injection immediately after this block.

- [ ] **Step 4: Build context in `scheduler::build_handoffs`**

Change `build_handoffs` signature from:

```rust
fn build_handoffs(wave: &Wave, manifest: &Manifest, manifest_dir: &Path) -> Result<Vec<Handoff>, SchedulerError>
```

to:

```rust
fn build_handoffs(
    wave: &Wave,
    manifest: &Manifest,
    manifest_dir: &Path,
    ctx: &StepContext,
    prior_digest: Option<String>,
) -> Result<Vec<Handoff>, SchedulerError>
```

Update call site in `run_wave_execution()`:

```rust
let prior_digest = crate::deviation_journal::digest_for_wave(&ctx.workdir, wave);
let handoffs = match build_handoffs(wave, manifest, &manifest_dir, ctx, prior_digest) {
```

Add this temporary helper in `src/deviation_journal.rs`; Task 5 replaces it with the real digest implementation:

```rust
pub fn digest_for_wave(_execution_root: &Path, _wave: &crate::scheduler::Wave) -> Option<String> {
    None
}
```

Inside `build_handoffs`, construct `deviation_context`:

```rust
let deviation_context = Some(handoff::DeviationContext {
    journal_path: crate::deviation_journal::journal_path(&ctx.workdir),
    job_id: ctx.job_id.clone(),
    phase: "wave_execution".to_string(),
    wave_id: Some(wave.id),
    task_id: Some(tid.clone()),
    agent_index: idx + 1,
    prior_digest: prior_digest.clone(),
});
```

Then include it in `Handoff`:

```rust
deviation_context,
```

Use `ctx.daemon_hooks.as_ref().map(|hooks| hooks.job_id().to_string()).unwrap_or_else(|| "foreground".to_string())` for `job_id`; `SchedulerHooks::job_id()` already exists in daemon-backed runs.

- [ ] **Step 5: Update scheduler tests**

Fix compile failures in tests constructing `Handoff`. For `build_handoffs_resolves_prompt_paths_against_manifest_dir`, assert:

```rust
assert!(handoffs[0].deviation_context.is_some());
assert_eq!(handoffs[0].deviation_context.as_ref().unwrap().task_id.as_deref(), Some("t1"));
```

- [ ] **Step 6: Run focused tests**

Run:

```bash
cargo test --lib handoff:: scheduler::
```

Expected: handoff and scheduler tests pass.

- [ ] **Step 7: Commit**

```bash
git add src/handoff.rs src/scheduler.rs src/deviation_journal.rs
git commit -m "feat(deviation-journal): inject write protocol into sub-agent prompts"
```

---

### Task 5: Implement journal digests and archive journal artifacts

**Files:**
- Modify: `src/deviation_journal.rs`
- Modify: `src/scheduler.rs`

- [ ] **Step 1: Implement digest selection and formatting**

Replace the temporary `digest_for_wave` in `src/deviation_journal.rs` with:

```rust
pub fn digest(entries: &[DeviationJournalEntry], scope: DigestScope<'_>) -> String {
    let mut selected: Vec<&DeviationJournalEntry> = entries
        .iter()
        .filter(|entry| match scope {
            DigestScope::All => true,
            DigestScope::ChangedFiles(files) => entry.evidence.iter().any(|e| {
                e.path.as_ref().is_some_and(|p| files.iter().any(|f| f == Path::new(p)))
            }),
            DigestScope::TaskDependencyContext(task_ids) => entry
                .task_id
                .as_ref()
                .is_some_and(|tid| task_ids.iter().any(|x| x == tid)),
            DigestScope::FileSpecific(path) => entry.evidence.iter().any(|e| {
                e.path.as_ref().is_some_and(|p| Path::new(p) == path)
            }),
        })
        .collect();

    selected.sort_by_key(|entry| match entry.severity {
        DeviationSeverity::Critical => 0,
        DeviationSeverity::Warning => 1,
        DeviationSeverity::Info => 2,
    });

    let mut out = String::new();
    for entry in selected {
        let task = entry.task_id.as_deref().unwrap_or("repo-wide");
        out.push_str(&format!(
            "- Task {task} / {:?} / {:?}:\n  Claim: {}\n",
            entry.category,
            entry.severity,
            entry.claim
        ));
        for evidence in &entry.evidence {
            match evidence.kind {
                EvidenceKind::FileLine => out.push_str(&format!(
                    "  Evidence: {}:{} — {}\n",
                    evidence.path.as_deref().unwrap_or("<missing-path>"),
                    evidence.lines.as_deref().unwrap_or("?"),
                    evidence.summary
                )),
                EvidenceKind::CommandLog | EvidenceKind::TestResult => out.push_str(&format!(
                    "  Evidence: {} — {}\n",
                    evidence.path.as_deref().unwrap_or("<missing-path>"),
                    evidence.summary
                )),
                EvidenceKind::Commit => out.push_str(&format!(
                    "  Evidence: commit {} — {}\n",
                    evidence.commit.as_deref().unwrap_or("<missing-commit>"),
                    evidence.summary
                )),
            }
        }
        out.push_str(&format!("  Impact: {}\n", entry.impact));
        if out.len() >= MAX_DIGEST_BYTES || out.lines().count() >= MAX_DIGEST_LINES {
            out.push_str("[deviation digest truncated]\n");
            break;
        }
    }
    out
}

pub fn digest_for_wave(execution_root: &Path, _wave: &crate::scheduler::Wave) -> Option<String> {
    let path = journal_path(execution_root);
    if !path.is_file() {
        return None;
    }
    let (entries, warnings) = read_valid_entries(&path).ok()?;
    for warning in warnings {
        tracing::warn!(line = warning.line, message = %warning.message, "skipping malformed deviation journal line");
    }
    let rendered = digest(&entries, DigestScope::All);
    if rendered.trim().is_empty() { None } else { Some(rendered) }
}
```

- [ ] **Step 2: Add archive function**

Add:

```rust
pub fn archive_to_job(job_dir: &Path, execution_root: &Path) {
    let src = journal_path(execution_root);
    if !src.is_file() {
        return;
    }
    let dst = job_dir.join("deviations.jsonl");
    if let Err(err) = std::fs::copy(&src, &dst) {
        tracing::warn!(src = %src.display(), dst = %dst.display(), error = %err, "failed to archive deviation journal");
    }
}
```

- [ ] **Step 3: Archive after wave execution summary**

In `scheduler::run_wave_execution()`, after `write_step_summary(ctx, &wave_outcomes, true)` succeeds or immediately before returning `AttemptOutcome::Success`, call:

```rust
if let Some(job_dir) = ctx.job_dir.as_ref() {
    crate::deviation_journal::archive_to_job(job_dir, &ctx.workdir);
}
```

`StepContext` exposes `job_dir`; use it exactly as shown. Do not block execution on archive failures.

- [ ] **Step 4: Add digest tests**

Add tests in `src/deviation_journal.rs`:

```rust
#[test]
fn digest_renders_evidence_and_impact() {
    let entry = valid_entry(DeviationCategory::Skip);
    let rendered = digest(&[entry], DigestScope::All);
    assert!(rendered.contains("Task 18"), "{rendered}");
    assert!(rendered.contains("plugins/foo/SKILL.md:31-59"), "{rendered}");
    assert!(rendered.contains("Impact:"), "{rendered}");
}

#[test]
fn digest_file_scope_filters_unrelated_entries() {
    let entry = valid_entry(DeviationCategory::Skip);
    let rendered = digest(&[entry], DigestScope::FileSpecific(Path::new("other/file.rs")));
    assert!(rendered.is_empty(), "{rendered}");
}
```

- [ ] **Step 5: Run focused tests**

Run:

```bash
cargo test --lib deviation_journal:: scheduler::
```

Expected: tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/deviation_journal.rs src/scheduler.rs
git commit -m "feat(deviation-journal): digest and archive journal entries"
```

---

### Task 6: Pass journal digest into review and validation helpers

**Files:**
- Modify: `src/helper.rs`
- Modify: `src/job/steps/plan.rs`

- [ ] **Step 1: Extend helper input structs**

In `src/helper.rs`, add these fields to `ReviewTeamInput` after `execution_root`:

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub deviation_journal_path: Option<PathBuf>,
#[serde(default, skip_serializing_if = "String::is_empty")]
pub deviation_digest: String,
```

Add the same fields to `ValidatorInput` after `execution_root`:

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub deviation_journal_path: Option<PathBuf>,
#[serde(default, skip_serializing_if = "String::is_empty")]
pub deviation_digest: String,
```

- [ ] **Step 2: Build digest in review input**

In `build_review_team_input()` in `src/job/steps/plan.rs`, before constructing `ReviewTeamInput`, add:

```rust
let deviation_journal_path = crate::deviation_journal::journal_path(&ctx.workdir);
let deviation_digest = if deviation_journal_path.is_file() {
    let (entries, warnings) = crate::deviation_journal::read_valid_entries(&deviation_journal_path)
        .unwrap_or_else(|err| {
            tracing::warn!(error = %err, "failed to read deviation journal for review input");
            (Vec::new(), Vec::new())
        });
    for warning in warnings {
        tracing::warn!(line = warning.line, message = %warning.message, "skipping malformed deviation journal line in review input");
    }
    crate::deviation_journal::digest(&entries, crate::deviation_journal::DigestScope::All)
} else {
    String::new()
};
```

Then set fields:

```rust
deviation_journal_path: deviation_journal_path.is_file().then_some(deviation_journal_path),
deviation_digest,
```

- [ ] **Step 3: Build digest in validator input**

Apply the same logic in `build_validator_input()`, using the same `DigestScope::All` for the first version.

- [ ] **Step 4: Update helper prompt/input expectations**

Find the non-interactive helper skill prompt generation in plugin files or `src/helper.rs` sidecar serialization tests. Ensure the JSON sidecar now includes `deviation_digest` when non-empty. Add prompt contract text in the helper skill files if those are in this repo; otherwise add Rust comments documenting that the plugin must consume these fields.

The validation helper contract must state:

```text
Deviation journal entries are advisory. PASS only if the plan requirement is implemented in code or the deviation's evidence still verifies. If the evidence is stale, missing, or free-text only, treat the requirement as unmet.
```

The review helper contract must state:

```text
Use deviation entries as leads. Re-read evidence before accepting the claim. Do not suppress a finding solely because a deviation exists.
```

- [ ] **Step 5: Add serialization tests**

In `src/helper.rs` tests, add:

```rust
#[test]
fn review_team_input_serializes_deviation_fields_when_present() {
    let input = ReviewTeamInput {
        plan_context: "plan.md".into(),
        execution_outputs: "output".into(),
        changed_files: vec![],
        language: "rust".into(),
        recipe_list: vec![],
        prior_review_context: serde_json::json!({}),
        execution_root: PathBuf::from("/tmp/work"),
        deviation_journal_path: Some(PathBuf::from("/tmp/work/.plan-executor/deviations.jsonl")),
        deviation_digest: "- Task 1 / discovery / info".into(),
        attempt: 1,
        prior_handoff_outputs_path: String::new(),
    };
    let value = serde_json::to_value(input).unwrap();
    assert!(value.get("deviation_journal_path").is_some());
    assert_eq!(value.get("deviation_digest").unwrap(), "- Task 1 / discovery / info");
}
```

Add a matching `ValidatorInput` serialization test.

- [ ] **Step 6: Run focused tests**

Run:

```bash
cargo test --lib helper:: job::steps::plan::
```

Expected: helper and plan-step tests pass.

- [ ] **Step 7: Commit**

```bash
git add src/helper.rs src/job/steps/plan.rs
git commit -m "feat(deviation-journal): pass digest to review and validation helpers"
```

---

### Task 7: Include journal context for helper-dispatched reviewers and fix agents

**Files:**
- Modify: `src/job/steps/plan.rs`
- Modify: `src/handoff.rs`

- [ ] **Step 1: Add context for helper-dispatched reviewer/validator handoffs**

In `dispatch_handoffs_and_resume()` when pushing `Handoff`, set `deviation_context`:

```rust
let journal_path = crate::deviation_journal::journal_path(&ctx.workdir);
let prior_digest = if journal_path.is_file() {
    let (entries, warnings) = crate::deviation_journal::read_valid_entries(&journal_path)
        .unwrap_or_else(|err| {
            tracing::warn!(error = %err, "failed to read deviation journal for helper handoff context");
            (Vec::new(), Vec::new())
        });
    for warning in warnings {
        tracing::warn!(line = warning.line, message = %warning.message, "skipping malformed deviation journal line in helper handoff context");
    }
    let rendered = crate::deviation_journal::digest(&entries, crate::deviation_journal::DigestScope::All);
    (!rendered.trim().is_empty()).then_some(rendered)
} else {
    None
};
```

Then set:

```rust
deviation_context: Some(crate::handoff::DeviationContext {
    journal_path,
    job_id: ctx
        .daemon_hooks
        .as_ref()
        .map(|hooks| hooks.job_id().to_string())
        .unwrap_or_else(|| "foreground".to_string()),
    phase: kind.display_kind().to_string(),
    wave_id: None,
    task_id: None,
    agent_index: entry.index,
    prior_digest: prior_digest.clone(),
}),
```

If `kind.display_kind()` is not public/string-returning, use `format!("{:?}", kind)` or add a helper method.

- [ ] **Step 2: Ensure fix waves inherit journal context**

Fix waves call `scheduler::run_wave_execution(ctx, &scoped, &execution_root)`, so Task 4 already covers them. Add a comment near that call:

```rust
// Fix waves re-enter scheduler::run_wave_execution, so every fix-agent gets
// the same deviation-journal write/read prompt injection as implementation waves.
```

- [ ] **Step 3: Add helper handoff test**

If `dispatch_handoffs_and_resume()` has existing tests, extend one to assert `deviation_context.is_some()` on constructed handoffs. If not easily testable because it dispatches processes, add a small helper function:

```rust
fn build_deviation_context_for_helper_handoff(
    ctx: &StepContext,
    kind: HelperLoopKind,
    index: usize,
    prior_digest: Option<String>,
) -> crate::handoff::DeviationContext
```

Test that function directly.

- [ ] **Step 4: Run focused tests**

Run:

```bash
cargo test --lib job::steps::plan:: handoff::
```

Expected: tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/job/steps/plan.rs src/handoff.rs
git commit -m "feat(deviation-journal): carry context into helper handoffs"
```

---

### Task 8: Surface notable deviations in summaries and PR text

**Files:**
- Modify: `src/deviation_journal.rs`
- Modify: `src/job/steps/plan.rs`

- [ ] **Step 1: Add notable-deviation summary renderer**

In `src/deviation_journal.rs`, add:

```rust
pub fn notable_summary(entries: &[DeviationJournalEntry]) -> String {
    let notable: Vec<&DeviationJournalEntry> = entries
        .iter()
        .filter(|entry| {
            matches!(
                entry.category,
                DeviationCategory::Skip
                    | DeviationCategory::Substitute
                    | DeviationCategory::ScopeChange
                    | DeviationCategory::Blocker
            )
        })
        .collect();
    if notable.is_empty() {
        return String::new();
    }
    let mut out = String::from("## Plan deviations\n\n");
    for entry in notable {
        let task = entry.task_id.as_deref().unwrap_or("repo-wide");
        out.push_str(&format!(
            "- Task {task}: {:?} / {:?} — {}\n",
            entry.category,
            entry.severity,
            entry.claim
        ));
        if let Some(first) = entry.evidence.first() {
            match first.kind {
                EvidenceKind::FileLine => out.push_str(&format!(
                    "  Evidence: {}:{}\n",
                    first.path.as_deref().unwrap_or("<missing-path>"),
                    first.lines.as_deref().unwrap_or("?")
                )),
                EvidenceKind::CommandLog | EvidenceKind::TestResult => out.push_str(&format!(
                    "  Evidence: {}\n",
                    first.path.as_deref().unwrap_or("<missing-path>")
                )),
                EvidenceKind::Commit => out.push_str(&format!(
                    "  Evidence: commit {}\n",
                    first.commit.as_deref().unwrap_or("<missing-commit>")
                )),
            }
        }
    }
    out
}
```

- [ ] **Step 2: Add summary test**

Add:

```rust
#[test]
fn notable_summary_omits_plain_discovery() {
    let entry = valid_entry(DeviationCategory::Discovery);
    assert!(notable_summary(&[entry]).is_empty());
}

#[test]
fn notable_summary_includes_skip() {
    let entry = valid_entry(DeviationCategory::Skip);
    let rendered = notable_summary(&[entry]);
    assert!(rendered.contains("## Plan deviations"), "{rendered}");
    assert!(rendered.contains("Task 18"), "{rendered}");
}
```

- [ ] **Step 3: Add summary/PR plumbing**

In `write_local_summary()` (`src/job/steps/plan.rs`, currently around line 4066), append the deviation section immediately after `body.push_str(&render_execution_counts(counts));` and before writing the summary file:

```rust
let journal_path = crate::deviation_journal::journal_path(&ctx.workdir);
if journal_path.is_file() {
    match crate::deviation_journal::read_valid_entries(&journal_path) {
        Ok((entries, warnings)) => {
            for warning in warnings {
                tracing::warn!(line = warning.line, message = %warning.message, "skipping malformed deviation journal line in final summary");
            }
            let deviation_summary = crate::deviation_journal::notable_summary(&entries);
            if !deviation_summary.trim().is_empty() {
                body.push_str("\n");
                body.push_str(&deviation_summary);
            }
        }
        Err(err) => tracing::warn!(error = %err, "failed to read deviation journal for final summary"),
    }
}
```

Use the existing local variable `body`. Do not introduce a new summary file format.

- [ ] **Step 4: Archive journal at final summary if not already archived**

In the same final-summary path, call:

```rust
if let Some(job_dir) = ctx.job_dir.as_ref() {
    crate::deviation_journal::archive_to_job(job_dir, &ctx.workdir);
}
```

`ctx.job_dir` exists on `StepContext`; call `archive_to_job(&ctx.job_dir, &ctx.workdir)` directly and keep it best-effort.

- [ ] **Step 5: Run focused tests**

Run:

```bash
cargo test --lib deviation_journal:: job::steps::plan::
```

Expected: tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/deviation_journal.rs src/job/steps/plan.rs
git commit -m "feat(deviation-journal): summarize notable deviations"
```

---

### Task 9: End-to-end verification and docs cleanup

**Files:**
- Modify only if tests reveal drift: `src/deviation_journal.rs`, `src/handoff.rs`, `src/scheduler.rs`, `src/helper.rs`, `src/job/steps/plan.rs`, `src/schema_registry.rs`, `src/cli.rs`

- [ ] **Step 1: Run schema list check**

Run:

```bash
cargo run --quiet -- validate --list-schemas | grep -E 'deviation-journal-entry|deviation-journal'
```

Expected output includes both:

```text
deviation-journal-entry
deviation-journal
```

- [ ] **Step 2: Run entry validation check**

Run:

```bash
printf '%s\n' '{"version":1,"job_id":"job","phase":"wave_execution","wave_id":1,"task_id":"1","agent_index":1,"category":"discovery","severity":"info","claim":"found context","plan_anchor":"Task 1","evidence":[],"impact":"later stages should know","recommended_followup":null,"created_at":"2026-05-06T15:30:00Z"}' | cargo run --quiet -- validate --schema=deviation-journal-entry -
```

Expected:

```text
VALID: <stdin> (schema: deviation-journal-entry)
```

- [ ] **Step 3: Run JSONL validation check**

Run:

```bash
tmp=$(mktemp)
printf '%s\n' '{"version":1,"job_id":"job","phase":"wave_execution","wave_id":1,"task_id":"1","agent_index":1,"category":"discovery","severity":"info","claim":"found context","plan_anchor":"Task 1","evidence":[],"impact":"later stages should know","recommended_followup":null,"created_at":"2026-05-06T15:30:00Z"}' > "$tmp"
cargo run --quiet -- validate --schema=deviation-journal "$tmp"
rm -f "$tmp"
```

Expected:

```text
VALID: <tmp-path> (schema: deviation-journal, entries: 1)
```

- [ ] **Step 4: Run focused unit suites**

Run:

```bash
cargo test --lib deviation_journal:: schema_registry:: handoff:: scheduler:: helper:: job::steps::plan::
```

Expected: all selected tests pass.

- [ ] **Step 5: Run full library tests**

Run:

```bash
cargo test --lib
```

Expected: all library tests pass.

- [ ] **Step 6: Run formatting check**

Run:

```bash
cargo fmt --check
```

Expected: no formatting diffs.

- [ ] **Step 7: Commit any verification fixes**

If formatting or tests required changes:

```bash
git add src/deviation_journal.rs src/handoff.rs src/scheduler.rs src/helper.rs src/job/steps/plan.rs src/schema_registry.rs src/cli.rs src/schemas/deviation_journal_entry.schema.json src/schemas/deviation_journal.schema.json
git commit -m "fix(deviation-journal): address verification findings"
```

If no changes were needed, do not create an empty commit.

---

### Task 10: Foreground integration test for prompt injection

**Files:**
- Modify: `tests/d3_full_plan.rs` (or create `tests/deviation_journal_foreground.rs`).

The deviation-journal block is injected by `handoff::dispatch_agent`, which runs identically under daemon and `--foreground` modes. This task adds a focused end-to-end check that the foreground binary path actually rewrites a per-task prompt file with the block before dispatch. The test must not require a real Claude/Codex/Gemini binary — it inspects the prompt file after `ensure_deviation_block_in_prompt` runs, not the dispatched sub-agent output.

- [ ] **Step 1: Pick the integration target**

Use `tests/` so the test exercises the public API. Locate the existing handoff-prompt assertion test (search for `ensure_hygiene_in_prompt` or `HYGIENE_MARKER` in `tests/`). Add the new test next to it; if no existing handoff prompt-injection test exists, create `tests/deviation_journal_foreground.rs`.

- [ ] **Step 2: Write the test**

```rust
use std::fs;

use plan_executor::deviation_journal::{journal_path, DEVIATION_MARKER};
use plan_executor::handoff::{ensure_deviation_block_in_prompt, DeviationContext, Handoff};

#[test]
fn deviation_block_is_injected_for_foreground_handoff() {
    let dir = tempfile::tempdir().expect("tempdir");
    let prompt = dir.path().join("task-1.md");
    fs::write(&prompt, "# Task body\n").expect("write prompt");

    let ctx = DeviationContext {
        journal_path: journal_path(dir.path()),
        job_id: "test-job".into(),
        phase: "wave_execution".into(),
        wave_id: Some(1),
        task_id: Some("1".into()),
        agent_index: 1,
        prior_digest: None,
    };

    ensure_deviation_block_in_prompt(&prompt, &ctx);

    let body = fs::read_to_string(&prompt).expect("read prompt");
    assert!(
        body.contains(DEVIATION_MARKER),
        "prompt missing deviation marker: {body}"
    );
    assert!(
        body.contains(&format!("journal_path: `{}`", ctx.journal_path.display())),
        "prompt missing journal path: {body}"
    );
    assert!(body.contains("task_id: `1`"), "prompt missing task_id: {body}");

    // Idempotent: second call must not re-prepend.
    let before_len = body.len();
    ensure_deviation_block_in_prompt(&prompt, &ctx);
    let after = fs::read_to_string(&prompt).expect("read prompt");
    assert_eq!(after.len(), before_len, "second injection must be no-op");
    let _ = Handoff::default(); // ensure type is reachable from tests
}
```

If `Handoff::default()` does not exist, drop that line. If `ensure_deviation_block_in_prompt` and `DeviationContext` are not yet `pub`, expose them under `pub` in `src/handoff.rs` (Task 4 already adds them; this step just confirms they are reachable from integration tests). If `DEVIATION_MARKER` is private to `handoff.rs`, re-export it through `crate::deviation_journal` or expose it as `pub const` so the test can reference it.

- [ ] **Step 3: Run the test**

```bash
cargo test --test d3_full_plan deviation_block_is_injected_for_foreground_handoff
```

Or for the standalone file:

```bash
cargo test --test deviation_journal_foreground
```

Expected: 1 test passes.

- [ ] **Step 4: Run focused unit suite**

```bash
cargo test --lib deviation_journal:: handoff::
```

Expected: existing unit tests still pass.

- [ ] **Step 5: Commit**

```bash
git add tests/deviation_journal_foreground.rs # or tests/d3_full_plan.rs if extended in place
git add src/handoff.rs src/deviation_journal.rs # only if visibility changes were required
git commit -m "test(deviation-journal): assert foreground handoff prompt injection"
```

If no source visibility change was required, only commit the test file.
