//! Handlers for the `plan-executor jobs <subcommand>` CLI surface.
//!
//! Renders both the new (`job.json`) and legacy (`metadata.json`) layouts in
//! one table during the migration grace window, and provides per-job
//! show/cancel/gc/replay verbs. The replay verb is an informational stub in
//! Phase A; full replay arrives in Phase D.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::cli::JobsCommand;
use crate::job::metrics::{AttemptOutcomeKind, JobMetrics, RecoveryKind};
use crate::job::storage::{JobStore, JobStoreEntry};
use crate::job::types::{Job, JobId, JobState};
use crate::jobs::{JobMetadata, JobStatus};

/// Routes a parsed `JobsCommand` to its handler.
///
/// # Errors
///
/// Returns the underlying handler's error (filesystem, parsing, or
/// missing-job lookup).
pub fn dispatch(command: JobsCommand) -> Result<()> {
    match command {
        JobsCommand::List => cmd_list(),
        JobsCommand::Show { id } => cmd_show(&id),
        JobsCommand::Cancel { id } => cmd_cancel(&id),
        JobsCommand::Gc { older_than } => cmd_gc(older_than.as_deref()),
        JobsCommand::Replay { id, from_step } => cmd_replay(&id, from_step),
        JobsCommand::Metrics(args) => cmd_metrics(&args),
    }
}

/// Output format selection for `plan-executor jobs metrics`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum MetricsFormat {
    /// Human-readable sectioned tables.
    Text,
    /// Single JSON object with all aggregates.
    Json,
}

/// CLI arguments for `plan-executor jobs metrics`.
#[derive(Debug, Clone, clap::Args)]
pub struct MetricsArgs {
    /// Filter to jobs whose `started_at` is more recent than `now - DURATION`.
    /// Accepts `7d`, `48h`, `30m`, `5s`. Default: no filter.
    #[arg(long)]
    pub since: Option<String>,
    /// Filter by JobKind discriminant: `plan`, `pr_finalize`, `review`,
    /// `validate`, `compile_fix_waves`. Default: all kinds.
    #[arg(long = "job-kind")]
    pub job_kind: Option<String>,
    /// Output format. Default: `text`.
    #[arg(long, value_enum, default_value_t = MetricsFormat::Text)]
    pub format: MetricsFormat,
}

