//! Application configuration — layered TOML + XDG + environment overrides.
//!
//! ## Precedence (highest wins)
//!
//! 1. **Environment variables** (`EPHEMERIS_*`) — override everything.
//! 2. **TOML file** — overrides compiled-in defaults; missing keys fall back to
//!    the default value, so a partial config file is valid.
//! 3. **Compiled-in defaults** ([`Config::default()`]) — always present as the
//!    lowest layer.
//!
//! ## XDG path
//!
//! The default config file lives at
//! `$XDG_CONFIG_HOME/ephemeris/ephemeris.toml` (or
//! `~/.config/ephemeris/ephemeris.toml` when `XDG_CONFIG_HOME` is unset).
//!
//! ## Environment variables
//!
//! | Variable                   | Config field          |
//! |----------------------------|-----------------------|
//! | `EPHEMERIS_THEME`          | `theme`               |
//! | `EPHEMERIS_SYNC_INTERVAL`  | `sync_interval_secs`  |
//! | `EPHEMERIS_LOG_LEVEL`      | `logging_level`       |

use crate::AppError;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

// ── Data type ────────────────────────────────────────────────────────────────

/// Application configuration.
///
/// All fields carry serde defaults so that a partial TOML file is valid — only
/// the keys that are present in the file override the compiled-in defaults.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// UI colour theme (`"light"` or `"dark"`).
    #[serde(default = "default_theme")]
    pub theme: String,

    /// Background sync interval in seconds.
    #[serde(default = "default_sync_interval")]
    pub sync_interval_secs: u64,

    /// Log level filter (`"error"`, `"warn"`, `"info"`, `"debug"`, `"trace"`).
    #[serde(default = "default_logging_level")]
    pub logging_level: String,

    /// Arbitrary key-value overrides for experimental / future settings.
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

// ── Load / save ──────────────────────────────────────────────────────────────

impl Config {
    /// Load configuration using the real XDG config path.
    ///
    /// Equivalent to `Config::load_from(xdg_config_path())`, which means it
    /// reads `$XDG_CONFIG_HOME/ephemeris/ephemeris.toml` (or
    /// `~/.config/ephemeris/ephemeris.toml`) when present, then applies any
    /// `EPHEMERIS_*` environment overrides.
    ///
    /// If the config file does not exist the call still succeeds — compiled-in
    /// defaults (plus any env overrides) are returned.
    pub fn load() -> crate::Result<Self> {
        let path = xdg_config_path()?;
        Self::load_from(&path)
    }

    /// Load configuration from an explicit TOML file path.
    ///
    /// The layering is: **defaults** → **file** (if it exists) → **env vars**.
    /// Useful in tests where you do not want to touch the real home directory.
    pub fn load_from(path: &Path) -> crate::Result<Self> {
        // Layer 1: compiled-in defaults.
        let mut config = Config::default();

        // Layer 2: TOML file (only if present; absence is not an error).
        if path.exists() {
            let content = std::fs::read_to_string(path).map_err(AppError::Io)?;
            let file_config: Config = toml::from_str(&content)
                .map_err(|e| AppError::Config(format!("Failed to parse config: {e}")))?;
            config = file_config;
        }

        // Layer 3: environment variables (individual overrides).
        apply_env_overrides(&mut config);

        Ok(config)
    }

    /// Save this configuration to the real XDG config path.
    ///
    /// Parent directories are created if they do not already exist.
    pub fn save(&self) -> crate::Result<()> {
        let path = xdg_config_path()?;
        self.save_to(&path)
    }

    /// Save this configuration to an explicit path.
    ///
    /// Parent directories are created if they do not already exist.  Useful in
    /// tests where you want to exercise the round-trip without touching the real
    /// home directory.
    pub fn save_to(&self, path: &Path) -> crate::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(AppError::Io)?;
        }
        let content = toml::to_string_pretty(self)
            .map_err(|e| AppError::Config(format!("Failed to serialize config: {e}")))?;
        std::fs::write(path, content).map_err(AppError::Io)?;
        Ok(())
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Resolve the XDG config file path.
///
/// Prefers `$XDG_CONFIG_HOME`; falls back to `$HOME/.config` as per the XDG
/// Base Directory Specification.  Returns an error only when neither variable
/// is set (extremely unusual on any real UNIX system).
fn xdg_config_path() -> crate::Result<PathBuf> {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".config"))
        })
        .ok_or_else(|| {
            AppError::Config(
                "Cannot determine config directory: neither XDG_CONFIG_HOME nor HOME is set"
                    .to_string(),
            )
        })?;

    Ok(base.join("ephemeris").join("ephemeris.toml"))
}

