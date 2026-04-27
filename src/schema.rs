//! JSON Schema validation for compiled-plan manifests.

use std::sync::OnceLock;

use serde::Serialize;
use thiserror::Error;

/// Baked-in schema JSON — single source of truth matches the plugin-repo copy.
const SCHEMA_JSON: &str = include_str!("schemas/tasks.schema.json");

/// Returns the embedded `tasks.schema.json` content as a static string.
///
/// Crate-internal accessor used by `compile::embedded_schema_path` to
/// materialize the schema to a temp file at runtime.
pub(crate) fn embedded_schema_json() -> &'static str {
    SCHEMA_JSON
}

#[derive(Debug, Error)]
pub enum SchemaError {
    #[error("schema JSON failed to parse: {0}")]
    ParseFailed(#[from] serde_json::Error),
    #[error("schema is not a valid JSON Schema: {0}")]
    InvalidSchema(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct ValidationError {
    /// JSON path (e.g. `/tasks/t1/agent_type`) to the offending node.
    pub path: String,
    /// Human-readable violation message.
    pub message: String,
}

/// Compiles the embedded schema once and caches the `Validator`.
/// Panics only on a schema file that fails schema-meta validation — that is a
/// build-time / ship-time bug, not a runtime failure the caller can recover from.
pub fn compile_schema() -> Result<&'static jsonschema::Validator, SchemaError> {
    static VALIDATOR: OnceLock<jsonschema::Validator> = OnceLock::new();
    if let Some(v) = VALIDATOR.get() {
        return Ok(v);
    }
    let schema: serde_json::Value = serde_json::from_str(SCHEMA_JSON)?;
    let v = jsonschema::validator_for(&schema)
        .map_err(|e| SchemaError::InvalidSchema(e.to_string()))?;
    Ok(VALIDATOR.get_or_init(|| v))
}

/// Validates a parsed manifest against the schema. Returns `Ok(())` on pass or
/// `Err(Vec<ValidationError>)` on any schema violations.
pub fn validate_manifest(
    manifest: &serde_json::Value,
) -> Result<(), Vec<ValidationError>> {
    let validator = match compile_schema() {
        Ok(v) => v,
        Err(e) => return Err(vec![ValidationError {
            path: String::new(),
            message: format!("schema compile failed: {e}"),
        }]),
    };
    let errors: Vec<_> = validator
        .iter_errors(manifest)
        .map(|e| ValidationError {
            path: e.instance_path().to_string(),
            message: e.to_string(),
        })
        .collect();
    if errors.is_empty() { Ok(()) } else { Err(errors) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_valid_plan_block() -> serde_json::Value {
        serde_json::json!({
            "goal": "test",
            "type": "feature",
            "path": "/tmp/plan.md",
            "status": "READY",
            "flags": {
                "merge": false, "merge_admin": false, "skip_pr": false,
                "skip_code_review": false, "no_worktree": false, "draft_pr": false
            }
        })
    }

    #[test]
    fn schema_parses_at_compile_time() {
        assert!(compile_schema().is_ok(), "embedded schema must meta-validate");
    }

    #[test]
    fn valid_manifest_passes() {
        let manifest = serde_json::json!({
            "version": 1,
            "plan": minimal_valid_plan_block(),
            "waves": [{"id": 1, "task_ids": ["t1"], "depends_on": []}],
            "tasks": {"t1": {"prompt_file": "tasks/t1.md", "agent_type": "claude"}}
        });
        assert!(validate_manifest(&manifest).is_ok());
    }

    #[test]
    fn invalid_agent_type_rejected() {
        let manifest = serde_json::json!({
            "version": 1,
            "plan": minimal_valid_plan_block(),
            "waves": [{"id": 1, "task_ids": ["t1"], "depends_on": []}],
            "tasks": {"t1": {"prompt_file": "tasks/t1.md", "agent_type": "general-purpose"}}
        });
        assert!(validate_manifest(&manifest).is_err());
    }

    #[test]
    fn missing_required_flag_rejected() {
        // Omit `draft_pr` from plan.flags.
        let manifest = serde_json::json!({
            "version": 1,
            "plan": {
                "goal": "t", "type": "feature",
                "path": "/tmp/plan.md", "status": "READY",
                "flags": {
                    "merge": false, "merge_admin": false, "skip_pr": false,
                    "skip_code_review": false, "no_worktree": false
                }
            },
            "waves": [{"id": 1, "task_ids": ["t1"], "depends_on": []}],
            "tasks": {"t1": {"prompt_file": "tasks/t1.md", "agent_type": "claude"}}
        });
        assert!(validate_manifest(&manifest).is_err());
    }

    #[test]
    fn missing_plan_path_rejected() {
        let manifest = serde_json::json!({
            "version": 1,
            "plan": {
                "goal": "t", "type": "feature", "status": "READY",
                "flags": {
                    "merge": false, "merge_admin": false, "skip_pr": false,
                    "skip_code_review": false, "no_worktree": false, "draft_pr": false
                }
            },
            "waves": [{"id": 1, "task_ids": ["t1"], "depends_on": []}],
            "tasks": {"t1": {"prompt_file": "tasks/t1.md", "agent_type": "claude"}}
        });
        assert!(validate_manifest(&manifest).is_err());
    }

    #[test]
    fn invalid_plan_status_rejected() {
        let manifest = serde_json::json!({
            "version": 1,
            "plan": {
                "goal": "t", "type": "feature",
                "path": "/tmp/plan.md", "status": "WHATEVER",
                "flags": {
                    "merge": false, "merge_admin": false, "skip_pr": false,
                    "skip_code_review": false, "no_worktree": false, "draft_pr": false
                }
            },
            "waves": [{"id": 1, "task_ids": ["t1"], "depends_on": []}],
            "tasks": {"t1": {"prompt_file": "tasks/t1.md", "agent_type": "claude"}}
        });
        assert!(validate_manifest(&manifest).is_err());
    }
}
