//! Per-step append-only on-disk storage for jobs.
//!
//! Layout under `~/.plan-executor/jobs/<job-id>/`:
//!
//! ```text
//! job.json                       # full Job, updated on transitions
//! steps/
//!   001-preflight/
//!     input.json                 # immutable per attempt write
//!     checkpoint.json            # snapshot before run (optional)
//!     attempts/
//!       1/
//!         started_at             # ISO 8601 timestamp file
//!         finished_at            # ISO 8601 timestamp file
//!         outcome.json           # AttemptOutcome JSON
//!         stdout.log             # may be empty
//!         stderr.log             # may be empty
//!     output.json                # only after step Succeeded
//! ```
//!
//! Step directory naming uses `NNN-<name>` with `seq` zero-padded to 3 digits
//! and `<name>` taken from the step's `name()`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::Config;
use crate::job::metrics::JobMetrics;
use crate::job::types::{AttemptOutcome, Job, JobId, JobState};

/// Top-level handle on `~/.plan-executor/jobs/`.
#[derive(Debug, Clone)]
pub struct JobStore {
    base: PathBuf,
}

/// Handle on a single job's directory (`~/.plan-executor/jobs/<id>/`).
#[derive(Debug, Clone)]
pub struct JobDir {
    job_id: JobId,
    path: PathBuf,
}

/// Lightweight summary used by `list()` (avoids reading every step on disk).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSummary {
    /// Job identifier.
    pub id: JobId,
    /// Lifecycle state at the time of listing.
    pub state: JobState,
    /// ISO 8601 UTC timestamp of creation.
    pub created_at: String,
    /// Short string tag of the JobKind variant (e.g. `plan`, `pr_finalize`).
    pub kind_tag: String,
}

/// Returned from `record_attempt_start`; required to call `record_attempt_finish`.
/// Carries the seq/attempt number and the directory path so callers don't have
/// to recompute it.
#[derive(Debug, Clone)]
pub struct AttemptHandle {
    /// Sequence number of the parent step.
    pub seq: u32,
    /// 1-based attempt number for this step.
    pub attempt: u32,
    /// Directory holding the attempt's files.
    pub dir: PathBuf,
}

/// Lightweight handle on either a new-layout (`job.json`) or legacy-layout
/// (`metadata.json` only) job. The `jobs list` / `jobs show` commands use
/// this to render both in one table during the migration grace window.
#[derive(Debug, Clone)]
pub enum JobStoreEntry {
    /// New-layout job; `summary` is parsed from `job.json`.
    New {
        /// Parsed summary of the new-layout job.
        summary: JobSummary,
        /// Filesystem path to the job directory.
        path: PathBuf,
    },
    /// Legacy-layout job with `metadata.json` but no `job.json`. The id is
    /// the directory name; the caller decides how to render the rest.
    Legacy {
        /// Directory-name id for the legacy job.
        id: String,
        /// Filesystem path to the legacy job directory.
        path: PathBuf,
    },
}

/// Errors produced by `JobStore` and `JobDir` operations.
#[derive(Debug, Error)]
pub enum JobStoreError {
    /// I/O error against the on-disk layout.
    #[error("io error at {path}: {source}")]
    Io {
        /// Path that triggered the error.
        path: PathBuf,
        /// Underlying I/O error.
        source: io::Error,
    },
    /// JSON serialization or deserialization failure.
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    /// No job directory exists for the given id.
    #[error("job not found: {0:?}")]
    JobNotFound(JobId),
    /// Step directory missing for the given seq.
    #[error("step seq {seq} directory not found in job {job_id:?}")]
    StepNotFound {
        /// Owning job id.
        job_id: JobId,
        /// Step sequence number that was missing.
        seq: u32,
    },
    /// Base directory is unavailable for store creation.
    #[error("base dir is unavailable: {0}")]
    BaseDirUnavailable(String),
}

type Result<T> = std::result::Result<T, JobStoreError>;

