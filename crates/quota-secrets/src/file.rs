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
    cursor_secret_path: Option<String>,
    cursor_cookie_file: Option<PathBuf>,
    #[cfg(test)]
    claude_credentials_file: Option<PathBuf>,
}

impl FileOauthBackend {
    pub fn with_codex_home(path: PathBuf) -> Self {
        Self {
            codex_home: Some(path),
            ..Self::default()
        }
    }

    /// Also serve the Cursor cookie file for the configured logical path.
    pub fn with_cursor_secret_path(mut self, path: impl Into<String>) -> Self {
        self.cursor_secret_path = Some(path.into());
        self
    }

    #[cfg(test)]
    fn with_cursor_cookie_file(mut self, path: PathBuf) -> Self {
        self.cursor_cookie_file = Some(path);
        self
    }

    #[cfg(test)]
    fn with_claude_credentials_file(mut self, path: PathBuf) -> Self {
        self.claude_credentials_file = Some(path);
        self
    }

    fn claude_candidate_paths(&self) -> Vec<PathBuf> {
        #[cfg(test)]
        if let Some(path) = &self.claude_credentials_file {
            return vec![path.clone()];
        }
        claude_candidates()
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

fn read_cursor_cookie_file(
    configured_file: Option<&Path>,
) -> Result<Option<SecretRecord>, SecretsError> {
    let candidates =
        configured_file.map_or_else(cursor_cookie_candidates, |path| vec![path.to_path_buf()]);
    for path in candidates {
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
        let configured_cursor_path = self
            .cursor_secret_path
            .as_deref()
            .map(|configured| configured.trim().trim_start_matches("file:"));
        if configured_cursor_path == Some(key) {
            return read_cursor_cookie_file(self.cursor_cookie_file.as_deref());
        }
        let (candidates, extract): (Vec<PathBuf>, TokenExtract) = match key {
            "codex" | "codex/access_token" | "oauth/codex" => (
                codex_candidates(self.codex_home.as_deref()),
                extract_codex_token,
            ),
            "claude" | "claude/access_token" | "oauth/claude" => {
                (self.claude_candidate_paths(), extract_claude_token)
            }
            "cursor" | "cursor/session"
                if configured_cursor_path.is_none_or(|configured| configured == key) =>
            {
                return read_cursor_cookie_file(self.cursor_cookie_file.as_deref());
            }
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

    #[cfg(unix)]
    #[test]
    fn custom_cursor_logical_path_reads_only_the_injected_cookie_file() {
        use std::os::unix::fs::PermissionsExt;

        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("quota-secrets-cursor-custom-{stamp}"));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("session");
        fs::write(&file, "WorkosCursorSessionToken=test-session\n").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();

        let backend = FileOauthBackend::default()
            .with_cursor_secret_path("workspace/custom")
            .with_cursor_cookie_file(file);
        let record = backend.get("workspace/custom").unwrap().unwrap();
        assert_eq!(record.value, "WorkosCursorSessionToken=test-session");
        assert!(matches!(
            backend.get("cursor/session"),
            Err(SecretsError::NotFound(_))
        ));

        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn configured_cursor_path_takes_precedence_over_oauth_aliases() {
        use std::os::unix::fs::PermissionsExt;

        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("quota-secrets-cursor-collision-{stamp}"));
        fs::create_dir_all(&dir).unwrap();
        let cookie = dir.join("session");
        let claude = dir.join("claude.json");
        let codex = dir.join("codex");
        fs::write(&cookie, "WorkosCursorSessionToken=cursor-fixture\n").unwrap();
        fs::set_permissions(&cookie, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(
            &claude,
            r#"{"claudeAiOauth":{"accessToken":"claude-fixture"}}"#,
        )
        .unwrap();
        fs::create_dir_all(&codex).unwrap();
        fs::write(
            codex.join("auth.json"),
            r#"{"access_token":"codex-fixture"}"#,
        )
        .unwrap();

        let claude_path = FileOauthBackend::default()
            .with_cursor_secret_path("claude")
            .with_cursor_cookie_file(cookie.clone())
            .with_claude_credentials_file(claude);
        assert_eq!(
            claude_path.get("claude").unwrap().unwrap().value,
            "WorkosCursorSessionToken=cursor-fixture"
        );

        let codex_path = FileOauthBackend::with_codex_home(codex)
            .with_cursor_secret_path("oauth/codex")
            .with_cursor_cookie_file(cookie)
            .with_claude_credentials_file(dir.join("absent-claude.json"));
        assert_eq!(
            codex_path.get("oauth/codex").unwrap().unwrap().value,
            "WorkosCursorSessionToken=cursor-fixture"
        );
        fs::remove_dir_all(dir).unwrap();
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
