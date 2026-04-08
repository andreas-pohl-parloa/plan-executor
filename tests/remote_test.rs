use plan_executor::remote::{ExecutionMetadata, pr_title};

#[test]
fn test_pr_title_format() {
    let meta = ExecutionMetadata {
        target_repo: "owner/my-service".to_string(),
        target_ref: "abc123def456".to_string(),
        target_branch: "feat/cool".to_string(),
        plan_filename: "plan-add-feature.md".to_string(),
        started_at: "2026-04-08T14:30:00Z".to_string(),
    };
    assert_eq!(pr_title(&meta), "exec: plan-add-feature.md @ owner/my-service");
}

#[test]
fn test_execution_metadata_serialization() {
    let meta = ExecutionMetadata {
        target_repo: "owner/repo".to_string(),
        target_ref: "abc123".to_string(),
        target_branch: "main".to_string(),
        plan_filename: "plan-foo.md".to_string(),
        started_at: "2026-04-08T14:30:00Z".to_string(),
    };
    let json = serde_json::to_string_pretty(&meta).unwrap();
    let parsed: ExecutionMetadata = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.target_repo, "owner/repo");
    assert_eq!(parsed.target_ref, "abc123");
    assert_eq!(parsed.plan_filename, "plan-foo.md");
}

#[test]
fn test_branch_name_format() {
    let name = plan_executor::remote::branch_name("plan-add-feature.md", "2026-04-08T14:30:22Z");
    // Should be exec/<date-time>-<plan-stem>
    assert!(name.starts_with("exec/"));
    assert!(name.contains("plan-add-feature"));
    // No .md extension in branch name
    assert!(!name.ends_with(".md"));
}
