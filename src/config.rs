use anyhow::Result;
use serde::Deserialize;

use crate::error::ConfigError;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub token: String,
    pub app_id: u64,
}

impl Config {
    /// Reads the config from a given TOML path.
    pub fn from_path(path: &str) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| ConfigError::InvalidPath(path.to_string(), e))?;
        toml::from_str(&content).map_err(|e| ConfigError::InvalidContent(path.to_string(), e))
    }
}
