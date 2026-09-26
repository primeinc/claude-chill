//! TOML configuration file loading and defaults.

use crate::key_parser::{self, KeyCombination};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

const DEFAULT_LOOKBACK_KEY: &str = "[ctrl][6]";
const DEFAULT_AUTO_LOOKBACK_TIMEOUT_MS: u64 = 15000;

/// User configuration loaded from `claude-chill.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub history_lines: usize,
    pub lookback_key: String,
    pub auto_lookback_timeout_ms: u64,
    pub verbose: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            history_lines: 100_000,
            lookback_key: DEFAULT_LOOKBACK_KEY.to_string(),
            auto_lookback_timeout_ms: DEFAULT_AUTO_LOOKBACK_TIMEOUT_MS,
            verbose: false,
        }
    }
}

impl Config {
    /// Load configuration from the platform config directory, falling back to defaults.
    pub fn load() -> Self {
        let config_path = Self::config_path();
        match config_path {
            Some(path) if path.exists() => Self::load_from_file(&path),
            _ => Self::default(),
        }
    }

    /// Return the platform-specific path to the config file.
    pub fn config_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("claude-chill.toml"))
    }

    fn load_from_file(path: &Path) -> Self {
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

    /// Parse the configured lookback key string into a [`KeyCombination`].
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

    #[test]
    fn test_empty_toml_uses_defaults() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.history_lines, 100_000);
        assert_eq!(config.lookback_key, "[ctrl][6]");
        assert_eq!(config.auto_lookback_timeout_ms, 15000);
    }

    #[test]
    fn test_partial_config() {
        // Only one field set, rest should use defaults
        let config: Config = toml::from_str("auto_lookback_timeout_ms = 0").unwrap();
        assert_eq!(config.history_lines, 100_000);
        assert_eq!(config.auto_lookback_timeout_ms, 0);
    }

    #[test]
    fn test_unknown_fields_ignored() {
        // serde(default) without deny_unknown_fields means unknown fields are accepted
        let toml_str = r#"
            history_lines = 50000
            some_future_field = true
        "#;
        let config: Config = toml::from_str(toml_str).expect("unknown fields should be accepted");
        assert_eq!(config.history_lines, 50000);
    }

    #[test]
    fn test_load_from_nonexistent_file() {
        let config = Config::load_from_file(Path::new("/nonexistent/path/config.toml"));
        // Should return defaults when file doesn't exist
        assert_eq!(config.history_lines, 100_000);
    }

    #[test]
    fn test_config_path_returns_some() {
        // config_path should return Some on all platforms
        let path = Config::config_path();
        assert!(path.is_some(), "config_path should return Some");
        let path = path.unwrap();
        let path_str = path.to_string_lossy();
        assert!(
            path_str.contains("claude-chill"),
            "config path should contain 'claude-chill', got: {path_str}"
        );
        // Platform-specific checks
        #[cfg(windows)]
        assert!(
            path_str.contains("AppData") || path_str.contains("Roaming"),
            "Windows config path should be in AppData, got: {path_str}"
        );
        #[cfg(target_os = "macos")]
        assert!(
            path_str.contains("Application Support"),
            "macOS config path should be in Application Support, got: {path_str}"
        );
        #[cfg(target_os = "linux")]
        assert!(
            path_str.contains(".config"),
            "Linux config path should be in .config, got: {path_str}"
        );
    }

    #[test]
    fn test_custom_lookback_key_parses() {
        let config: Config = toml::from_str(r#"lookback_key = "[f12]""#).unwrap();
        let key = config.parse_lookback_key().unwrap();
        assert_eq!(key.to_escape_sequence(), b"\x1b[24~".to_vec());
    }

    #[test]
    fn test_invalid_lookback_key_returns_error() {
        let config: Config = toml::from_str(r#"lookback_key = "invalid""#).unwrap();
        assert!(config.parse_lookback_key().is_err());
    }

    #[test]
    fn test_verbose_default_false() {
        let config = Config::default();
        assert!(!config.verbose);
    }

    #[test]
    fn test_verbose_from_toml() {
        let config: Config = toml::from_str("verbose = true").unwrap();
        assert!(config.verbose);
    }
}
