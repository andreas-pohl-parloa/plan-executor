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
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use thiserror::Error;

use crate::config::Config;
use crate::job::metrics::JobMetrics;
use crate::job::types::{Job, JobId, JobState};

/// Top-level handle on `~/.plan-executor/jobs/`.
#[derive(Debug, Clone)]
pub struct JobStore {
    base: PathBuf,
}

/// Handle on a single job's directory (`~/.plan-executor/jobs/<id>/`).
#[derive(Debug, Clone)]
pub struct JobDir {
    path: PathBuf,
}

/// Lightweight summary used by `list_all()` (avoids reading every step on disk).
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
}

type Result<T> = std::result::Result<T, JobStoreError>;

impl JobStore {
    /// Default store at `Config::base_dir().join("jobs")`.
    ///
    /// Hardens both `~/.plan-executor/` (the user-wide base) and
    /// `~/.plan-executor/jobs/` to mode `0700` on Unix so other users on the
    /// host cannot enumerate or read job state.
    ///
    /// # Errors
    ///
    /// Returns `JobStoreError::Io` if the base directory cannot be created.
    pub fn new() -> Result<Self> {
        let user_base = Config::base_dir();
        fs::create_dir_all(&user_base).map_err(|source| JobStoreError::Io {
            path: user_base.clone(),
            source,
        })?;
        harden_dir_mode(&user_base)?;

        let base = user_base.join("jobs");
        fs::create_dir_all(&base).map_err(|source| JobStoreError::Io {
            path: base.clone(),
            source,
        })?;
        harden_dir_mode(&base)?;
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
            return Ok(JobDir { path });
        }
        fs::create_dir_all(path.join("steps")).map_err(|source| JobStoreError::Io {
            path: path.clone(),
            source,
        })?;
        // Per-job dir holds plan inputs, sub-agent outputs, and credentials
        // surfaces; lock it down so other users on the host cannot read it.
        harden_dir_mode(&path)?;
        for step in &job.steps {
            let step_dir = step_dir_for(&path, step.seq, &step.name);
            fs::create_dir_all(step_dir.join("attempts")).map_err(|source| JobStoreError::Io {
                path: step_dir.clone(),
                source,
            })?;
        }
        let job_dir = JobDir { path };
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
        Ok(JobDir { path })
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

}

/// Atomic write: stage bytes in a unique `<path>.<random>` temp file in the
/// destination's parent directory, then atomically rename to `path`.
///
/// Uses [`tempfile::NamedTempFile`] so each writer gets its own randomized
/// temp filename — concurrent writers no longer race on a shared `<path>.tmp`
/// path. On error, the temp file is dropped (and its
/// inode unlinked) automatically; on success, `persist` performs the rename
/// atomically on the same filesystem.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| JobStoreError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    let mut tmp = NamedTempFile::new_in(parent).map_err(|source| JobStoreError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    tmp.write_all(bytes).map_err(|source| JobStoreError::Io {
        path: tmp.path().to_path_buf(),
        source,
    })?;
    tmp.as_file_mut()
        .sync_all()
        .map_err(|source| JobStoreError::Io {
            path: tmp.path().to_path_buf(),
            source,
        })?;
    tmp.persist(path).map_err(|e| JobStoreError::Io {
        path: path.to_path_buf(),
        source: e.error,
    })?;
    Ok(())
}

/// Apply `0o700` to `path` on Unix so only the owning user can list or read
/// the directory. No-op on non-Unix targets.
fn harden_dir_mode(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path)
            .map_err(|source| JobStoreError::Io {
                path: path.to_path_buf(),
                source,
            })?
            .permissions();
        perms.set_mode(0o700);
        fs::set_permissions(path, perms).map_err(|source| JobStoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

