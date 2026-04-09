//! Remote execution metadata, branch management, and PR creation.
//!
//! Provides types and functions for triggering plan execution in a
//! remote GitHub repository via the GitHub API and CLI.

use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};
use anyhow::{Context, Result};

/// Metadata describing a remote execution request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionMetadata {
    pub target_repo: String,
    pub target_ref: String,
    pub target_branch: String,
    pub plan_filename: String,
    pub started_at: String,
}

/// Formats the PR title for an execution.
pub fn pr_title(meta: &ExecutionMetadata) -> String {
    format!("exec: {} @ {}", meta.plan_filename, meta.target_repo)
}

/// Generates the branch name from the plan filename and ISO timestamp.
/// Format: `exec/<YYYYMMDD-HHMMSS>-<plan-stem>`
pub fn branch_name(plan_filename: &str, iso_timestamp: &str) -> String {
    let stem = Path::new(plan_filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(plan_filename);
    // Sanitize stem: keep only safe characters for git branch names
    let safe_stem: String = stem.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '-' })
        .collect();
    // Parse "2026-04-08T14:30:22Z" -> "20260408-143022"
    let ts = iso_timestamp
        .replace(['-', ':'], "")
        .replace('T', "-")
        .replace('Z', "");
    // Truncate to YYYYMMDD-HHMMSS (15 chars)
    let ts_short = &ts[..ts.len().min(15)];
    format!("exec/{}-{}", ts_short, safe_stem)
}

/// Gathers git context from the specified directory.
///
/// Returns `(owner/repo, HEAD SHA, branch name)`.
///
/// # Errors
///
/// Returns an error if git commands fail or the remote URL cannot be parsed.
pub fn gather_git_context(repo_dir: &Path) -> Result<(String, String, String)> {
    let origin_url = run_git(repo_dir, &["remote", "get-url", "origin"])?;
    let repo_slug = parse_repo_slug(&origin_url)
        .context("Could not parse owner/repo from git remote URL")?;
    let head_sha = run_git(repo_dir, &["rev-parse", "HEAD"])?;
    let branch = run_git(repo_dir, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    Ok((repo_slug, head_sha, branch))
}

/// Extracts `owner/repo` from a git remote URL.
/// Supports HTTPS (`https://github.com/owner/repo.git`) and
/// SSH (`git@github.com:owner/repo.git`) formats.
pub fn parse_repo_slug(url: &str) -> Option<String> {
    let url = url.trim();
    let slug = if let Some(path) = url.strip_prefix("https://github.com/") {
        path.trim_end_matches(".git").to_string()
    } else if let Some(path) = url.strip_prefix("git@github.com:") {
        path.trim_end_matches(".git").to_string()
    } else {
        return None;
    };
    // Validate owner/repo format — reject traversal or injection attempts
    if validate_repo_slug(&slug) { Some(slug) } else { None }
}

/// Returns true if the string matches a valid `owner/repo` GitHub slug.
pub fn validate_repo_slug(slug: &str) -> bool {
    let parts: Vec<&str> = slug.splitn(3, '/').collect();
    if parts.len() != 2 { return false; }
    let valid_part = |s: &str| {
        !s.is_empty()
            && !s.contains("..")
            && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    };
    valid_part(parts[0]) && valid_part(parts[1])
}

/// Finds `.tmp-subtask-*.md` files co-located with the plan file.
pub fn find_prompt_files(plan_path: &Path) -> Vec<PathBuf> {
    let Some(dir) = plan_path.parent() else { return vec![] };
    let Ok(entries) = std::fs::read_dir(dir) else { return vec![] };
    entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with(".tmp-subtask-") && name.ends_with(".md") {
                Some(e.path())
            } else {
                None
            }
        })
        .collect()
}

/// Pushes the Codex OAuth token to the execution repo secrets (idempotent).
///
/// # Errors
///
/// Returns an error if reading the auth file or the `gh` command fails.
pub fn push_codex_auth(remote_repo: &str) -> Result<()> {
    let auth_path = dirs::home_dir()
        .context("could not determine home directory")?
        .join(".codex")
        .join("auth.json");
    if !auth_path.exists() {
        return Ok(()); // no auth file, skip
    }
    let content = std::fs::read_to_string(&auth_path)?;
    gh_secret_set_stdin("CODEX_AUTH", remote_repo, &content)
}

/// Returns true if the repo exists and is accessible.
pub fn repo_exists(repo: &str) -> bool {
    run_gh(&["repo", "view", repo, "--json", "name"]).is_ok()
}

/// Creates a private repo with an initial README commit.
/// The `repo` slug must be `owner/name`.
pub fn create_repo(repo: &str) -> Result<()> {
    // --add-readme ensures the repo has at least one commit on main,
    // which is required for the Contents API to push the workflow file.
    run_gh(&[
        "repo", "create", repo, "--private",
        "--description", "Remote plan execution",
        "--add-readme",
    ])?;
    Ok(())
}

