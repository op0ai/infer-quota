//! Optional JSON config (`~/.config/quota/config.json`). Missing file = defaults.
//! Unknown keys are ignored for forward compatibility.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::fsutil::{read_file_capped, CapReadError};
use crate::paths::{
    default_codexbar_dir, default_config_path, default_socket_path, default_state_dir,
};

/// Default in-memory snapshot ring size. 128 * ~1 KiB snapshots is a few hundred KiB.
pub const DEFAULT_RING_CAPACITY: usize = 128;
pub const DEFAULT_REFRESH_MIN_SECS: u64 = 30;
pub const DEFAULT_REFRESH_MAX_SECS: u64 = 300;
pub const DEFAULT_HTTP_TIMEOUT_SECS: u64 = 10;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("cannot read config: {0}")]
    Read(String),
    #[error("malformed config JSON: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("invalid config: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub socket: Option<PathBuf>,
    /// Append-only JSONL of snapshots. Off by default (privacy + disk).
    #[serde(default)]
    pub history: bool,
    #[serde(default)]
    pub history_path: Option<PathBuf>,
    #[serde(default = "default_ring")]
    pub ring_capacity: usize,
    #[serde(default = "default_refresh_min")]
    pub refresh_min_secs: u64,
    #[serde(default = "default_refresh_max")]
    pub refresh_max_secs: u64,
    #[serde(default = "default_http_timeout")]
    pub http_timeout_secs: u64,
    /// Probe Codex when true (default).
    #[serde(default = "default_true")]
    pub enable_codex: bool,
    /// Probe Claude when true (default).
    #[serde(default = "default_true")]
    pub enable_claude: bool,
    /// Override path for the account-metadata book (no secrets).
    #[serde(default)]
    pub accounts_path: Option<PathBuf>,
    /// Read CodexBar on-disk snapshots/history when present (macOS path).
    #[serde(default = "default_true")]
    pub enable_codexbar_files: bool,
    /// Override CodexBar support dir (`QUOTA_CODEXBAR_DIR` also works).
    #[serde(default)]
    pub codexbar_dir: Option<PathBuf>,
}

fn default_ring() -> usize {
    DEFAULT_RING_CAPACITY
}
fn default_refresh_min() -> u64 {
    DEFAULT_REFRESH_MIN_SECS
}
fn default_refresh_max() -> u64 {
    DEFAULT_REFRESH_MAX_SECS
}
fn default_http_timeout() -> u64 {
    DEFAULT_HTTP_TIMEOUT_SECS
}
fn default_true() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            socket: None,
            history: false,
            history_path: None,
            ring_capacity: DEFAULT_RING_CAPACITY,
            refresh_min_secs: DEFAULT_REFRESH_MIN_SECS,
            refresh_max_secs: DEFAULT_REFRESH_MAX_SECS,
            http_timeout_secs: DEFAULT_HTTP_TIMEOUT_SECS,
            enable_codex: true,
            enable_claude: true,
            accounts_path: None,
            enable_codexbar_files: true,
            codexbar_dir: None,
        }
    }
}

impl Config {
    pub fn load_default() -> Result<Self, ConfigError> {
        Self::load_path(&default_config_path())
    }

    pub fn load_path(path: &Path) -> Result<Self, ConfigError> {
        let bytes = match read_file_capped(path, 64 * 1024) {
            Ok(bytes) => bytes,
            Err(CapReadError::NotFound(_)) => return Ok(Self::default()),
            Err(e) => return Err(ConfigError::Read(e.to_string())),
        };
        let config: Self = serde_json::from_slice(&bytes)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if !(8..=4096).contains(&self.ring_capacity) {
            return Err(ConfigError::Invalid(
                "ring_capacity must be between 8 and 4096".into(),
            ));
        }
        if !(5..=3600).contains(&self.refresh_min_secs) {
            return Err(ConfigError::Invalid(
                "refresh_min_secs must be between 5 and 3600".into(),
            ));
        }
        if self.refresh_max_secs < self.refresh_min_secs || self.refresh_max_secs > 86_400 {
            return Err(ConfigError::Invalid(
                "refresh_max_secs must be at least refresh_min_secs and at most 86400".into(),
            ));
        }
        if !(1..=120).contains(&self.http_timeout_secs) {
            return Err(ConfigError::Invalid(
                "http_timeout_secs must be between 1 and 120".into(),
            ));
        }
        Ok(())
    }

