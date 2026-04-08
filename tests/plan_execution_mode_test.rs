use plan_executor::plan::{parse_execution_mode, ExecutionMode};
use tempfile::NamedTempFile;
use std::io::Write;

fn write_plan(content: &str) -> NamedTempFile {
    let mut f = NamedTempFile::new().unwrap();
    write!(f, "{}", content).unwrap();
    f
}

#[test]
fn test_parse_execution_mode_remote() {
    let f = write_plan("# Plan\n**execution:** remote\n**Status:** READY\n");
    assert_eq!(parse_execution_mode(f.path()), ExecutionMode::Remote);
}

#[test]
fn test_parse_execution_mode_local_explicit() {
    let f = write_plan("# Plan\n**execution:** local\n**Status:** READY\n");
    assert_eq!(parse_execution_mode(f.path()), ExecutionMode::Local);
}

#[test]
fn test_parse_execution_mode_missing_defaults_to_local() {
    let f = write_plan("# Plan\n**Status:** READY\n");
    assert_eq!(parse_execution_mode(f.path()), ExecutionMode::Local);
}

#[test]
fn test_parse_execution_mode_unknown_defaults_to_local() {
    let f = write_plan("# Plan\n**execution:** cloud\n");
    assert_eq!(parse_execution_mode(f.path()), ExecutionMode::Local);
}