/// Creates the `execution` GitHub Actions environment on the repo.
/// Idempotent — succeeds if the environment already exists.
pub fn ensure_environment(repo: &str) -> Result<()> {
    // GitHub REST API: PUT /repos/{owner}/{repo}/environments/{name}
    // This creates or updates the environment.
    run_gh(&[
        "api", &format!("repos/{}/environments/execution", repo),
        "-X", "PUT",
    ])?;
    Ok(())
}

/// The embedded workflow YAML for the execution repo.
const EXECUTE_PLAN_WORKFLOW: &str = include_str!("../docs/remote-execution/execute-plan.yml");

/// Pushes the execute-plan workflow to `.github/workflows/execute-plan.yml`
/// on the main branch of the execution repo. Uses git clone+commit+push
/// because the GitHub Contents API blocks writes to `.github/workflows/`
/// when org-level workflow security policies are active.
pub fn push_workflow(remote_repo: &str) -> Result<()> {
    let tmp = std::env::temp_dir().join("plan-executor-setup");
    let _ = std::fs::remove_dir_all(&tmp);

    // Clone
    run_gh(&["repo", "clone", remote_repo, &tmp.to_string_lossy(), "--", "--depth=1"])?;

    // Write workflow file
    let wf_dir = tmp.join(".github").join("workflows");
    std::fs::create_dir_all(&wf_dir)?;
    std::fs::write(wf_dir.join("execute-plan.yml"), EXECUTE_PLAN_WORKFLOW)?;

    // Commit and push
    run_git(&tmp, &["add", ".github/workflows/execute-plan.yml"])?;

    // Check if there's anything to commit (file might already be up to date)
    let status = run_git(&tmp, &["status", "--porcelain"])?;
    if status.trim().is_empty() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Ok(()); // already up to date
    }

    run_git(&tmp, &[
        "-c", "user.name=plan-executor",
        "-c", "user.email=plan-executor@noreply",
        "commit", "-m", "chore: update execute-plan workflow",
    ])?;
    run_git(&tmp, &["push"])?;

    let _ = std::fs::remove_dir_all(&tmp);
    Ok(())
}

/// Creates a branch with plan files and execution metadata in the execution repo,
/// then opens a PR. Returns the PR URL.
///
/// # Errors
///
/// Returns an error if any GitHub API call or file read fails.
pub fn trigger_remote_execution(
    remote_repo: &str,
    plan_path: &Path,
    meta: &ExecutionMetadata,
) -> Result<String> {
    let plan_content = std::fs::read_to_string(plan_path)?;
    let meta_json = serde_json::to_string_pretty(meta)?;
    let branch = branch_name(&meta.plan_filename, &meta.started_at);
    let title = pr_title(meta);
    let prompt_files = find_prompt_files(plan_path);

    // Create branch from main
    run_gh(&[
        "api", &format!("repos/{}/git/refs", remote_repo),
        "-X", "POST",
        "-f", &format!("ref=refs/heads/{}", branch),
        "-f", &format!("sha={}", get_main_sha(remote_repo)?),
    ])?;

    // Push execution.json
    push_file_to_branch(remote_repo, &branch, "execution.json", &meta_json)?;

    // Push plan.md
    push_file_to_branch(remote_repo, &branch, "plan.md", &plan_content)?;

    // Push prompt files
    for pf in &prompt_files {
        let name = pf.file_name().and_then(|n| n.to_str()).unwrap_or("prompt.md");
        let content = std::fs::read_to_string(pf)?;
        let dest = format!("prompt-files/{}", name);
        push_file_to_branch(remote_repo, &branch, &dest, &content)?;
    }

    // Create PR
    let pr_url = run_gh(&[
        "pr", "create",
        "--repo", remote_repo,
        "--head", &branch,
        "--title", &title,
        "--body", &format!(
            "## Remote Execution\n\n\
             **Target:** {repo}@{ref_short}\n\
             **Branch:** {branch}\n\
             **Plan:** {plan}\n\
             **Started:** {started}",
            repo = meta.target_repo,
            ref_short = &meta.target_ref[..meta.target_ref.len().min(12)],
            branch = meta.target_branch,
            plan = meta.plan_filename,
            started = meta.started_at,
        ),
    ])?;

    Ok(pr_url.trim().to_string())
}

