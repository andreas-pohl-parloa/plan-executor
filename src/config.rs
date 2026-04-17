use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};
use anyhow::Result;

/// Per-agent-type command templates.
/// Each string is split on whitespace into [program, args...]; the final
/// argument (prompt path or -p) is appended automatically by the caller.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Main orchestrator agent. Appended args: `-p "<prompt>"`.
    /// Must produce `--output-format stream-json` output.
    #[serde(default = "AgentConfig::default_main")]
    pub main: String,
    /// Sub-agent: claude type. Appended arg: `<prompt_file_path>`.
    #[serde(default = "AgentConfig::default_claude")]
    pub claude: String,
    /// Sub-agent: codex type. Appended arg: `<prompt_file_path>`.
    #[serde(default = "AgentConfig::default_codex")]
    pub codex: String,
    /// Sub-agent: gemini type. Appended arg: `<prompt_file_path>`.
    #[serde(default = "AgentConfig::default_gemini")]
    pub gemini: String,
    /// Sub-agent: bash type. Runs `<cmd> <script_file_path>`.
    #[serde(default = "AgentConfig::default_bash")]
    pub bash: String,
}

impl AgentConfig {
    fn default_main() -> String {
        "claude --dangerously-skip-permissions --verbose --output-format stream-json".to_string()
    }
    fn default_claude() -> String {
        "claude --dangerously-skip-permissions -p".to_string()
    }
    fn default_codex() -> String {
        "codex --dangerously-bypass-approvals-and-sandbox exec".to_string()
    }
    fn default_gemini() -> String {
        "gemini --yolo -p".to_string()
    }
    fn default_bash() -> String {
        "bash".to_string()
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            main:   Self::default_main(),
            claude: Self::default_claude(),
            codex:  Self::default_codex(),
            gemini: Self::default_gemini(),
            bash:   Self::default_bash(),
        }
    }
}

/// Application configuration loaded from `~/.plan-executor/config.json`
/// (or a custom path supplied via `--config`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Agent command overrides. Uses built-in defaults when absent.
    #[serde(default)]
    pub agents: AgentConfig,
    /// GitHub repo slug for remote execution (e.g. "owner/plan-executions").
    /// Set via `plan-executor remote-setup`.
    #[serde(default)]
    pub remote_repo: Option<String>,
    /// Watchdog: a job that emits no events for this many seconds is killed
    /// and marked Failed. A hung sub-agent (e.g. a Bash script stuck in
    /// `wait`) will stop producing output, so this is the primary liveness
    /// signal. Default: 900 (15 min).
    #[serde(default = "Config::default_stall_timeout_seconds")]
    pub stall_timeout_seconds: u64,
    /// Watchdog: absolute ceiling on total job runtime regardless of
    /// activity. Caps genuinely long runs that never go idle. Default:
    /// 10800 (3h).
    #[serde(default = "Config::default_hard_cap_seconds")]
    pub hard_cap_seconds: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            agents: AgentConfig::default(),
            remote_repo: None,
            stall_timeout_seconds: Self::default_stall_timeout_seconds(),
            hard_cap_seconds: Self::default_hard_cap_seconds(),
        }
    }
}

impl Config {
    fn default_stall_timeout_seconds() -> u64 { 900 }
    fn default_hard_cap_seconds() -> u64 { 10_800 }

    /// Returns the base directory: `~/.plan-executor/`
    pub fn base_dir() -> PathBuf {
        dirs::home_dir()
            .expect("home dir must exist")
            .join(".plan-executor")
    }

    /// Returns the default config file path: `~/.plan-executor/config.json`
    pub fn config_path() -> PathBuf {
        Self::base_dir().join("config.json")
    }

    /// Loads config from `path` (or the default location when `None`).
    /// Writes and returns the default config if the file does not exist.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let p = path.map(|p| p.to_path_buf()).unwrap_or_else(Self::config_path);
        if !p.exists() {
            let cfg = Self::default();
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&p, serde_json::to_string_pretty(&cfg)?)?;
            tracing::info!("wrote default config to {}", p.display());
            return Ok(cfg);
        }
        let content = std::fs::read_to_string(&p)?;
        // Parse leniently: unknown fields are ignored by serde default behavior,
        // and all fields have #[serde(default)] so missing fields get defaults.
        // Fall back to full defaults only on truly broken JSON (syntax errors).
        let mut config: Self = match serde_json::from_str(&content) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("config at {} could not be parsed ({}), using defaults", p.display(), e);
                Self::default()
            }
        };

        // Resolve relative agent command paths against the config file's directory
        // so that `./main-agent.sh` works correctly after daemonize changes CWD to `/`.
        if let Some(dir) = p.parent() {
            let resolve = |cmd: &str| -> String {
                let prog = cmd.split_whitespace().next().unwrap_or("");
                if prog.starts_with("./") || prog.starts_with("../") {
                    let abs = dir.join(prog);
                    let rest = cmd[prog.len()..].to_string();
                    format!("{}{}", abs.display(), rest)
                } else {
                    cmd.to_string()
                }
            };
            config.agents.main   = resolve(&config.agents.main);
            config.agents.claude = resolve(&config.agents.claude);
            config.agents.codex  = resolve(&config.agents.codex);
            config.agents.gemini = resolve(&config.agents.gemini);
            config.agents.bash   = resolve(&config.agents.bash);
        }

        Ok(config)
    }

    /// Splits a command template string into (program, leading_args).
    /// Callers append the final argument(s) themselves.
    pub fn parse_cmd(template: &str) -> (String, Vec<String>) {
        let mut parts = template.split_whitespace();
        let program = parts.next().unwrap_or("claude").to_string();
        let args: Vec<String> = parts.map(String::from).collect();
        (program, args)
    }
}

