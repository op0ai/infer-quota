//! Read-only credential extraction. Files are size-capped. Only access-token
//! and account-id fields are retained; refresh tokens are dropped immediately
//! after JSON parse of the tiny auth file.

use std::path::{Path, PathBuf};

use quota_core::fsutil::{read_file_capped, CapReadError};
use serde::Deserialize;
use thiserror::Error;

const MAX_CRED_BYTES: usize = 64 * 1024;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CredsError {
    #[error("credential file not found: {0}")]
    NotFound(String),
    #[error("credential file too large (>{MAX_CRED_BYTES} bytes)")]
    TooLarge,
    #[error("failed to read credentials: {0}")]
    Io(String),
    #[error("failed to parse credentials: {0}")]
    Parse(String),
    #[error("no access token in {0}")]
    NoToken(String),
    #[error("refusing symlink credential file: {0}")]
    Symlink(String),
}

#[derive(Debug, Clone)]
pub struct CodexCreds {
    pub access_token: String,
    pub account_id: Option<String>,
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ClaudeCreds {
    pub access_token: String,
    pub path: PathBuf,
    pub expires_at: Option<i64>,
}

#[derive(Deserialize)]
struct CodexAuthFile {
    #[serde(default)]
    tokens: Option<CodexTokens>,
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    chatgpt_account_id: Option<String>,
}

#[derive(Deserialize)]
struct CodexTokens {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
    #[serde(default)]
    chatgpt_account_id: Option<String>,
}

#[derive(Deserialize)]
struct ClaudeCredFile {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: Option<ClaudeOauth>,
}

#[derive(Deserialize)]
struct ClaudeOauth {
    #[serde(rename = "accessToken")]
    access_token: Option<String>,
    #[serde(rename = "expiresAt")]
    expires_at: Option<serde_json::Value>,
}

fn read_capped(path: &Path) -> Result<Vec<u8>, CredsError> {
    match read_file_capped(path, MAX_CRED_BYTES) {
        Ok(b) => Ok(b),
        Err(CapReadError::NotFound(p)) => Err(CredsError::NotFound(p)),
        Err(CapReadError::TooLarge(_)) => Err(CredsError::TooLarge),
        Err(CapReadError::Symlink(p)) => Err(CredsError::Symlink(p)),
        Err(CapReadError::Io(e)) => Err(CredsError::Io(e)),
    }
}

/// Locations consulted for Codex OAuth, in order, when `CODEX_HOME` is unset.
/// When `CODEX_HOME` is set, only that home's `auth.json` is used (CodexBar
/// isolation rule).
pub fn codex_auth_candidates(explicit_home: Option<&Path>) -> Vec<PathBuf> {
    if let Some(home) = explicit_home {
        return vec![home.join("auth.json")];
    }
    if let Ok(dir) = std::env::var("CODEX_HOME") {
        let dir = dir.trim();
        if !dir.is_empty() {
            return vec![PathBuf::from(dir).join("auth.json")];
        }
    }
    let Some(home) = quota_core::home_dir() else {
        return Vec::new();
    };
    vec![
        home.join(".codex/auth.json"),
        home.join(".config/codex/auth.json"),
    ]
}

pub fn load_codex_creds(explicit_home: Option<&Path>) -> Result<CodexCreds, CredsError> {
    let candidates = codex_auth_candidates(explicit_home);
    let primary = candidates
        .first()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "auth.json".into());
    for path in &candidates {
        match read_capped(path) {
            Ok(bytes) => return parse_codex_auth(path, &bytes),
            Err(CredsError::NotFound(_)) => {}
            Err(e) => return Err(e),
        }
    }
    Err(CredsError::NotFound(primary))
}

pub fn parse_codex_auth(path: &Path, bytes: &[u8]) -> Result<CodexCreds, CredsError> {
    let file: CodexAuthFile =
        serde_json::from_slice(bytes).map_err(|e| CredsError::Parse(e.to_string()))?;
    let token = file
        .tokens
        .as_ref()
        .and_then(|t| t.access_token.clone())
        .or(file.access_token)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| CredsError::NoToken(path.display().to_string()))?;
    let account_id = file
        .tokens
        .as_ref()
        .and_then(|t| t.account_id.clone().or(t.chatgpt_account_id.clone()))
        .or(file.chatgpt_account_id)
        .filter(|s| !s.is_empty());
    Ok(CodexCreds {
        access_token: token,
        account_id,
        path: path.to_path_buf(),
    })
}

