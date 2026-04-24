//! Semantic checks beyond the JSON Schema.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct SemanticError {
    pub category: String,
    pub message: String,
}

/// Runs all semantic checks. Returns `Ok(())` if all pass, or `Err(Vec<...>)`
/// with one entry per defect. Does NOT short-circuit — callers may want the
/// full list for a single-pass reporter.
///
/// `manifest_dir` is the directory containing `tasks.json`; `prompt_file`
/// paths are resolved relative to it.
pub fn semantic_check(
    manifest: &serde_json::Value,
    manifest_dir: &Path,
) -> Result<(), Vec<SemanticError>> {
    let mut errors: Vec<SemanticError> = Vec::new();

    let waves = manifest.get("waves").and_then(|v| v.as_array());
    let tasks = manifest.get("tasks").and_then(|v| v.as_object());

    // Skip semantic checks if the shape is so broken the schema pass should
    // already have failed. The CLI runs schema first and still prints its
    // errors, so callers will see those first.
    let (Some(waves), Some(tasks)) = (waves, tasks) else {
        return Ok(());
    };

    // Check 5 + 6 + 1 + 2 + 3 together — one pass over waves.
    let task_keys: HashSet<&str> = tasks.keys().map(String::as_str).collect();
    let mut wave_ids: HashSet<i64> = HashSet::new();
    let mut adjacency: HashMap<i64, Vec<i64>> = HashMap::new();

    for wave in waves {
        let id = wave.get("id").and_then(|v| v.as_i64()).unwrap_or(-1);
        if id < 1 { continue; }

        if !wave_ids.insert(id) {
            errors.push(SemanticError {
                category: "duplicate_wave_id".into(),
                message: format!("wave id {id} appears more than once"),
            });
        }

        // Check 6 — duplicate task_ids within same wave.
        let mut seen_in_wave: HashSet<&str> = HashSet::new();
        if let Some(ids) = wave.get("task_ids").and_then(|v| v.as_array()) {
            for tid_v in ids {
                if let Some(tid) = tid_v.as_str() {
                    if !seen_in_wave.insert(tid) {
                        errors.push(SemanticError {
                            category: "duplicate_task_in_wave".into(),
                            message: format!("task_id `{tid}` appears more than once in wave {id}"),
                        });
                    }
                    // Check 1 — task_id must exist.
                    if !task_keys.contains(tid) {
                        errors.push(SemanticError {
                            category: "missing_task".into(),
                            message: format!("wave {id} references task_id `{tid}` which is not in tasks map"),
                        });
                    }
                }
            }
        }

        // Check 2 — depends_on references existing waves. Build adjacency while we're here.
        let deps: Vec<i64> = wave.get("depends_on")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|d| d.as_i64()).collect())
            .unwrap_or_default();
        adjacency.insert(id, deps);
    }

    for (id, deps) in &adjacency {
        for dep in deps {
            if !wave_ids.contains(dep) {
                errors.push(SemanticError {
                    category: "missing_wave".into(),
                    message: format!("wave {id} depends_on missing wave {dep}"),
                });
            }
        }
    }

    // Check 3 — acyclic DAG via Kahn's algorithm. Build in-degree using the
    // REVERSE of depends_on so that Kahn processes "earlier" waves first.
    let mut in_degree: HashMap<i64, usize> = wave_ids.iter().map(|&id| (id, 0)).collect();
    for (id, deps) in &adjacency {
        // `id` depends on each `dep`; so `id` has in_degree equal to number of live deps.
        let live_deps = deps.iter().filter(|d| wave_ids.contains(d)).count();
        *in_degree.entry(*id).or_insert(0) += live_deps;
    }
    let mut queue: Vec<i64> = in_degree
        .iter()
        .filter_map(|(id, d)| if *d == 0 { Some(*id) } else { None })
        .collect();
    let mut visited: HashSet<i64> = HashSet::new();
    while let Some(w) = queue.pop() {
        visited.insert(w);
        // any wave depending on `w` loses 1 from its in-degree
        for (other_id, deps) in &adjacency {
            if deps.contains(&w) && !visited.contains(other_id) {
                if let Some(d) = in_degree.get_mut(other_id) {
                    *d = d.saturating_sub(1);
                    if *d == 0 { queue.push(*other_id); }
                }
            }
        }
    }
    if visited.len() != wave_ids.len() {
        let unvisited: Vec<i64> = wave_ids.difference(&visited).copied().collect();
        errors.push(SemanticError {
            category: "cycle".into(),
            message: format!("wave DAG has a cycle involving waves {unvisited:?}"),
        });
    }

    // Check 4 — prompt_file paths exist on disk.
    for (tid, task_spec) in tasks {
        if let Some(pf) = task_spec.get("prompt_file").and_then(|v| v.as_str()) {
            let full = manifest_dir.join(pf);
            if !full.is_file() {
                errors.push(SemanticError {
                    category: "missing_prompt_file".into(),
                    message: format!(
                        "task `{tid}` prompt_file `{pf}` does not exist (resolved to {})",
                        full.display()
                    ),
                });
            }
        }
    }

    if errors.is_empty() { Ok(()) } else { Err(errors) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn make_manifest(waves: serde_json::Value, tasks: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "version": 1,
            "plan": {
                "goal": "x", "type": "feature",
                "flags": {
                    "merge": false, "merge_admin": false, "skip_pr": false,
                    "skip_code_review": false, "no_worktree": false, "draft_pr": false
                }
            },
            "waves": waves,
            "tasks": tasks
        })
    }

    fn tempdir_with_prompt(rel_path: &str) -> tempfile::TempDir {
        let dir = tempdir().unwrap();
        let full = dir.path().join(rel_path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(&full, "dummy").unwrap();
        dir
    }

    #[test]
    fn task_id_not_in_tasks_is_reported() {
        let m = make_manifest(
            serde_json::json!([{"id": 1, "task_ids": ["ghost"], "depends_on": []}]),
            serde_json::json!({"real": {"prompt_file": "tasks/real.md", "agent_type": "claude"}}),
        );
        let dir = tempdir_with_prompt("tasks/real.md");
        let errs = semantic_check(&m, dir.path()).unwrap_err();
        assert!(errs.iter().any(|e| e.category == "missing_task" && e.message.contains("ghost")));
    }

    #[test]
    fn cyclic_dag_is_reported() {
        let m = make_manifest(
            serde_json::json!([
                {"id": 1, "task_ids": ["t1"], "depends_on": [2]},
                {"id": 2, "task_ids": ["t2"], "depends_on": [1]}
            ]),
            serde_json::json!({
                "t1": {"prompt_file": "tasks/t1.md", "agent_type": "claude"},
                "t2": {"prompt_file": "tasks/t2.md", "agent_type": "claude"}
            }),
        );
        let dir = tempdir();
        let dir = dir.unwrap();
        fs::create_dir_all(dir.path().join("tasks")).unwrap();
        fs::write(dir.path().join("tasks/t1.md"), "").unwrap();
        fs::write(dir.path().join("tasks/t2.md"), "").unwrap();
        let errs = semantic_check(&m, dir.path()).unwrap_err();
        assert!(errs.iter().any(|e| e.category == "cycle"));
    }

    #[test]
    fn missing_prompt_file_is_reported() {
        let m = make_manifest(
            serde_json::json!([{"id": 1, "task_ids": ["t1"], "depends_on": []}]),
            serde_json::json!({"t1": {"prompt_file": "tasks/missing.md", "agent_type": "claude"}}),
        );
        let dir = tempdir().unwrap();
        let errs = semantic_check(&m, dir.path()).unwrap_err();
        assert!(errs.iter().any(|e| e.category == "missing_prompt_file"));
    }

    #[test]
    fn duplicate_wave_id_is_reported() {
        let m = make_manifest(
            serde_json::json!([
                {"id": 1, "task_ids": ["t1"], "depends_on": []},
                {"id": 1, "task_ids": ["t2"], "depends_on": []}
            ]),
            serde_json::json!({
                "t1": {"prompt_file": "tasks/t1.md", "agent_type": "claude"},
                "t2": {"prompt_file": "tasks/t2.md", "agent_type": "claude"}
            }),
        );
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("tasks")).unwrap();
        fs::write(dir.path().join("tasks/t1.md"), "").unwrap();
        fs::write(dir.path().join("tasks/t2.md"), "").unwrap();
        let errs = semantic_check(&m, dir.path()).unwrap_err();
        assert!(errs.iter().any(|e| e.category == "duplicate_wave_id"));
    }

    #[test]
    fn duplicate_task_in_wave_is_reported() {
        let m = make_manifest(
            serde_json::json!([{"id": 1, "task_ids": ["t1", "t1"], "depends_on": []}]),
            serde_json::json!({"t1": {"prompt_file": "tasks/t1.md", "agent_type": "claude"}}),
        );
        let dir = tempdir_with_prompt("tasks/t1.md");
        let errs = semantic_check(&m, dir.path()).unwrap_err();
        assert!(errs.iter().any(|e| e.category == "duplicate_task_in_wave"));
    }

    #[test]
    fn missing_depends_on_wave_is_reported() {
        let m = make_manifest(
            serde_json::json!([{"id": 1, "task_ids": ["t1"], "depends_on": [99]}]),
            serde_json::json!({"t1": {"prompt_file": "tasks/t1.md", "agent_type": "claude"}}),
        );
        let dir = tempdir_with_prompt("tasks/t1.md");
        let errs = semantic_check(&m, dir.path()).unwrap_err();
        assert!(errs.iter().any(|e| e.category == "missing_wave" && e.message.contains("99")));
    }

    #[test]
    fn valid_manifest_passes() {
        let m = make_manifest(
            serde_json::json!([{"id": 1, "task_ids": ["t1"], "depends_on": []}]),
            serde_json::json!({"t1": {"prompt_file": "tasks/t1.md", "agent_type": "claude"}}),
        );
        let dir = tempdir_with_prompt("tasks/t1.md");
        assert!(semantic_check(&m, dir.path()).is_ok());
    }
}
