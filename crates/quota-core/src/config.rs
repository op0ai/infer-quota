//! Optional JSON config (`~/.config/quota/config.json`). Missing file = defaults.
//! Unknown keys are ignored for forward compatibility.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::paths::{default_config_path, default_socket_path, default_state_dir};

/// Default in-memory snapshot ring size. 128 * ~1 KiB snapshots is a few hundred KiB.
pub const DEFAULT_RING_CAPACITY: usize = 128;
pub const DEFAULT_REFRESH_MIN_SECS: u64 = 30;
pub const DEFAULT_REFRESH_MAX_SECS: u64 = 300;
pub const DEFAULT_HTTP_TIMEOUT_SECS: u64 = 10;

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
        }
    }
}

impl Config {
    pub fn load_default() -> Self {
        Self::load_path(&default_config_path())
    }

    pub fn load_path(path: &Path) -> Self {
        let Ok(bytes) = fs::read(path) else {
            return Self::default();
        };
        // Cap: config is tiny. Refuse multi-megabyte junk.
        if bytes.len() > 64 * 1024 {
            return Self::default();
        }
        serde_json::from_slice(&bytes).unwrap_or_default()
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_default() {
        let c = Config::load_path(Path::new("/no/such/quota-config-xyz.json"));
        assert_eq!(c, Config::default());
    }

    #[test]
    fn parses_subset() {
        let c: Config = serde_json::from_str(r#"{"history":true,"ring_capacity":32}"#).unwrap();
        assert!(c.history);
        assert_eq!(c.ring_capacity, 32);
        assert!(c.enable_codex);
    }
}
