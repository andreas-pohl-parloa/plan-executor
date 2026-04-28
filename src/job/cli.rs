//! Handlers for the `plan-executor jobs <subcommand>` CLI surface.
//!
//! Renders both the new (`job.json`) and legacy (`metadata.json`) layouts in
//! one table during the migration grace window, and provides per-job
//! show/cancel/gc/replay verbs. The replay verb is an informational stub in
//! Phase A; full replay arrives in Phase D.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Context, Result};

use crate::cli::JobsCommand;
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
    }
}

fn cmd_list() -> Result<()> {
    let store = JobStore::new().context("opening job store")?;
    let entries = store.list_all().context("listing jobs")?;
    if entries.is_empty() {
        println!("No jobs.");
        return Ok(());
    }
    println!(
        "{:<38} {:<18} {:<16} {:<24} {}",
        "ID", "KIND", "STATE", "CREATED_AT", "LAYOUT"
    );
    for entry in entries {
        match entry {
            JobStoreEntry::New { summary, .. } => {
                println!(
                    "{:<38} {:<18} {:<16} {:<24} {}",
                    summary.id.0,
                    summary.kind_tag,
                    state_label(&summary.state),
                    summary.created_at,
                    "new"
                );
            }
            JobStoreEntry::Legacy { id, path } => {
                let (kind, state, created) = legacy_summary(&path);
                println!(
                    "{:<38} {:<18} {:<16} {:<24} {}",
                    id, kind, state, created, "legacy"
                );
            }
        }
    }
    Ok(())
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

fn cmd_cancel(id_prefix: &str) -> Result<()> {
    let store = JobStore::new()?;
    let found_id = find_new_id_by_prefix(&store, id_prefix)?.ok_or_else(|| {
        anyhow!("cancel only applies to new-layout jobs; no match for {id_prefix:?}")
    })?;
    let dir = store.open(&found_id)?;
    let mut job: Job = dir.read_job()?;
    job.state = JobState::Failed {
        reason: "cancelled by user".to_string(),
        recoverable: false,
    };
    dir.write_job_metadata(&job)?;
    println!("Cancelled {}", found_id.0);
    Ok(())
}

fn cmd_gc(older_than: Option<&str>) -> Result<()> {
    let threshold = parse_duration(older_than.unwrap_or("30d"))?;
    let cutoff = SystemTime::now()
        .checked_sub(threshold)
        .ok_or_else(|| anyhow!("threshold too large"))?;
    let store = JobStore::new()?;
    let entries = store.list_all()?;
    let mut deleted = 0_u32;
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
        let Ok(metadata) = fs::metadata(&path) else {
            continue;
        };
        let created = metadata.created().or_else(|_| metadata.modified()).ok();
        if let Some(t) = created {
            if t < cutoff && fs::remove_dir_all(&path).is_ok() {
                deleted += 1;
            }
        }
    }
    println!(
        "Garbage-collected {deleted} job director{}.",
        if deleted == 1 { "y" } else { "ies" }
    );
    Ok(())
}

fn cmd_replay(id: &str, from_step: Option<u32>) -> Result<()> {
    let from = from_step.map_or_else(|| "<beginning>".to_string(), |n| n.to_string());
    println!("replay {id} (from-step {from}): not yet implemented (Phase D)");
    Ok(())
}

fn find_new_id_by_prefix(store: &JobStore, prefix: &str) -> Result<Option<JobId>> {
    for entry in store.list_all()? {
        if let JobStoreEntry::New { summary, .. } = entry {
            if summary.id.0.starts_with(prefix) {
                return Ok(Some(summary.id));
            }
        }
    }
    Ok(None)
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

fn legacy_summary(path: &std::path::Path) -> (String, String, String) {
    let mut kind = "plan".to_string();
    let mut state = "?".to_string();
    let mut created = "-".to_string();
    if let Ok(raw) = fs::read_to_string(path.join("metadata.json")) {
        if let Ok(meta) = serde_json::from_str::<JobMetadata>(&raw) {
            kind = if meta.remote_repo.is_some() {
                "plan(remote)".to_string()
            } else {
                "plan".to_string()
            };
            state = legacy_state_label(&meta.status);
            created = meta.started_at.to_rfc3339();
        }
    }
    (kind, state, created)
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
        PrFinalize { owner, repo, pr } => format!("pr_finalize({owner}/{repo}#{pr})"),
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
        let result = parse_duration("7").is_err();
        assert_eq!(result, true);
    }

    #[test]
    fn parse_duration_rejects_unknown_unit() {
        let result = parse_duration("5x").is_err();
        assert_eq!(result, true);
    }

    #[test]
    fn parse_duration_rejects_empty_input() {
        let result = parse_duration("").is_err();
        assert_eq!(result, true);
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
}