impl JobStore {
    /// Default store at `Config::base_dir().join("jobs")`.
    ///
    /// # Errors
    ///
    /// Returns `JobStoreError::Io` if the base directory cannot be created.
    pub fn new() -> Result<Self> {
        let base = Config::base_dir().join("jobs");
        fs::create_dir_all(&base).map_err(|source| JobStoreError::Io {
            path: base.clone(),
            source,
        })?;
        Ok(Self { base })
    }

    /// Test-friendly constructor; allows pointing at an arbitrary base dir
    /// (e.g., a tempdir).
    ///
    /// # Errors
    ///
    /// Returns `JobStoreError::Io` if the base directory cannot be created.
    pub fn with_base(base: PathBuf) -> Result<Self> {
        fs::create_dir_all(&base).map_err(|source| JobStoreError::Io {
            path: base.clone(),
            source,
        })?;
        Ok(Self { base })
    }

    /// Create the on-disk layout for a new `Job`.
    ///
    /// Writes `job.json`, creates `steps/` and per-step subdirs, but does
    /// NOT write per-step inputs/outputs (callers do that). Idempotent: if the
    /// job dir already exists, returns the existing handle.
    ///
    /// # Errors
    ///
    /// Returns `JobStoreError::Io` for filesystem errors and
    /// `JobStoreError::Serde` for serialization errors.
    pub fn create(&self, job: &Job) -> Result<JobDir> {
        let path = self.base.join(&job.id.0);
        if path.exists() {
            return Ok(JobDir {
                job_id: job.id.clone(),
                path,
            });
        }
        fs::create_dir_all(path.join("steps")).map_err(|source| JobStoreError::Io {
            path: path.clone(),
            source,
        })?;
        for step in &job.steps {
            let step_dir = step_dir_for(&path, step.seq, &step.name);
            fs::create_dir_all(step_dir.join("attempts")).map_err(|source| JobStoreError::Io {
                path: step_dir.clone(),
                source,
            })?;
        }
        let job_dir = JobDir {
            job_id: job.id.clone(),
            path,
        };
        job_dir.write_job_metadata(job)?;
        Ok(job_dir)
    }

    /// Open an existing job directory by id.
    ///
    /// # Errors
    ///
    /// Returns `JobStoreError::JobNotFound` if no directory exists for `id`.
    pub fn open(&self, id: &JobId) -> Result<JobDir> {
        let path = self.base.join(&id.0);
        if !path.exists() {
            return Err(JobStoreError::JobNotFound(id.clone()));
        }
        Ok(JobDir {
            job_id: id.clone(),
            path,
        })
    }

    /// Lists jobs in the store (only those with a readable `job.json`).
    ///
    /// Returns most-recent first by `created_at` (string sort works because
    /// `created_at` is ISO 8601 UTC).
    ///
    /// # Errors
    ///
    /// Returns `JobStoreError::Io` if the base directory cannot be read.
    pub fn list(&self) -> Result<Vec<JobSummary>> {
        let mut out = Vec::new();
        let entries = match fs::read_dir(&self.base) {
            Ok(e) => e,
            Err(source) => {
                return Err(JobStoreError::Io {
                    path: self.base.clone(),
                    source,
                });
            }
        };
        for entry in entries.flatten() {
            let job_path = entry.path();
            let json_path = job_path.join("job.json");
            if !json_path.is_file() {
                continue;
            }
            let raw = match fs::read_to_string(&json_path) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let job: Job = match serde_json::from_str(&raw) {
                Ok(j) => j,
                Err(_) => continue,
            };
            out.push(JobSummary {
                id: job.id,
                state: job.state,
                created_at: job.created_at,
                kind_tag: kind_tag(&job.kind),
            });
        }
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(out)
    }

