use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};
use anyhow::Result;

/// Application configuration loaded from ~/.plan-executor/config.json
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Directories to watch for plan files (tilde-expanded)
    pub watch_dirs: Vec<String>,
    /// Glob patterns relative to each watch_dir, e.g. [".my/plans/*.md"]
    pub plan_patterns: Vec<String>,
    /// If true, auto-execute READY plans after 15s countdown
    pub auto_execute: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            watch_dirs: vec!["~/tools".to_string()],
            plan_patterns: vec![".my/plans/*.md".to_string()],
            auto_execute: false,
        }
    }
}

impl Config {
    /// Returns the base directory: ~/.plan-executor/
    pub fn base_dir() -> PathBuf {
        dirs::home_dir()
            .expect("home dir must exist")
            .join(".plan-executor")
    }

    /// Returns the config file path: ~/.plan-executor/config.json
    pub fn config_path() -> PathBuf {
        Self::base_dir().join("config.json")
    }

    /// Loads config from disk; returns Default if file does not exist.
    pub fn load() -> Result<Self> {
        let path = Self::config_path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(&path)?;
        let config: Self = serde_json::from_str(&content)?;
        Ok(config)
    }

    /// Expands tilde in watch_dirs to absolute paths.
    pub fn expanded_watch_dirs(&self) -> Vec<PathBuf> {
        self.watch_dirs
            .iter()
            .map(|d| expand_tilde(d))
            .collect()
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
