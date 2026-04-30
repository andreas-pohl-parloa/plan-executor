//! Integration tests for F2 metrics: per-job `JobMetrics` writer and the
//! `plan-executor jobs metrics` aggregation CLI.
//!
//! Test design (Test Design Artifact):
//!
//! ECP — Equivalence Class Partitions
//! | Partition | Input class                                        | Expected outcome                                         |
//! |-----------|----------------------------------------------------|----------------------------------------------------------|
//! | P1        | empty job (no attempts) → write_metrics            | metrics.json with attempts_total=0, both maps empty      |
//! | P2        | single Success attempt, no recovery                | attempts_total=1, outcomes_by_kind={success:1}, recov={} |
//! | P3        | multi-attempt: 2 transient retries then success    | attempts_total=3, outcomes={transient_infra:2,success:1} |
//! | P4        | aggregator over 0 jobs                             | exit 0, "no metrics found" / job_count=0                 |
//! | P5        | aggregator over 5 jobs with known counts           | sum of counts, percentages match                         |
//! | P6        | --since filter, jobs older/newer than cutoff       | only newer jobs included                                 |
//! | P7        | --job-kind filter                                  | only matching kind included                              |
//!
//! BVA — Boundary Values
//! | Boundary             | Value         | Expected outcome              |
//! | empty attempts       | 0             | both buckets empty            |
//! | single attempt       | 1             | one bucket entry              |
//! | multi attempts       | 3             | mixed buckets                 |
//! | --since 3d cutoff    | 2d in / 14d out | 2d included, 14d excluded   |
//! | --since 7d cutoff    | 2d in / 14d out | 2d included, 14d excluded   |
//! | --since 30d cutoff   | 14d / 2d / now in | all 3 included              |

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::{Duration as ChronoDuration, Utc};
use plan_executor::job::metrics::{AttemptOutcomeKind, JobMetrics, RecoveryKind};
use plan_executor::job::recovery::{Backoff, RecoveryPolicy};
use plan_executor::job::types::{AttemptOutcome, Job, JobId, JobKind, JobState};
use serde_json::Value;
use tempfile::TempDir;

// ---------- Test helpers (each ≤ 20 lines) ----------

fn make_job(id: &str, kind: JobKind, created_at: &str, state: JobState) -> Job {
    Job {
        id: JobId(id.to_string()),
        kind,
        state,
        created_at: created_at.to_string(),
        steps: Vec::new(),
    }
}

fn plan_kind() -> JobKind {
    JobKind::Plan {
        manifest_path: PathBuf::from("/tmp/plan.json"),
    }
}

fn pr_finalize_kind() -> JobKind {
    JobKind::PrFinalize {
        owner: "octo".to_string(),
        repo: "demo".to_string(),
        pr: 1,
        merge_mode: plan_executor::job::types::MergeMode::None,
    }
}

fn fake_metrics(
    job_id: &str,
    started_at: &str,
    outcomes: &[(AttemptOutcomeKind, u32)],
    recoveries: &[(RecoveryKind, u32)],
) -> JobMetrics {
    let attempts_total: u32 = outcomes.iter().map(|(_, n)| *n).sum();
    JobMetrics {
        job_id: JobId(job_id.to_string()),
        step_count: 0,
        attempts_total,
        recoveries_by_kind: recoveries.iter().cloned().collect(),
        outcomes_by_kind: outcomes.iter().cloned().collect(),
        started_at: started_at.to_string(),
        finished_at: Some(started_at.to_string()),
    }
}

/// Writes a `job.json` + `metrics.json` pair into `<base>/<job-id>/` without
/// going through `JobStore`. The `plan-executor jobs metrics` CLI reads
/// these by file shape only, so the on-disk layout (`base/<id>/{job,metrics}.json`)
/// is what matters — bypassing `JobStore::create` keeps the test free of
/// `JobStore`-internal plumbing while still exercising the same wire format.
fn write_fixture(base: &Path, job: &Job, metrics: &JobMetrics) {
    let dir = base.join(&job.id.0);
    fs::create_dir_all(&dir).expect("job dir");
    fs::write(
        dir.join("job.json"),
        serde_json::to_string_pretty(job).expect("serialize job"),
    )
    .expect("write job.json");
    fs::write(
        dir.join("metrics.json"),
        serde_json::to_string_pretty(metrics).expect("serialize metrics"),
    )
    .expect("write metrics.json");
}

fn run_metrics_json(home: &Path, args: &[&str]) -> (bool, String, String) {
    let bin = env!("CARGO_BIN_EXE_plan-executor");
    let mut cmd = Command::new(bin);
    cmd.env("HOME", home).arg("jobs").arg("metrics");
    for a in args {
        cmd.arg(a);
    }
    cmd.arg("--format").arg("json");
    let out = cmd.output().expect("spawn plan-executor");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    (out.status.success(), stdout, stderr)
}

