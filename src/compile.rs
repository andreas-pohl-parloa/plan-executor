//! Pre-compile step: transform a plan markdown file into a schema-validated
//! tasks.json manifest via the `plan-executor:compile-plan` skill. Cached by
//! content hash so re-runs are cheap.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

const SCHEMA_VERSION: u32 = 1;
const MAX_ATTEMPTS: u32 = 3;

/// Error variants produced during the compile-plan step.
#[derive(Debug, Error)]
pub enum CompileError {
    #[error("cannot read plan file {path}: {source}")]
    ReadPlan {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot create cache directory {path}: {source}")]
    CreateCache {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("compile-plan skill not available in `claude` CLI: {0}. Install: claude plugin install plan-executor@plan-executor")]
    SkillMissing(String),
    #[error("compile-plan subprocess failed: exit {exit} stdout={stdout:?} stderr={stderr:?}")]
    SubprocessFailed {
        exit: i32,
        stdout: String,
        stderr: String,
    },
    #[error("compile-plan output did not contain `COMPILED:` line; stdout={0:?}")]
    MissingCompiledMarker(String),
    #[error("compile-plan produced an invalid manifest after {attempts} attempts; errors: {errors:?}")]
    ValidationFailed {
        attempts: u32,
        errors: Vec<CompileValidationError>,
    },
}

/// Single validation error surfaced from the compiled manifest.
#[derive(Debug, Clone, Serialize)]
pub struct CompileValidationError {
    /// One of `"schema"` or `"semantic"`.
    pub kind: String,
    /// Human-readable message.
    pub message: String,
}

/// Compiles the plan at `plan_path` to a schema-validated manifest and returns
/// the absolute path to `tasks.json` on success. Caches results by content hash
/// so repeated invocations are cheap when the plan hasn't changed.
///
/// # Errors
///
/// Returns `CompileError` if the plan cannot be read, the cache directory
/// cannot be created, the skill subprocess is missing or fails, or the
/// produced manifest is invalid after `MAX_ATTEMPTS` retries.
pub fn compile_plan_to_manifest(
    plan_path: &Path,
    execution_root: &Path,
) -> Result<PathBuf, CompileError> {
    let hash = content_hash(plan_path)?;
    let cache_dir = execution_root.join(".tmp-plan-compiled").join(&hash);
    let tasks_json = cache_dir.join("tasks.json");

    if tasks_json.exists() {
        match read_and_validate(&tasks_json, &cache_dir) {
            Ok(()) => return Ok(tasks_json),
            Err(_) => {
                // Poisoned cache — remove and fall through.
                let _ = std::fs::remove_dir_all(&cache_dir);
            }
        }
    }

    std::fs::create_dir_all(&cache_dir).map_err(|e| CompileError::CreateCache {
        path: cache_dir.clone(),
        source: e,
    })?;

    let schema_path = find_schema_path(execution_root);

    let mut last_errors: Vec<CompileValidationError> = Vec::new();
    for attempt in 1..=MAX_ATTEMPTS {
        invoke_compile_skill(plan_path, schema_path.as_deref(), &cache_dir, &last_errors)?;
        match read_and_validate(&tasks_json, &cache_dir) {
            Ok(()) => return Ok(tasks_json),
            Err(errors) => {
                if attempt == MAX_ATTEMPTS {
                    return Err(CompileError::ValidationFailed {
                        attempts: MAX_ATTEMPTS,
                        errors,
                    });
                }
                last_errors = errors;
            }
        }
    }
    unreachable!()
}

fn content_hash(plan_path: &Path) -> Result<String, CompileError> {
    let bytes = std::fs::read(plan_path).map_err(|e| CompileError::ReadPlan {
        path: plan_path.to_path_buf(),
        source: e,
    })?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    hasher.update(SCHEMA_VERSION.to_le_bytes());
    Ok(hex_encode(&hasher.finalize()))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn read_and_validate(
    tasks_json: &Path,
    manifest_dir: &Path,
) -> Result<(), Vec<CompileValidationError>> {
    let raw = std::fs::read_to_string(tasks_json).map_err(|e| vec![CompileValidationError {
        kind: "schema".into(),
        message: format!("cannot read manifest: {e}"),
    }])?;
    let manifest: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
        vec![CompileValidationError {
            kind: "schema".into(),
            message: format!("manifest is not valid JSON: {e}"),
        }]
    })?;

