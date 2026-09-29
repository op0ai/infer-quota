//! Read-only view of the Keychain item Claude Code itself keeps its OAuth in.
//!
//! On macOS the live session lives in the Keychain (service
//! `Claude Code-credentials`); `~/.claude/.credentials.json` is only a stale
//! fallback there. macOS asks the user to approve the first read from an
//! unrelated binary. Other platforms have no such item, so they answer `None`.

use crate::file::extract_claude_token;
use crate::types::{SecretRecord, SecretsBackend, SecretsError};

/// Keychain service name Claude Code writes its credentials under.
pub const CLAUDE_CODE_SERVICE: &str = "Claude Code-credentials";

/// Set to `1` to skip every Keychain read (CI, sandboxes, or to avoid the prompt).
pub const ENV_NO_KEYCHAIN: &str = "QUOTA_NO_KEYCHAIN";

#[derive(Debug, Default, Clone)]
pub struct ClaudeCodeKeychain;

fn is_claude_path(path: &str) -> bool {
    matches!(
        path.trim(),
        "claude" | "claude/access_token" | "oauth/claude"
    )
}

/// True when the operator asked us not to touch the Keychain.
pub fn keychain_disabled() -> bool {
    std::env::var(ENV_NO_KEYCHAIN).is_ok_and(|v| v.trim() == "1")
}

impl SecretsBackend for ClaudeCodeKeychain {
    fn name(&self) -> &'static str {
        "claude-code-keychain"
    }

    fn get(&self, path: &str) -> Result<Option<SecretRecord>, SecretsError> {
        if !is_claude_path(path) || keychain_disabled() {
            return Ok(None);
        }
        let Some(bytes) = platform::read_item()? else {
            return Ok(None);
        };
        Ok(extract_claude_token(&bytes).map(|value| SecretRecord {
            backend: "claude-code-keychain",
            path: format!("keychain:{CLAUDE_CODE_SERVICE}"),
            value,
        }))
    }

    fn put(&self, _path: &str, _value: &str) -> Result<(), SecretsError> {
        Err(SecretsError::ReadOnly)
    }

    fn delete(&self, _path: &str) -> Result<(), SecretsError> {
        Err(SecretsError::ReadOnly)
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use security_framework::item::{ItemClass, ItemSearchOptions, Limit, SearchResult};

    use super::CLAUDE_CODE_SERVICE;
    use crate::types::SecretsError;

    /// Apple `errSecItemNotFound`.
    const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

    pub fn read_item() -> Result<Option<Vec<u8>>, SecretsError> {
        let found = ItemSearchOptions::new()
            .class(ItemClass::generic_password())
            .service(CLAUDE_CODE_SERVICE)
            .load_data(true)
            .limit(Limit::Max(1))
            .search();
        match found {
            Ok(results) => Ok(results.into_iter().find_map(|result| match result {
                SearchResult::Data(bytes) => Some(bytes),
                _ => None,
            })),
            Err(err) if err.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(None),
            Err(err) => Err(SecretsError::Unavailable(format!(
                "keychain error {}",
                err.code()
            ))),
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use crate::types::SecretsError;

    pub fn read_item() -> Result<Option<Vec<u8>>, SecretsError> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_claude_logical_paths_are_served() {
        assert!(is_claude_path("claude"));
        assert!(is_claude_path(" oauth/claude "));
        assert!(!is_claude_path("codex"));
        assert!(!is_claude_path("cursor/session"));
    }

    #[test]
    fn backend_is_read_only() {
        let k = ClaudeCodeKeychain;
        assert_eq!(k.put("claude", "x"), Err(SecretsError::ReadOnly));
        assert_eq!(k.delete("claude"), Err(SecretsError::ReadOnly));
    }

    #[test]
    fn other_paths_never_reach_the_keychain() {
        let k = ClaudeCodeKeychain;
        assert_eq!(k.get("codex"), Ok(None));
        assert_eq!(k.get("cursor/session"), Ok(None));
    }
}