    /// Lists every job directory under `base`, classifying each as `New`
    /// (has `job.json`) or `Legacy` (has `metadata.json` only). Sorted with
    /// new-layout entries first (by `created_at` descending), then legacy
    /// entries by directory name descending.
    ///
    /// # Errors
    ///
    /// Returns `JobStoreError::Io` if the base directory cannot be read.
    pub fn list_all(&self) -> Result<Vec<JobStoreEntry>> {
        let mut out = Vec::new();
        let entries = match fs::read_dir(&self.base) {
            Ok(e) => e,
            Err(source) => {
                return Err(JobStoreError::Io {
                    path: self.base.clone(),
                    source,
                });
            }
        };
        for entry in entries.flatten() {
            let job_path = entry.path();
            if !job_path.is_dir() {
                continue;
            }
            let job_json = job_path.join("job.json");
            if job_json.is_file() {
                if let Ok(raw) = fs::read_to_string(&job_json) {
                    if let Ok(job) = serde_json::from_str::<Job>(&raw) {
                        out.push(JobStoreEntry::New {
                            summary: JobSummary {
                                id: job.id,
                                state: job.state,
                                created_at: job.created_at,
                                kind_tag: kind_tag(&job.kind),
                            },
                            path: job_path,
                        });
                        continue;
                    }
                }
            }
            if job_path.join("metadata.json").is_file() {
                let id = job_path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
                    .to_string();
                out.push(JobStoreEntry::Legacy { id, path: job_path });
            }
        }
        out.sort_by(|a, b| match (a, b) {
            (JobStoreEntry::New { summary: a, .. }, JobStoreEntry::New { summary: b, .. }) => {
                b.created_at.cmp(&a.created_at)
            }
            (JobStoreEntry::New { .. }, JobStoreEntry::Legacy { .. }) => std::cmp::Ordering::Less,
            (JobStoreEntry::Legacy { .. }, JobStoreEntry::New { .. }) => {
                std::cmp::Ordering::Greater
            }
            (JobStoreEntry::Legacy { id: a, .. }, JobStoreEntry::Legacy { id: b, .. }) => b.cmp(a),
        });
        Ok(out)
    }
}

fn kind_tag(kind: &crate::job::types::JobKind) -> String {
    use crate::job::types::JobKind::{CompileFixWaves, Plan, PrFinalize, Review, Validate};
    match kind {
        Plan { .. } => "plan",
        PrFinalize { .. } => "pr_finalize",
        Review { .. } => "review",
        Validate { .. } => "validate",
        CompileFixWaves { .. } => "compile_fix_waves",
    }
    .to_string()
}

fn step_dir_for(job_path: &Path, seq: u32, name: &str) -> PathBuf {
    job_path.join("steps").join(format!("{seq:03}-{name}"))
}

impl JobDir {
    /// Job identifier this directory belongs to.
    pub fn job_id(&self) -> &JobId {
        &self.job_id
    }

    /// Filesystem path to the job directory.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Persist the full `Job` metadata at `job.json`.
    ///
    /// # Errors
    ///
    /// Returns `JobStoreError::Io` or `JobStoreError::Serde` on failure.
    pub fn write_job_metadata(&self, job: &Job) -> Result<()> {
        let path = self.path.join("job.json");
        let json = serde_json::to_string_pretty(job)?;
        write_atomic(&path, json.as_bytes())
    }

    /// Reads back `job.json`. Useful for replay.
    ///
    /// # Errors
    ///
    /// Returns `JobStoreError::Io` if the file cannot be read,
    /// `JobStoreError::Serde` if it cannot be parsed.
    pub fn read_job(&self) -> Result<Job> {
        let path = self.path.join("job.json");
        let raw = fs::read_to_string(&path).map_err(|source| JobStoreError::Io {
            path: path.clone(),
            source,
        })?;
        Ok(serde_json::from_str(&raw)?)
    }

    /// Persist a [`JobMetrics`] snapshot to `metrics.json`.
    ///
    /// Uses the same atomic write-temp + rename strategy as `job.json` so a
    /// crash mid-write cannot leave a torn file behind.
    ///
    /// # Errors
    ///
    /// Returns `JobStoreError::Io` or `JobStoreError::Serde` on failure.
    pub fn write_metrics(&self, metrics: &JobMetrics) -> Result<()> {
        let path = self.path.join("metrics.json");
        let json = serde_json::to_string_pretty(metrics)?;
        write_atomic(&path, json.as_bytes())
    }