/// Watchdog decision for a single running job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchdogVerdict {
    /// Job is healthy — recent activity and within the hard cap.
    Ok,
    /// No event for longer than `stall_timeout`.
    Stalled { silent_seconds: u64 },
    /// Total runtime exceeded `hard_cap`.
    HardCapped { total_seconds: u64 },
}

/// Pure decision function: given timing inputs, decide whether to kill a job.
///
/// HardCapped wins over Stalled when both apply (a job that's been silent for
/// hours is also over the hard cap; we report the more severe reason).
/// Thresholds of 0 disable the corresponding check — useful for tests and
/// intentional opt-out.
pub fn watchdog_verdict(
    now_since_start: std::time::Duration,
    now_since_last_activity: std::time::Duration,
    stall_timeout: std::time::Duration,
    hard_cap: std::time::Duration,
) -> WatchdogVerdict {
    let over_hard_cap =
        !hard_cap.is_zero() && now_since_start >= hard_cap;
    let stalled =
        !stall_timeout.is_zero() && now_since_last_activity >= stall_timeout;

    if over_hard_cap {
        WatchdogVerdict::HardCapped {
            total_seconds: now_since_start.as_secs(),
        }
    } else if stalled {
        WatchdogVerdict::Stalled {
            silent_seconds: now_since_last_activity.as_secs(),
        }
    } else {
        WatchdogVerdict::Ok
    }
}

/// Expands a leading `~/` to the home directory.
pub fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        dirs::home_dir()
            .expect("home dir must exist")
            .join(rest)
    } else {
        PathBuf::from(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expand_tilde_replaces_home() {
        let result = expand_tilde("~/foo/bar");
        let home = dirs::home_dir().unwrap();
        assert_eq!(result, home.join("foo/bar"));
    }

    #[test]
    fn test_expand_tilde_no_tilde() {
        let result = expand_tilde("/absolute/path");
        assert_eq!(result, std::path::PathBuf::from("/absolute/path"));
    }

    #[test]
    fn test_config_default() {
        let config = Config::default();
        assert!(config.remote_repo.is_none());
    }

    #[test]
    fn test_config_serde_roundtrip() {
        let json = r#"{"remote_repo": "owner/plan-executions"}"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert_eq!(config.remote_repo.as_deref(), Some("owner/plan-executions"));
    }

    #[test]
    fn test_config_remote_repo_none_by_default() {
        let config = Config::default();
        assert!(config.remote_repo.is_none());
    }

    #[test]
    fn test_config_remote_repo_from_json() {
        let json = r#"{
            "remote_repo": "owner/plan-executions"
        }"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert_eq!(config.remote_repo.as_deref(), Some("owner/plan-executions"));
    }

    #[test]
    fn test_config_remote_repo_absent_in_json() {
        let json = r#"{}"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert!(config.remote_repo.is_none());
    }

    #[test]
    fn test_config_watchdog_defaults_when_absent() {
        let json = r#"{}"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert_eq!(config.stall_timeout_seconds, 900);
        assert_eq!(config.hard_cap_seconds, 10_800);
    }

    #[test]
    fn test_config_watchdog_overrides() {
        let json = r#"{"stall_timeout_seconds": 60, "hard_cap_seconds": 300}"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert_eq!(config.stall_timeout_seconds, 60);
        assert_eq!(config.hard_cap_seconds, 300);
    }

    use std::time::Duration;

    #[test]
    fn test_watchdog_healthy_job_within_thresholds() {
        let verdict = watchdog_verdict(
            Duration::from_secs(120),
            Duration::from_secs(5),
            Duration::from_secs(900),
            Duration::from_secs(10_800),
        );
        assert_eq!(verdict, WatchdogVerdict::Ok);
    }

    #[test]
    fn test_watchdog_detects_stall_exactly_at_threshold() {
        let verdict = watchdog_verdict(
            Duration::from_secs(901),
            Duration::from_secs(900),
            Duration::from_secs(900),
            Duration::from_secs(10_800),
        );
        assert_eq!(
            verdict,
            WatchdogVerdict::Stalled { silent_seconds: 900 }
        );
    }

    #[test]
    fn test_watchdog_detects_stall_just_below_threshold_is_ok() {
        let verdict = watchdog_verdict(
            Duration::from_secs(900),
            Duration::from_secs(899),
            Duration::from_secs(900),
            Duration::from_secs(10_800),
        );
        assert_eq!(verdict, WatchdogVerdict::Ok);
    }

    #[test]
    fn test_watchdog_hard_cap_wins_over_stall() {
        // Both conditions true — hard cap reported for severity.
        let verdict = watchdog_verdict(
            Duration::from_secs(11_000),
            Duration::from_secs(5_000),
            Duration::from_secs(900),
            Duration::from_secs(10_800),
        );
        assert_eq!(
            verdict,
            WatchdogVerdict::HardCapped { total_seconds: 11_000 }
        );
    }

    #[test]
    fn test_watchdog_hard_cap_fires_even_with_recent_activity() {
        let verdict = watchdog_verdict(
            Duration::from_secs(10_800),
            Duration::from_secs(1),
            Duration::from_secs(900),
            Duration::from_secs(10_800),
        );
        assert_eq!(
            verdict,
            WatchdogVerdict::HardCapped { total_seconds: 10_800 }
        );
    }

    #[test]
    fn test_watchdog_zero_thresholds_disable_checks() {
        // A job idle and running forever is Ok if both thresholds are 0.
        let verdict = watchdog_verdict(
            Duration::from_secs(999_999),
            Duration::from_secs(999_999),
            Duration::from_secs(0),
            Duration::from_secs(0),
        );
        assert_eq!(verdict, WatchdogVerdict::Ok);
    }
}
