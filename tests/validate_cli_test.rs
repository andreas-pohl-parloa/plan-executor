use std::fs;
use std::process::Command;
use tempfile::tempdir;

fn valid_manifest(prompt_rel: &str) -> String {
    let v = serde_json::json!({
        "version": 1,
        "plan": {
            "goal": "x", "type": "feature",
            "flags": {
                "merge": false, "merge_admin": false, "skip_pr": false,
                "skip_code_review": false, "no_worktree": false, "draft_pr": false
            }
        },
        "waves": [{"id": 1, "task_ids": ["t1"], "depends_on": []}],
        "tasks": {"t1": {"prompt_file": prompt_rel, "agent_type": "claude"}}
    });
    serde_json::to_string_pretty(&v).unwrap()
}

#[test]
fn validate_cli_happy_path() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("tasks")).unwrap();
    fs::write(dir.path().join("tasks/t1.md"), "").unwrap();
    let path = dir.path().join("tasks.json");
    fs::write(&path, valid_manifest("tasks/t1.md")).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_plan-executor"))
        .arg("validate")
        .arg(&path)
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("VALID:"), "stdout was: {stdout}");
}

#[test]
fn validate_cli_reports_missing_task_id() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("tasks")).unwrap();
    fs::write(dir.path().join("tasks/t1.md"), "").unwrap();
    let path = dir.path().join("tasks.json");
    let bad = serde_json::json!({
        "version": 1,
        "plan": {
            "goal": "x", "type": "feature",
            "flags": {
                "merge": false, "merge_admin": false, "skip_pr": false,
                "skip_code_review": false, "no_worktree": false, "draft_pr": false
            }
        },
        "waves": [{"id": 1, "task_ids": ["ghost"], "depends_on": []}],
        "tasks": {"t1": {"prompt_file": "tasks/t1.md", "agent_type": "claude"}}
    });
    fs::write(&path, serde_json::to_string_pretty(&bad).unwrap()).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_plan-executor"))
        .arg("validate")
        .arg(&path)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("missing_task"), "stderr was: {stderr}");
}