    /// Read the persisted [`JobMetrics`] snapshot if present.
    ///
    /// Returns `Ok(None)` when `metrics.json` does not exist (e.g., a job
    /// that has not yet recorded any attempts).
    ///
    /// # Errors
    ///
    /// Returns `JobStoreError::Io` for read errors other than `NotFound`,
    /// and `JobStoreError::Serde` if the file cannot be parsed.
    pub fn read_metrics(&self) -> Result<Option<JobMetrics>> {
        let path = self.path.join("metrics.json");
        let raw = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(JobStoreError::Io {
                    path: path.clone(),
                    source,
                });
            }
        };
        Ok(Some(serde_json::from_str(&raw)?))
    }

    /// Write the immutable per-step input.
    ///
    /// # Errors
    ///
    /// Returns `JobStoreError::Io` or `JobStoreError::Serde` on failure.
    pub fn write_step_input(&self, seq: u32, name: &str, input: &serde_json::Value) -> Result<()> {
        let dir = step_dir_for(&self.path, seq, name);
        fs::create_dir_all(&dir).map_err(|source| JobStoreError::Io {
            path: dir.clone(),
            source,
        })?;
        write_atomic(
            &dir.join("input.json"),
            serde_json::to_string_pretty(input)?.as_bytes(),
        )
    }

    /// Write a pre-run checkpoint for a step.
    ///
    /// # Errors
    ///
    /// Returns `JobStoreError::Io` or `JobStoreError::Serde` on failure.
    pub fn write_step_checkpoint(
        &self,
        seq: u32,
        name: &str,
        value: &serde_json::Value,
    ) -> Result<()> {
        let dir = step_dir_for(&self.path, seq, name);
        fs::create_dir_all(&dir).map_err(|source| JobStoreError::Io {
            path: dir.clone(),
            source,
        })?;
        write_atomic(
            &dir.join("checkpoint.json"),
            serde_json::to_string_pretty(value)?.as_bytes(),
        )
    }

    /// Begin a step attempt. Creates `attempts/<n>/` and writes `started_at`.
    ///
    /// # Errors
    ///
    /// Returns `JobStoreError::Io` if the attempt directory cannot be created.
    pub fn record_attempt_start(&self, seq: u32, name: &str, n: u32) -> Result<AttemptHandle> {
        let attempt_dir = step_dir_for(&self.path, seq, name)
            .join("attempts")
            .join(n.to_string());
        fs::create_dir_all(&attempt_dir).map_err(|source| JobStoreError::Io {
            path: attempt_dir.clone(),
            source,
        })?;
        let started = Utc::now().to_rfc3339();
        write_atomic(&attempt_dir.join("started_at"), started.as_bytes())?;
        Ok(AttemptHandle {
            seq,
            attempt: n,
            dir: attempt_dir,
        })
    }

    /// Finish a step attempt. Writes `finished_at` and `outcome.json`.
    ///
    /// # Errors
    ///
    /// Returns `JobStoreError::Io` or `JobStoreError::Serde` on failure.
    pub fn record_attempt_finish(
        &self,
        handle: AttemptHandle,
        outcome: &AttemptOutcome,
    ) -> Result<()> {
        let finished = Utc::now().to_rfc3339();
        write_atomic(&handle.dir.join("finished_at"), finished.as_bytes())?;
        let outcome_json = serde_json::to_string_pretty(outcome)?;
        write_atomic(&handle.dir.join("outcome.json"), outcome_json.as_bytes())
    }

    /// Persist per-step output (only after a successful run).
    ///
    /// # Errors
    ///
    /// Returns `JobStoreError::Io` or `JobStoreError::Serde` on failure.
    pub fn write_step_output(
        &self,
        seq: u32,
        name: &str,
        output: &serde_json::Value,
    ) -> Result<()> {
        let dir = step_dir_for(&self.path, seq, name);
        write_atomic(
            &dir.join("output.json"),
            serde_json::to_string_pretty(output)?.as_bytes(),
        )
    }

    /// Returns `<job>/steps/NNN-<name>/attempts/M/sub-agents/`, creating the
    /// directory if it does not exist. Used by future Phase B+ daemon code
    /// that knows the current step seq and attempt number; the legacy daemon
    /// still uses `legacy_sub_agent_dir()`.
    ///
    /// # Errors
    ///
    /// Returns `JobStoreError::Io` if the directory cannot be created.
    pub fn sub_agent_dir_for_attempt(
        &self,
        seq: u32,
        name: &str,
        attempt_n: u32,
    ) -> Result<PathBuf> {
        let dir = step_dir_for(&self.path, seq, name)
            .join("attempts")
            .join(attempt_n.to_string())
            .join("sub-agents");
        fs::create_dir_all(&dir).map_err(|source| JobStoreError::Io {
            path: dir.clone(),
            source,
        })?;
        Ok(dir)
    }

    /// Returns `<job>/sub-agents/` — the pre-Job-framework layout. Used by
    /// the upcoming `jobs show` command to render historical sub-agent
    /// outputs that predate Phase A. Does not create the directory.
    #[must_use]
    pub fn legacy_sub_agent_dir(&self) -> PathBuf {
        self.path.join("sub-agents")
    }

    /// Returns the seq of the next step that has no `output.json` yet,
    /// based on the persisted `job.json` order.
    ///
    /// # Errors
    ///
    /// Returns `JobStoreError::Io` or `JobStoreError::Serde` if `job.json`
    /// cannot be read.
    pub fn next_pending_step(&self) -> Result<Option<u32>> {
        let job = self.read_job()?;
        for step in &job.steps {
            let dir = step_dir_for(&self.path, step.seq, &step.name);
            if !dir.join("output.json").is_file() {
                return Ok(Some(step.seq));
            }
        }
        Ok(None)
    }
}

