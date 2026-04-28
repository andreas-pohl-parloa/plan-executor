//! Integration tests for `plan-executor run pr-finalize --remote`.
//!
//! These tests drive the compiled binary end-to-end with a fake `gh`
//! intercepting every subprocess invocation. The fake records argv +
//! the value of every `-f field=value` payload so each test can assert
//! both call sequence AND the JSON content pushed to the execution repo.
//!
//! ### Test design
//!
//! Per the plan, the `--remote` flag triggers four `gh` invocations in
//! order:
//!   1. `gh api repos/<remote_repo>/git/ref/heads/main --jq .object.sha`
//!   2. `gh api repos/<remote_repo>/git/refs -X POST -f ref=… -f sha=…`
//!   3. `gh api repos/<remote_repo>/contents/job-spec.json -X PUT
//!       -f message=… -f branch=… -f content=<base64>`
//!   4. `gh pr create --repo <remote_repo> --head … --title … --body …`
//!
//! Each test isolates its own tempdir + counter file so cargo-test's
//! parallel runner cannot cross-contaminate observations. PATH mutation
//! is serialized via [`PATH_LOCK`].

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use tempfile::TempDir;

/// Process-wide PATH lock. `std::env::set_var` is not thread-safe; tests
/// in this file run sequentially against PATH for the duration of each
/// fake-gh harness.
static PATH_LOCK: Mutex<()> = Mutex::new(());

/// RAII PATH override. Drop restores the original PATH so the next test
/// starts clean even if the binary spawns a subprocess.
struct PathGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    original: Option<String>,
}

impl PathGuard {
    fn new(prepend: &Path) -> Self {
        let lock = PATH_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let original = std::env::var("PATH").ok();
        let new_path = match &original {
            Some(p) => format!("{}:{}", prepend.display(), p),
            None => prepend.display().to_string(),
        };
        // SAFETY: tests serialize on PATH_LOCK; no other thread mutates
        // PATH for the duration of this guard.
        unsafe { std::env::set_var("PATH", new_path) };
        Self {
            _lock: lock,
            original,
        }
    }
}

impl Drop for PathGuard {
    fn drop(&mut self) {
        match &self.original {
            // SAFETY: tests serialize on PATH_LOCK.
            Some(p) => unsafe { std::env::set_var("PATH", p) },
            None => unsafe { std::env::remove_var("PATH") },
        }
    }
}

/// Writes `body` to `path` and chmods it executable.
fn write_script(path: &Path, body: &str) {
    fs::write(path, body).expect("write script");
    let mut perms = fs::metadata(path).expect("stat script").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).expect("chmod script");
}

/// Builds a fake `gh` script that:
///  * captures argv tokens 1+2 into `<dir>/calls.log` (one
///    space-separated line per invocation; argv that contains newlines
///    is not preserved verbatim — only the leading subcommand pair is
///    recorded for sequence assertions)
///  * for every `-f key=value` pair, writes the raw value to
///    `<dir>/<key>.field` (overwriting on each call) so tests can
///    inspect the most recent payload of any specific field
///  * branches on `$1 $2` to satisfy each step of the remote dispatch
const HAPPY_GH_BODY: &str = r#"#!/bin/sh
SCRIPT_DIR="$(dirname "$0")"

# Record only the first two argv tokens (subcommand pair) per call.
# Multiline arguments (PR body) make full-argv recording fragile and we
# don't need full argv for sequence assertions.
SUB1="$1"
SUB2="$2"
printf '%s\t%s\n' "$SUB1" "$SUB2" >> "$SCRIPT_DIR/calls.log"

# Walk argv for `-f key=value` pairs and snapshot the most-recent value
# of each key. The next call overwrites prior values for the same key,
# but our tests only assert against the final invocation's payload.
prev=""
for a in "$@"; do
  case "$prev" in
    -f|--field|--raw-field)
      key="${a%%=*}"
      val="${a#*=}"
      printf '%s' "$val" > "$SCRIPT_DIR/${key}.field"
      ;;
  esac
  prev="$a"
done

case "$SUB1 $SUB2" in
  "api repos/octo/exec-repo/git/ref/heads/main")
    echo "deadbeefcafe"
    exit 0
    ;;
  "api repos/octo/exec-repo/git/refs")
    echo '{"ref":"refs/heads/exec/foo"}'
    exit 0
    ;;
  "api repos/octo/exec-repo/contents/job-spec.json")
    echo '{"content":{"sha":"abc"}}'
    exit 0
    ;;
  "pr create")
    echo "https://github.com/octo/exec-repo/pull/77"
    exit 0
    ;;
  "repo view")
    # Auto-detect path: emits owner/name JSON.
    echo '{"owner":{"login":"target-owner"},"name":"target-repo"}'
    exit 0
    ;;
  *)
    echo "unexpected gh args: $SUB1 $SUB2" >&2
    exit 99
    ;;
esac
"#;

struct Harness {
    dir: TempDir,
    config_path: PathBuf,
    _path_guard: PathGuard,
}