pub fn claude_cred_candidates(explicit_config_dir: Option<&Path>) -> Vec<PathBuf> {
    if let Some(dir) = explicit_config_dir {
        return vec![dir.join(".credentials.json")];
    }
    quota_core::claude_config_dirs()
        .into_iter()
        .map(|d| d.join(".credentials.json"))
        .collect()
}

pub fn load_claude_creds(explicit_config_dir: Option<&Path>) -> Result<ClaudeCreds, CredsError> {
    let candidates = claude_cred_candidates(explicit_config_dir);
    let primary = candidates
        .first()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| ".credentials.json".into());
    for path in &candidates {
        match read_capped(path) {
            Ok(bytes) => return parse_claude_creds(path, &bytes),
            Err(CredsError::NotFound(_)) => {}
            Err(e) => return Err(e),
        }
    }
    Err(CredsError::NotFound(primary))
}

pub fn parse_claude_creds(path: &Path, bytes: &[u8]) -> Result<ClaudeCreds, CredsError> {
    let file: ClaudeCredFile =
        serde_json::from_slice(bytes).map_err(|e| CredsError::Parse(e.to_string()))?;
    let oauth = file.claude_ai_oauth.ok_or_else(|| {
        CredsError::NoToken(format!(
            "{} (no claudeAiOauth block; API-key mode cannot use the OAuth usage endpoint)",
            path.display()
        ))
    })?;
    let token = oauth
        .access_token
        .filter(|s| !s.is_empty())
        .ok_or_else(|| CredsError::NoToken(path.display().to_string()))?;
    let expires_at = oauth
        .expires_at
        .as_ref()
        .and_then(quota_core::timeutil::parse_reset_at);
    Ok(ClaudeCreds {
        access_token: token,
        path: path.to_path_buf(),
        expires_at,
    })
}

/// Best-effort `chatgpt_base_url` from `config.toml` without a TOML crate.
pub fn parse_chatgpt_base_url(toml_text: &str) -> Option<String> {
    for line in toml_text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(rest) = line.strip_prefix("chatgpt_base_url") else {
            continue;
        };
        let rest = rest.trim().trim_start_matches('=').trim();
        let unquoted = rest.trim_matches('"').trim_matches('\'').trim().to_string();
        if unquoted.is_empty() {
            return None;
        }
        return Some(unquoted);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_nested_tokens() {
        let json = br#"{
            "tokens": {"access_token":"tok-a","account_id":"acct-1","refresh_token":"IGNORE"},
            "last_refresh": "x"
        }"#;
        let c = parse_codex_auth(Path::new("/tmp/auth.json"), json).unwrap();
        assert_eq!(c.access_token, "tok-a");
        assert_eq!(c.account_id.as_deref(), Some("acct-1"));
    }

    #[test]
    fn codex_top_level_token() {
        let json = br#"{"access_token":"tok-b","chatgpt_account_id":"acct-2"}"#;
        let c = parse_codex_auth(Path::new("auth.json"), json).unwrap();
        assert_eq!(c.access_token, "tok-b");
        assert_eq!(c.account_id.as_deref(), Some("acct-2"));
    }

    #[test]
    fn claude_oauth_block() {
        let json = br#"{
            "claudeAiOauth": {
                "accessToken":"sk-ant-oat01-x",
                "refreshToken":"IGNORE",
                "expiresAt": 1759700000000
            }
        }"#;
        let c = parse_claude_creds(Path::new("creds.json"), json).unwrap();
        assert_eq!(c.access_token, "sk-ant-oat01-x");
        assert_eq!(c.expires_at, Some(1_759_700_000));
    }

    #[test]
    fn claude_api_key_mode() {
        let json = br#"{"someOther":true}"#;
        let err = parse_claude_creds(Path::new("creds.json"), json).unwrap_err();
        assert!(matches!(err, CredsError::NoToken(_)));
    }

    #[test]
    fn chatgpt_base_url_line() {
        let t = "# comment\nchatgpt_base_url = \"https://example.com/backend-api\"\n";
        assert_eq!(
            parse_chatgpt_base_url(t).as_deref(),
            Some("https://example.com/backend-api")
        );
    }

    #[test]
    fn read_capped_refuses_symlink() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("quota-creds-sym-{stamp}"));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("auth.json");
        let link = dir.join("link.json");
        std::fs::write(&target, br#"{"access_token":"tok"}"#).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let err = read_capped(&link).unwrap_err();
        assert!(matches!(err, CredsError::Symlink(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