fn parse_report(stdout: &str) -> Value {
    serde_json::from_str::<Value>(stdout).expect("parse JSON report")
}

fn count_in(report: &Value, bucket: &str, key: &str) -> u32 {
    report
        .get(bucket)
        .and_then(|m| m.get(key))
        .and_then(|b| b.get("count"))
        .and_then(serde_json::Value::as_u64)
        .map(|n| u32::try_from(n).unwrap_or(0))
        .unwrap_or(0)
}

fn pct_in(report: &Value, bucket: &str, key: &str) -> f64 {
    report
        .get(bucket)
        .and_then(|m| m.get(key))
        .and_then(|b| b.get("pct"))
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(-1.0)
}

fn iso_offset_days(days: i64) -> String {
    let now = Utc::now();
    now.checked_sub_signed(ChronoDuration::days(days))
        .expect("offset within range")
        .to_rfc3339()
}

// ============================================================
// Per-job metrics writer tests (3)
// ============================================================

/// Round-trip a `JobMetrics` instance through JSON file IO using the same
/// serde shape `JobDir::write_metrics` / `read_metrics` use, then return the
/// reparsed value. Bypasses `JobStore` so the test stays focused on
/// `JobMetrics` semantics.
fn round_trip_metrics(metrics: &JobMetrics) -> JobMetrics {
    let tmp = TempDir::new().expect("tempdir");
    let path = tmp.path().join("metrics.json");
    fs::write(&path, serde_json::to_string_pretty(metrics).expect("serialize")).expect("write");
    let raw = fs::read_to_string(&path).expect("read");
    serde_json::from_str(&raw).expect("parse")
}

#[test]
fn write_metrics_for_empty_job_persists_zero_counters() {
    let mut metrics = JobMetrics::new(JobId("job-empty".to_string()));
    metrics.finalize();
    let read_back = round_trip_metrics(&metrics);

    let observed = (
        read_back.attempts_total,
        read_back.outcomes_by_kind.is_empty(),
        read_back.recoveries_by_kind.is_empty(),
        read_back.started_at.is_empty(),
        read_back.finished_at.is_some(),
    );
    assert_eq!(observed, (0, true, true, false, true));
}

#[test]
fn write_metrics_for_single_success_records_one_outcome() {
    let mut metrics = JobMetrics::new(JobId("job-single".to_string()));
    metrics.record_step();
    metrics.record_attempt(&AttemptOutcome::Success, None);
    metrics.finalize();
    let read_back = round_trip_metrics(&metrics);

    let mut expected_outcomes = HashMap::new();
    expected_outcomes.insert(AttemptOutcomeKind::Success, 1u32);
    let observed = (
        read_back.attempts_total,
        read_back.outcomes_by_kind,
        read_back.recoveries_by_kind.is_empty(),
    );
    assert_eq!(observed, (1, expected_outcomes, true));
}

#[test]
fn write_metrics_for_multi_attempt_records_mixed_outcomes_and_recoveries() {
    let policy = RecoveryPolicy::RetryTransient {
        max: 3,
        backoff: Backoff::Fixed { ms: 10 },
    };
    let mut metrics = JobMetrics::new(JobId("job-multi".to_string()));
    metrics.record_step();
    let transient = AttemptOutcome::TransientInfra {
        error: "rate".to_string(),
    };
    metrics.record_attempt(&transient, Some(&policy));
    metrics.record_attempt(&transient, Some(&policy));
    metrics.record_attempt(&AttemptOutcome::Success, None);
    metrics.finalize();
    let read_back = round_trip_metrics(&metrics);

    let mut expected_outcomes = HashMap::new();
    expected_outcomes.insert(AttemptOutcomeKind::TransientInfra, 2u32);
    expected_outcomes.insert(AttemptOutcomeKind::Success, 1u32);
    let mut expected_recoveries = HashMap::new();
    expected_recoveries.insert(RecoveryKind::RetryTransient, 2u32);
    let observed = (
        read_back.attempts_total,
        read_back.outcomes_by_kind,
        read_back.recoveries_by_kind,
    );
    assert_eq!(observed, (3, expected_outcomes, expected_recoveries));
}

// ============================================================
// `plan-executor jobs metrics` aggregation tests (4)
// ============================================================

#[test]
fn metrics_cli_reports_zero_jobs_when_store_is_empty() {
    let home = TempDir::new().expect("home");
    fs::create_dir_all(home.path().join(".plan-executor").join("jobs")).expect("jobs dir");

    let (success, stdout, stderr) = run_metrics_json(home.path(), &[]);
    let report = parse_report(&stdout);
    let job_count = report
        .get("job_count")
        .and_then(Value::as_u64)
        .unwrap_or(99);

    let observed = (success, job_count, stderr.is_empty());
    assert_eq!(observed, (true, 0, true));
}

