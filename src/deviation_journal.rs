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
    execution_root
        .join(".plan-executor")
        .join("deviations.jsonl")
}

/// Returns a formatted, severity-sorted digest of the entries matching `scope`.
///
/// Entries are sorted by severity (Critical → Warning → Info). Output is
/// capped at [`MAX_DIGEST_BYTES`] bytes and [`MAX_DIGEST_LINES`] lines;
/// a truncation marker is appended when either limit is reached.
///
/// # Examples
///
/// ```
/// use plan_executor::deviation_journal::{digest, DigestScope, DeviationJournalEntry,
///     DeviationCategory, DeviationSeverity, DeviationEvidence, EvidenceKind, ENTRY_VERSION};
/// use std::path::Path;
///
/// let entry = DeviationJournalEntry {
///     version: ENTRY_VERSION,
///     job_id: "j".into(), phase: "p".into(), wave_id: None, task_id: None,
///     agent_index: None, category: DeviationCategory::Discovery,
///     severity: DeviationSeverity::Info,
///     claim: "c".into(), plan_anchor: "a".into(), evidence: vec![],
///     impact: "i".into(), recommended_followup: None,
///     created_at: "2026-05-06T00:00:00Z".into(),
/// };
/// let out = digest(&[entry], DigestScope::All);
/// assert!(out.contains("Impact:"));
/// ```
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

/// Returns a human-readable digest of all prior deviations for `wave`.
///
/// Reads the journal at `execution_root`, renders all entries with
/// [`DigestScope::All`], and returns `None` when the journal is absent or
/// produces an empty digest.
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

/// Copies the deviation journal into the job artifact directory.
///
/// Best-effort: a failed copy logs a warning but does not block the caller.
/// The destination file is always named `deviations.jsonl`.
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

pub fn validate_entry_bytes(bytes: &[u8]) -> Result<DeviationJournalEntry, JournalError> {
    if bytes.len() > MAX_ENTRY_BYTES {
        return Err(JournalError::EntryTooLarge);
    }
    let entry: DeviationJournalEntry =
        serde_json::from_slice(bytes).map_err(JournalError::InvalidJson)?;
    validate_entry_semantics(&entry)?;
    Ok(entry)
}

/// Reads a JSONL journal file and returns valid entries plus any per-line warnings.
///
/// Lines that fail validation are recorded as [`JournalWarning`]s instead of
/// aborting the read, giving callers a complete picture of the file's health.
///
/// # Errors
///
/// Returns [`JournalError::JournalTooLarge`] when the file exceeds
/// [`MAX_JOURNAL_BYTES`], or [`JournalError::Read`] on I/O failure.
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

/// Validates a JSONL journal file, returning all valid entries or a list of warnings.
///
/// # Errors
///
/// Returns `Err(Vec<JournalWarning>)` when any line fails validation or an
/// I/O or size error occurs. An I/O or size error is reported as a single
/// warning with `line == 0`.
pub fn validate_journal_file(path: &Path) -> Result<Vec<DeviationJournalEntry>, Vec<JournalWarning>> {
    match read_valid_entries(path) {
        Ok((entries, warnings)) if warnings.is_empty() => Ok(entries),
        Ok((_entries, warnings)) => Err(warnings),
        Err(e) => Err(vec![JournalWarning { line: 0, message: e.to_string() }]),
    }
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
        return Err(JournalError::Semantic(
            "plan_anchor must be non-empty".into(),
        ));
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
                if evidence
                    .path
                    .as_deref()
                    .unwrap_or_default()
                    .trim()
                    .is_empty()
                {
                    return Err(JournalError::Semantic(format!(
                        "evidence[{idx}] file_line requires path"
                    )));
                }
                if evidence
                    .lines
                    .as_deref()
                    .unwrap_or_default()
                    .trim()
                    .is_empty()
                {
                    return Err(JournalError::Semantic(format!(
                        "evidence[{idx}] file_line requires lines"
                    )));
                }
            }
            EvidenceKind::CommandLog | EvidenceKind::TestResult => {
                if evidence
                    .path
                    .as_deref()
                    .unwrap_or_default()
                    .trim()
                    .is_empty()
                {
                    return Err(JournalError::Semantic(format!(
                        "evidence[{idx}] {:?} requires path",
                        evidence.kind
                    )));
                }
            }
            EvidenceKind::Commit => {
                if evidence
                    .commit
                    .as_deref()
                    .unwrap_or_default()
                    .trim()
                    .is_empty()
                {
                    return Err(JournalError::Semantic(format!(
                        "evidence[{idx}] commit requires commit"
                    )));
                }
            }
        }
    }
    Ok(())
}

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
}
