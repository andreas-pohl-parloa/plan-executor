//! Remote execution metadata, branch management, and PR creation.
//!
//! Provides types and functions for triggering plan execution in a
//! remote GitHub repository via the GitHub API and CLI.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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
    let safe_stem: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
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
    let repo_slug =
        parse_repo_slug(&origin_url).context("Could not parse owner/repo from git remote URL")?;
    let head_sha = run_git(repo_dir, &["rev-parse", "HEAD"])?;
    let branch = run_git(repo_dir, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    Ok((repo_slug, head_sha, branch))
}

/// Extracts `owner/repo` from a git remote URL.
/// Supports HTTPS (`https://github.com/owner/repo.git`) and
/// SSH (`git@github.com:owner/repo.git`) formats.
/// Also handles SSH config aliases like `git@github.com-priv:owner/repo.git`
/// (common for multi-key SSH setups via `~/.ssh/config` Host entries).
pub fn parse_repo_slug(url: &str) -> Option<String> {
    let url = url.trim();
    let slug = if let Some(path) = url.strip_prefix("https://github.com/") {
        path.trim_end_matches(".git").to_string()
    } else if let Some(rest) = url.strip_prefix("git@") {
        // Match git@github.com:owner/repo or git@github.com-alias:owner/repo
        let colon_pos = rest.find(':')?;
        let host = &rest[..colon_pos];
        if host != "github.com" && !host.starts_with("github.com-") {
            return None;
        }
        rest[colon_pos + 1..].trim_end_matches(".git").to_string()
    } else {
        return None;
    };
    // Validate owner/repo format — reject traversal or injection attempts
    if validate_repo_slug(&slug) {
        Some(slug)
    } else {
        None
    }
}

/// Returns true if the string matches a valid `owner/repo` GitHub slug.
pub fn validate_repo_slug(slug: &str) -> bool {
    let parts: Vec<&str> = slug.splitn(3, '/').collect();
    if parts.len() != 2 {
        return false;
    }
    let valid_part = |s: &str| {
        !s.is_empty()
            && !s.contains("..")
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    };
    valid_part(parts[0]) && valid_part(parts[1])
}

/// Finds `.tmp-subtask-*.md` files co-located with the plan file.
pub fn find_prompt_files(plan_path: &Path) -> Vec<PathBuf> {
    let Some(dir) = plan_path.parent() else {
        return vec![];
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return vec![];
    };
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
        "repo",
        "create",
        repo,
        "--private",
        "--description",
        "Remote plan execution",
        "--add-readme",
    ])?;
    Ok(())
}

/// The embedded workflow YAML for the execution repo.
const EXECUTE_PLAN_WORKFLOW: &str = include_str!("../docs/remote-execution/execute-plan.yml");

