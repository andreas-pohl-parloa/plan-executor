//! Code-review finding type. Mirrors `findings.schema.json` from the plugin repo.
//!
//! This type is the input to `compile::append_fix_waves`: each `Finding` becomes
//! one (or more) tasks in a fix-wave appended to an existing compiled manifest.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Severity buckets recognized by the review helper output.
///
/// The string variants exactly match the JSON enum in `findings.schema.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Critical,
    Major,
    Minor,
    Nit,
}

/// One reviewer finding. Required fields are mandatory at the JSON-schema level
/// and at the Rust-type level too; missing fields fail deserialization.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    pub id: String,
    pub severity: Severity,
    pub category: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_fix: Option<String>,
}

/// Top-level shape of a `findings.json` document.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FindingsFile {
    pub findings: Vec<Finding>,
}

/// Errors specific to parsing a findings file.
#[derive(Debug, Error)]
pub enum FindingError {
    #[error("findings JSON failed to parse: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("findings file failed to read: {0}")]
    Io(#[from] std::io::Error),
}

impl FindingsFile {
    /// Parses a findings document from raw JSON bytes.
    pub fn from_slice(bytes: &[u8]) -> Result<Self, FindingError> {
        Ok(serde_json::from_slice(bytes)?)
    }

    /// Reads + parses a findings document from a file path.
    pub fn from_path(path: &std::path::Path) -> Result<Self, FindingError> {
        let bytes = std::fs::read(path)?;
        Self::from_slice(&bytes)
    }
}

impl TryFrom<&serde_json::Value> for Finding {
    type Error = serde_json::Error;
    fn try_from(value: &serde_json::Value) -> Result<Self, Self::Error> {
        serde_json::from_value(value.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_json() -> &'static str {
        r#"{
          "findings": [
            {
              "id": "F1",
              "severity": "major",
              "category": "error-handling",
              "description": "swallows parse errors",
              "files": ["src/compile.rs"],
              "suggested_fix": "return Err(...)"
            },
            {
              "id": "F2",
              "severity": "minor",
              "category": "naming",
              "description": "field name shadows prelude term"
            }
          ]
        }"#
    }

    #[test]
    fn parses_findings_file() {
        let parsed = FindingsFile::from_slice(sample_json().as_bytes())
            .expect("sample must parse");
        assert_eq!(parsed.findings.len(), 2);

        let f1 = &parsed.findings[0];
        assert_eq!(f1.id, "F1");
        assert_eq!(f1.severity, Severity::Major);
        assert_eq!(f1.category, "error-handling");
        assert_eq!(f1.files, vec!["src/compile.rs".to_string()]);
        assert_eq!(f1.suggested_fix.as_deref(), Some("return Err(...)"));

        let f2 = &parsed.findings[1];
        assert_eq!(f2.severity, Severity::Minor);
        assert!(f2.files.is_empty());
        assert!(f2.suggested_fix.is_none());
    }

    #[test]
    fn roundtrip_preserves_shape() {
        let original = FindingsFile::from_slice(sample_json().as_bytes()).unwrap();
        let serialized = serde_json::to_string(&original).unwrap();
        let reparsed = FindingsFile::from_slice(serialized.as_bytes()).unwrap();
        assert_eq!(original, reparsed);
    }

    #[test]
    fn rejects_unknown_severity() {
        let bad = r#"{"findings":[{"id":"x","severity":"trivia","category":"c","description":"d"}]}"#;
        let err = FindingsFile::from_slice(bad.as_bytes())
            .expect_err("unknown severity must reject");
        let msg = format!("{err}");
        assert!(msg.contains("severity") || msg.contains("variant"), "msg was: {msg}");
    }

    #[test]
    fn rejects_unknown_top_level_field() {
        let bad = r#"{"findings":[],"extra":"nope"}"#;
        let err = FindingsFile::from_slice(bad.as_bytes())
            .expect_err("unknown field must reject");
        let msg = format!("{err}");
        assert!(msg.contains("extra") || msg.contains("unknown"), "msg was: {msg}");
    }

    #[test]
    fn try_from_json_value() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"id":"x","severity":"critical","category":"c","description":"d"}"#
        ).unwrap();
        let f: Finding = (&v).try_into().expect("value must convert");
        assert_eq!(f.severity, Severity::Critical);
    }

    #[test]
    fn requires_required_fields() {
        let bad = r#"{"findings":[{"id":"x","severity":"nit"}]}"#;
        let err = FindingsFile::from_slice(bad.as_bytes())
            .expect_err("missing required fields must reject");
        let _ = err; // we just need ANY error
    }
}