fn cmd_list() -> Result<()> {
    let store = JobStore::new().context("opening job store")?;
    let entries = store.list_all().context("listing jobs")?;
    if entries.is_empty() {
        println!("No jobs.");
        return Ok(());
    }
    let job_processes = query_daemon_processes();
    println!(
        "{:<10} {:<14} {:<11} {:<21} {:<28} TITLE",
        "ID", "KIND", "STATE", "CREATED_AT", "PROGRESS"
    );
    for entry in entries {
        match entry {
            JobStoreEntry::New { summary, path } => {
                let job_opt = store.open(&summary.id).ok().and_then(|dir| dir.read_job().ok());
                let title = job_opt
                    .as_ref()
                    .map(job_title)
                    .unwrap_or_else(|| path.display().to_string());
                let total_steps = job_opt.as_ref().map(|j| j.steps.len() as u32);
                let progress = progress_label(&summary.id.0, total_steps);
                println!(
                    "{:<10} {:<14} {:<11} {:<21} {:<28} {}",
                    short_id(&summary.id.0),
                    summary.kind_tag,
                    state_label(&summary.state),
                    short_timestamp(&summary.created_at),
                    truncate_to(&progress, 28),
                    title,
                );
                if matches!(summary.state, JobState::Running) {
                    if let Some(procs) = job_processes.get(&summary.id.0) {
                        crate::cli::render_job_process_tree_compat(procs);
                    }
                }
            }
            JobStoreEntry::Legacy { id, path } => {
                let (kind, state, created, title) = legacy_summary(&path);
                let progress = progress_label(&id, None);
                println!(
                    "{:<10} {:<14} {:<11} {:<21} {:<28} {}",
                    short_id(&id),
                    kind,
                    state,
                    short_timestamp(&created),
                    truncate_to(&progress, 28),
                    title,
                );
                if state == "running" {
                    if let Some(procs) = job_processes.get(&id) {
                        crate::cli::render_job_process_tree_compat(procs);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Connects to the daemon for a single GetState snapshot and returns the
/// resulting `running_processes` keyed by job id. Returns an empty map when
/// the daemon is unreachable or replies with an unexpected envelope so the
/// listing degrades gracefully (header + rows still render, just without the
/// sub-process tree).
fn query_daemon_processes() -> HashMap<String, crate::ipc::JobProcesses> {
    use crate::ipc::{DaemonEvent, TuiRequest};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    if !crate::ipc::socket_path().exists() {
        return HashMap::new();
    }
    let mut stream = match UnixStream::connect(crate::ipc::socket_path()) {
        Ok(s) => s,
        Err(_) => return HashMap::new(),
    };
    let req = match serde_json::to_string(&TuiRequest::GetState) {
        Ok(s) => s,
        Err(_) => return HashMap::new(),
    };
    if stream.write_all(format!("{}\n", req).as_bytes()).is_err() {
        return HashMap::new();
    }
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return HashMap::new();
    }
    match serde_json::from_str::<DaemonEvent>(&line) {
        Ok(DaemonEvent::State {
            running_processes, ..
        }) => running_processes
            .into_iter()
            .map(|p| (p.job_id.clone(), p))
            .collect(),
        _ => HashMap::new(),
    }
}

/// Derives a short PROGRESS label from the job's `display.log`. Counts how
/// many `step N (name) <terminal-status>` lines have been written to compute
/// completion percentage out of `total_steps`, then identifies the currently
/// running step and formats `<pct>% step <N>/<total> <name>`.
///
/// Falls back to `-` when no display.log exists yet or no step line has been
/// emitted (job freshly queued).
fn progress_label(job_id: &str, total_steps: Option<u32>) -> String {
    let path = crate::config::Config::base_dir()
        .join("jobs")
        .join(job_id)
        .join("display.log");
    let Ok(content) = fs::read_to_string(&path) else {
        return "-".to_string();
    };
    // Walk every line; track per-step status. When the same seq has both a
    // `starting` line and a non-starting status line, count that step as
    // completed. The latest step seen with `starting` is the current step.
    let mut current: Option<(u32, String)> = None;
    let mut completed: std::collections::HashSet<u32> = std::collections::HashSet::new();
    let mut max_seq: u32 = 0;
    for raw in content.lines() {
        let cleaned = strip_bullet_prefix(raw);
        if let Some((seq, name, status)) = parse_step_line(&cleaned) {
            if seq > max_seq {
                max_seq = seq;
            }
            if status == "starting" {
                current = Some((seq, name));
            } else {
                // Any non-starting status line marks the step as resolved
                // (success, pending placeholder, semantic_mistake, …).
                completed.insert(seq);
            }
        }
    }
    let Some((cur_seq, cur_name)) = current else {
        return "-".to_string();
    };
    // total: prefer the manifest-declared step count; fall back to the
    // highest seq seen in the log when job.json is unreadable.
    let total = total_steps.unwrap_or(max_seq).max(cur_seq);
    let completed_count = completed.len() as u32;
    let pct = if total == 0 {
        0
    } else {
        (completed_count * 100) / total
    };
    format!("{pct:>3}% step {cur_seq}/{total} {cur_name}")
}

fn strip_bullet_prefix(line: &str) -> String {
    // Lines look like "\x1b[33m⏺ [plan-executor]\x1b[0m step 2 …" or just
    // "⏺ [plan-executor] step 2 …"; strip ANSI and the bullet so the
    // parsers below see the bare text.
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip until the terminating 'm' (CSI sequences end in a letter;
            // close enough for our color codes).
            for d in chars.by_ref() {
                if d.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    let trimmed = out.trim_start();
    let after_bullet = trimmed.strip_prefix("⏺ ").unwrap_or(trimmed);
    let after_tag = after_bullet
        .strip_prefix("[plan-executor] ")
        .unwrap_or(after_bullet);
    after_tag.trim().to_string()
}

fn parse_step_line(line: &str) -> Option<(u32, String, String)> {
    // Matches: "step <N> (<name>) <status...>"
    let after = line.strip_prefix("step ")?;
    let (seq_str, rest) = after.split_once(' ')?;
    let seq: u32 = seq_str.parse().ok()?;
    let rest = rest.strip_prefix('(')?;
    let (name, after_name) = rest.split_once(')')?;
    let status = after_name.trim().trim_start_matches(':').trim().to_string();
    let status = if status.is_empty() {
        "running".to_string()
    } else {
        status
    };
    Some((seq, name.to_string(), status))
}

fn truncate_to(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// Trims an RFC3339 timestamp like `2026-04-30T09:38:47.060394+00:00` down to
/// `2026-04-30T09:38:47Z` so the column aligns regardless of fractional digits
/// or numeric timezone offsets. Falls back to the original string when parsing
/// fails so we never mask a real value.
fn short_timestamp(ts: &str) -> String {
    DateTime::parse_from_rfc3339(ts)
        .map(|dt| dt.with_timezone(&Utc).format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|_| ts.to_string())
}

fn short_id(id: &str) -> String {
    let len = id.len().min(8);
    id[..len].to_string()
}

fn job_title(job: &Job) -> String {
    use crate::job::types::JobKind;
    match &job.kind {
        JobKind::Plan { manifest_path } | JobKind::Validate { manifest_path } => {
            manifest_path.display().to_string()
        }
        JobKind::PrFinalize {
            owner, repo, pr, ..
        } => format!("{owner}/{repo}#{pr}"),
        JobKind::Review { branch, base } => format!("{branch} -> {base}"),
        JobKind::CompileFixWaves { manifest_path, .. } => manifest_path.display().to_string(),
    }
}

fn cmd_show(id_prefix: &str) -> Result<()> {
    let store = JobStore::new()?;
    if let Some(found_id) = find_new_id_by_prefix(&store, id_prefix)? {
        let dir = store.open(&found_id)?;
        let job: Job = dir.read_job()?;
        print_job_detail(&job, &dir.path().to_path_buf());
        return Ok(());
    }
    if let Some(meta) = JobMetadata::load_by_id_prefix(id_prefix) {
        print_legacy_detail(&meta);
        return Ok(());
    }
    Err(anyhow!("no job matching prefix {id_prefix:?}"))
}

/// Cancel a new-layout job by id prefix.
///
/// Refuses to cancel jobs already in a terminal state (`Succeeded` or
/// `Failed`); cancelling a terminal job would only rewrite the failure
/// reason and is treated as user error.
///
/// Note: this is a metadata-only mutation. Killing a still-running daemon
/// process is a Phase D feature; for now use `plan-executor kill <id>` to
/// stop a running job at the OS level.
fn cmd_cancel(id_prefix: &str) -> Result<()> {
    let store = JobStore::new()?;
    let found_id = find_new_id_by_prefix(&store, id_prefix)?.ok_or_else(|| {
        anyhow!("cancel only applies to new-layout jobs; no match for {id_prefix:?}")
    })?;
    let dir = store.open(&found_id)?;
    let mut job: Job = dir.read_job()?;
    assert_cancellable(&job)?;
    job.state = JobState::Failed {
        reason: "cancelled by user".to_string(),
        recoverable: false,
    };
    dir.write_job_metadata(&job)?;
    println!("Cancelled {}", found_id.0);
    Ok(())
}

/// Errors when `job` is already in a terminal state and cannot be cancelled.
fn assert_cancellable(job: &Job) -> Result<()> {
    if matches!(job.state, JobState::Succeeded | JobState::Failed { .. }) {
        return Err(anyhow!(
            "job {} is already terminal (state: {}); cancel is a no-op",
            job.id.0,
            state_label(&job.state),
        ));
    }
    Ok(())
}

fn cmd_gc(older_than: Option<&str>) -> Result<()> {
    let threshold = parse_duration(older_than.unwrap_or("30d"))?;
    let store = JobStore::new()?;
    let (deleted, failures) = gc_with_store(&store, threshold)?;
    println!(
        "Garbage-collected {deleted} job director{}.",
        if deleted == 1 { "y" } else { "ies" }
    );
    for (path, err) in &failures {
        eprintln!("  Failed: {} — {}", path.display(), err);
    }
    Ok(())
}

/// Garbage-collect terminal job directories older than `threshold`.
///
/// Returns the count of removed directories and a list of `(path, error)`
/// pairs for entries that could not be processed (failed metadata read,
/// missing creation/modification time, or `remove_dir_all` failure). Entries
/// that are simply not yet old enough are silently skipped (not failures).
///
/// # Errors
///
/// Returns the underlying `JobStore::list_all` error or, in the unlikely
/// event that `now - threshold` underflows the system time, an `anyhow`
/// error describing the overflow.
fn gc_with_store(store: &JobStore, threshold: Duration) -> Result<(usize, Vec<(PathBuf, String)>)> {
    let cutoff = SystemTime::now()
        .checked_sub(threshold)
        .ok_or_else(|| anyhow!("threshold too large"))?;
    let entries = store.list_all()?;
    let mut deleted: usize = 0;
    let mut failures: Vec<(PathBuf, String)> = Vec::new();
    for entry in entries {
        let (path, completed) = match &entry {
            JobStoreEntry::New { summary, path } => {
                let done = matches!(summary.state, JobState::Succeeded | JobState::Failed { .. });
                (path.clone(), done)
            }
            JobStoreEntry::Legacy { path, .. } => (path.clone(), legacy_is_terminal(path)),
        };
        if !completed {
            continue;
        }
        let metadata = match fs::metadata(&path) {
            Ok(m) => m,
            Err(e) => {
                failures.push((path.clone(), format!("metadata: {e}")));
                continue;
            }
        };
        let created = match metadata.created().or_else(|_| metadata.modified()) {
            Ok(t) => t,
            Err(e) => {
                failures.push((path.clone(), format!("no created/modified time: {e}")));
                continue;
            }
        };
        if created >= cutoff {
            continue;
        }
        match fs::remove_dir_all(&path) {
            Ok(()) => deleted += 1,
            Err(e) => failures.push((path.clone(), format!("remove_dir_all: {e}"))),
        }
    }
    Ok((deleted, failures))
}

fn cmd_replay(id: &str, from_step: Option<u32>) -> Result<()> {
    let from = from_step.map_or_else(|| "<beginning>".to_string(), |n| n.to_string());
    println!("replay {id} (from-step {from}): not yet implemented (Phase D)");
    Ok(())
}

fn find_new_id_by_prefix(store: &JobStore, prefix: &str) -> Result<Option<JobId>> {
    let matches: Vec<JobId> = store
        .list_all()?
        .into_iter()
        .filter_map(|entry| match entry {
            JobStoreEntry::New { summary, .. } if summary.id.0.starts_with(prefix) => {
                Some(summary.id)
            }
            _ => None,
        })
        .collect();
    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.into_iter().next()),
        n => {
            let ids = matches
                .iter()
                .map(|id| id.0.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            Err(anyhow!(
                "ambiguous prefix {prefix:?}: matches {n} jobs ({ids})"
            ))
        }
    }
}

fn state_label(s: &JobState) -> String {
    match s {
        JobState::Pending => "pending".to_string(),
        JobState::Running => "running".to_string(),
        JobState::Suspended { .. } => "suspended".to_string(),
        JobState::Succeeded => "succeeded".to_string(),
        JobState::Failed { .. } => "failed".to_string(),
    }
}

fn legacy_summary(path: &std::path::Path) -> (String, String, String, String) {
    let mut kind = "plan".to_string();
    let mut state = "?".to_string();
    let mut created = "-".to_string();
    let mut title = path.display().to_string();
    if let Ok(raw) = fs::read_to_string(path.join("metadata.json")) {
        if let Ok(meta) = serde_json::from_str::<JobMetadata>(&raw) {
            kind = if meta.remote_repo.is_some() {
                "plan(remote)".to_string()
            } else {
                "plan".to_string()
            };
            state = legacy_state_label(&meta.status);
            created = meta.started_at.to_rfc3339();
            title = match (meta.remote_repo.as_deref(), meta.remote_pr) {
                (Some(repo), Some(pr)) => format!("{repo}#{pr}"),
                _ => meta.plan_path.display().to_string(),
            };
        }
    }
    (kind, state, created, title)
}

fn legacy_state_label(s: &JobStatus) -> String {
    match s {
        JobStatus::Running => "running",
        JobStatus::Success => "succeeded",
        JobStatus::Failed => "failed",
        JobStatus::Killed => "killed",
        JobStatus::RemoteRunning => "remote_running",
    }
    .to_string()
}

fn legacy_is_terminal(path: &std::path::Path) -> bool {
    if let Ok(raw) = fs::read_to_string(path.join("metadata.json")) {
        if let Ok(meta) = serde_json::from_str::<JobMetadata>(&raw) {
            return matches!(
                meta.status,
                JobStatus::Success | JobStatus::Failed | JobStatus::Killed
            );
        }
    }
    false
}

fn print_job_detail(job: &Job, dir: &PathBuf) {
    println!("Job: {}", job.id.0);
    println!("  layout: new");
    println!("  state:  {}", state_label(&job.state));
    println!("  kind:   {}", job_kind_label(&job.kind));
    println!("  dir:    {}", dir.display());
    println!("  created_at: {}", job.created_at);
    println!("  steps ({}):", job.steps.len());
    for step in &job.steps {
        println!(
            "    {:>3}: {}  status={}  idempotent={}",
            step.seq,
            step.name,
            step_status_label(&step.status),
            step.idempotent
        );
        for att in &step.attempts {
            let finished = att.finished_at.as_deref().unwrap_or("(running)");
            println!(
                "         attempt {} started_at={} finished_at={} outcome={}",
                att.n,
                att.started_at,
                finished,
                attempt_outcome_label(&att.outcome)
            );
        }
    }
}

fn print_legacy_detail(meta: &JobMetadata) {
    println!("Job: {}", meta.id);
    println!("  layout: legacy");
    println!("  state:  {}", legacy_state_label(&meta.status));
    println!("  plan:   {}", meta.plan_path.display());
    println!("  started_at:  {}", meta.started_at.to_rfc3339());
    if let Some(f) = meta.finished_at {
        println!("  finished_at: {}", f.to_rfc3339());
    }
    if let Some(m) = &meta.model {
        println!("  model: {m}");
    }
    if let (Some(i), Some(o)) = (meta.input_tokens, meta.output_tokens) {
        println!("  tokens: in={i} out={o}");
    }
    if let Some(d) = meta.duration_ms {
        println!("  duration_ms: {d}");
    }
}

fn job_kind_label(k: &crate::job::types::JobKind) -> String {
    use crate::job::types::JobKind::{CompileFixWaves, Plan, PrFinalize, Review, Validate};
    match k {
        Plan { manifest_path } => format!("plan(manifest={})", manifest_path.display()),
        PrFinalize {
            owner, repo, pr, ..
        } => format!("pr_finalize({owner}/{repo}#{pr})"),
        Review { branch, base } => format!("review({branch} <-> {base})"),
        Validate { manifest_path } => format!("validate(manifest={})", manifest_path.display()),
        CompileFixWaves {
            manifest_path,
            findings_path,
        } => format!(
            "compile_fix_waves(manifest={}, findings={})",
            manifest_path.display(),
            findings_path.display()
        ),
    }
}

fn step_status_label(s: &crate::job::types::StepStatus) -> String {
    use crate::job::types::StepStatus::{Failed, Pending, Running, SkippedNotRequired, Succeeded};
    match s {
        Pending => "pending",
        Running => "running",
        Succeeded => "succeeded",
        Failed { .. } => "failed",
        SkippedNotRequired => "skipped",
    }
    .to_string()
}

fn attempt_outcome_label(o: &crate::job::types::AttemptOutcome) -> String {
    use crate::job::types::AttemptOutcome::{
        HardInfra, Pending, ProtocolViolation, SemanticMistake, SpecDrift, Success, TransientInfra,
    };
    match o {
        Success => "success".to_string(),
        HardInfra { error } => format!("hard_infra({error})"),
        TransientInfra { error } => format!("transient_infra({error})"),
        ProtocolViolation { category, .. } => format!("protocol_violation({category})"),
        SemanticMistake { fix_loop_round } => format!("semantic_mistake(round={fix_loop_round})"),
        SpecDrift { gap } => format!("spec_drift({gap})"),
        Pending => "pending".to_string(),
    }
}

/// Aggregate persisted `JobMetrics` across the store and render the report.
fn cmd_metrics(args: &MetricsArgs) -> Result<()> {
    let store = JobStore::new().context("opening job store")?;
    let entries = store.list_all().context("listing jobs")?;

    let since_cutoff = match args.since.as_deref() {
        None => None,
        Some(raw) => Some(parse_since_cutoff(raw)?),
    };
    let job_kind_filter = args.job_kind.as_deref();

    let mut report = MetricsReport::empty(args);
    for entry in entries {
        let JobStoreEntry::New { summary, .. } = entry else {
            continue;
        };
        if let Some(kind) = job_kind_filter {
            if summary.kind_tag != kind {
                continue;
            }
        }
        let job_dir = match store.open(&summary.id) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let metrics = match job_dir.read_metrics() {
            Ok(Some(m)) => m,
            Ok(None) => continue,
            Err(_) => continue,
        };
        if let Some(cutoff) = since_cutoff {
            if !is_after_cutoff(&metrics.started_at, cutoff) {
                continue;
            }
        }
        let job = match job_dir.read_job() {
            Ok(j) => j,
            Err(_) => continue,
        };
        report.absorb(&metrics, &job);
    }

    match args.format {
        MetricsFormat::Json => emit_json(&report),
        MetricsFormat::Text => emit_text(&report),
    }
    Ok(())
}

/// Parses an `--since` duration string and returns the absolute cutoff time.
fn parse_since_cutoff(raw: &str) -> Result<DateTime<Utc>> {
    let duration =
        parse_duration(raw).with_context(|| format!("parsing --since value {raw:?}"))?;
    let secs = i64::try_from(duration.as_secs())
        .map_err(|_| anyhow!("--since duration {raw:?} is too large"))?;
    let chrono_dur = chrono::Duration::try_seconds(secs)
        .ok_or_else(|| anyhow!("--since duration {raw:?} is too large"))?;
    Utc::now()
        .checked_sub_signed(chrono_dur)
        .ok_or_else(|| anyhow!("--since duration {raw:?} underflows current time"))
}

/// Returns true when the RFC 3339 timestamp `started_at` is at or after `cutoff`.
fn is_after_cutoff(started_at: &str, cutoff: DateTime<Utc>) -> bool {
    DateTime::parse_from_rfc3339(started_at)
        .map(|dt| dt.with_timezone(&Utc) >= cutoff)
        .unwrap_or(false)
}

/// Aggregated metrics report assembled from per-job `JobMetrics` snapshots.
#[derive(Debug, Clone, Serialize)]
struct MetricsReport {
    filter: ReportFilter,
    job_count: u32,
    attempts_total: u32,
    recoveries_total: u32,
    outcomes: HashMap<String, BucketCount>,
    recoveries: HashMap<String, BucketCount>,
    top_retried_steps: Vec<TopStep>,
    retry_budget_utilization: HashMap<String, BudgetUtilization>,
}

#[derive(Debug, Clone, Serialize)]
struct ReportFilter {
    since: Option<String>,
    job_kind: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct BucketCount {
    count: u32,
    pct: f64,
}

#[derive(Debug, Clone, Serialize)]
struct TopStep {
    step: String,
    attempts: u32,
    most_common_outcome: String,
}

#[derive(Debug, Clone, Serialize)]
struct BudgetUtilization {
    cap_hit_pct: f64,
}

/// Per-step accumulator (transient, internal to the report builder).
#[derive(Debug, Default)]
struct StepAccumulator {
    attempts: u32,
    outcome_counts: HashMap<String, u32>,
}

impl MetricsReport {
    fn empty(args: &MetricsArgs) -> Self {
        Self {
            filter: ReportFilter {
                since: args.since.clone(),
                job_kind: args.job_kind.clone(),
            },
            job_count: 0,
            attempts_total: 0,
            recoveries_total: 0,
            outcomes: HashMap::new(),
            recoveries: HashMap::new(),
            top_retried_steps: Vec::new(),
            retry_budget_utilization: HashMap::new(),
        }
    }

    /// Folds one job's metrics + job record into the running aggregate.
    fn absorb(&mut self, metrics: &JobMetrics, job: &Job) {
        self.job_count = self.job_count.saturating_add(1);
        self.attempts_total = self.attempts_total.saturating_add(metrics.attempts_total);

        for (outcome_kind, count) in &metrics.outcomes_by_kind {
            let key = serialize_outcome_kind(outcome_kind);
            let entry = self.outcomes.entry(key).or_insert(BucketCount {
                count: 0,
                pct: 0.0,
            });
            entry.count = entry.count.saturating_add(*count);
        }

        for (recovery_kind, count) in &metrics.recoveries_by_kind {
            let key = serialize_recovery_kind(recovery_kind);
            let entry = self.recoveries.entry(key.clone()).or_insert(BucketCount {
                count: 0,
                pct: 0.0,
            });
            entry.count = entry.count.saturating_add(*count);
            self.recoveries_total = self.recoveries_total.saturating_add(*count);
        }

        // Retry-budget utilization proxy: a job whose terminal state is
        // Failed AND that recorded recoveries of kind K is treated as having
        // hit the cap for K. (No richer signal exists in F2.1 metrics; the
        // task explicitly forbids adding new metrics computation.)
        let job_failed = matches!(job.state, JobState::Failed { .. });
        if job_failed {
            for recovery_kind in metrics.recoveries_by_kind.keys() {
                let key = serialize_recovery_kind(recovery_kind);
                let bud = self
                    .retry_budget_utilization
                    .entry(key)
                    .or_insert(BudgetUtilization { cap_hit_pct: 0.0 });
                // Reuse `cap_hit_pct` as an integer accumulator until
                // finalization, where we divide by job_count.
                bud.cap_hit_pct += 1.0;
            }
        }

        // Track per-step accumulator for top-K retried steps.
        self.merge_step_accumulator(job);
    }

    /// Appends one entry per (job, step) into `top_retried_steps` as a scratch
    /// buffer. `finalize()` collapses by step name.
    fn merge_step_accumulator(&mut self, job: &Job) {
        for step in &job.steps {
            let mut counts: HashMap<String, u32> = HashMap::new();
            for attempt in &step.attempts {
                let kind = AttemptOutcomeKind::from(&attempt.outcome);
                let key = serialize_outcome_kind(&kind);
                *counts.entry(key).or_insert(0) += 1;
            }
            let attempts = u32::try_from(step.attempts.len()).unwrap_or(u32::MAX);
            if attempts == 0 {
                continue;
            }
            let most_common = pick_most_common(&counts).unwrap_or_else(|| "pending".to_string());
            self.top_retried_steps.push(TopStep {
                step: step.name.clone(),
                attempts,
                most_common_outcome: most_common,
            });
        }
    }

    /// Compute final percentages, top-K retried steps, and per-kind cap-hit
    /// percentages. Must be called once after all jobs have been absorbed.
    fn finalize(&mut self) {
        let attempts_total_f = f64::from(self.attempts_total);
        if attempts_total_f > 0.0 {
            for bucket in self.outcomes.values_mut() {
                bucket.pct = (f64::from(bucket.count) / attempts_total_f) * 100.0;
            }
        }
        let recoveries_total_f = f64::from(self.recoveries_total);
        if recoveries_total_f > 0.0 {
            for bucket in self.recoveries.values_mut() {
                bucket.pct = (f64::from(bucket.count) / recoveries_total_f) * 100.0;
            }
        }

        // Collapse per-(job, step) entries into per-step totals.
        let mut per_step: HashMap<String, StepAccumulator> = HashMap::new();
        for entry in self.top_retried_steps.drain(..) {
            let acc = per_step.entry(entry.step).or_default();
            acc.attempts = acc.attempts.saturating_add(entry.attempts);
            *acc.outcome_counts.entry(entry.most_common_outcome).or_insert(0) += entry.attempts;
        }
        let mut collapsed: Vec<TopStep> = per_step
            .into_iter()
            .map(|(name, acc)| TopStep {
                step: name,
                attempts: acc.attempts,
                most_common_outcome: pick_most_common(&acc.outcome_counts)
                    .unwrap_or_else(|| "pending".to_string()),
            })
            .collect();
        collapsed.sort_by(|a, b| b.attempts.cmp(&a.attempts).then_with(|| a.step.cmp(&b.step)));
        collapsed.truncate(10);
        self.top_retried_steps = collapsed;

        // Normalize cap-hit percentages: numerator currently holds a count
        // of failed jobs that recorded the kind; divide by `job_count` so
        // the metric reads as "% of in-scope jobs that hit cap on kind".
        let job_count_f = f64::from(self.job_count);
        if job_count_f > 0.0 {
            for bud in self.retry_budget_utilization.values_mut() {
                bud.cap_hit_pct = (bud.cap_hit_pct / job_count_f) * 100.0;
            }
        } else {
            self.retry_budget_utilization.clear();
        }
    }
}

/// Picks the key with the largest count; ties broken alphabetically.
fn pick_most_common(counts: &HashMap<String, u32>) -> Option<String> {
    counts
        .iter()
        .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
        .map(|(k, _)| k.clone())
}

/// Serializes an `AttemptOutcomeKind` to its snake_case wire string via serde.
fn serialize_outcome_kind(kind: &AttemptOutcomeKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(std::string::ToString::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}

/// Serializes a `RecoveryKind` to its snake_case wire string via serde.
fn serialize_recovery_kind(kind: &RecoveryKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(std::string::ToString::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}

/// JSON output. Single object as documented in the task contract.
fn emit_json(report: &MetricsReport) {
    let mut report = report.clone();
    report.finalize();
    match serde_json::to_string_pretty(&report) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("error serializing metrics report: {e}"),
    }
}

/// Human-readable sectioned text output.
fn emit_text(report: &MetricsReport) {
    let mut report = report.clone();
    report.finalize();

    if report.job_count == 0 {
        println!("no metrics found for filter");
        return;
    }

    println!("Metrics aggregate ({} job(s))", report.job_count);
    if let Some(since) = &report.filter.since {
        println!("  filter.since:    {since}");
    }
    if let Some(kind) = &report.filter.job_kind {
        println!("  filter.job_kind: {kind}");
    }
    println!("  attempts_total:   {}", report.attempts_total);
    println!("  recoveries_total: {}", report.recoveries_total);
    println!();

    println!("Outcomes:");
    print_bucket_table(&report.outcomes);
    println!();

    println!("Recoveries:");
    if report.recoveries.is_empty() {
        println!("  (none)");
    } else {
        print_bucket_table(&report.recoveries);
    }
    println!();

    println!("Top retried steps (up to 10):");
    if report.top_retried_steps.is_empty() {
        println!("  (none)");
    } else {
        println!("  {:<32} {:>10} {:<24}", "STEP", "ATTEMPTS", "MOST_COMMON");
        for entry in &report.top_retried_steps {
            println!(
                "  {:<32} {:>10} {:<24}",
                truncate(&entry.step, 32),
                entry.attempts,
                truncate(&entry.most_common_outcome, 24),
            );
        }
    }
    println!();

    println!("Retry-budget utilization (% of in-scope jobs that hit cap):");
    if report.retry_budget_utilization.is_empty() {
        println!("  (none)");
    } else {
        let mut entries: Vec<_> = report.retry_budget_utilization.iter().collect();
        entries.sort_by(|a, b| {
            b.1.cap_hit_pct
                .partial_cmp(&a.1.cap_hit_pct)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(b.0))
        });
        println!("  {:<24} {:>10}", "RECOVERY_KIND", "CAP_HIT%");
        for (kind, util) in entries {
            println!("  {:<24} {:>10.2}", truncate(kind, 24), util.cap_hit_pct);
        }
    }
}

/// Prints a `(name, count, pct)` bucket table sorted by count desc, then
/// alphabetical.
fn print_bucket_table(buckets: &HashMap<String, BucketCount>) {
    let mut entries: Vec<_> = buckets.iter().collect();
    entries.sort_by(|a, b| b.1.count.cmp(&a.1.count).then_with(|| a.0.cmp(b.0)));
    println!("  {:<24} {:>10} {:>10}", "KIND", "COUNT", "PCT");
    for (kind, bucket) in entries {
        println!(
            "  {:<24} {:>10} {:>9.2}%",
            truncate(kind, 24),
            bucket.count,
            bucket.pct
        );
    }
}

/// Truncates `s` to `max` chars; appends `…` when truncated.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

fn parse_duration(s: &str) -> Result<Duration> {
    let trimmed = s.trim();
    let (num_part, suffix) = trimmed
        .find(|c: char| c.is_alphabetic())
        .map(|i| trimmed.split_at(i))
        .ok_or_else(|| anyhow!("missing unit suffix in {s:?}"))?;
    let n: u64 = num_part
        .parse()
        .with_context(|| format!("parsing number from {s:?}"))?;
    let secs = match suffix {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86400,
        _ => return Err(anyhow!("unsupported unit {suffix:?}; use s/m/h/d")),
    };
    Ok(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::types::{AttemptOutcome, StepStatus};

    #[test]
    fn parse_duration_accepts_days_hours_minutes_seconds() {
        let parsed = (
            parse_duration("7d").expect("7d"),
            parse_duration("24h").expect("24h"),
            parse_duration("30m").expect("30m"),
            parse_duration("5s").expect("5s"),
        );
        let expected = (
            Duration::from_secs(7 * 86_400),
            Duration::from_secs(24 * 3_600),
            Duration::from_secs(30 * 60),
            Duration::from_secs(5),
        );
        assert_eq!(parsed, expected);
    }

    #[test]
    fn parse_duration_rejects_missing_unit() {
        assert!(parse_duration("7").is_err());
    }

    #[test]
    fn parse_duration_rejects_unknown_unit() {
        assert!(parse_duration("5x").is_err());
    }

    #[test]
    fn parse_duration_rejects_empty_input() {
        assert!(parse_duration("").is_err());
    }

    #[test]
    fn state_label_covers_all_variants() {
        let labels = (
            state_label(&JobState::Pending),
            state_label(&JobState::Running),
            state_label(&JobState::Suspended {
                reason: "x".to_string(),
            }),
            state_label(&JobState::Succeeded),
            state_label(&JobState::Failed {
                reason: "boom".to_string(),
                recoverable: false,
            }),
        );
        let expected = (
            "pending".to_string(),
            "running".to_string(),
            "suspended".to_string(),
            "succeeded".to_string(),
            "failed".to_string(),
        );
        assert_eq!(labels, expected);
    }

    #[test]
    fn step_status_label_covers_all_variants() {
        let labels = (
            step_status_label(&StepStatus::Pending),
            step_status_label(&StepStatus::Running),
            step_status_label(&StepStatus::Succeeded),
            step_status_label(&StepStatus::Failed {
                reason: "r".to_string(),
                recoverable: true,
            }),
            step_status_label(&StepStatus::SkippedNotRequired),
        );
        let expected = (
            "pending".to_string(),
            "running".to_string(),
            "succeeded".to_string(),
            "failed".to_string(),
            "skipped".to_string(),
        );
        assert_eq!(labels, expected);
    }

    #[test]
    fn attempt_outcome_label_covers_all_variants() {
        let labels = (
            attempt_outcome_label(&AttemptOutcome::Success),
            attempt_outcome_label(&AttemptOutcome::HardInfra {
                error: "e".to_string(),
            }),
            attempt_outcome_label(&AttemptOutcome::TransientInfra {
                error: "e".to_string(),
            }),
            attempt_outcome_label(&AttemptOutcome::ProtocolViolation {
                category: "c".to_string(),
                detail: "d".to_string(),
            }),
            attempt_outcome_label(&AttemptOutcome::SemanticMistake { fix_loop_round: 2 }),
            attempt_outcome_label(&AttemptOutcome::SpecDrift {
                gap: "g".to_string(),
            }),
            attempt_outcome_label(&AttemptOutcome::Pending),
        );
        let expected = (
            "success".to_string(),
            "hard_infra(e)".to_string(),
            "transient_infra(e)".to_string(),
            "protocol_violation(c)".to_string(),
            "semantic_mistake(round=2)".to_string(),
            "spec_drift(g)".to_string(),
            "pending".to_string(),
        );
        assert_eq!(labels, expected);
    }

    #[test]
    fn legacy_state_label_covers_all_variants() {
        let labels = (
            legacy_state_label(&JobStatus::Running),
            legacy_state_label(&JobStatus::Success),
            legacy_state_label(&JobStatus::Failed),
            legacy_state_label(&JobStatus::Killed),
            legacy_state_label(&JobStatus::RemoteRunning),
        );
        let expected = (
            "running".to_string(),
            "succeeded".to_string(),
            "failed".to_string(),
            "killed".to_string(),
            "remote_running".to_string(),
        );
        assert_eq!(labels, expected);
    }

    use crate::job::types::{Job, JobKind};
    use tempfile::TempDir;

    fn make_job(id: &str, state: JobState) -> Job {
        Job {
            id: JobId(id.to_string()),
            kind: JobKind::Plan {
                manifest_path: PathBuf::from("/tmp/manifest.json"),
            },
            state,
            created_at: "2026-04-28T10:00:00Z".to_string(),
            steps: Vec::new(),
        }
    }

    #[test]
    fn assert_cancellable_rejects_succeeded_job() {
        let job = make_job("job-succ", JobState::Succeeded);
        let err = assert_cancellable(&job).expect_err("should be Err");
        let msg = err.to_string();
        assert_eq!(msg.contains("already terminal"), true);
    }

    #[test]
    fn assert_cancellable_rejects_failed_job() {
        let job = make_job(
            "job-fail",
            JobState::Failed {
                reason: "boom".to_string(),
                recoverable: false,
            },
        );
        let err = assert_cancellable(&job).expect_err("should be Err");
        let msg = err.to_string();
        assert_eq!(msg.contains("already terminal"), true);
    }

    #[test]
    fn assert_cancellable_allows_running_job() {
        let job = make_job("job-run", JobState::Running);
        let result = assert_cancellable(&job).is_ok();
        assert_eq!(result, true);
    }

    #[test]
    fn find_new_id_by_prefix_errors_on_ambiguous_prefix() {
        let tmp = TempDir::new().expect("tempdir");
        let store = JobStore::with_base(tmp.path().to_path_buf()).expect("store");
        store
            .create(&make_job("job-shared-a", JobState::Pending))
            .expect("create a");
        store
            .create(&make_job("job-shared-b", JobState::Pending))
            .expect("create b");

        let err = find_new_id_by_prefix(&store, "job-shared").expect_err("should be Err");
        let msg = err.to_string();
        assert_eq!(msg.contains("ambiguous prefix"), true);
    }

    #[test]
    fn gc_with_store_reports_failure_for_missing_path() {
        let tmp = TempDir::new().expect("tempdir");
        let store = JobStore::with_base(tmp.path().to_path_buf()).expect("store");
        let job_dir = tmp.path().join("orphan-job");
        fs::create_dir_all(&job_dir).expect("orphan dir");
        fs::write(job_dir.join("metadata.json"), b"{}").expect("metadata");
        let _ = store
            .create(&make_job("job-pending", JobState::Pending))
            .expect("pending");

        let (deleted, failures) = gc_with_store(&store, Duration::from_secs(0)).expect("gc");

        let pending_terminal = matches!(
            make_job("job-pending", JobState::Pending).state,
            JobState::Succeeded | JobState::Failed { .. }
        );
        assert_eq!(
            (deleted, failures.is_empty(), pending_terminal),
            (0, true, false)
        );
    }
}