fn execution_repo_readme(repo: &str) -> String {
    format!(
        r#"# {repo}

Remote plan execution repo. Plans marked with `**execution:** remote` are
executed here on GitHub Actions runners instead of locally.

## How it works

1. `plan-executor execute <plan>` detects the `**execution:** remote` header.
2. It pushes the plan content and metadata to an `exec/` branch in this repo and opens a PR.
3. The GitHub Actions workflow (`.github/workflows/execute-plan.yml`) triggers on the PR:
   - Clones the **target repo** at the specified commit.
   - Downloads pre-built `plan-executor` and `sjv` binaries.
   - Installs Claude Code, Codex, and Gemini CLIs.
   - Installs Claude plugin marketplaces and plugins declared in the plan headers.
   - Runs the plan via Claude.
   - Posts an execution summary as a PR comment.
   - Closes the PR with a `succeeded` or `failed` label.
4. The local daemon monitors the PR and updates the plan status to `COMPLETED` or `FAILED`.

## Secrets

Configured via `plan-executor remote-setup`:

| Secret | Purpose |
|--------|---------|
| `TARGET_REPO_TOKEN` | GitHub PAT (classic, `repo` scope) for cloning target repos and accessing releases |
| `ANTHROPIC_API_KEY` | Claude API key |
| `OPENAI_API_KEY` | Codex API key (optional) |
| `CODEX_AUTH` | Codex OAuth token JSON (optional, alternative to API key) |
| `GEMINI_API_KEY` | Gemini API key (optional) |

## References

- [plan-executor](https://github.com/andreas-pohl-parloa/plan-executor) — the CLI daemon and execution engine
- [plan-executor-plugin](https://github.com/andreas-pohl-parloa/plan-executor-plugin) — Claude Code plugin with orchestration skills
"#,
        repo = repo
    )
}

/// Pushes the execute-plan workflow and README to the execution repo.
/// Uses git clone+commit+push because the GitHub Contents API blocks
/// writes to `.github/workflows/` when org-level workflow security
/// policies are active.
///
/// Falls back to a PR flow when direct push to `main` is rejected by
/// branch protection or org-wide ruleset. The fallback:
/// 1. Pushes the commit to a `setup/execute-plan-workflow` branch.
/// 2. Opens a PR against `main`.
/// 3. Enables auto-merge so the PR merges once required checks pass.
pub fn push_workflow(remote_repo: &str) -> Result<()> {
    let tmp = std::env::temp_dir().join("plan-executor-setup");
    let _ = std::fs::remove_dir_all(&tmp);

    // Clone
    run_gh(&[
        "repo",
        "clone",
        remote_repo,
        &tmp.to_string_lossy(),
        "--",
        "--depth=1",
    ])?;

    // Write workflow file
    let wf_dir = tmp.join(".github").join("workflows");
    std::fs::create_dir_all(&wf_dir)?;
    std::fs::write(wf_dir.join("execute-plan.yml"), EXECUTE_PLAN_WORKFLOW)?;

    // Write README
    std::fs::write(tmp.join("README.md"), execution_repo_readme(remote_repo))?;

    // Commit and push
    run_git(
        &tmp,
        &["add", ".github/workflows/execute-plan.yml", "README.md"],
    )?;

    // Check if there's anything to commit (files might already be up to date)
    let status = run_git(&tmp, &["status", "--porcelain"])?;
    if status.trim().is_empty() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Ok(()); // already up to date
    }

    run_git(
        &tmp,
        &[
            "-c",
            "user.name=plan-executor",
            "-c",
            "user.email=plan-executor@noreply",
            "commit",
            "-m",
            "chore: update workflow and README",
        ],
    )?;

    match run_git(&tmp, &["push"]) {
        Ok(_) => {
            let _ = std::fs::remove_dir_all(&tmp);
            Ok(())
        }
        Err(e) if is_branch_protection_error(&e) => {
            eprintln!("  Direct push to main blocked by branch protection. Opening PR...");
            push_workflow_via_pr(remote_repo, &tmp)?;
            let _ = std::fs::remove_dir_all(&tmp);
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            Err(e)
        }
    }
}

/// Heuristic for detecting GitHub branch-protection / ruleset push rejections.
fn is_branch_protection_error(err: &anyhow::Error) -> bool {
    let msg = err.to_string();
    msg.contains("protected branch")
        || msg.contains("push declined")
        || msg.contains("repository rule violations")
        || msg.contains("GH013")
        || msg.contains("Required workflow")
        || msg.contains("Changes must be made through a pull request")
}

/// Fallback path when `main` is protected: push the already-committed change
/// to a setup branch, open a PR, and enable auto-merge.
fn push_workflow_via_pr(remote_repo: &str, repo_dir: &Path) -> Result<()> {
    let branch = "setup/execute-plan-workflow";

    // Create (or reset) local branch from the current commit.
    run_git(repo_dir, &["checkout", "-B", branch])?;
    // Force-push to overwrite any stale branch from a previous failed run.
    run_git(repo_dir, &["push", "--force", "-u", "origin", branch])?;

    // Enable auto-merge on the repo (idempotent; ignore errors, we fall
    // back to a direct merge attempt below).
    let _ = run_gh(&[
        "api",
        &format!("repos/{}", remote_repo),
        "-X",
        "PATCH",
        "-F",
        "allow_auto_merge=true",
    ]);

    // Open PR (or reuse an existing one). If PR creation fails because a
    // PR already exists for this branch, look it up.
    let pr_url = match run_gh(&[
        "pr",
        "create",
        "--repo",
        remote_repo,
        "--head",
        branch,
        "--base",
        "main",
        "--title",
        "chore: add execute-plan workflow",
        "--body",
        "Automated setup by `plan-executor remote-setup`.\n\n\
                   Adds the execute-plan GitHub Actions workflow and the README.",
    ]) {
        Ok(url) => url.trim().to_string(),
        Err(_) => {
            let out = run_gh(&[
                "pr",
                "list",
                "--repo",
                remote_repo,
                "--head",
                branch,
                "--state",
                "open",
                "--json",
                "url",
                "--jq",
                ".[0].url",
            ])?;
            let url = out.trim().to_string();
            anyhow::ensure!(!url.is_empty(), "failed to create or find setup PR");
            url
        }
    };
    println!("  Opened PR: {}", pr_url);

    let pr_num = pr_number_from_url(&pr_url)
        .ok_or_else(|| anyhow::anyhow!("could not parse PR number from {}", pr_url))?;
    let pr_num_s = pr_num.to_string();

    // Try immediate merge first (succeeds if user has bypass rights or no
    // required checks exist). Otherwise fall back to auto-merge, which
    // completes once required checks pass.
    match run_gh(&[
        "pr",
        "merge",
        &pr_num_s,
        "--repo",
        remote_repo,
        "--squash",
        "--delete-branch",
    ]) {
        Ok(_) => {
            println!("  Merged.");
            Ok(())
        }
        Err(immediate_err) => match run_gh(&[
            "pr",
            "merge",
            &pr_num_s,
            "--repo",
            remote_repo,
            "--squash",
            "--auto",
            "--delete-branch",
        ]) {
            Ok(_) => {
                println!("  Auto-merge enabled. PR will merge once required checks pass.");
                Ok(())
            }
            Err(auto_err) => {
                eprintln!(
                    "  Could not merge automatically (direct: {}; auto-merge: {}).",
                    immediate_err, auto_err
                );
                eprintln!("  Merge the PR manually to finish setup: {}", pr_url);
                Ok(())
            }
        },
    }
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
    manifest_path: &Path,
    meta: &ExecutionMetadata,
) -> Result<String> {
    let plan_content = std::fs::read_to_string(plan_path)?;
    let meta_json = serde_json::to_string_pretty(meta)?;
    let branch = branch_name(&meta.plan_filename, &meta.started_at);
    let title = pr_title(meta);
    let prompt_files = find_prompt_files(plan_path);

    // Create branch from main
    run_gh(&[
        "api",
        &format!("repos/{}/git/refs", remote_repo),
        "-X",
        "POST",
        "-f",
        &format!("ref=refs/heads/{}", branch),
        "-f",
        &format!("sha={}", get_main_sha(remote_repo)?),
    ])?;

    // Push execution.json
    push_file_to_branch(remote_repo, &branch, "execution.json", &meta_json)?;

    // Push plan.md
    push_file_to_branch(remote_repo, &branch, "plan.md", &plan_content)?;

    let manifest_content = std::fs::read_to_string(manifest_path)
        .with_context(|| format!("read manifest {}", manifest_path.display()))?;
    push_file_to_branch(remote_repo, &branch, "tasks.json", &manifest_content)?;

    // Push prompt files
    for pf in &prompt_files {
        let name = pf
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("prompt.md");
        let content = std::fs::read_to_string(pf)?;
        let dest = format!("prompt-files/{}", name);
        push_file_to_branch(remote_repo, &branch, &dest, &content)?;
    }

    // Create PR
    let pr_url = run_gh(&[
        "pr",
        "create",
        "--repo",
        remote_repo,
        "--head",
        &branch,
        "--title",
        &title,
        "--body",
        &format!(
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

/// Queries the state and labels of a PR by number. Returns `(state, labels)`.
/// `state` is "OPEN", "CLOSED", or "MERGED".
pub fn get_pr_status(remote_repo: &str, pr_number: u64) -> Result<(String, Vec<String>)> {
    let output = run_gh(&[
        "pr",
        "view",
        &pr_number.to_string(),
        "--repo",
        remote_repo,
        "--json",
        "state,labels",
    ])?;
    let val: serde_json::Value = serde_json::from_str(&output)?;
    let state = val["state"].as_str().unwrap_or("UNKNOWN").to_string();
    let labels: Vec<String> = val["labels"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|l| l["name"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    Ok((state, labels))
}

/// Extracts the PR number from a PR URL like `https://github.com/owner/repo/pull/42`.
pub fn pr_number_from_url(url: &str) -> Option<u64> {
    url.rsplit('/').next().and_then(|s| s.parse().ok())
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

pub(crate) fn get_main_sha(remote_repo: &str) -> Result<String> {
    let output = run_gh(&[
        "api",
        &format!("repos/{}/git/ref/heads/main", remote_repo),
        "--jq",
        ".object.sha",
    ])?;
    Ok(output.trim().to_string())
}

pub(crate) fn push_file_to_branch(
    repo: &str,
    branch: &str,
    path: &str,
    content: &str,
) -> Result<()> {
    // Reject path traversal attempts
    anyhow::ensure!(
        !path.contains("..") && !path.starts_with('/'),
        "invalid file path: {}",
        path
    );
    // GitHub Contents API requires base64-encoded content
    let encoded = base64_encode(content.as_bytes());
    run_gh(&[
        "api",
        &format!("repos/{}/contents/{}", repo, path),
        "-X",
        "PUT",
        "-f",
        &format!("message=add {}", path),
        "-f",
        &format!("branch={}", branch),
        "-f",
        &format!("content={}", encoded),
    ])
    .map(|_| ())
}

/// Returns true if the execution repo already has a secret with this name.
/// Uses `gh secret list --json name` so the secret value never crosses the
/// wire.
pub fn gh_secret_exists(repo: &str, name: &str) -> Result<bool> {
    let output = run_gh(&["secret", "list", "--repo", repo, "--json", "name"])?;
    let secrets: Vec<serde_json::Value> =
        serde_json::from_str(&output).map_err(|e| anyhow::anyhow!("parse gh secret list: {e}"))?;
    Ok(secrets
        .iter()
        .any(|s| s.get("name").and_then(|v| v.as_str()) == Some(name)))
}

/// Marker embedded in a CI signing key's uid comment so subsequent
/// `remote-setup` runs can find and reuse the key instead of regenerating.
pub const CI_SIGNING_KEY_MARKER: &str = "plan-executor CI";

/// A plan-executor CI signing key discovered on the local GPG keyring.
#[derive(Debug, Clone)]
pub struct CiSigningKey {
    /// Full hex fingerprint.
    pub fingerprint: String,
    /// uid name portion (before the `<email>`).
    pub name: String,
    /// uid email portion.
    pub email: String,
}

/// Scans the local GPG secret keyring for a non-expired ed25519 signing
/// key whose uid contains `CI_SIGNING_KEY_MARKER`. Returns the most
/// recently created match, or `None` if nothing usable is found.
pub fn find_ci_signing_key() -> Option<CiSigningKey> {
    let output = std::process::Command::new("gpg")
        .args(["--list-secret-keys", "--with-colons"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);

    let mut current_fpr: Option<String> = None;
    let mut current_created: i64 = 0;
    let mut current_expired: bool = false;
    let mut best: Option<(i64, CiSigningKey)> = None;
    for line in text.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        match fields.first().copied() {
            Some("sec") => {
                // Field 2 = validity (e for expired, - for unknown, u/f for valid).
                current_fpr = None;
                current_created = fields.get(5).and_then(|s| s.parse().ok()).unwrap_or(0);
                current_expired = matches!(fields.get(1).copied(), Some("e") | Some("r"));
            }
            Some("fpr") if current_fpr.is_none() => {
                current_fpr = fields.get(9).map(|s| s.to_string());
            }
            Some("uid") if !current_expired => {
                let uid = fields.get(9).copied().unwrap_or("");
                if !uid.contains(CI_SIGNING_KEY_MARKER) {
                    continue;
                }
                let Some(fpr) = current_fpr.clone() else {
                    continue;
                };
                let (name, email) = parse_uid(uid);
                let key = CiSigningKey {
                    fingerprint: fpr,
                    name,
                    email,
                };
                let is_newer = best.as_ref().map_or(true, |(ts, _)| current_created > *ts);
                if is_newer {
                    best = Some((current_created, key));
                }
            }
            _ => {}
        }
    }
    best.map(|(_, k)| k)
}

/// Splits a uid like `"Andreas Pohl (plan-executor CI) <bot@example.com>"`
/// into `(name, email)`. Returns the raw uid in `name` and an empty email
/// if the expected `<...>` segment is missing.
fn parse_uid(uid: &str) -> (String, String) {
    match (uid.find('<'), uid.rfind('>')) {
        (Some(lt), Some(gt)) if gt > lt => {
            let name = uid[..lt].trim().to_string();
            let email = uid[lt + 1..gt].trim().to_string();
            (name, email)
        }
        _ => (uid.trim().to_string(), String::new()),
    }
}

/// Generates a passphraseless ed25519 GPG signing key whose uid carries
/// `CI_SIGNING_KEY_MARKER` in the comment field so future runs can find it.
/// The Name-Real field stays clean so commits show just the real author.
/// Returns the new key's full fingerprint.
pub fn gpg_generate_ci_key(name: &str, email: &str) -> Result<String> {
    use std::io::Write;
    use std::process::Stdio;

    // Forbid characters that break the uid schema. `(` and `)` are reserved
    // for the comment block; `<` and `>` terminate Name-Real.
    anyhow::ensure!(
        !name.contains('<') && !name.contains('>') && !name.contains('(') && !name.contains(')'),
        "signing-key name must not contain angle brackets or parentheses"
    );

    let recipe = format!(
        "%no-protection\n\
         Key-Type: EDDSA\n\
         Key-Curve: ed25519\n\
         Key-Usage: sign\n\
         Name-Real: {name}\n\
         Name-Comment: {marker}\n\
         Name-Email: {email}\n\
         Expire-Date: 2y\n\
         %commit\n",
        marker = CI_SIGNING_KEY_MARKER,
    );

    let mut child = std::process::Command::new("gpg")
        .args(["--batch", "--status-fd", "2", "--gen-key"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn gpg: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(recipe.as_bytes())?;
    }
    let output = child.wait_with_output()?;
    anyhow::ensure!(
        output.status.success(),
        "gpg --gen-key failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Parse the KEY_CREATED line from status stderr for the new fingerprint.
    //   [GNUPG:] KEY_CREATED <type> <fingerprint> <handle>
    let stderr = String::from_utf8_lossy(&output.stderr);
    for line in stderr.lines() {
        if let Some(rest) = line.strip_prefix("[GNUPG:] KEY_CREATED ") {
            let mut parts = rest.split_whitespace();
            let _kind = parts.next();
            if let Some(fpr) = parts.next() {
                return Ok(fpr.to_string());
            }
        }
    }
    // Fallback: look the key up by email (most recent wins).
    find_ci_signing_key()
        .filter(|k| k.email == email)
        .map(|k| k.fingerprint)
        .ok_or_else(|| anyhow::anyhow!("generated key but could not find its fingerprint"))
}

/// Exports the armored public key for the given fingerprint.
pub fn gpg_export_public(fingerprint: &str) -> Result<String> {
    gpg_export(fingerprint, false)
}

/// Exports the armored secret key for the given fingerprint.
pub fn gpg_export_secret(fingerprint: &str) -> Result<String> {
    gpg_export(fingerprint, true)
}

fn gpg_export(fingerprint: &str, secret: bool) -> Result<String> {
    let flag = if secret {
        "--export-secret-keys"
    } else {
        "--export"
    };
    let output = std::process::Command::new("gpg")
        .args(["--batch", "--armor", flag, fingerprint])
        .output()
        .map_err(|e| anyhow::anyhow!("failed to spawn gpg export: {e}"))?;
    anyhow::ensure!(
        output.status.success(),
        "gpg {} failed: {}",
        flag,
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Result of querying the current user's uploaded GPG keys.
///
/// Distinguishes "scope missing" from a genuine network/API error so the
/// caller can give the operator an actionable manual-upload fallback
/// instead of a scary stacktrace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GithubGpgKeyCheck {
    Present,
    Absent,
    MissingScope,
}

/// Recognizes the `gh` error that fires when the cached OAuth token lacks
/// `admin:gpg_key` / `read:gpg_key` scope. The default `gh auth login` flow
/// does not request those scopes, so this is the common case on a fresh
/// machine.
fn is_gpg_scope_error(stderr: &str) -> bool {
    stderr.contains("admin:gpg_key")
        || stderr.contains("read:gpg_key")
        || stderr.contains("write:gpg_key")
}

/// Checks whether the current `gh auth` user already has a GPG key with
/// this fingerprint uploaded, returning `MissingScope` when the OAuth
/// token can't see the endpoint at all.
pub fn github_check_gpg_key(fingerprint: &str) -> Result<GithubGpgKeyCheck> {
    let output = std::process::Command::new("gh")
        .args(["api", "user/gpg_keys", "--jq", ".[].key_id"])
        .output()
        .map_err(|e| anyhow::anyhow!("failed to run gh: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if is_gpg_scope_error(&stderr) {
            return Ok(GithubGpgKeyCheck::MissingScope);
        }
        anyhow::bail!("gh api user/gpg_keys failed: {stderr}");
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let fp_upper = fingerprint.to_uppercase();
    // GitHub reports either the long key id or the full fingerprint
    // depending on vintage; match against either.
    let hit = stdout.lines().any(|l| {
        let s = l.trim().to_uppercase();
        !s.is_empty() && (fp_upper.ends_with(&s) || s.ends_with(&fp_upper))
    });
    Ok(if hit {
        GithubGpgKeyCheck::Present
    } else {
        GithubGpgKeyCheck::Absent
    })
}

/// Outcome of attempting to upload the armored public key to GitHub.
pub enum GithubGpgUploadResult {
    Uploaded,
    MissingScope,
}

/// Uploads the armored public key to the current `gh auth` user's GPG
/// keys. Returns `MissingScope` when the OAuth token lacks the required
/// scope so the caller can fall back to a manual-paste flow instead of
/// exiting with an error.
pub fn github_upload_gpg_key(armored_public: &str) -> Result<GithubGpgUploadResult> {
    use std::io::Write;
    use std::process::Stdio;
    // Using `-f armored_public_key=@-` so the key body travels through
    // stdin rather than argv; large keys and multiline content don't fit
    // in a shell argument cleanly anyway.
    let mut child = std::process::Command::new("gh")
        .args([
            "api",
            "user/gpg_keys",
            "-X",
            "POST",
            "-H",
            "Accept: application/vnd.github+json",
            "-f",
            "armored_public_key=@-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn gh: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(armored_public.as_bytes())?;
    }
    let output = child.wait_with_output()?;
    if output.status.success() {
        return Ok(GithubGpgUploadResult::Uploaded);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if is_gpg_scope_error(&stderr) {
        return Ok(GithubGpgUploadResult::MissingScope);
    }
    anyhow::bail!("gh api user/gpg_keys POST failed: {stderr}");
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
        anyhow::bail!(
            "gh secret set {} failed: {}",
            name,
            String::from_utf8_lossy(&output.stderr)
        );
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
    fn test_is_branch_protection_error_matches_known_messages() {
        let cases = [
            "error: GH013: Repository rule violations found for refs/heads/main",
            "remote rejected: push declined due to repository rule violations",
            "Required workflow 'Compliance Checks - Wiz scan' is not satisfied",
            "Changes must be made through a pull request",
            "protected branch hook declined",
        ];
        for msg in cases {
            let err = anyhow::anyhow!(msg.to_string());
            assert!(
                is_branch_protection_error(&err),
                "expected match for: {msg}",
            );
        }
    }

    #[test]
    fn test_is_branch_protection_error_ignores_unrelated() {
        let err = anyhow::anyhow!("fatal: could not read from remote repository");
        assert!(!is_branch_protection_error(&err));
    }

    #[test]
    fn test_pr_title_format() {
        let meta = ExecutionMetadata {
            target_repo: "owner/my-service".to_string(),
            target_ref: "abc123def456".to_string(),
            target_branch: "feat/cool".to_string(),
            plan_filename: "plan-add-feature.md".to_string(),
            started_at: "2026-04-08T14:30:00Z".to_string(),
        };
        assert_eq!(
            pr_title(&meta),
            "exec: plan-add-feature.md @ owner/my-service"
        );
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
    fn test_parse_repo_slug_https() {
        assert_eq!(
            parse_repo_slug("https://github.com/owner/repo.git"),
            Some("owner/repo".to_string())
        );
    }

    #[test]
    fn test_parse_repo_slug_https_no_git_suffix() {
        assert_eq!(
            parse_repo_slug("https://github.com/owner/repo"),
            Some("owner/repo".to_string())
        );
    }

    #[test]
    fn test_parse_repo_slug_ssh() {
        assert_eq!(
            parse_repo_slug("git@github.com:owner/repo.git"),
            Some("owner/repo".to_string())
        );
    }

    #[test]
    fn test_parse_repo_slug_ssh_alias() {
        assert_eq!(
            parse_repo_slug("git@github.com-priv:apohl79/cycle-maps.git"),
            Some("apohl79/cycle-maps".to_string())
        );
    }

    #[test]
    fn test_parse_repo_slug_ssh_alias_no_git_suffix() {
        assert_eq!(
            parse_repo_slug("git@github.com-work:org/repo"),
            Some("org/repo".to_string())
        );
    }

    #[test]
    fn test_parse_repo_slug_non_github_rejected() {
        assert_eq!(parse_repo_slug("git@gitlab.com:owner/repo.git"), None);
    }

    #[test]
    fn test_parse_repo_slug_traversal_rejected() {
        assert_eq!(parse_repo_slug("git@github.com:../etc/passwd.git"), None);
    }

    #[test]
    fn test_parse_repo_slug_whitespace_trimmed() {
        assert_eq!(
            parse_repo_slug("  git@github.com:owner/repo.git\n"),
            Some("owner/repo".to_string())
        );
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
