//! Read-only last-resort backend: session files the official CLIs already wrote.
//!
//! We never write `~/.codex/auth.json` or `~/.claude/.credentials.json`.

use std::fs;
use std::path::{Path, PathBuf};

use crate::types::{SecretRecord, SecretsBackend, SecretsError};

const MAX_CRED_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Default)]
pub struct FileOauthBackend {
    pub codex_home: Option<PathBuf>,
}

impl FileOauthBackend {
    pub fn with_codex_home(path: PathBuf) -> Self {
        Self {
            codex_home: Some(path),
        }
    }
}

fn read_capped(path: &Path) -> Result<Vec<u8>, SecretsError> {
    let meta = fs::metadata(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            SecretsError::NotFound(path.display().to_string())
        } else {
            SecretsError::Io(e.to_string())
        }
    })?;
    if meta.len() > MAX_CRED_BYTES {
        return Err(SecretsError::Io("credential file too large".into()));
    }
    fs::read(path).map_err(|e| SecretsError::Io(e.to_string()))
}

fn codex_candidates(explicit: Option<&Path>) -> Vec<PathBuf> {
    if let Some(home) = explicit {
        return vec![home.join("auth.json")];
    }
    if let Ok(dir) = std::env::var("CODEX_HOME") {
        let dir = dir.trim();
        if !dir.is_empty() {
            return vec![PathBuf::from(dir).join("auth.json")];
        }
    }
    let home = quota_core::home_dir();
    vec![
        home.join(".codex/auth.json"),
        home.join(".config/codex/auth.json"),
    ]
}

fn claude_candidates() -> Vec<PathBuf> {
    quota_core::claude_config_dirs()
        .into_iter()
        .map(|d| d.join(".credentials.json"))
        .collect()
}

fn extract_codex_token(bytes: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    v.pointer("/tokens/access_token")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            v.get("access_token")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
                .filter(|s| !s.is_empty())
        })
}

type TokenExtract = fn(&[u8]) -> Option<String>;

fn extract_claude_token(bytes: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    v.pointer("/claudeAiOauth/accessToken")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
}

impl SecretsBackend for FileOauthBackend {
    fn name(&self) -> &'static str {
        "file"
    }

    fn get(&self, path: &str) -> Result<Option<SecretRecord>, SecretsError> {
        let key = path.trim().trim_start_matches("file:");
        let (candidates, extract): (Vec<PathBuf>, TokenExtract) = match key {
            "codex" | "codex/access_token" | "oauth/codex" => (
                codex_candidates(self.codex_home.as_deref()),
                extract_codex_token,
            ),
            "claude" | "claude/access_token" | "oauth/claude" => {
                (claude_candidates(), extract_claude_token)
            }
            other => {
                return Err(SecretsError::NotFound(format!(
                    "unknown file secret path {other} (try file:codex or file:claude)"
                )));
            }
        };
        for p in candidates {
            match read_capped(&p) {
                Ok(bytes) => {
                    if let Some(value) = extract(&bytes) {
                        return Ok(Some(SecretRecord {
                            backend: "file",
                            path: p.display().to_string(),
                            value,
                        }));
                    }
                    return Ok(None);
                }
                Err(SecretsError::NotFound(_)) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(None)
    }

    fn put(&self, _path: &str, _value: &str) -> Result<(), SecretsError> {
        Err(SecretsError::ReadOnly)
    }

    fn delete(&self, _path: &str) -> Result<(), SecretsError> {
        Err(SecretsError::ReadOnly)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn reads_codex_auth_json_and_refuses_write() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("quota-secrets-file-{stamp}"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("auth.json"),
            r#"{"tokens":{"access_token":"tok-file","refresh_token":"IGNORE"}}"#,
        )
        .unwrap();
        let backend = FileOauthBackend::with_codex_home(dir.clone());
        let rec = backend.get("codex").unwrap().unwrap();
        assert_eq!(rec.value, "tok-file");
        assert!(backend.put("codex", "nope").is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
