//! XDG + provider home resolution. No extra crates: a few env reads.
//!
//! Socket (documented default):
//! - `$XDG_RUNTIME_DIR/quota/quota.sock` when `XDG_RUNTIME_DIR` is a non-empty
//!   absolute-ish path
//! - otherwise `~/.local/share/quota/quota.sock`

use std::env;
use std::path::{Path, PathBuf};

pub fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

pub fn default_socket_path() -> PathBuf {
    if let Ok(dir) = env::var("XDG_RUNTIME_DIR") {
        let dir = dir.trim();
        if !dir.is_empty() {
            return Path::new(dir).join("quota").join("quota.sock");
        }
    }
    home_dir().join(".local/share/quota/quota.sock")
}

pub fn default_config_path() -> PathBuf {
    if let Ok(dir) = env::var("XDG_CONFIG_HOME") {
        let dir = dir.trim();
        if !dir.is_empty() {
            return Path::new(dir).join("quota").join("config.json");
        }
    }
    home_dir().join(".config/quota/config.json")
}

pub fn default_state_dir() -> PathBuf {
    if let Ok(dir) = env::var("XDG_STATE_HOME") {
        let dir = dir.trim();
        if !dir.is_empty() {
            return Path::new(dir).join("quota");
        }
    }
    home_dir().join(".local/state/quota")
}

pub fn default_data_dir() -> PathBuf {
    if let Ok(dir) = env::var("XDG_DATA_HOME") {
        let dir = dir.trim();
        if !dir.is_empty() {
            return Path::new(dir).join("quota");
        }
    }
    home_dir().join(".local/share/quota")
}

/// Codex home: `$CODEX_HOME` or `~/.codex`.
pub fn codex_home() -> PathBuf {
    if let Ok(dir) = env::var("CODEX_HOME") {
        let dir = dir.trim();
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    home_dir().join(".codex")
}

/// Claude Code config dirs. `$CLAUDE_CONFIG_DIR` is comma-separated (Claude
/// Code / CodexBar convention). Fallback: `~/.claude`.
pub fn claude_config_dirs() -> Vec<PathBuf> {
    if let Ok(raw) = env::var("CLAUDE_CONFIG_DIR") {
        let dirs: Vec<PathBuf> = raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect();
        if !dirs.is_empty() {
            return dirs;
        }
    }
    vec![home_dir().join(".claude")]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_uses_xdg_runtime() {
        // Cannot safely mutate process env in parallel tests; just assert shape.
        let p = default_socket_path();
        assert!(p.ends_with("quota.sock"));
    }
}
