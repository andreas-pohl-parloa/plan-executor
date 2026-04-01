use std::path::{Path, PathBuf};
use anyhow::Result;

/// Represents a discovered plan file.
#[derive(Debug, Clone)]
pub struct PlanFile {
    pub path: PathBuf,
    #[allow(dead_code)]
    pub status: PlanStatus,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlanStatus {
    Ready,
    Wip,
    Executing,
    Completed,
    Unknown(String),
}

impl PlanStatus {
    fn from_str(s: &str) -> Self {
        match s.trim() {
            "READY" => PlanStatus::Ready,
            "WIP" => PlanStatus::Wip,
            "EXECUTING" => PlanStatus::Executing,
            "COMPLETED" => PlanStatus::Completed,
            other => PlanStatus::Unknown(other.to_string()),
        }
    }
}

/// Reads a plan file and extracts its **Status:** field.
pub fn parse_plan_status(path: &Path) -> Result<PlanStatus> {
    let content = std::fs::read_to_string(path)?;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("**Status:**") {
            return Ok(PlanStatus::from_str(rest));
        }
    }
    Ok(PlanStatus::Unknown("missing".to_string()))
}

/// Scans a directory for files matching a glob pattern.
/// Returns all matching paths.
pub fn scan_for_plans(base_dir: &Path, pattern: &str) -> Vec<PathBuf> {
    let full_pattern = base_dir.join(pattern);
    let pattern_str = full_pattern.to_string_lossy();
    match glob::glob(&pattern_str) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter(|p| p.is_file())
            .collect(),
        Err(_) => vec![],
    }
}

/// Scans all watch_dirs with all patterns and returns READY plan files.
pub fn find_ready_plans(watch_dirs: &[PathBuf], patterns: &[String]) -> Vec<PlanFile> {
    let mut results = Vec::new();
    for dir in watch_dirs {
        for pattern in patterns {
            for path in scan_for_plans(dir, pattern) {
                if let Ok(status) = parse_plan_status(&path) {
                    if status == PlanStatus::Ready {
                        results.push(PlanFile { path, status });
                    }
                }
            }
        }
    }
    results
}
