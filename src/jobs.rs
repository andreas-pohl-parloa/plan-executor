use std::path::PathBuf;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use anyhow::Result;
use crate::config::Config;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum JobStatus {
    Running,
    Success,
    Failed,
    Killed,
    RemoteRunning,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobMetadata {
    pub id: String,
    pub plan_path: PathBuf,
    pub status: JobStatus,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    /// Model used (from stream-json system/init event)
    pub model: Option<String>,
    /// Total input tokens (from result event)
    pub input_tokens: Option<u64>,
    /// Total output tokens (from result event)
    pub output_tokens: Option<u64>,
    /// Cache write tokens
    pub cache_creation_tokens: Option<u64>,
    /// Cache read tokens
    pub cache_read_tokens: Option<u64>,
    /// Duration in milliseconds (from result event)
    pub duration_ms: Option<u64>,
    /// Claude session ID (from stream-json system/init), used for --resume in handoff loop
    pub session_id: Option<String>,
    /// Remote execution repo (e.g. "owner/plan-executions")
    #[serde(default)]
    pub remote_repo: Option<String>,
    /// Remote execution PR number
    #[serde(default)]
    pub remote_pr: Option<u64>,
}

impl JobMetadata {
    pub fn new(plan_path: PathBuf) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            plan_path,
            status: JobStatus::Running,
            started_at: Utc::now(),
            finished_at: None,
            model: None,
            input_tokens: None,
            output_tokens: None,
            cache_creation_tokens: None,
            cache_read_tokens: None,
            duration_ms: None,
            session_id: None,
            remote_repo: None,
            remote_pr: None,
        }
    }

    pub fn new_remote(plan_path: PathBuf, remote_repo: String, pr_number: u64) -> Self {
        let mut job = Self::new(plan_path);
        job.status = JobStatus::RemoteRunning;
        job.remote_repo = Some(remote_repo);
        job.remote_pr = Some(pr_number);
        job
    }

    /// Returns the job's directory under ~/.plan-executor/jobs/<id>/
    pub fn job_dir(&self) -> PathBuf {
        Config::base_dir().join("jobs").join(&self.id)
    }

    pub fn metadata_path(&self) -> PathBuf {
        self.job_dir().join("metadata.json")
    }

    /// Rendered display lines (sjv + [plan-executor] messages) for `output` CLI.
    pub fn display_path(&self) -> PathBuf {
        self.job_dir().join("display.log")
    }

    pub fn output_path(&self) -> PathBuf {
        self.job_dir().join("output.jsonl")
    }

    /// Persists metadata to disk.
    pub fn save(&self) -> Result<()> {
        let dir = self.job_dir();
        std::fs::create_dir_all(&dir)?;
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(self.metadata_path(), json)?;
        Ok(())
    }

    /// Loads a single job by full or prefix-matched ID.
    pub fn load_by_id_prefix(prefix: &str) -> Option<Self> {
        let jobs_dir = Config::base_dir().join("jobs");
        let Ok(entries) = std::fs::read_dir(&jobs_dir) else { return None };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with(prefix) {
                let meta_path = entry.path().join("metadata.json");
                if let Ok(content) = std::fs::read_to_string(meta_path) {
                    if let Ok(meta) = serde_json::from_str::<Self>(&content) {
                        return Some(meta);
                    }
                }
            }
        }
        None
    }

    /// Loads all jobs from ~/.plan-executor/jobs/
    pub fn load_all() -> Vec<Self> {
        let jobs_dir = Config::base_dir().join("jobs");
        let Ok(entries) = std::fs::read_dir(&jobs_dir) else {
            return vec![];
        };
        let mut jobs = Vec::new();
        for entry in entries.flatten() {
            let meta_path = entry.path().join("metadata.json");
            if let Ok(content) = std::fs::read_to_string(&meta_path) {
                if let Ok(meta) = serde_json::from_str::<Self>(&content) {
                    jobs.push(meta);
                }
            }
        }
        // Sort by started_at descending
        jobs.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        jobs
    }
}