/// Queries recent remote execution PRs from the execution repo.
///
/// # Errors
///
/// Returns an error if the `gh` command or JSON parsing fails.
pub fn list_remote_executions(remote_repo: &str) -> Result<Vec<RemoteJob>> {
    let output = run_gh(&[
        "pr", "list",
        "--repo", remote_repo,
        "--state", "all",
        "--limit", "20",
        "--json", "number,title,state,labels",
    ])?;
    let prs: Vec<serde_json::Value> = serde_json::from_str(&output)?;
    let mut jobs = Vec::new();
    for pr in prs {
        let number = pr["number"].as_u64().unwrap_or(0);
        let title = pr["title"].as_str().unwrap_or("");
        let state = pr["state"].as_str().unwrap_or("UNKNOWN");
        let labels: Vec<&str> = pr["labels"]
            .as_array()
            .map(|arr| arr.iter().filter_map(|l| l["name"].as_str()).collect())
            .unwrap_or_default();

        // Parse title: "exec: plan-foo.md @ owner/repo"
        let (plan_name, target) = if let Some(rest) = title.strip_prefix("exec: ") {
            if let Some((plan, tgt)) = rest.split_once(" @ ") {
                (plan.to_string(), tgt.to_string())
            } else {
                (rest.to_string(), "?".to_string())
            }
        } else {
            (title.to_string(), "?".to_string())
        };

        let status = match state {
            "OPEN" => "running".to_string(),
            "CLOSED" | "MERGED" => {
                if labels.contains(&"succeeded") {
                    "succeeded".to_string()
                } else if labels.contains(&"failed") {
                    "failed".to_string()
                } else {
                    "closed".to_string()
                }
            }
            other => other.to_lowercase(),
        };

        jobs.push(RemoteJob { number, plan_name, status, target });
    }
    Ok(jobs)
}

/// A remote execution job entry parsed from a GitHub PR.
#[derive(Debug)]
pub struct RemoteJob {
    pub number: u64,
    pub plan_name: String,
    pub status: String,
    pub target: String,
}

// -- Helpers ------------------------------------------------------------------

fn run_git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| anyhow::anyhow!("failed to run git: {e}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn run_gh(args: &[&str]) -> Result<String> {
    let output = std::process::Command::new("gh")
        .args(args)
        .output()
        .map_err(|e| anyhow::anyhow!("failed to run gh: {e}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "gh {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn get_main_sha(remote_repo: &str) -> Result<String> {
    let output = run_gh(&[
        "api", &format!("repos/{}/git/ref/heads/main", remote_repo),
        "--jq", ".object.sha",
    ])?;
    Ok(output.trim().to_string())
}

fn push_file_to_branch(repo: &str, branch: &str, path: &str, content: &str) -> Result<()> {
    // Reject path traversal attempts
    anyhow::ensure!(
        !path.contains("..") && !path.starts_with('/'),
        "invalid file path: {}", path
    );
    // GitHub Contents API requires base64-encoded content
    let encoded = base64_encode(content.as_bytes());
    run_gh(&[
        "api", &format!("repos/{}/contents/{}", repo, path),
        "-X", "PUT",
        "-f", &format!("message=add {}", path),
        "-f", &format!("branch={}", branch),
        "-f", &format!("content={}", encoded),
    ]).map(|_| ())
}

/// Pipes a secret value via stdin to `gh secret set` to avoid leaking it
/// in process arguments visible via `ps aux` / `/proc/*/cmdline`.
pub fn gh_secret_set_stdin(name: &str, repo: &str, value: &str) -> Result<()> {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = std::process::Command::new("gh")
        .args(["secret", "set", name, "--repo", repo])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to run gh: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(value.as_bytes())?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        anyhow::bail!("gh secret set {} failed: {}", name, String::from_utf8_lossy(&output.stderr));
    }
    Ok(())
}

fn base64_encode(input: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::new();
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        result.push(CHARS[((triple >> 18) & 0x3F) as usize] as char);
        result.push(CHARS[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            result.push(CHARS[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
        if chunk.len() > 2 {
            result.push(CHARS[(triple & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pr_title_format() {
        let meta = ExecutionMetadata {
            target_repo: "owner/my-service".to_string(),
            target_ref: "abc123def456".to_string(),
            target_branch: "feat/cool".to_string(),
            plan_filename: "plan-add-feature.md".to_string(),
            started_at: "2026-04-08T14:30:00Z".to_string(),
        };
        assert_eq!(pr_title(&meta), "exec: plan-add-feature.md @ owner/my-service");
    }

    #[test]
    fn test_execution_metadata_serialization() {
        let meta = ExecutionMetadata {
            target_repo: "owner/repo".to_string(),
            target_ref: "abc123".to_string(),
            target_branch: "main".to_string(),
            plan_filename: "plan-foo.md".to_string(),
            started_at: "2026-04-08T14:30:00Z".to_string(),
        };
        let json = serde_json::to_string_pretty(&meta).unwrap();
        let parsed: ExecutionMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.target_repo, "owner/repo");
        assert_eq!(parsed.target_ref, "abc123");
        assert_eq!(parsed.plan_filename, "plan-foo.md");
    }

    #[test]
    fn test_branch_name_format() {
        let name = branch_name("plan-add-feature.md", "2026-04-08T14:30:22Z");
        // Should be exec/<date-time>-<plan-stem>
        assert!(name.starts_with("exec/"));
        assert!(name.contains("plan-add-feature"));
        // No .md extension in branch name
        assert!(!name.ends_with(".md"));
    }
}
