use plan_executor::config::{Config, expand_tilde};

#[test]
fn test_expand_tilde_replaces_home() {
    let result = expand_tilde("~/foo/bar");
    let home = dirs::home_dir().unwrap();
    assert_eq!(result, home.join("foo/bar"));
}

#[test]
fn test_expand_tilde_no_tilde() {
    let result = expand_tilde("/absolute/path");
    assert_eq!(result, std::path::PathBuf::from("/absolute/path"));
}

#[test]
fn test_config_default() {
    let config = Config::default();
    assert!(!config.auto_execute);
    assert!(!config.watch_dirs.is_empty());
    assert!(!config.plan_patterns.is_empty());
}

#[test]
fn test_config_serde_roundtrip() {
    let json = r#"{"watch_dirs": ["~/workspace"], "plan_patterns": [".my/plans/*.md"], "auto_execute": true}"#;
    let config: Config = serde_json::from_str(json).unwrap();
    assert!(config.auto_execute);
    assert_eq!(config.watch_dirs, vec!["~/workspace"]);
}
