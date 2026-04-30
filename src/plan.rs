use std::path::Path;
use anyhow::Result;

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
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn set_plan_header_replaces_existing_key() {
        let mut f = NamedTempFile::new().unwrap();
        write!(f, "# Plan\n\n**Status:** READY\n**Goal:** test\n").unwrap();
        set_plan_header(f.path(), "Status", "EXECUTING").unwrap();
        let content = std::fs::read_to_string(f.path()).unwrap();
        assert!(content.contains("**Status:** EXECUTING"));
        assert!(!content.contains("**Status:** READY"));
        assert!(content.contains("**Goal:** test"));
    }

    #[test]
    fn set_plan_header_inserts_after_last_header_when_key_missing() {
        let mut f = NamedTempFile::new().unwrap();
        write!(f, "# Plan\n\n**Goal:** test\n\n## Tasks\n").unwrap();
        set_plan_header(f.path(), "remote-pr", "42").unwrap();
        let content = std::fs::read_to_string(f.path()).unwrap();
        assert!(content.contains("**remote-pr:** 42"));
        // remote-pr should land between Goal (last header) and Tasks (heading).
        let goal_pos = content.find("**Goal:**").unwrap();
        let pr_pos = content.find("**remote-pr:**").unwrap();
        let tasks_pos = content.find("## Tasks").unwrap();
        assert!(goal_pos < pr_pos);
        assert!(pr_pos < tasks_pos);
    }
}