    let mut errors: Vec<CompileValidationError> = Vec::new();
    if let Err(schema_errs) = crate::schema::validate_manifest(&manifest) {
        errors.extend(schema_errs.into_iter().map(|e| CompileValidationError {
            kind: "schema".into(),
            message: format!("{} (at {})", e.message, e.path),
        }));
    }
    if let Err(sem_errs) = crate::validate::semantic_check(&manifest, manifest_dir) {
        errors.extend(sem_errs.into_iter().map(|e| CompileValidationError {
            kind: "semantic".into(),
            message: format!("{}: {}", e.category, e.message),
        }));
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn find_schema_path(execution_root: &Path) -> Option<PathBuf> {
    // Prefer the plugin-installed copy. If plugins are not checked out yet,
    // the subprocess reads its own packaged schema — skill receives Option<path>
    // via the third argument when present.
    let candidates = [execution_root
        .join(".claude")
        .join("plugins")
        .join("plan-executor")
        .join("skills")
        .join("compile-plan")
        .join("tasks.schema.json")];
    candidates.into_iter().find(|p| p.is_file())
}

fn invoke_compile_skill(
    plan_path: &Path,
    schema_path: Option<&Path>,
    cache_dir: &Path,
    prior_errors: &[CompileValidationError],
) -> Result<(), CompileError> {
    // The skill takes three positional arguments: plan-path, schema-path, output-dir.
    // If schema-path is None, pass the string "default" which the skill must resolve
    // to its packaged copy. If prior errors exist, prepend them to the prompt.
    let schema_arg = schema_path
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "default".to_string());

    let mut prompt = format!(
        "/plan-executor:compile-plan \"{}\" \"{}\" \"{}\"",
        plan_path.display(),
        schema_arg,
        cache_dir.display(),
    );
    if !prior_errors.is_empty() {
        prompt.push_str("\n\nPrevious attempt produced these validation errors. Fix them and recompile:\n");
        for e in prior_errors {
            prompt.push_str(&format!("- [{}] {}\n", e.kind, e.message));
        }
    }

    let output = Command::new("claude")
        .arg("-p")
        .arg(&prompt)
        .output()
        .map_err(|e| CompileError::SkillMissing(e.to_string()))?;

    if !output.status.success() {
        return Err(CompileError::SubprocessFailed {
            exit: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.contains("COMPILED:") {
        return Err(CompileError::MissingCompiledMarker(stdout.into_owned()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write_plan(dir: &Path, contents: &str) -> PathBuf {
        let path = dir.join("plan.md");
        fs::write(&path, contents).unwrap();
        path
    }

    fn write_valid_manifest(cache_dir: &Path) {
        fs::create_dir_all(cache_dir.join("tasks")).unwrap();
        fs::write(cache_dir.join("tasks/t1.md"), "").unwrap();
        let manifest = serde_json::json!({
            "version": 1,
            "plan": {
                "goal": "t", "type": "feature",
                "flags": {
                    "merge": false, "merge_admin": false, "skip_pr": false,
                    "skip_code_review": false, "no_worktree": false, "draft_pr": false
                }
            },
            "waves": [{"id": 1, "task_ids": ["t1"], "depends_on": []}],
            "tasks": {"t1": {"prompt_file": "tasks/t1.md", "agent_type": "claude"}}
        });
        fs::write(
            cache_dir.join("tasks.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn cache_hit_returns_without_invoking_claude() {
        let dir = tempdir().unwrap();
        let plan = write_plan(dir.path(), "# Plan\n");
        let hash = content_hash(&plan).unwrap();
        let cache_dir = dir.path().join(".tmp-plan-compiled").join(&hash);
        write_valid_manifest(&cache_dir);

        let result = compile_plan_to_manifest(&plan, dir.path());
        assert!(result.is_ok(), "cache hit should return Ok");
        assert_eq!(result.unwrap(), cache_dir.join("tasks.json"));
    }

    #[test]
    fn invalid_cache_is_removed_and_recompile_attempted() {
        // Without a working `claude` binary, invoke_compile_skill will fail —
        // but we only care that the poisoned cache was removed before the
        // attempt. We stop short of asserting exit path; we verify the cache
        // dir is scrubbed.
        let dir = tempdir().unwrap();
        let plan = write_plan(dir.path(), "# Plan v2\n");
        let hash = content_hash(&plan).unwrap();
        let cache_dir = dir.path().join(".tmp-plan-compiled").join(&hash);
        fs::create_dir_all(&cache_dir).unwrap();
        // poisoned manifest — missing required fields
        fs::write(cache_dir.join("tasks.json"), r#"{"not": "valid"}"#).unwrap();

        // Run compile; we expect it to fail (either SkillMissing, SubprocessFailed,
        // or ValidationFailed after MAX_ATTEMPTS) but BEFORE that, it must have
        // attempted to remove the poisoned cache and recreated the dir.
        let _ = compile_plan_to_manifest(&plan, dir.path());

        // The cache dir should exist (recreated) but tasks.json should be absent
        // (no successful compile happened).
        assert!(cache_dir.exists(), "cache dir should have been recreated");
        assert!(
            !cache_dir.join("tasks.json").exists(),
            "poisoned manifest should have been removed"
        );
    }

    #[test]
    fn hash_changes_when_plan_content_changes() {
        let dir = tempdir().unwrap();
        let plan = write_plan(dir.path(), "# Plan\n");
        let h1 = content_hash(&plan).unwrap();
        fs::write(&plan, "# Plan (modified)\n").unwrap();
        let h2 = content_hash(&plan).unwrap();
        assert_ne!(h1, h2);
    }
}
