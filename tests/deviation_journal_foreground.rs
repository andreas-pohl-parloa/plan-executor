use std::fs;

use plan_executor::deviation_journal::journal_path;
use plan_executor::handoff::{ensure_deviation_block_in_prompt, DeviationContext, DEVIATION_MARKER};

#[test]
fn deviation_block_is_injected_for_foreground_handoff() {
    let dir = tempfile::tempdir().expect("tempdir");
    let prompt = dir.path().join("task-1.md");
    fs::write(&prompt, "# Task body\n").expect("write prompt");

    let ctx = DeviationContext {
        journal_path: journal_path(dir.path()),
        job_id: "test-job".into(),
        phase: "wave_execution".into(),
        wave_id: Some(1),
        task_id: Some("1".into()),
        agent_index: 1,
        prior_digest: None,
    };

    ensure_deviation_block_in_prompt(&prompt, &ctx);

    let body = fs::read_to_string(&prompt).expect("read prompt");
    assert!(
        body.contains(DEVIATION_MARKER),
        "prompt missing deviation marker: {body}"
    );
    assert!(
        body.contains(&format!("journal_path: `{}`", ctx.journal_path.display())),
        "prompt missing journal path: {body}"
    );
    assert!(
        body.contains("task_id: `1`"),
        "prompt missing task_id: {body}"
    );
    assert!(
        body.contains("job_id: `test-job`"),
        "prompt missing job_id: {body}"
    );
}

#[test]
fn deviation_block_injection_is_idempotent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let prompt = dir.path().join("task-2.md");
    fs::write(&prompt, "# Task body\n").expect("write prompt");

    let ctx = DeviationContext {
        journal_path: journal_path(dir.path()),
        job_id: "test-job".into(),
        phase: "wave_execution".into(),
        wave_id: Some(1),
        task_id: Some("2".into()),
        agent_index: 1,
        prior_digest: None,
    };

    ensure_deviation_block_in_prompt(&prompt, &ctx);
    let after_first = fs::read_to_string(&prompt).expect("read prompt");

    ensure_deviation_block_in_prompt(&prompt, &ctx);
    let after_second = fs::read_to_string(&prompt).expect("read prompt");

    assert_eq!(
        after_first, after_second,
        "second injection must be a no-op; got\n--- first ---\n{after_first}\n--- second ---\n{after_second}"
    );
}