/// Atomic write: write to `<path>.tmp` then rename. Avoids torn files when
/// the process crashes mid-write.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes).map_err(|source| JobStoreError::Io {
        path: tmp.clone(),
        source,
    })?;
    fs::rename(&tmp, path).map_err(|source| JobStoreError::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::types::{JobKind, StepInstance, StepStatus};
    use tempfile::TempDir;

    fn sample_job(id: &str, steps: Vec<(u32, &str)>) -> Job {
        Job {
            id: JobId(id.to_string()),
            kind: JobKind::Plan {
                manifest_path: PathBuf::from("/tmp/manifest.json"),
            },
            state: JobState::Pending,
            created_at: "2026-04-28T10:00:00Z".to_string(),
            steps: steps
                .into_iter()
                .map(|(seq, name)| StepInstance {
                    seq,
                    name: name.to_string(),
                    status: StepStatus::Pending,
                    attempts: Vec::new(),
                    idempotent: true,
                })
                .collect(),
        }
    }

    #[test]
    fn with_base_creates_directory() {
        let tmp = TempDir::new().expect("tempdir");
        let base = tmp.path().join("store");
        let _store = JobStore::with_base(base.clone()).expect("with_base");
        assert!(base.is_dir());
    }

    #[test]
    fn create_writes_job_json_and_step_dirs() {
        let tmp = TempDir::new().expect("tempdir");
        let store = JobStore::with_base(tmp.path().to_path_buf()).expect("store");
        let job = sample_job("job-A", vec![(1, "preflight"), (2, "wave_execution")]);

        let dir = store.create(&job).expect("create");

        assert!(dir.path().join("job.json").is_file());
        assert!(dir.path().join("steps").join("001-preflight").is_dir());
        assert!(dir
            .path()
            .join("steps")
            .join("001-preflight")
            .join("attempts")
            .is_dir());
        assert!(dir.path().join("steps").join("002-wave_execution").is_dir());
    }

    #[test]
    fn round_trip_create_open_read_returns_identical_job() {
        let tmp = TempDir::new().expect("tempdir");
        let store = JobStore::with_base(tmp.path().to_path_buf()).expect("store");
        let job = sample_job("job-rt", vec![(1, "preflight"), (2, "wave_execution")]);

        store.create(&job).expect("create");
        let opened = store.open(&job.id).expect("open");
        let read_back = opened.read_job().expect("read_job");

        assert_eq!(read_back, job);
    }

    #[test]
    fn per_step_input_checkpoint_output_writes_land_at_expected_paths() {
        let tmp = TempDir::new().expect("tempdir");
        let store = JobStore::with_base(tmp.path().to_path_buf()).expect("store");
        let job = sample_job("job-paths", vec![(1, "preflight")]);
        let dir = store.create(&job).expect("create");

        let value = serde_json::json!({"k": "v"});
        dir.write_step_input(1, "preflight", &value).expect("input");
        dir.write_step_checkpoint(1, "preflight", &value)
            .expect("checkpoint");
        dir.write_step_output(1, "preflight", &value)
            .expect("output");

        let step_dir = dir.path().join("steps").join("001-preflight");
        let on_disk = (
            step_dir.join("input.json").is_file(),
            step_dir.join("checkpoint.json").is_file(),
            step_dir.join("output.json").is_file(),
        );
        assert_eq!(on_disk, (true, true, true));
    }

    #[test]
    fn record_attempt_start_then_finish_writes_three_files() {
        let tmp = TempDir::new().expect("tempdir");
        let store = JobStore::with_base(tmp.path().to_path_buf()).expect("store");
        let job = sample_job("job-attempt", vec![(1, "preflight")]);
        let dir = store.create(&job).expect("create");

        let handle = dir.record_attempt_start(1, "preflight", 1).expect("start");
        dir.record_attempt_finish(handle.clone(), &AttemptOutcome::Success)
            .expect("finish");

        let attempt_dir = dir
            .path()
            .join("steps")
            .join("001-preflight")
            .join("attempts")
            .join("1");
        let on_disk = (
            attempt_dir.join("started_at").is_file(),
            attempt_dir.join("finished_at").is_file(),
            attempt_dir.join("outcome.json").is_file(),
        );
        assert_eq!(on_disk, (true, true, true));
    }

    #[test]
    fn record_attempt_start_n2_does_not_overwrite_n1() {
        let tmp = TempDir::new().expect("tempdir");
        let store = JobStore::with_base(tmp.path().to_path_buf()).expect("store");
        let job = sample_job("job-multi", vec![(1, "preflight")]);
        let dir = store.create(&job).expect("create");

        let h1 = dir
            .record_attempt_start(1, "preflight", 1)
            .expect("start n1");
        dir.record_attempt_finish(h1, &AttemptOutcome::Success)
            .expect("finish n1");
        let _h2 = dir
            .record_attempt_start(1, "preflight", 2)
            .expect("start n2");

        let attempts_dir = dir
            .path()
            .join("steps")
            .join("001-preflight")
            .join("attempts");
        let dirs_present = (
            attempts_dir.join("1").is_dir(),
            attempts_dir.join("1").join("outcome.json").is_file(),
            attempts_dir.join("2").is_dir(),
            attempts_dir.join("2").join("started_at").is_file(),
        );
        assert_eq!(dirs_present, (true, true, true, true));
    }

    #[test]
    fn next_pending_step_returns_first_step_missing_output() {
        let tmp = TempDir::new().expect("tempdir");
        let store = JobStore::with_base(tmp.path().to_path_buf()).expect("store");
        let job = sample_job(
            "job-pend",
            vec![(1, "preflight"), (2, "wave_execution"), (3, "verify")],
        );
        let dir = store.create(&job).expect("create");
        let val = serde_json::json!({"ok": true});

        dir.write_step_output(1, "preflight", &val).expect("o1");
        dir.write_step_output(2, "wave_execution", &val)
            .expect("o2");
        let mid = dir.next_pending_step().expect("mid");
        dir.write_step_output(3, "verify", &val).expect("o3");
        let after = dir.next_pending_step().expect("after");

        assert_eq!((mid, after), (Some(3), None));
    }

    #[test]
    fn list_orders_most_recent_first() {
        let tmp = TempDir::new().expect("tempdir");
        let store = JobStore::with_base(tmp.path().to_path_buf()).expect("store");
        let mut older = sample_job("job-old", vec![(1, "preflight")]);
        older.created_at = "2026-01-01T00:00:00Z".to_string();
        let mut newer = sample_job("job-new", vec![(1, "preflight")]);
        newer.created_at = "2026-04-01T00:00:00Z".to_string();
        store.create(&older).expect("create old");
        store.create(&newer).expect("create new");

        let summaries = store.list().expect("list");
        let ids: Vec<JobId> = summaries.into_iter().map(|s| s.id).collect();

        assert_eq!(ids, vec![newer.id.clone(), older.id.clone()]);
    }

    #[test]
    fn list_skips_entries_without_readable_job_json() {
        let tmp = TempDir::new().expect("tempdir");
        let store = JobStore::with_base(tmp.path().to_path_buf()).expect("store");
        let job = sample_job("job-only", vec![(1, "preflight")]);
        store.create(&job).expect("create");
        fs::create_dir_all(tmp.path().join("orphan-dir")).expect("orphan");

        let summaries = store.list().expect("list");
        let ids: Vec<JobId> = summaries.into_iter().map(|s| s.id).collect();

        assert_eq!(ids, vec![JobId("job-only".to_string())]);
    }

    #[test]
    fn sub_agent_dir_for_attempt_creates_and_returns_expected_path() {
        let tmp = TempDir::new().expect("tempdir");
        let store = JobStore::with_base(tmp.path().to_path_buf()).expect("store");
        let job = sample_job("job-sa", vec![(2, "wave_execution")]);
        let dir = store.create(&job).expect("create");

        let path = dir
            .sub_agent_dir_for_attempt(2, "wave_execution", 3)
            .expect("sub_agent_dir");

        let suffix: PathBuf = ["steps", "002-wave_execution", "attempts", "3", "sub-agents"]
            .iter()
            .collect();
        let suffix_match = path.ends_with(&suffix);
        assert_eq!((suffix_match, path.is_dir()), (true, true));
    }

    #[test]
    fn sub_agent_dir_for_attempt_is_idempotent() {
        let tmp = TempDir::new().expect("tempdir");
        let store = JobStore::with_base(tmp.path().to_path_buf()).expect("store");
        let job = sample_job("job-sa-idem", vec![(1, "preflight")]);
        let dir = store.create(&job).expect("create");

        let p1 = dir
            .sub_agent_dir_for_attempt(1, "preflight", 1)
            .expect("first");
        let p2 = dir
            .sub_agent_dir_for_attempt(1, "preflight", 1)
            .expect("second");

        assert_eq!((p1.clone(), p1.is_dir()), (p2, true));
    }

    #[test]
    fn legacy_sub_agent_dir_returns_path_without_creating() {
        let tmp = TempDir::new().expect("tempdir");
        let store = JobStore::with_base(tmp.path().to_path_buf()).expect("store");
        let job = sample_job("job-legacy-sa", vec![(1, "preflight")]);
        let dir = store.create(&job).expect("create");

        let path = dir.legacy_sub_agent_dir();

        let expected = dir.path().join("sub-agents");
        assert_eq!((path.clone(), path.is_dir()), (expected, false));
    }

    #[test]
    fn list_all_returns_only_new_layout_when_no_legacy_dirs() {
        let tmp = TempDir::new().expect("tempdir");
        let store = JobStore::with_base(tmp.path().to_path_buf()).expect("store");
        let mut older = sample_job("job-la-old", vec![(1, "preflight")]);
        older.created_at = "2026-01-01T00:00:00Z".to_string();
        let mut newer = sample_job("job-la-new", vec![(1, "preflight")]);
        newer.created_at = "2026-04-01T00:00:00Z".to_string();
        store.create(&older).expect("create old");
        store.create(&newer).expect("create new");

        let entries = store.list_all().expect("list_all");
        let ids: Vec<JobId> = entries
            .into_iter()
            .map(|e| match e {
                JobStoreEntry::New { summary, .. } => summary.id,
                JobStoreEntry::Legacy { id, .. } => JobId(id),
            })
            .collect();

        assert_eq!(ids, vec![newer.id.clone(), older.id.clone()]);
    }

    #[test]
    fn list_all_classifies_metadata_only_dirs_as_legacy() {
        let tmp = TempDir::new().expect("tempdir");
        let store = JobStore::with_base(tmp.path().to_path_buf()).expect("store");
        let legacy_dir = tmp.path().join("legacy-job-1");
        fs::create_dir_all(&legacy_dir).expect("legacy dir");
        fs::write(legacy_dir.join("metadata.json"), b"{\"any\":\"json\"}").expect("metadata");

        let entries = store.list_all().expect("list_all");
        let classified: Vec<(String, bool)> = entries
            .into_iter()
            .map(|e| match e {
                JobStoreEntry::New { summary, .. } => (summary.id.0, true),
                JobStoreEntry::Legacy { id, .. } => (id, false),
            })
            .collect();

        assert_eq!(classified, vec![("legacy-job-1".to_string(), false)]);
    }

    #[test]
    fn list_all_sorts_new_entries_before_legacy_entries() {
        let tmp = TempDir::new().expect("tempdir");
        let store = JobStore::with_base(tmp.path().to_path_buf()).expect("store");
        let new_job = sample_job("job-new-mixed", vec![(1, "preflight")]);
        store.create(&new_job).expect("create new");
        let legacy_dir = tmp.path().join("zzz-legacy");
        fs::create_dir_all(&legacy_dir).expect("legacy dir");
        fs::write(legacy_dir.join("metadata.json"), b"{}").expect("metadata");

        let entries = store.list_all().expect("list_all");
        let kinds: Vec<&'static str> = entries
            .iter()
            .map(|e| match e {
                JobStoreEntry::New { .. } => "new",
                JobStoreEntry::Legacy { .. } => "legacy",
            })
            .collect();

        assert_eq!(kinds, vec!["new", "legacy"]);
    }

    #[test]
    fn list_all_skips_dirs_that_are_neither_new_nor_legacy() {
        let tmp = TempDir::new().expect("tempdir");
        let store = JobStore::with_base(tmp.path().to_path_buf()).expect("store");
        fs::create_dir_all(tmp.path().join("empty-orphan")).expect("empty");
        let with_other = tmp.path().join("other-file-orphan");
        fs::create_dir_all(&with_other).expect("with_other");
        fs::write(with_other.join("README"), b"hi").expect("readme");

        let entries = store.list_all().expect("list_all");

        assert_eq!(entries.len(), 0);
    }

    #[test]
    fn mixed_outcomes_round_trip_per_attempt() {
        let tmp = TempDir::new().expect("tempdir");
        let store = JobStore::with_base(tmp.path().to_path_buf()).expect("store");
        let job = sample_job("job-mix", vec![(1, "preflight")]);
        let dir = store.create(&job).expect("create");

        let outcomes = vec![
            AttemptOutcome::Success,
            AttemptOutcome::TransientInfra {
                error: "rate limited".to_string(),
            },
            AttemptOutcome::ProtocolViolation {
                category: "missing_artifact".to_string(),
                detail: "no findings.json".to_string(),
            },
        ];
        for (idx, outcome) in outcomes.iter().enumerate() {
            let n = u32::try_from(idx + 1).expect("attempt fits u32");
            let handle = dir.record_attempt_start(1, "preflight", n).expect("start");
            dir.record_attempt_finish(handle, outcome).expect("finish");
        }

        let attempts_dir = dir
            .path()
            .join("steps")
            .join("001-preflight")
            .join("attempts");
        let parsed: Vec<AttemptOutcome> = (1..=3u32)
            .map(|n| {
                let raw = fs::read_to_string(attempts_dir.join(n.to_string()).join("outcome.json"))
                    .expect("read outcome");
                serde_json::from_str(&raw).expect("parse outcome")
            })
            .collect();

        assert_eq!(parsed, outcomes);
    }
}