impl Harness {
    fn new_with_config(remote_repo: Option<&str>) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        // Write the fake gh.
        let gh = dir.path().join("gh");
        write_script(&gh, HAPPY_GH_BODY);
        // Initialize an empty calls log.
        fs::write(dir.path().join("calls.log"), "").expect("init counter");

        // Write a config file. The `--config` arg is passed to the binary
        // so `Config::load` reads from this path instead of the default
        // `~/.plan-executor/config.json`.
        let config_path = dir.path().join("config.json");
        let cfg = match remote_repo {
            Some(r) => format!(r#"{{ "remote_repo": "{r}" }}"#),
            None => "{}".to_string(),
        };
        fs::write(&config_path, cfg).expect("write config");

        let path_guard = PathGuard::new(dir.path());
        Self {
            dir,
            config_path,
            _path_guard: path_guard,
        }
    }

    fn dir(&self) -> &Path {
        self.dir.path()
    }

    /// Returns the recorded `(subcommand, target)` pair for each
    /// invocation in order. Only the first two argv tokens are captured;
    /// see `HAPPY_GH_BODY` for the rationale.
    fn calls(&self) -> Vec<(String, String)> {
        let raw = fs::read_to_string(self.dir().join("calls.log")).unwrap_or_default();
        raw.lines()
            .filter(|l| !l.is_empty())
            .map(|l| {
                let mut it = l.splitn(2, '\t');
                let a = it.next().unwrap_or("").to_string();
                let b = it.next().unwrap_or("").to_string();
                (a, b)
            })
            .collect()
    }

    /// Reads the most-recent value of an `-f <key>=<value>` field.
    fn field(&self, key: &str) -> Option<String> {
        fs::read_to_string(self.dir().join(format!("{key}.field"))).ok()
    }

