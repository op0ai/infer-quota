//! Read-only last-resort backend: session files the official CLIs already wrote.
//!
//! We never write `~/.codex/auth.json` or `~/.claude/.credentials.json`.

use std::path::{Path, PathBuf};

use quota_core::fsutil::{read_file_capped, CapReadError};

use crate::types::{SecretRecord, SecretsBackend, SecretsError};

const MAX_CRED_BYTES: usize = 64 * 1024;

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
    match read_file_capped(path, MAX_CRED_BYTES) {
        Ok(b) => Ok(b),
        Err(CapReadError::NotFound(p)) => Err(SecretsError::NotFound(p)),
        Err(CapReadError::TooLarge(_)) => Err(SecretsError::Io("credential file too large".into())),
        Err(CapReadError::Symlink(p)) => Err(SecretsError::Io(format!("refusing symlink {p}"))),
        Err(CapReadError::NotRegular(p)) => {
            Err(SecretsError::Io(format!("not a regular file: {p}")))
        }
        Err(CapReadError::Io(e)) => Err(SecretsError::Io(e)),
    }
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
    let Some(home) = quota_core::home_dir() else {
        return Vec::new();
    };
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

/// `claudeAiOauth.accessToken` from the JSON Claude Code stores in either its
/// credentials file or its Keychain item.
pub(crate) fn extract_claude_token(bytes: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    v.pointer("/claudeAiOauth/accessToken")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
}

/// Env override for the Cursor session-cookie file.
pub const ENV_CURSOR_COOKIE_FILE: &str = "QUOTA_CURSOR_COOKIE_FILE";

fn cursor_cookie_candidates() -> Vec<PathBuf> {
    if let Ok(path) = std::env::var(ENV_CURSOR_COOKIE_FILE) {
        let path = path.trim();
        if !path.is_empty() {
            return vec![PathBuf::from(path)];
        }
    }
    quota_core::home_dir()
        .map(|home| vec![home.join(".config/quota/cursor-session")])
        .unwrap_or_default()
}

/// The user-written cookie file is a secret: refuse it when group or other
/// can read it, the same bar `ssh` applies to private keys.
#[cfg(unix)]
fn reject_open_permissions(path: &Path) -> Result<(), SecretsError> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)
        .map_err(|e| SecretsError::Io(e.to_string()))?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        return Err(SecretsError::Io(format!(
            "{} is readable by group/other (mode {:o}); chmod 600 it",
            path.display(),
            mode & 0o777
        )));
    }
    Ok(())
}

fn read_cursor_cookie_file() -> Result<Option<SecretRecord>, SecretsError> {
    for path in cursor_cookie_candidates() {
        match read_capped(&path) {
            Ok(bytes) => {
                #[cfg(unix)]
                reject_open_permissions(&path)?;
                let value = String::from_utf8(bytes)
                    .map_err(|_| SecretsError::Parse("cursor cookie file is not utf-8".into()))?;
                let value = value.trim();
                if value.is_empty() {
                    return Ok(None);
                }
                return Ok(Some(SecretRecord {
                    backend: "file",
                    path: path.display().to_string(),
                    value: value.to_string(),
                }));
            }
            Err(SecretsError::NotFound(_)) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(None)
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
            "cursor" | "cursor/session" => return read_cursor_cookie_file(),
            other => {
                return Err(SecretsError::NotFound(format!(
                    "unknown file secret path {other} (try file:codex, file:claude or file:cursor)"
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

    #[cfg(unix)]
    #[test]
    fn cursor_cookie_file_must_be_private_and_is_trimmed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("quota-secrets-cursor-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("cursor-session");
        fs::write(&file, "WorkosCursorSessionToken=abc\n").unwrap();

        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        let open = super::reject_open_permissions(&file).unwrap_err();
        assert!(open.to_string().contains("chmod 600"));
        assert!(!open.to_string().contains("abc"));

        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(super::reject_open_permissions(&file).is_ok());
        let _ = fs::remove_dir_all(&dir);
    }

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
