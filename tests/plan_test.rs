use plan_executor::plan::{parse_plan_status, PlanStatus};
use tempfile::NamedTempFile;
use std::io::Write;

fn write_plan(content: &str) -> NamedTempFile {
    let mut f = NamedTempFile::new().unwrap();
    write!(f, "{}", content).unwrap();
    f
}

#[test]
fn test_parse_ready_status() {
    let f = write_plan("# My Plan\n\n**Status:** READY\n\n## Tasks\n");
    let status = parse_plan_status(f.path()).unwrap();
    assert_eq!(status, PlanStatus::Ready);
}

#[test]
fn test_parse_wip_status() {
    let f = write_plan("**Status:** WIP\n");
    let status = parse_plan_status(f.path()).unwrap();
    assert_eq!(status, PlanStatus::Wip);
}

#[test]
fn test_parse_missing_status() {
    let f = write_plan("# No status here\n");
    let status = parse_plan_status(f.path()).unwrap();
    assert!(matches!(status, PlanStatus::Unknown(_)));
}
