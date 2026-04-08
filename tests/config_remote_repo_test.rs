use plan_executor::config::Config;

#[test]
fn test_config_remote_repo_none_by_default() {
    let config = Config::default();
    assert!(config.remote_repo.is_none());
}

#[test]
fn test_config_remote_repo_from_json() {
    let json = r#"{
        "watch_dirs": ["~/workspace"],
        "plan_patterns": [".my/plans/*.md"],
        "auto_execute": false,
        "remote_repo": "owner/plan-executions"
    }"#;
    let config: Config = serde_json::from_str(json).unwrap();
    assert_eq!(config.remote_repo.as_deref(), Some("owner/plan-executions"));
}

#[test]
fn test_config_remote_repo_absent_in_json() {
    let json = r#"{
        "watch_dirs": ["~/workspace"],
        "plan_patterns": [".my/plans/*.md"],
        "auto_execute": false
    }"#;
    let config: Config = serde_json::from_str(json).unwrap();
    assert!(config.remote_repo.is_none());
}
