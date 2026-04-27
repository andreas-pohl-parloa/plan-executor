//! Fix-loop integration with the `plan-executor:compile-plan` skill.
//!
//! `append_fix_waves` takes an existing compiled manifest and a slice of
//! reviewer findings, invokes the compile-plan skill in APPEND mode, and
//! returns the path to the updated manifest. The skill subprocess is the
//! authority for fix-wave layout; this module owns: locating the manifest,
//! synthesizing the meta.json sidecar from the manifest's `plan` block,
//! writing findings.json, validating the post-append manifest, and
//! enforcing structural invariants (original waves preserved, fix-wave
//! IDs >= 100, fix-wave depends_on includes last impl wave).

use std::path::{Path, PathBuf};
use std::process::Command;

use thiserror::Error;

use crate::finding::Finding;
use crate::schema::{validate_manifest, ValidationError};

/// Errors surfaced by `append_fix_waves`.
#[derive(Debug, Error)]
pub enum AppendError {
    #[error("manifest read failed: {0}")]
    ManifestRead(std::io::Error),
    #[error("manifest JSON parse failed: {0}")]
    ManifestParse(serde_json::Error),
    #[error("manifest schema validation failed: {0}")]
    ManifestSchema(String),
    #[error("manifest is missing required field: {0}")]
    ManifestField(String),
    #[error("findings serialize failed: {0}")]
    FindingsSerialize(serde_json::Error),
    #[error("findings write failed: {0}")]
    FindingsWrite(std::io::Error),
    #[error("meta.json synthesize failed: {0}")]
    MetaSynthesize(String),
    #[error("meta.json write failed: {0}")]
    MetaWrite(std::io::Error),
    #[error("compile-plan skill invocation failed: {0}")]
    Invoke(String),
    #[error("compile-plan skill exited non-zero: {0}")]
    InvokeExit(String),
    #[error("post-append manifest re-read failed: {0}")]
    PostReread(std::io::Error),
    #[error("post-append manifest invalid: {0}")]
    PostInvalid(String),
    #[error("post-append invariant violated: {0}")]
    InvariantViolation(String),
}

/// Trait used to invoke the compile-plan skill. Production callers use the
/// default `ClaudeInvoker`; tests inject `FakeInvoker` writing a canned
/// post-append manifest.
pub trait CompileInvoker {
    /// Run the compile-plan skill with the given arguments.
    /// `args` is `[plan_path, schema_path, output_dir, meta_json_path, findings_json_path]`.
    /// Returns Ok(()) on success or Err(message) on failure.
    fn invoke(&self, args: &[&Path]) -> Result<(), String>;
}

/// Production implementation: spawns `claude -p "/plan-executor:compile-plan ..."`.
pub struct ClaudeInvoker;

