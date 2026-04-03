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
    /// Calculated cost in USD
    pub cost_usd: Option<f64>,
    /// Duration in milliseconds (from result event)
    pub duration_ms: Option<u64>,
    /// Claude session ID (from stream-json system/init), used for --resume in handoff loop
    pub session_id: Option<String>,
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
            cost_usd: None,
            duration_ms: None,
            session_id: None,
        }
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
