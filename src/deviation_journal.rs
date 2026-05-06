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
