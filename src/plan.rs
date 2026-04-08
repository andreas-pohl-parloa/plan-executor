use std::path::{Path, PathBuf};
use anyhow::Result;

/// Represents a discovered plan file.
#[derive(Debug, Clone)]
pub struct PlanFile {
    pub path: PathBuf,
    #[allow(dead_code)]
    pub status: PlanStatus,
}

/// Execution mode for a plan: local (default) or remote.
#[derive(Debug, Clone, PartialEq)]
pub enum ExecutionMode {
    Local,
    Remote,
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

/// Reads a plan file and extracts its `**execution:**` field.
/// Defaults to `ExecutionMode::Local` when absent or unrecognized.
pub fn parse_execution_mode(path: &Path) -> ExecutionMode {
    let Ok(content) = std::fs::read_to_string(path) else {
        return ExecutionMode::Local;
    };
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("**execution:**") {
            return match rest.trim() {
                "remote" => ExecutionMode::Remote,
                _ => ExecutionMode::Local,
            };
        }
    }
    ExecutionMode::Local
}

/// Returns true if the plan file has `**non-interactive:** [x]` (checked).
/// The check is case-insensitive for the checkbox marker.
pub fn is_non_interactive(path: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else { return false };
    content.lines().any(|line| {
        let l = line.trim();
        l.starts_with("**non-interactive:**") && l.contains("[x]")
    })
}

// Directories that are never worth descending into when scanning for plans.
const SKIP_DIRS: &[&str] = &[
    "target", "node_modules", ".git", ".hg", ".svn",
    "dist", "build", ".next", ".nuxt", "__pycache__",
    ".tox", ".venv", "venv", ".cache",
];

/// Scans a directory for plan files matching a pattern.
///
/// When `pattern` starts with `**/` the function walks the directory tree
/// using `walkdir`, skipping known heavy directories (`target/`, `node_modules/`,
/// etc.) and collecting every `.my/plans/*.md`-like match efficiently.
///
/// For patterns without `**/` the existing `glob` behaviour is used (fast,
/// non-recursive).
pub fn scan_for_plans(base_dir: &Path, pattern: &str) -> Vec<PathBuf> {
    if pattern.starts_with("**/") {
        scan_recursive(base_dir, &pattern[3..])
    } else {
        scan_glob(base_dir, pattern)
    }
}

fn scan_glob(base_dir: &Path, pattern: &str) -> Vec<PathBuf> {
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

/// Recursive scan using walkdir. Walks `base_dir`, skips SKIP_DIRS, and for
/// every directory whose path ends with the prefix portion of `sub_pattern`
/// (e.g. `.my/plans`) collects matching `*.md` files via glob.
fn scan_recursive(base_dir: &Path, sub_pattern: &str) -> Vec<PathBuf> {
    // Split sub_pattern into a directory prefix and a file glob.
    // e.g. ".my/plans/*.md" → prefix=".my/plans", file_glob="*.md"
    let (dir_prefix, file_glob) = match sub_pattern.rfind('/') {
        Some(i) => (&sub_pattern[..i], &sub_pattern[i + 1..]),
        None    => ("", sub_pattern),
    };

    let mut results = Vec::new();

    for entry in walkdir::WalkDir::new(base_dir)
        .follow_links(false)
        .max_depth(5)   // plans live at depth ≤4 from watch_dir; 5 gives margin
        .into_iter()
        .filter_entry(|e| {
            if !e.file_type().is_dir() {
                return true;
            }
            let name = e.file_name().to_string_lossy();
            !SKIP_DIRS.contains(&name.as_ref())
        })
        .flatten()
    {
        if !entry.file_type().is_dir() {
            continue;
        }
        // Check if this directory ends with the expected prefix path
        let path = entry.path();
        let ends_with_prefix = if dir_prefix.is_empty() {
            true
        } else {
            path.ends_with(dir_prefix)
        };
        if !ends_with_prefix {
            continue;
        }
        // Collect matching files in this directory
        for file in scan_glob(path, file_glob) {
            results.push(file);
        }
    }
    results
}

/// Scans all watch_dirs with all patterns and returns READY non-interactive plan files.
/// A plan qualifies only when it has both `**Status:** READY` and
/// `**non-interactive:** [x]` set in its header.
pub fn find_ready_plans(watch_dirs: &[PathBuf], patterns: &[String]) -> Vec<PlanFile> {
    let mut results = Vec::new();
    for dir in watch_dirs {
        for pattern in patterns {
            for path in scan_for_plans(dir, pattern) {
                if let Ok(status) = parse_plan_status(&path) {
                    if status == PlanStatus::Ready && is_non_interactive(&path) {
                        results.push(PlanFile { path, status });
                    }
                }
            }
        }
    }
    results
}
