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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            agents: AgentConfig::default(),
            remote_repo: None,
        }
    }
}

impl Config {
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
}