/// Apply `EPHEMERIS_*` environment variable overrides to an already-loaded
/// [`Config`].  Only variables that are actually set take effect.
fn apply_env_overrides(config: &mut Config) {
    if let Ok(v) = std::env::var("EPHEMERIS_THEME") {
        config.theme = v;
    }
    if let Ok(v) = std::env::var("EPHEMERIS_SYNC_INTERVAL") {
        match v.parse::<u64>() {
            Ok(secs) => config.sync_interval_secs = secs,
            Err(_) => {
                // Log but don't fail — the rest of the config is still valid.
                tracing::warn!(
                    "EPHEMERIS_SYNC_INTERVAL={:?} is not a valid u64; ignoring",
                    v
                );
            }
        }
    }
    if let Ok(v) = std::env::var("EPHEMERIS_LOG_LEVEL") {
        config.logging_level = v;
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use uuid::Uuid;

    // ── helpers ──────────────────────────────────────────────────────────────

    /// Create a unique temp directory path under the system temp dir.
    ///
    /// The directory is created eagerly so callers can write files into it
    /// immediately.  Callers are responsible for deleting it if needed; since
    /// these are test temps we let the OS clean them up.
    fn tmp_dir() -> PathBuf {
        let path = env::temp_dir().join(format!("ephemeris-cfg-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    /// Write `content` to a new temp file; return its path (and the owning dir
    /// path so the caller can keep it alive via a binding).
    fn write_temp_toml(content: &str) -> (PathBuf, PathBuf) {
        let dir = tmp_dir();
        let path = dir.join("ephemeris.toml");
        std::fs::write(&path, content).expect("write temp toml");
        (path, dir)
    }

    // ── default values ───────────────────────────────────────────────────────

    #[test]
    fn config_defaults() {
        let cfg = Config::default();
        assert_eq!(cfg.theme, "light");
        assert_eq!(cfg.sync_interval_secs, 60);
        assert_eq!(cfg.logging_level, "info");
        assert!(cfg.custom.is_empty());
    }

    // ── TOML file overrides defaults ──────────────────────────────────────────
    //
    // These tests also hold ENV_LOCK to prevent concurrent env-mutating tests
    // from accidentally polluting the env vars read by Config::load_from.

    #[test]
    fn file_overrides_defaults() {
        let _guard = ENV_LOCK.lock().unwrap();
        let (path, _dir) = write_temp_toml(
            r#"
            theme = "dark"
            sync_interval_secs = 300
            logging_level = "debug"
            "#,
        );
        let cfg = Config::load_from(&path).expect("load");
        assert_eq!(cfg.theme, "dark");
        assert_eq!(cfg.sync_interval_secs, 300);
        assert_eq!(cfg.logging_level, "debug");
    }

    #[test]
    fn partial_file_keeps_defaults_for_missing_keys() {
        let _guard = ENV_LOCK.lock().unwrap();
        // Only theme is set in the file; the other fields must fall back to
        // the compiled-in defaults.
        let (path, _dir) = write_temp_toml(r#"theme = "dark""#);
        let cfg = Config::load_from(&path).expect("load");
        assert_eq!(cfg.theme, "dark");
        assert_eq!(cfg.sync_interval_secs, 60, "default should be kept");
        assert_eq!(cfg.logging_level, "info", "default should be kept");
    }

    #[test]
    fn missing_file_returns_defaults() {
        let _guard = ENV_LOCK.lock().unwrap();
        // A non-existent path must not cause an error.
        let cfg = Config::load_from(Path::new("/does/not/exist/ephemeris.toml"))
            .expect("missing file should succeed with defaults");
        assert_eq!(cfg, Config::default());
    }

    // ── env overrides file (and defaults) ─────────────────────────────────────

    /// Run a closure with a set of env vars, then restore the originals.
    fn with_env<F: FnOnce()>(vars: &[(&str, &str)], f: F) {
        let saved: Vec<(String, Option<String>)> = vars
            .iter()
            .map(|(k, _)| (k.to_string(), env::var(k).ok()))
            .collect();

        for (k, v) in vars {
            // SAFETY: tests that mutate env vars must not run in parallel.
            // Cargo runs unit tests in a single binary; the ENV_LOCK mutex
            // serialises all env-mutating tests so there are no data races.
            #[allow(unsafe_code)]
            unsafe {
                env::set_var(k, v)
            };
        }

        f();

        for (k, maybe_v) in saved {
            #[allow(unsafe_code)]
            match maybe_v {
                Some(v) => unsafe { env::set_var(&k, &v) },
                None => unsafe { env::remove_var(&k) },
            }
        }
    }

    // Mutex ensures the env-mutating tests run one at a time even if Cargo
    // decides to parallelise within this binary.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn env_overrides_defaults() {
        let _guard = ENV_LOCK.lock().unwrap();
        with_env(
            &[
                ("EPHEMERIS_THEME", "dark"),
                ("EPHEMERIS_SYNC_INTERVAL", "120"),
                ("EPHEMERIS_LOG_LEVEL", "trace"),
            ],
            || {
                // Load from a non-existent path so only env takes effect.
                let cfg =
                    Config::load_from(Path::new("/does/not/exist/ephemeris.toml")).expect("load");
                assert_eq!(cfg.theme, "dark");
                assert_eq!(cfg.sync_interval_secs, 120);
                assert_eq!(cfg.logging_level, "trace");
            },
        );
    }

    #[test]
    fn env_overrides_file() {
        let _guard = ENV_LOCK.lock().unwrap();
        let (path, _dir) = write_temp_toml(
            r#"
            theme = "light"
            sync_interval_secs = 60
            logging_level = "info"
            "#,
        );
        with_env(
            &[
                ("EPHEMERIS_THEME", "dark"),
                ("EPHEMERIS_SYNC_INTERVAL", "999"),
                ("EPHEMERIS_LOG_LEVEL", "warn"),
            ],
            || {
                let cfg = Config::load_from(&path).expect("load");
                // env wins over file
                assert_eq!(cfg.theme, "dark");
                assert_eq!(cfg.sync_interval_secs, 999);
                assert_eq!(cfg.logging_level, "warn");
            },
        );
    }

    // ── save / reload round-trip ─────────────────────────────────────────────

    #[test]
    fn save_reload_round_trip() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tmp_dir();
        // Nested sub-path to also test parent-dir creation.
        let path = dir.join("config").join("ephemeris.toml");

        let mut original = Config {
            theme: "dark".to_string(),
            sync_interval_secs: 42,
            logging_level: "debug".to_string(),
            ..Default::default()
        };
        original
            .custom
            .insert("key".to_string(), "value".to_string());

        original.save_to(&path).expect("save");
        assert!(path.exists(), "file must be created by save_to");

        // Reload; the ENV_LOCK guard ensures no env vars are set by a
        // concurrent test that would alter the result.
        let reloaded = Config::load_from(&path).expect("reload");

        assert_eq!(original, reloaded, "round-trip must be lossless");
    }

    #[test]
    fn save_to_creates_parent_dirs() {
        let dir = tmp_dir();
        // Deeply nested path; none of the intermediate dirs exist yet.
        let path = dir.join("a").join("b").join("c").join("cfg.toml");
        Config::default()
            .save_to(&path)
            .expect("save with deep path");
        assert!(path.exists());
    }

    // ── serialization smoke ───────────────────────────────────────────────────

    #[test]
    fn config_serialize_round_trip() {
        let cfg = Config::default();
        let s = toml::to_string(&cfg).expect("serialize");
        assert!(s.contains("theme"));
        assert!(s.contains("sync_interval_secs"));
        let cfg2: Config = toml::from_str(&s).expect("deserialize");
        assert_eq!(cfg, cfg2);
    }

    #[test]
    fn config_custom_fields_round_trip() {
        let mut cfg = Config::default();
        cfg.custom
            .insert("user_name".to_string(), "Alice".to_string());
        let s = toml::to_string(&cfg).expect("serialize");
        let cfg2: Config = toml::from_str(&s).expect("deserialize");
        assert_eq!(cfg2.custom.get("user_name"), Some(&"Alice".to_string()));
    }
}
