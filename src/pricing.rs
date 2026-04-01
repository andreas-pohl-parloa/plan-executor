use std::collections::HashMap;
use std::path::PathBuf;
use serde::Deserialize;
use anyhow::Result;

#[derive(Debug, Clone, Deserialize)]
pub struct ModelPricing {
    #[serde(rename = "inputPerMtok")]
    pub input_per_mtok: f64,
    #[serde(rename = "outputPerMtok")]
    pub output_per_mtok: f64,
    #[serde(rename = "cacheWrite5mPerMtok", default)]
    pub cache_write_per_mtok: f64,
    #[serde(rename = "cacheReadPerMtok", default)]
    pub cache_read_per_mtok: f64,
}

pub type PricingTable = HashMap<String, ModelPricing>;

pub fn pricing_path() -> PathBuf {
    dirs::home_dir()
        .expect("home dir must exist")
        .join(".claude-code-proxy")
        .join("pricing.json")
}

/// Loads pricing from ~/.claude-code-proxy/pricing.json.
/// Returns empty table if file does not exist.
pub fn load_pricing() -> PricingTable {
    let path = pricing_path();
    let Ok(content) = std::fs::read_to_string(&path) else {
        return HashMap::new();
    };
    serde_json::from_str(&content).unwrap_or_default()
}

/// Calculates cost in USD for a job given token counts and model.
pub fn calculate_cost(
    pricing: &PricingTable,
    model: &str,
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_tokens: u64,
    cache_read_tokens: u64,
) -> Option<f64> {
    // Try exact match, then prefix match (e.g. "claude-sonnet-4-6[1m]" -> "claude-sonnet-4-6")
    let p = pricing.get(model).or_else(|| {
        pricing.iter().find(|(k, _)| model.starts_with(k.as_str())).map(|(_, v)| v)
    })?;

    let cost = (input_tokens as f64 * p.input_per_mtok
        + output_tokens as f64 * p.output_per_mtok
        + cache_creation_tokens as f64 * p.cache_write_per_mtok
        + cache_read_tokens as f64 * p.cache_read_per_mtok)
        / 1_000_000.0;
    Some(cost)
}
