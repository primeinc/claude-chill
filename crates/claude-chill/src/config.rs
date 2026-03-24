use crate::key_parser::{self, KeyCombination};
use serde::Deserialize;
use std::fs;
use std::path::PathBuf;

const DEFAULT_LOOKBACK_KEY: &str = "[ctrl][6]";
const DEFAULT_AUTO_LOOKBACK_TIMEOUT_MS: u64 = 15000;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub history_lines: usize,
    pub lookback_key: String,
    pub auto_lookback_timeout_ms: u64,
    #[allow(dead_code)]
    #[serde(default)]
    refresh_rate: Option<u64>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            history_lines: 100_000,
            lookback_key: DEFAULT_LOOKBACK_KEY.to_string(),
            auto_lookback_timeout_ms: DEFAULT_AUTO_LOOKBACK_TIMEOUT_MS,
            refresh_rate: None,
        }
    }
}

impl Config {
    pub fn load() -> Self {
        let config_path = Self::config_path();
        match config_path {
            Some(path) if path.exists() => Self::load_from_file(&path),
            _ => Self::default(),
        }
    }

    pub fn config_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("claude-chill.toml"))
    }

    fn load_from_file(path: &PathBuf) -> Self {
        match fs::read_to_string(path) {
            Ok(content) => match toml::from_str(&content) {
                Ok(config) => config,
                Err(e) => {
                    eprintln!(
                        "Warning: Failed to parse config file {}: {}",
                        path.display(),
                        e
                    );
                    Self::default()
                }
            },
            Err(e) => {
                eprintln!(
                    "Warning: Failed to read config file {}: {}",
                    path.display(),
                    e
                );
                Self::default()
            }
        }
    }

    pub fn parse_lookback_key(&self) -> Result<KeyCombination, key_parser::ParseKeyError> {
        key_parser::parse(&self.lookback_key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.history_lines, 100_000);
        assert_eq!(config.lookback_key, "[ctrl][6]");
        assert_eq!(config.auto_lookback_timeout_ms, 15000);
    }

    #[test]
    fn test_default_lookback_key_parses() {
        let config = Config::default();
        let key = config.parse_lookback_key().unwrap();
        assert_eq!(key.to_escape_sequence(), vec![0x1E]);
    }

    #[test]
    fn test_backward_compat_refresh_rate() {
        // Old config files may have refresh_rate — should not break parsing
        let toml_str = r#"
            history_lines = 50000
            lookback_key = "[f12]"
            refresh_rate = 20
            auto_lookback_timeout_ms = 30000
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.history_lines, 50000);
        assert_eq!(config.lookback_key, "[f12]");
        assert_eq!(config.auto_lookback_timeout_ms, 30000);
    }

    #[test]
    fn test_config_without_refresh_rate() {
        // New config files without refresh_rate should also work
        let toml_str = r#"
            history_lines = 75000
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.history_lines, 75000);
        assert_eq!(config.lookback_key, "[ctrl][6]");
        assert_eq!(config.auto_lookback_timeout_ms, 15000);
    }
}
