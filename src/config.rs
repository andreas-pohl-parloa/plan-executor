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
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            main:   Self::default_main(),
            claude: Self::default_claude(),
            codex:  Self::default_codex(),
            gemini: Self::default_gemini(),
        }
    }
}

/// Application configuration loaded from `~/.plan-executor/config.json`
/// (or a custom path supplied via `--config`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Directories to watch for plan files (tilde-expanded).
    pub watch_dirs: Vec<String>,
    /// Glob patterns relative to each watch_dir, e.g. `[".my/plans/*.md"]`.
    pub plan_patterns: Vec<String>,
    /// If true, auto-execute READY plans after 15 s countdown.
    pub auto_execute: bool,
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
            watch_dirs: vec!["~/tools".to_string()],
            plan_patterns: vec!["**/.my/plans/*.md".to_string()],
            auto_execute: false,
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
        let mut config: Self = serde_json::from_str(&content)?;

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
        }

        Ok(config)
    }

    /// Expands tilde in watch_dirs to absolute paths.
    pub fn expanded_watch_dirs(&self) -> Vec<PathBuf> {
        self.watch_dirs.iter().map(|d| expand_tilde(d)).collect()
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
