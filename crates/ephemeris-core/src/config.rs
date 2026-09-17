use crate::AppError;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Application configuration loaded from TOML files and environment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_theme")]
    pub theme: String,

    #[serde(default = "default_sync_interval")]
    pub sync_interval_secs: u64,

    #[serde(default = "default_logging_level")]
    pub logging_level: String,

    #[serde(default)]
    pub custom: HashMap<String, String>,
}

fn default_theme() -> String {
    "light".to_string()
}

fn default_sync_interval() -> u64 {
    60
}

fn default_logging_level() -> String {
    "info".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Config {
            theme: default_theme(),
            sync_interval_secs: default_sync_interval(),
            logging_level: default_logging_level(),
            custom: HashMap::new(),
        }
    }
}

impl Config {
    /// Load configuration from TOML file + environment overrides.
    pub fn load() -> crate::Result<Self> {
        let mut config = Config::default();

        // Load from ~/.config/ephemeris/ephemeris.toml if exists
        if let Ok(content) = std::fs::read_to_string(config_file_path()?) {
            let file_config: Config = toml::from_str(&content)
                .map_err(|e| AppError::Config(format!("Failed to parse config: {}", e)))?;
            config = file_config;
        }

        // Override from environment
        if let Ok(theme) = std::env::var("EPHEMERIS_THEME") {
            config.theme = theme;
        }
        if let Ok(interval) = std::env::var("EPHEMERIS_SYNC_INTERVAL") {
            if let Ok(secs) = interval.parse() {
                config.sync_interval_secs = secs;
            }
        }
        if let Ok(level) = std::env::var("EPHEMERIS_LOG_LEVEL") {
            config.logging_level = level;
        }

        Ok(config)
    }

    /// Save configuration to ~/.config/ephemeris/ephemeris.toml.
    pub fn save(&self) -> crate::Result<()> {
        let path = config_file_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(AppError::Io)?;
        }
        let content = toml::to_string_pretty(self)
            .map_err(|e| AppError::Config(format!("Failed to serialize config: {}", e)))?;
        std::fs::write(path, content)
            .map_err(AppError::Io)?;
        Ok(())
    }
}

/// Get the XDG config file path for Ephemeris.
fn config_file_path() -> crate::Result<PathBuf> {
    let xdg_config = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".config"))
        })
        .ok_or_else(|| AppError::Config("No config directory found".to_string()))?;

    Ok(xdg_config.join("ephemeris").join("ephemeris.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults() {
        let cfg = Config::default();
        assert_eq!(cfg.theme, "light");
        assert_eq!(cfg.sync_interval_secs, 60);
        assert_eq!(cfg.logging_level, "info");
    }

    #[test]
    fn config_serialize() {
        let cfg = Config::default();
        let toml_str = toml::to_string(&cfg).expect("should serialize");
        assert!(toml_str.contains("theme"));
        assert!(toml_str.contains("sync_interval_secs"));
    }

    #[test]
    fn config_deserialize() {
        let toml_str = r#"
            theme = "dark"
            sync_interval_secs = 120
            logging_level = "debug"
        "#;
        let cfg: Config = toml::from_str(toml_str).expect("should deserialize");
        assert_eq!(cfg.theme, "dark");
        assert_eq!(cfg.sync_interval_secs, 120);
        assert_eq!(cfg.logging_level, "debug");
    }

    #[test]
    fn config_custom_fields() {
        let mut cfg = Config::default();
        cfg.custom.insert("user_name".to_string(), "Alice".to_string());
        let toml_str = toml::to_string(&cfg).expect("should serialize");
        let cfg2: Config = toml::from_str(&toml_str).expect("should deserialize");
        assert_eq!(cfg2.custom.get("user_name"), Some(&"Alice".to_string()));
    }
}
