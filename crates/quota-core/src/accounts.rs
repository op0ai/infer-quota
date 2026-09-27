//! Account *metadata* (not secrets). The daemon persists this; `quota-ctl`
//! mutates it over the socket. Tokens and passwords never live here.

use serde::{Deserialize, Serialize};

use crate::types::ProviderId;

/// Pointer to material in a secrets backend. The bytes stay in OpenBao,
/// the OS keychain, or a local CLI file — never in this record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretRef {
    /// `"openbao"`, `"keychain"`, or `"file"`.
    pub backend: String,
    /// Backend-specific path or key (e.g. `quota/codex/work`).
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountRecord {
    pub id: String,
    pub provider: ProviderId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub login_method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_account_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret_ref: Option<SecretRef>,
    /// Isolated Codex/Claude home for this account (path only, never tokens).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub home_path: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AccountBook {
    #[serde(default = "book_version")]
    pub version: u32,
    #[serde(default)]
    pub active_id: Option<String>,
    #[serde(default)]
    pub accounts: Vec<AccountRecord>,
}

fn book_version() -> u32 {
    1
}

impl AccountBook {
    pub fn get(&self, id: &str) -> Option<&AccountRecord> {
        self.accounts.iter().find(|a| a.id == id)
    }

    pub fn active(&self) -> Option<&AccountRecord> {
        self.active_id.as_deref().and_then(|id| self.get(id))
    }

    pub fn upsert(&mut self, rec: AccountRecord) {
        if let Some(existing) = self.accounts.iter_mut().find(|a| a.id == rec.id) {
            *existing = rec;
        } else {
            self.accounts.push(rec);
        }
    }

    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.accounts.len();
        self.accounts.retain(|a| a.id != id);
        if self.active_id.as_deref() == Some(id) {
            self.active_id = None;
        }
        self.accounts.len() != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_clears_active() {
        let mut book = AccountBook {
            version: 1,
            active_id: Some("a".into()),
            accounts: vec![AccountRecord {
                id: "a".into(),
                provider: ProviderId::Codex,
                email: None,
                workspace_label: None,
                login_method: None,
                workspace_account_id: None,
                secret_ref: None,
                home_path: None,
                created_at: 1,
                updated_at: 1,
                source: None,
            }],
        };
        assert!(book.remove("a"));
        assert!(book.active_id.is_none());
        assert!(book.accounts.is_empty());
    }
}