    pub fn socket_path(&self) -> PathBuf {
        if let Some(p) = &self.socket {
            return p.clone();
        }
        if let Ok(p) = std::env::var("QUOTA_SOCKET") {
            let p = p.trim();
            if !p.is_empty() {
                return PathBuf::from(p);
            }
        }
        default_socket_path()
    }

    pub fn history_file(&self) -> Option<PathBuf> {
        if !self.history {
            return None;
        }
        Some(
            self.history_path
                .clone()
                .unwrap_or_else(|| default_state_dir().join("history.jsonl")),
        )
    }

    pub fn ring_capacity(&self) -> usize {
        self.ring_capacity.clamp(8, 4096)
    }

    pub fn refresh_min_secs(&self) -> u64 {
        self.refresh_min_secs.clamp(5, 3600)
    }

    pub fn refresh_max_secs(&self) -> u64 {
        self.refresh_max_secs
            .clamp(self.refresh_min_secs(), 24 * 3600)
    }

    pub fn accounts_file(&self) -> PathBuf {
        self.accounts_path
            .clone()
            .unwrap_or_else(|| default_state_dir().join("accounts.json"))
    }

    pub fn codexbar_dir(&self) -> PathBuf {
        if let Some(p) = &self.codexbar_dir {
            return p.clone();
        }
        default_codexbar_dir()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_default() {
        let c = Config::load_path(Path::new("/no/such/quota-config-xyz.json")).unwrap();
        assert_eq!(c, Config::default());
    }

    #[test]
    fn parses_subset() {
        let c: Config = serde_json::from_str(r#"{"history":true,"ring_capacity":32}"#).unwrap();
        assert!(c.history);
        assert_eq!(c.ring_capacity, 32);
        assert!(c.enable_codex);
    }

    #[test]
    fn malformed_config_is_an_error_instead_of_defaults() {
        let path = std::env::temp_dir().join(format!(
            "quota-invalid-config-{}-{}.json",
            std::process::id(),
            crate::timeutil::now_unix()
        ));
        std::fs::write(&path, b"{ malformed").unwrap();
        let error = Config::load_path(&path).unwrap_err();
        let _ = std::fs::remove_file(path);
        assert!(error.to_string().contains("malformed config JSON"));
    }

    #[test]
    fn invalid_config_values_are_rejected() {
        let error = serde_json::from_str::<Config>(r#"{"http_timeout_secs":0}"#).unwrap();
        assert!(error.validate().is_err());
    }

    #[test]
    fn socket_path_prefers_config_over_env() {
        let with_cfg: Config =
            serde_json::from_str(r#"{"socket":"/tmp/from-config.sock"}"#).unwrap();
        assert_eq!(
            with_cfg.socket_path(),
            PathBuf::from("/tmp/from-config.sock")
        );

        let prev = std::env::var("QUOTA_SOCKET").ok();
        std::env::set_var("QUOTA_SOCKET", "/tmp/from-env.sock");
        assert_eq!(
            Config::default().socket_path(),
            PathBuf::from("/tmp/from-env.sock")
        );
        assert_eq!(
            with_cfg.socket_path(),
            PathBuf::from("/tmp/from-config.sock"),
            "config.json socket must beat QUOTA_SOCKET"
        );
        match prev {
            Some(v) => std::env::set_var("QUOTA_SOCKET", v),
            None => std::env::remove_var("QUOTA_SOCKET"),
        }
    }
}