impl CompileInvoker for ClaudeInvoker {
    fn invoke(&self, args: &[&Path]) -> Result<(), String> {
        if args.len() != 5 {
            return Err(format!("expected 5 args, got {}", args.len()));
        }
        let prompt = format!(
            "/plan-executor:compile-plan {} {} {} {} {}",
            args[0].display(),
            args[1].display(),
            args[2].display(),
            args[3].display(),
            args[4].display(),
        );
        let out = Command::new("claude")
            .arg("-p")
            .arg(&prompt)
            .output()
            .map_err(|e| format!("spawn failed: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "claude exited {:?}; stderr={}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        if !stdout.lines().any(|l| l.starts_with("COMPILED:")) {
            return Err(format!(
                "compile-plan did not emit COMPILED: line; stdout was: {}",
                stdout.trim()
            ));
        }
        Ok(())
    }
}

/// Public entry point. Production callers pass &ClaudeInvoker.
pub fn append_fix_waves(
    manifest_path: &Path,
    findings: &[Finding],
) -> Result<PathBuf, AppendError> {
    append_fix_waves_with_invoker(&ClaudeInvoker, manifest_path, findings)
}

/// Test-injection variant. Splits invoker out so unit tests can supply a fake.
pub fn append_fix_waves_with_invoker(
    invoker: &dyn CompileInvoker,
    manifest_path: &Path,
    findings: &[Finding],
) -> Result<PathBuf, AppendError> {
    // Step 1 — read existing manifest.
    let manifest_bytes = std::fs::read(manifest_path).map_err(AppendError::ManifestRead)?;
    let manifest: serde_json::Value =
        serde_json::from_slice(&manifest_bytes).map_err(AppendError::ManifestParse)?;
    if let Err(errors) = validate_manifest(&manifest) {
        return Err(AppendError::ManifestSchema(errors_summary(&errors)));
    }

    // Step 2 — capture pre-append snapshot of waves + tasks for invariant check.
    let pre_waves = manifest
        .get("waves")
        .and_then(|v| v.as_array())
        .cloned()
        .ok_or_else(|| AppendError::ManifestField("waves".into()))?;
    let pre_tasks_keys: Vec<String> = manifest
        .get("tasks")
        .and_then(|v| v.as_object())
        .map(|o| o.keys().cloned().collect())
        .ok_or_else(|| AppendError::ManifestField("tasks".into()))?;
    let pre_max_fix_wave_id = max_fix_wave_id(&pre_waves);
    let pre_last_impl_wave_id = last_implementation_wave_id(&pre_waves)
        .ok_or_else(|| AppendError::ManifestField("no implementation wave found".into()))?;

    // Step 3 — locate plan markdown ($1) from manifest.
    let plan_path_str = manifest
        .pointer("/plan/path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppendError::ManifestField("plan.path".into()))?;
    let plan_path = PathBuf::from(plan_path_str);

    // Step 4 — schema path. Use the embedded schema's filesystem location next
    // to the binary's source. For the runtime, `tasks.schema.json` lives at
    // `src/schemas/tasks.schema.json` relative to the crate root. Production
    // code resolves it via `CARGO_MANIFEST_DIR` at compile time.
    let schema_path = embedded_schema_path();

    // Step 5 — output-dir is the manifest's parent.
    let manifest_dir = manifest_path
        .parent()
        .ok_or_else(|| AppendError::ManifestField("manifest_path has no parent".into()))?
        .to_path_buf();

    // Step 6 — synthesize meta.json from manifest.plan block.
    let meta_path = manifest_dir.join(".append-meta.json");
    write_synthetic_meta(&meta_path, &manifest)?;

    // Step 7 — write findings.json.
    let findings_path = manifest_dir.join("findings.json");
    let findings_doc = serde_json::json!({ "findings": findings });
    let findings_bytes =
        serde_json::to_vec_pretty(&findings_doc).map_err(AppendError::FindingsSerialize)?;
    std::fs::write(&findings_path, &findings_bytes).map_err(AppendError::FindingsWrite)?;

    // Step 8 — invoke compile-plan APPEND mode via injected invoker.
    let args: [&Path; 5] = [
        &plan_path,
        &schema_path,
        &manifest_dir,
        &meta_path,
        &findings_path,
    ];
    invoker.invoke(&args).map_err(AppendError::Invoke)?;

    // Step 9 — re-read updated manifest.
    let updated_bytes = std::fs::read(manifest_path).map_err(AppendError::PostReread)?;
    let updated: serde_json::Value =
        serde_json::from_slice(&updated_bytes).map_err(AppendError::ManifestParse)?;
    if let Err(errors) = validate_manifest(&updated) {
        return Err(AppendError::PostInvalid(errors_summary(&errors)));
    }

    // Step 10 — invariants.
    enforce_post_append_invariants(
        &updated,
        &pre_waves,
        &pre_tasks_keys,
        pre_last_impl_wave_id,
        pre_max_fix_wave_id,
    )?;

    Ok(manifest_path.to_path_buf())
}

/// Returns the maximum existing fix-wave id (id >= 100), or `None` if none.
fn max_fix_wave_id(waves: &[serde_json::Value]) -> Option<u64> {
    waves
        .iter()
        .filter_map(|w| w.get("id")?.as_u64())
        .filter(|id| *id >= 100)
        .max()
}

/// Returns the maximum impl-wave id (id < 100). `None` if no impl waves.
fn last_implementation_wave_id(waves: &[serde_json::Value]) -> Option<u64> {
    waves
        .iter()
        .filter_map(|w| w.get("id")?.as_u64())
        .filter(|id| *id < 100)
        .max()
}

/// Synthesizes a meta.json sidecar from `manifest.plan` and writes it to `path`.
fn write_synthetic_meta(path: &Path, manifest: &serde_json::Value) -> Result<(), AppendError> {
    let plan = manifest
        .get("plan")
        .ok_or_else(|| AppendError::MetaSynthesize("manifest.plan missing".into()))?;

    let plan_path = plan
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppendError::MetaSynthesize("plan.path missing".into()))?;
    let goal = plan
        .get("goal")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppendError::MetaSynthesize("plan.goal missing".into()))?;
    let plan_type = plan
        .get("type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppendError::MetaSynthesize("plan.type missing".into()))?;

    let meta = serde_json::json!({
        "plan_path": plan_path,
        "goal": goal,
        "type": plan_type,
        "jira": plan.get("jira").and_then(|v| v.as_str()).unwrap_or(""),
        "target_repo": plan.get("target_repo").cloned().unwrap_or(serde_json::Value::Null),
        "target_branch": plan.get("target_branch").cloned().unwrap_or(serde_json::Value::Null),
        "flags": plan.get("flags").cloned().unwrap_or_else(|| serde_json::json!({
            "merge": false, "merge_admin": false, "skip_pr": false,
            "skip_code_review": false, "no_worktree": false, "draft_pr": false
        })),
    });

    let bytes =
        serde_json::to_vec_pretty(&meta).map_err(|e| AppendError::MetaSynthesize(e.to_string()))?;
    std::fs::write(path, &bytes).map_err(AppendError::MetaWrite)?;
    Ok(())
}

/// Resolves the path to the embedded `tasks.schema.json` at runtime.
fn embedded_schema_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/schemas/tasks.schema.json")
}

/// Enforces the post-append invariants the plan's APPEND-mode rules require.
fn enforce_post_append_invariants(
    updated: &serde_json::Value,
    pre_waves: &[serde_json::Value],
    pre_tasks_keys: &[String],
    pre_last_impl_wave_id: u64,
    pre_max_fix_wave_id: Option<u64>,
) -> Result<(), AppendError> {
    let post_waves = updated
        .get("waves")
        .and_then(|v| v.as_array())
        .ok_or_else(|| AppendError::InvariantViolation("post manifest missing waves".into()))?;
    let post_tasks_obj = updated
        .get("tasks")
        .and_then(|v| v.as_object())
        .ok_or_else(|| AppendError::InvariantViolation("post manifest missing tasks".into()))?;

    // Invariant 1: every original wave preserved verbatim.
    for pre in pre_waves {
        let pre_id = pre.get("id").and_then(|v| v.as_u64());
        let matched = post_waves
            .iter()
            .find(|w| w.get("id").and_then(|v| v.as_u64()) == pre_id);
        match matched {
            Some(post) if post == pre => {}
            Some(_) => {
                return Err(AppendError::InvariantViolation(format!(
                    "wave id {pre_id:?} was modified by APPEND"
                )));
            }
            None => {
                return Err(AppendError::InvariantViolation(format!(
                    "wave id {pre_id:?} was dropped by APPEND"
                )));
            }
        }
    }

    // Invariant 2: every original task key preserved.
    for k in pre_tasks_keys {
        if !post_tasks_obj.contains_key(k) {
            return Err(AppendError::InvariantViolation(format!(
                "original task `{k}` was dropped by APPEND"
            )));
        }
    }

    // Invariant 3: there is at least one new wave with id >= 100 and kind == "fix".
    let new_fix_waves: Vec<&serde_json::Value> = post_waves
        .iter()
        .filter(|w| {
            let id = w.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let kind = w
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("implementation");
            id >= 100 && kind == "fix"
        })
        .filter(|w| {
            // exclude waves that already existed pre-append
            let id = w.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            !pre_waves
                .iter()
                .any(|p| p.get("id").and_then(|v| v.as_u64()) == Some(id))
        })
        .collect();
    if new_fix_waves.is_empty() {
        return Err(AppendError::InvariantViolation(
            "no new fix-wave appended (expected at least one wave with id>=100 and kind=fix)"
                .into(),
        ));
    }

    // Invariant 4: each new fix-wave's id is strictly greater than pre_max_fix_wave_id.
    let threshold = pre_max_fix_wave_id.unwrap_or(99);
    for w in &new_fix_waves {
        let id = w.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
        if id <= threshold {
            return Err(AppendError::InvariantViolation(format!(
                "new fix-wave id {id} is not greater than prior max {threshold}"
            )));
        }
    }

    // Invariant 5: every new fix-wave's depends_on includes pre_last_impl_wave_id
    // OR a later fix-wave id. (Round-2 fix-waves may depend on round-1 fix-waves.)
    for w in &new_fix_waves {
        let deps: Vec<u64> = w
            .get("depends_on")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(serde_json::Value::as_u64).collect())
            .unwrap_or_default();
        let ok = deps.iter().any(|d| *d == pre_last_impl_wave_id || *d >= 100);
        if !ok {
            let id = w.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            return Err(AppendError::InvariantViolation(format!(
                "fix-wave {id} depends_on must include the last impl wave \
                 ({pre_last_impl_wave_id}) or a prior fix-wave id; got {deps:?}"
            )));
        }
    }

