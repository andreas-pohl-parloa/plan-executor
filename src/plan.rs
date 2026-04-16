use std::path::Path;
use anyhow::Result;

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

impl std::fmt::Display for PlanStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanStatus::Ready => write!(f, "READY"),
            PlanStatus::Wip => write!(f, "WIP"),
            PlanStatus::Executing => write!(f, "EXECUTING"),
            PlanStatus::Completed => write!(f, "COMPLETED"),
            PlanStatus::Unknown(s) => write!(f, "{}", s),
        }
    }
}

impl PlanStatus {
    fn from_str(s: &str) -> Self {
        match s.trim().to_ascii_uppercase().as_str() {
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
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("**status:**") {
            let rest = &line["**status:**".len()..];
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
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("**execution:**") {
            let rest = &line["**execution:**".len()..];
            return match rest.trim().to_ascii_lowercase().as_str() {
                "remote" => ExecutionMode::Remote,
                _ => ExecutionMode::Local,
            };
        }
    }
    ExecutionMode::Local
}

/// Reads a plan header value by key (case-insensitive).
/// E.g. `get_plan_header(path, "remote-pr")` reads `**remote-pr:** 42`.
pub fn get_plan_header(path: &Path, key: &str) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let prefix = format!("**{}:**", key.to_ascii_lowercase());
    for line in content.lines() {
        if line.trim().to_ascii_lowercase().starts_with(&prefix) {
            let rest = &line.trim()[prefix.len()..];
            return Some(rest.trim().to_string());
        }
    }
    None
}

/// Sets or inserts a plan header value. If the key already exists (case-insensitive),
/// the line is replaced. Otherwise the header is inserted after the last existing
/// `**...**` header line.
pub fn set_plan_header(path: &Path, key: &str, value: &str) -> Result<()> {
    let content = std::fs::read_to_string(path)?;
    let prefix_lower = format!("**{}:**", key.to_ascii_lowercase());
    let new_line = format!("**{}:** {}", key, value);

    let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
    let mut replaced = false;

    for line in &mut lines {
        if line.trim().to_ascii_lowercase().starts_with(&prefix_lower) {
            *line = new_line.clone();
            replaced = true;
            break;
        }
    }

    if !replaced {
        // Insert after the last **...: header line
        let mut insert_at = 0;
        for (i, line) in lines.iter().enumerate() {
            if line.trim().starts_with("**") && line.trim().contains(":**") {
                insert_at = i + 1;
            }
        }
        lines.insert(insert_at, new_line);
    }

    std::fs::write(path, lines.join("\n") + "\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;
    use std::io::Write;

    fn write_plan(content: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        write!(f, "{}", content).unwrap();
        f
    }

    #[test]
    fn test_parse_ready_status() {
        let f = write_plan("# My Plan\n\n**Status:** READY\n\n## Tasks\n");
        let status = parse_plan_status(f.path()).unwrap();
        assert_eq!(status, PlanStatus::Ready);
    }

    #[test]
    fn test_parse_wip_status() {
        let f = write_plan("**Status:** WIP\n");
        let status = parse_plan_status(f.path()).unwrap();
        assert_eq!(status, PlanStatus::Wip);
    }

    #[test]
    fn test_parse_missing_status() {
        let f = write_plan("# No status here\n");
        let status = parse_plan_status(f.path()).unwrap();
        assert!(matches!(status, PlanStatus::Unknown(_)));
    }

    #[test]
    fn test_parse_execution_mode_remote() {
        let f = write_plan("# Plan\n**execution:** remote\n**Status:** READY\n");
        assert_eq!(parse_execution_mode(f.path()), ExecutionMode::Remote);
    }

    #[test]
    fn test_parse_execution_mode_local_explicit() {
        let f = write_plan("# Plan\n**execution:** local\n**Status:** READY\n");
        assert_eq!(parse_execution_mode(f.path()), ExecutionMode::Local);
    }

    #[test]
    fn test_parse_execution_mode_missing_defaults_to_local() {
        let f = write_plan("# Plan\n**Status:** READY\n");
        assert_eq!(parse_execution_mode(f.path()), ExecutionMode::Local);
    }

    #[test]
    fn test_parse_execution_mode_unknown_defaults_to_local() {
        let f = write_plan("# Plan\n**execution:** cloud\n");
        assert_eq!(parse_execution_mode(f.path()), ExecutionMode::Local);
    }
}
