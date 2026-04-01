use std::process::Command;

/// Smoke test: `plan-executor jobs` exits 0 and prints a header or empty message.
#[test]
fn jobs_subcommand_exits_zero() {
    let bin = env!("CARGO_BIN_EXE_plan-executor");
    let output = Command::new(bin)
        .arg("jobs")
        .output()
        .expect("failed to run plan-executor");

    assert!(
        output.status.success(),
        "plan-executor jobs exited non-zero: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("No jobs found.") || stdout.contains("ID"),
        "unexpected output: {}", stdout
    );
}
