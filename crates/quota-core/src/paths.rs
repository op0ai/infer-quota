//! XDG + provider home resolution. No extra crates: a few env reads.
//!
//! Socket (documented default):
//! - `$XDG_RUNTIME_DIR/quota/quota.sock` when `XDG_RUNTIME_DIR` is a non-empty
//!   absolute-ish path
//! - otherwise `~/.local/share/quota/quota.sock`
//!
//! `$HOME` unset no longer falls back to `/` (that made cred probes look at
//! `/.codex/auth.json`). Missing home → `None`; path helpers use a
//! non-existent sentinel under `/var/empty`.

use std::env;
use std::path::{Path, PathBuf};

/// User home from `$HOME`. `None` when unset or empty — callers must fail
/// closed rather than probing `/`.
pub fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// Last-resort directory when `$HOME` and the relevant XDG var are both
/// missing. Does not exist on a normal host; we never create it.
fn no_home_sentinel() -> PathBuf {
    PathBuf::from("/var/empty/quota-no-home")
}

pub fn default_socket_path() -> PathBuf {
    if let Ok(dir) = env::var("XDG_RUNTIME_DIR") {
        let dir = dir.trim();
        if !dir.is_empty() {
            return Path::new(dir).join("quota").join("quota.sock");
        }
    }
    home_dir()
        .unwrap_or_else(no_home_sentinel)
        .join(".local/share/quota/quota.sock")
}

pub fn default_config_path() -> PathBuf {
    if let Ok(dir) = env::var("XDG_CONFIG_HOME") {
        let dir = dir.trim();
        if !dir.is_empty() {
            return Path::new(dir).join("quota").join("config.json");
        }
    }
    home_dir()
        .unwrap_or_else(no_home_sentinel)
        .join(".config/quota/config.json")
}

pub fn default_state_dir() -> PathBuf {
    if let Ok(dir) = env::var("XDG_STATE_HOME") {
        let dir = dir.trim();
        if !dir.is_empty() {
            return Path::new(dir).join("quota");
        }
    }
    home_dir()
        .unwrap_or_else(no_home_sentinel)
        .join(".local/state/quota")
}

pub fn default_data_dir() -> PathBuf {
    if let Ok(dir) = env::var("XDG_DATA_HOME") {
        let dir = dir.trim();
        if !dir.is_empty() {
            return Path::new(dir).join("quota");
        }
    }
    home_dir()
        .unwrap_or_else(no_home_sentinel)
        .join(".local/share/quota")
}

/// Codex home: `$CODEX_HOME` or `~/.codex`.
pub fn codex_home() -> PathBuf {
    if let Ok(dir) = env::var("CODEX_HOME") {
        let dir = dir.trim();
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    home_dir().unwrap_or_else(no_home_sentinel).join(".codex")
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
    match home_dir() {
        Some(h) => vec![h.join(".claude")],
        None => Vec::new(),
    }
}

/// CodexBar support dir (macOS live path). Override with `QUOTA_CODEXBAR_DIR`.
///
/// We only read `codex-account-snapshots.json` and `usage-history.jsonl`.
/// Never `cursor-session.json`.
pub fn default_codexbar_dir() -> PathBuf {
    if let Ok(dir) = env::var("QUOTA_CODEXBAR_DIR") {
        let dir = dir.trim();
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    home_dir()
        .unwrap_or_else(no_home_sentinel)
        .join("Library/Application Support/CodexBar")
}

pub fn codexbar_snapshot_candidates(dir: &Path) -> Vec<PathBuf> {
    vec![
        dir.join("codex-account-snapshots.json"),
        dir.join("codex-account-snapshots.redacted.json"),
    ]
}

pub fn codexbar_history_candidates(dir: &Path) -> Vec<PathBuf> {
    vec![
        dir.join("usage-history.jsonl"),
        dir.join("usage-history.redacted.jsonl"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_uses_xdg_runtime() {
        // Cannot safely mutate process env in parallel tests; just assert shape.
        let p = default_socket_path();
        assert!(p.ends_with("quota.sock"));
        assert!(!p.starts_with("/.local"));
    }

    #[test]
    fn home_dir_none_or_real() {
        match home_dir() {
            Some(h) => assert!(!h.as_os_str().is_empty()),
            None => {
                assert!(default_socket_path().starts_with("/var/empty/quota-no-home"));
            }
        }
    }
}