#[test]
fn metrics_cli_aggregates_counts_and_percentages_across_five_fixtures() {
    let home = TempDir::new().expect("home");
    let base = home.path().join(".plan-executor").join("jobs");
    fs::create_dir_all(&base).expect("jobs dir");
    let now = Utc::now().to_rfc3339();
    // 5 jobs, each with: 4 success + 1 transient_infra + 1 retry_transient recovery
    // Aggregate expected: success=20, transient_infra=5, attempts_total=25,
    // recoveries.retry_transient=5, success_pct=80.0, transient_pct=20.0
    for i in 0..5 {
        let id = format!("job-agg-{i}");
        let job = make_job(&id, plan_kind(), &now, JobState::Succeeded);
        let metrics = fake_metrics(
            &id,
            &now,
            &[
                (AttemptOutcomeKind::Success, 4),
                (AttemptOutcomeKind::TransientInfra, 1),
            ],
            &[(RecoveryKind::RetryTransient, 1)],
        );
        let _ = write_fixture(&base, &job, &metrics);
    }

    let (success, stdout, _stderr) = run_metrics_json(home.path(), &[]);
    let report = parse_report(&stdout);
    let observed = (
        success,
        report.get("job_count").and_then(Value::as_u64).unwrap_or(0),
        report
            .get("attempts_total")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        count_in(&report, "outcomes", "success"),
        count_in(&report, "outcomes", "transient_infra"),
        count_in(&report, "recoveries", "retry_transient"),
        (pct_in(&report, "outcomes", "success") - 80.0).abs() < 0.01,
        (pct_in(&report, "outcomes", "transient_infra") - 20.0).abs() < 0.01,
    );
    assert_eq!(observed, (true, 5, 25, 20, 5, 5, true, true));
}

#[test]
fn metrics_cli_since_filter_excludes_jobs_older_than_cutoff() {
    let home = TempDir::new().expect("home");
    let base = home.path().join(".plan-executor").join("jobs");
    fs::create_dir_all(&base).expect("jobs dir");
    let now_iso = Utc::now().to_rfc3339();
    let two_days_ago = iso_offset_days(2);
    let fourteen_days_ago = iso_offset_days(14);
    for (id, ts) in [
        ("job-now", now_iso.as_str()),
        ("job-2d", two_days_ago.as_str()),
        ("job-14d", fourteen_days_ago.as_str()),
    ] {
        let job = make_job(id, plan_kind(), ts, JobState::Succeeded);
        let metrics = fake_metrics(id, ts, &[(AttemptOutcomeKind::Success, 1)], &[]);
        let _ = write_fixture(&base, &job, &metrics);
    }

    let three = parse_report(&run_metrics_json(home.path(), &["--since", "3d"]).1);
    let seven = parse_report(&run_metrics_json(home.path(), &["--since", "7d"]).1);
    let thirty = parse_report(&run_metrics_json(home.path(), &["--since", "30d"]).1);

    let observed = (
        three.get("job_count").and_then(Value::as_u64).unwrap_or(99),
        seven.get("job_count").and_then(Value::as_u64).unwrap_or(99),
        thirty
            .get("job_count")
            .and_then(Value::as_u64)
            .unwrap_or(99),
    );
    assert_eq!(observed, (2, 2, 3));
}

#[test]
fn metrics_cli_job_kind_filter_isolates_kind() {
    let home = TempDir::new().expect("home");
    let base = home.path().join(".plan-executor").join("jobs");
    fs::create_dir_all(&base).expect("jobs dir");
    let now = Utc::now().to_rfc3339();
    let plan_job = make_job("job-plan-1", plan_kind(), &now, JobState::Succeeded);
    let plan_metrics = fake_metrics("job-plan-1", &now, &[(AttemptOutcomeKind::Success, 2)], &[]);
    let pr_job = make_job("job-pr-1", pr_finalize_kind(), &now, JobState::Succeeded);
    let pr_metrics = fake_metrics("job-pr-1", &now, &[(AttemptOutcomeKind::Success, 7)], &[]);
    let _ = write_fixture(&base, &plan_job, &plan_metrics);
    let _ = write_fixture(&base, &pr_job, &pr_metrics);

    let plan_only = parse_report(&run_metrics_json(home.path(), &["--job-kind", "plan"]).1);
    let pr_only = parse_report(&run_metrics_json(home.path(), &["--job-kind", "pr_finalize"]).1);

    let observed = (
        plan_only
            .get("job_count")
            .and_then(Value::as_u64)
            .unwrap_or(99),
        plan_only
            .get("attempts_total")
            .and_then(Value::as_u64)
            .unwrap_or(99),
        pr_only
            .get("job_count")
            .and_then(Value::as_u64)
            .unwrap_or(99),
        pr_only
            .get("attempts_total")
            .and_then(Value::as_u64)
            .unwrap_or(99),
    );
    assert_eq!(observed, (1, 2, 1, 7));
}