    /// Runs the binary and returns (status, stdout, stderr).
    fn run(&self, extra_args: &[&str]) -> (std::process::ExitStatus, String, String) {
        let bin = env!("CARGO_BIN_EXE_plan-executor");
        let mut cmd = Command::new(bin);
        cmd.arg("--config").arg(&self.config_path);
        cmd.args(extra_args);
        let out = cmd.output().expect("spawn plan-executor");
        (
            out.status,
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }
}

// ---------------------------------------------------------------------
// Scenario 1: happy path with explicit --owner/--repo. All four gh calls
// must fire in order, the printed PR URL must equal the fake's response,
// and the pushed job-spec.json must contain pr=42, merge=false,
// merge_admin=false, owner=target-owner, repo=target-repo.
// ---------------------------------------------------------------------

#[test]
fn happy_path_pushes_job_spec_and_prints_pr_url() {
    let h = Harness::new_with_config(Some("octo/exec-repo"));

    let (status, stdout, stderr) = h.run(&[
        "run",
        "pr-finalize",
        "--remote",
        "--pr",
        "42",
        "--owner",
        "target-owner",
        "--repo",
        "target-repo",
    ]);
    assert!(
        status.success(),
        "binary exited non-zero. stderr: {stderr}\nstdout: {stdout}"
    );

    // Trailing newline is fine; trim and compare.
    assert_eq!(stdout.trim(), "https://github.com/octo/exec-repo/pull/77");

    // Verify the four-call sequence: get base sha, create branch ref,
    // push contents, open PR.
    let calls = h.calls();
    assert_eq!(
        calls.len(),
        4,
        "expected 4 gh invocations, got {}: {:?}",
        calls.len(),
        calls
    );
    assert_eq!(calls[0].0, "api");
    assert!(
        calls[0].1.ends_with("git/ref/heads/main"),
        "first call should fetch base sha, got: {:?}",
        calls[0]
    );
    assert_eq!(calls[1].0, "api");
    assert!(
        calls[1].1.ends_with("git/refs"),
        "second call should create branch ref, got: {:?}",
        calls[1]
    );
    assert_eq!(calls[2].0, "api");
    assert!(
        calls[2].1.ends_with("contents/job-spec.json"),
        "third call should push job-spec.json, got: {:?}",
        calls[2]
    );
    assert_eq!(calls[3].0, "pr");
    assert_eq!(calls[3].1, "create");

    // Decode the base64 content sent to the Contents API and verify the
    // job-spec.json fields. The fake recorded the raw `content=…` value.
    let encoded = h.field("content").expect("content field recorded");
    let decoded = base64_decode(&encoded);
    let json: serde_json::Value =
        serde_json::from_str(&decoded).expect("valid JSON in pushed blob");
    assert_eq!(json["kind"], "pr-finalize");
    assert_eq!(json["pr"], 42);
    assert_eq!(json["merge"], false);
    assert_eq!(json["merge_admin"], false);
    assert_eq!(json["owner"], "target-owner");
    assert_eq!(json["repo"], "target-repo");
}

// ---------------------------------------------------------------------
// Scenario 2: --merge forwards as merge=true in the pushed job-spec.json.
// ---------------------------------------------------------------------

#[test]
fn merge_flag_forwards_into_job_spec() {
    let h = Harness::new_with_config(Some("octo/exec-repo"));

    let (status, _stdout, stderr) = h.run(&[
        "run",
        "pr-finalize",
        "--remote",
        "--pr",
        "100",
        "--owner",
        "octo",
        "--repo",
        "demo",
        "--merge",
    ]);
    assert!(status.success(), "stderr: {stderr}");

    let encoded = h.field("content").expect("content field recorded");
    let json: serde_json::Value =
        serde_json::from_str(&base64_decode(&encoded)).expect("valid JSON");
    assert_eq!(json["merge"], true);
    assert_eq!(json["merge_admin"], false);
    assert_eq!(json["pr"], 100);
}

// ---------------------------------------------------------------------
// Scenario 3: --merge-admin forwards as merge_admin=true.
// ---------------------------------------------------------------------

#[test]
fn merge_admin_flag_forwards_into_job_spec() {
    let h = Harness::new_with_config(Some("octo/exec-repo"));

    let (status, _stdout, stderr) = h.run(&[
        "run",
        "pr-finalize",
        "--remote",
        "--pr",
        "7",
        "--owner",
        "octo",
        "--repo",
        "demo",
        "--merge-admin",
    ]);
    assert!(status.success(), "stderr: {stderr}");

    let encoded = h.field("content").expect("content field recorded");
    let json: serde_json::Value =
        serde_json::from_str(&base64_decode(&encoded)).expect("valid JSON");
    assert_eq!(json["merge"], false);
    assert_eq!(json["merge_admin"], true);
}

// ---------------------------------------------------------------------
// Scenario 4: missing --owner/--repo triggers gh repo view auto-detect;
// the detected slug ends up in the pushed job-spec.json.
// ---------------------------------------------------------------------

#[test]
fn missing_owner_repo_triggers_auto_detect() {
    let h = Harness::new_with_config(Some("octo/exec-repo"));

    let (status, _stdout, stderr) = h.run(&["run", "pr-finalize", "--remote", "--pr", "9"]);
    assert!(status.success(), "stderr: {stderr}");

    // First call must be `gh repo view` for the auto-detect.
    let calls = h.calls();
    assert!(!calls.is_empty(), "expected at least one gh call");
    assert_eq!(calls[0].0, "repo");
    assert_eq!(calls[0].1, "view");

    // Decoded job-spec.json must carry the detected owner/repo from the
    // fake's gh repo view response (target-owner / target-repo).
    let encoded = h.field("content").expect("content field recorded");
    let json: serde_json::Value =
        serde_json::from_str(&base64_decode(&encoded)).expect("valid JSON");
    assert_eq!(json["owner"], "target-owner");
    assert_eq!(json["repo"], "target-repo");
}

// ---------------------------------------------------------------------
// Scenario 5: missing remote_repo in config produces an actionable error
// referencing `remote-setup` and exits non-zero. No gh calls should fire.
// ---------------------------------------------------------------------

#[test]
fn missing_remote_repo_config_errors_with_setup_hint() {
    let h = Harness::new_with_config(None);

    let (status, _stdout, stderr) = h.run(&[
        "run",
        "pr-finalize",
        "--remote",
        "--pr",
        "1",
        "--owner",
        "octo",
        "--repo",
        "demo",
    ]);
    assert!(!status.success(), "expected non-zero exit");
    assert!(
        stderr.contains("remote_repo") && stderr.contains("remote-setup"),
        "stderr should hint remote-setup: {stderr}"
    );
    // gh must never have been called — config check fails first.
    assert!(
        h.calls().is_empty(),
        "no gh calls expected, got: {:?}",
        h.calls()
    );
}

// ---------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------

/// Minimal base64 decode mirroring the encoder in `src/remote.rs`. Only
/// supports the subset produced by `base64_encode` (standard alphabet,
/// `=` padding, no whitespace).
fn base64_decode(input: &str) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut lookup = [255u8; 256];
    for (i, &c) in CHARS.iter().enumerate() {
        lookup[c as usize] = u8::try_from(i).unwrap_or(0);
    }
    let bytes: Vec<u8> = input
        .bytes()
        .filter(|b| *b != b'\n' && *b != b'\r' && *b != b' ')
        .collect();
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        if chunk.len() < 4 {
            break;
        }
        let v0 = lookup[chunk[0] as usize] as u32;
        let v1 = lookup[chunk[1] as usize] as u32;
        let v2 = if chunk[2] == b'=' {
            0
        } else {
            lookup[chunk[2] as usize] as u32
        };
        let v3 = if chunk[3] == b'=' {
            0
        } else {
            lookup[chunk[3] as usize] as u32
        };
        let triple = (v0 << 18) | (v1 << 12) | (v2 << 6) | v3;
        out.push(((triple >> 16) & 0xFF) as u8);
        if chunk[2] != b'=' {
            out.push(((triple >> 8) & 0xFF) as u8);
        }
        if chunk[3] != b'=' {
            out.push((triple & 0xFF) as u8);
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