    Ok(())
}

fn errors_summary(errors: &[ValidationError]) -> String {
    errors
        .iter()
        .take(5)
        .map(|e| format!("{}: {}", e.path, e.message))
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::finding::Severity;
    use std::cell::RefCell;
    use std::fs;

    /// Minimal valid pre-append manifest used by tests. Two impl waves.
    fn fixture_pre_append_manifest() -> serde_json::Value {
        serde_json::json!({
            "version": 1,
            "plan": {
                "goal": "test plan for fix-loop",
                "type": "feature",
                "jira": "",
                "target_repo": null,
                "target_branch": null,
                "path": "/tmp/fixtures/plan.md",
                "status": "READY",
                "flags": {
                    "merge": false, "merge_admin": false, "skip_pr": false,
                    "skip_code_review": false, "no_worktree": false, "draft_pr": false
                }
            },
            "waves": [
                { "id": 1, "task_ids": ["1.1"], "depends_on": [], "kind": "implementation" },
                { "id": 2, "task_ids": ["2.1"], "depends_on": [1], "kind": "implementation" }
            ],
            "tasks": {
                "1.1": { "prompt_file": "tasks/task-1.1.md", "agent_type": "claude" },
                "2.1": { "prompt_file": "tasks/task-2.1.md", "agent_type": "claude" }
            }
        })
    }

    /// Fake invoker that writes a caller-supplied manifest to the output dir.
    struct FakeInvoker {
        write_manifest: serde_json::Value,
        capture_args: RefCell<Vec<PathBuf>>,
    }
    impl CompileInvoker for FakeInvoker {
        fn invoke(&self, args: &[&Path]) -> Result<(), String> {
            *self.capture_args.borrow_mut() = args.iter().map(|p| p.to_path_buf()).collect();
            let output_dir = args[2];
            let target = output_dir.join("tasks.json");
            let bytes = serde_json::to_vec_pretty(&self.write_manifest).unwrap();
            fs::write(&target, &bytes).unwrap();
            Ok(())
        }
    }

    fn write_pre_manifest(dir: &Path, manifest: &serde_json::Value) -> PathBuf {
        let path = dir.join("tasks.json");
        fs::write(&path, serde_json::to_vec_pretty(manifest).unwrap()).unwrap();
        // mock plan.md so plan.path existence checks succeed if added later
        path
    }

    fn fresh_findings() -> Vec<Finding> {
        vec![Finding {
            id: "F1".into(),
            severity: Severity::Major,
            category: "error-handling".into(),
            description: "swallow error".into(),
            files: vec!["src/compile.rs".into()],
            suggested_fix: Some("propagate Err".into()),
        }]
    }

    #[test]
    fn fresh_findings_produce_fix_wave_with_id_100() {
        let tmp = tempfile::tempdir().unwrap();
        let pre = fixture_pre_append_manifest();
        let manifest_path = write_pre_manifest(tmp.path(), &pre);

        // Construct post-append manifest the fake invoker will write.
        let mut post = pre.clone();
        post["waves"].as_array_mut().unwrap().push(serde_json::json!({
            "id": 100,
            "task_ids": ["fix-100-1"],
            "depends_on": [2],
            "kind": "fix"
        }));
        post["tasks"]["fix-100-1"] = serde_json::json!({
            "prompt_file": "tasks/task-fix-100-1.md",
            "agent_type": "claude"
        });

        let invoker = FakeInvoker {
            write_manifest: post,
            capture_args: RefCell::new(vec![]),
        };
        let result = append_fix_waves_with_invoker(&invoker, &manifest_path, &fresh_findings())
            .expect("must succeed");
        assert_eq!(result, manifest_path);

        let reread: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        let waves = reread["waves"].as_array().unwrap();
        assert!(waves
            .iter()
            .any(|w| w["id"].as_u64() == Some(100) && w["kind"] == "fix"));

        // findings.json was written
        assert!(tmp.path().join("findings.json").exists());
        // synthetic meta.json was written
        assert!(tmp.path().join(".append-meta.json").exists());
    }

    #[test]
    fn original_waves_and_tasks_preserved_verbatim() {
        let tmp = tempfile::tempdir().unwrap();
        let pre = fixture_pre_append_manifest();
        let manifest_path = write_pre_manifest(tmp.path(), &pre);

        let mut post = pre.clone();
        post["waves"].as_array_mut().unwrap().push(serde_json::json!({
            "id": 100, "task_ids": ["fix-100-1"], "depends_on": [2], "kind": "fix"
        }));
        post["tasks"]["fix-100-1"] = serde_json::json!({
            "prompt_file": "tasks/task-fix-100-1.md", "agent_type": "claude"
        });

        let invoker = FakeInvoker {
            write_manifest: post,
            capture_args: RefCell::new(vec![]),
        };
        append_fix_waves_with_invoker(&invoker, &manifest_path, &fresh_findings()).unwrap();

        let reread: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        // Original waves identical
        assert_eq!(reread["waves"][0], pre["waves"][0]);
        assert_eq!(reread["waves"][1], pre["waves"][1]);
        // Original tasks identical
        assert_eq!(reread["tasks"]["1.1"], pre["tasks"]["1.1"]);
        assert_eq!(reread["tasks"]["2.1"], pre["tasks"]["2.1"]);
    }

    #[test]
    fn second_round_fix_wave_id_increments() {
        let tmp = tempfile::tempdir().unwrap();
        let mut pre = fixture_pre_append_manifest();
        // Pre-existing fix-wave from round 1
        pre["waves"].as_array_mut().unwrap().push(serde_json::json!({
            "id": 100, "task_ids": ["fix-100-1"], "depends_on": [2], "kind": "fix"
        }));
        pre["tasks"]["fix-100-1"] = serde_json::json!({
            "prompt_file": "tasks/task-fix-100-1.md", "agent_type": "claude"
        });
        let manifest_path = write_pre_manifest(tmp.path(), &pre);

        // Round 2 must use id >= 101
        let mut post = pre.clone();
        post["waves"].as_array_mut().unwrap().push(serde_json::json!({
            "id": 101, "task_ids": ["fix-101-1"], "depends_on": [100], "kind": "fix"
        }));
        post["tasks"]["fix-101-1"] = serde_json::json!({
            "prompt_file": "tasks/task-fix-101-1.md", "agent_type": "claude"
        });

        let invoker = FakeInvoker {
            write_manifest: post,
            capture_args: RefCell::new(vec![]),
        };
        append_fix_waves_with_invoker(&invoker, &manifest_path, &fresh_findings()).unwrap();
    }

    #[test]
    fn rejects_post_append_that_drops_original_wave() {
        let tmp = tempfile::tempdir().unwrap();
        let pre = fixture_pre_append_manifest();
        let manifest_path = write_pre_manifest(tmp.path(), &pre);

        // Bad: drop wave 1 entirely
        let post = serde_json::json!({
            "version": 1,
            "plan": pre["plan"],
            "waves": [
                pre["waves"][1].clone(),
                {
                    "id": 100, "task_ids": ["fix-100-1"], "depends_on": [2], "kind": "fix"
                }
            ],
            "tasks": {
                "1.1": pre["tasks"]["1.1"].clone(),
                "2.1": pre["tasks"]["2.1"].clone(),
                "fix-100-1": {
                    "prompt_file": "tasks/task-fix-100-1.md", "agent_type": "claude"
                }
            }
        });

        let invoker = FakeInvoker {
            write_manifest: post,
            capture_args: RefCell::new(vec![]),
        };
        let err = append_fix_waves_with_invoker(&invoker, &manifest_path, &fresh_findings())
            .expect_err("dropping a pre-existing wave must fail");
        assert!(matches!(err, AppendError::InvariantViolation(_)));
    }

    #[test]
    fn rejects_post_append_with_no_new_fix_wave() {
        let tmp = tempfile::tempdir().unwrap();
        let pre = fixture_pre_append_manifest();
        let manifest_path = write_pre_manifest(tmp.path(), &pre);

        // Bad: no fix-wave added
        let post = pre.clone();
        let invoker = FakeInvoker {
            write_manifest: post,
            capture_args: RefCell::new(vec![]),
        };
        let err = append_fix_waves_with_invoker(&invoker, &manifest_path, &fresh_findings())
            .expect_err("no new fix-wave must fail");
        assert!(matches!(err, AppendError::InvariantViolation(_)));
    }

    #[test]
    fn rejects_post_append_with_misnumbered_fix_wave() {
        let tmp = tempfile::tempdir().unwrap();
        let mut pre = fixture_pre_append_manifest();
        pre["waves"].as_array_mut().unwrap().push(serde_json::json!({
            "id": 100, "task_ids": ["fix-100-1"], "depends_on": [2], "kind": "fix"
        }));
        pre["tasks"]["fix-100-1"] = serde_json::json!({
            "prompt_file": "tasks/task-fix-100-1.md", "agent_type": "claude"
        });
        let manifest_path = write_pre_manifest(tmp.path(), &pre);

        // Bad: round-2 fix-wave reuses id 100 (must be >=101)
        let mut post = pre.clone();
        post["waves"].as_array_mut().unwrap().push(serde_json::json!({
            "id": 100, "task_ids": ["fix-100-1b"], "depends_on": [2], "kind": "fix"
        }));
        // skip adding fix-100-1b to tasks → schema will fail; actually, schema validation
        // catches the duplicate wave first. Use 99 instead to trigger our invariant only.
        post["waves"].as_array_mut().unwrap().pop();
        post["waves"].as_array_mut().unwrap().push(serde_json::json!({
            "id": 99, "task_ids": ["fix-99-1"], "depends_on": [2], "kind": "fix"
        }));
        post["tasks"]["fix-99-1"] = serde_json::json!({
            "prompt_file": "tasks/task-fix-99-1.md", "agent_type": "claude"
        });

        let invoker = FakeInvoker {
            write_manifest: post,
            capture_args: RefCell::new(vec![]),
        };
        let err = append_fix_waves_with_invoker(&invoker, &manifest_path, &fresh_findings())
            .expect_err("fix-wave id <= prior max must fail");
        assert!(matches!(err, AppendError::InvariantViolation(_)));
    }

    #[test]
    fn rejects_post_append_with_bad_dependency() {
        let tmp = tempfile::tempdir().unwrap();
        let pre = fixture_pre_append_manifest();
        let manifest_path = write_pre_manifest(tmp.path(), &pre);

        // Bad: fix-wave depends only on wave 1, not last impl wave (2) or a prior fix-wave
        let mut post = pre.clone();
        post["waves"].as_array_mut().unwrap().push(serde_json::json!({
            "id": 100, "task_ids": ["fix-100-1"], "depends_on": [1], "kind": "fix"
        }));
        post["tasks"]["fix-100-1"] = serde_json::json!({
            "prompt_file": "tasks/task-fix-100-1.md", "agent_type": "claude"
        });

        let invoker = FakeInvoker {
            write_manifest: post,
            capture_args: RefCell::new(vec![]),
        };
        let err = append_fix_waves_with_invoker(&invoker, &manifest_path, &fresh_findings())
            .expect_err("fix-wave with bad depends_on must fail");
        assert!(matches!(err, AppendError::InvariantViolation(_)));
    }

    #[test]
    fn synthesizes_meta_json_from_manifest_plan_block() {
        let tmp = tempfile::tempdir().unwrap();
        let pre = fixture_pre_append_manifest();
        let manifest_path = write_pre_manifest(tmp.path(), &pre);

        let mut post = pre.clone();
        post["waves"].as_array_mut().unwrap().push(serde_json::json!({
            "id": 100, "task_ids": ["fix-100-1"], "depends_on": [2], "kind": "fix"
        }));
        post["tasks"]["fix-100-1"] = serde_json::json!({
            "prompt_file": "tasks/task-fix-100-1.md", "agent_type": "claude"
        });

        let invoker = FakeInvoker {
            write_manifest: post,
            capture_args: RefCell::new(vec![]),
        };
        append_fix_waves_with_invoker(&invoker, &manifest_path, &fresh_findings()).unwrap();

        let meta_bytes = fs::read(tmp.path().join(".append-meta.json")).unwrap();
        let meta: serde_json::Value = serde_json::from_slice(&meta_bytes).unwrap();
        assert_eq!(meta["plan_path"], "/tmp/fixtures/plan.md");
        assert_eq!(meta["goal"], "test plan for fix-loop");
        assert_eq!(meta["type"], "feature");
        assert_eq!(meta["jira"], "");
        assert_eq!(meta["flags"]["merge"], false);
    }
}
