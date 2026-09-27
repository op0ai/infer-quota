//! Persist account metadata (no secrets) next to daemon state.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use quota_core::accounts::{AccountBook, AccountRecord};
use quota_core::protocol::{AccountsAddParams, AccountsListResult};
use quota_core::timeutil::now_unix;

pub struct AccountStore {
    path: PathBuf,
    book: AccountBook,
}

impl AccountStore {
    pub fn load(path: PathBuf) -> Self {
        let book = read_book(&path).unwrap_or_default();
        Self { path, book }
    }

    pub fn list(&self) -> AccountsListResult {
        AccountsListResult::from(&self.book)
    }

    pub fn book(&self) -> &AccountBook {
        &self.book
    }

    pub fn add(&mut self, params: AccountsAddParams) -> Result<AccountRecord, String> {
        let now = now_unix();
        let id = match params.id {
            Some(id) if !id.trim().is_empty() => id,
            _ => generate_id(params.email.as_deref(), now),
        };
        let created_at = self.book.get(&id).map(|e| e.created_at).unwrap_or(now);
        let rec = AccountRecord {
            id,
            provider: params.provider,
            email: params.email,
            workspace_label: params.workspace_label,
            login_method: params.login_method,
            workspace_account_id: params.workspace_account_id,
            secret_ref: params.secret_ref,
            home_path: params.home_path,
            created_at,
            updated_at: now,
            source: Some("ctl".into()),
        };
        if params.select {
            self.book.active_id = Some(rec.id.clone());
        }
        self.book.upsert(rec.clone());
        self.persist()?;
        Ok(rec)
    }

    pub fn remove(&mut self, id: &str) -> Result<bool, String> {
        let gone = self.book.remove(id);
        if gone {
            self.persist()?;
        }
        Ok(gone)
    }

    pub fn select(&mut self, id: Option<String>) -> Result<Option<String>, String> {
        if let Some(ref want) = id {
            if self.book.get(want).is_none() {
                return Err(format!("unknown account {want}"));
            }
        }
        self.book.active_id = id;
        self.persist()?;
        Ok(self.book.active_id.clone())
    }

    fn persist(&self) -> Result<(), String> {
        write_book(&self.path, &self.book)
    }
}

fn generate_id(email: Option<&str>, now: i64) -> String {
    match email {
        Some(e) if !e.is_empty() => format!("acct_{}", e.replace(['@', '.'], "_")),
        _ => format!("acct_{now}"),
    }
}

fn read_book(path: &Path) -> Option<AccountBook> {
    let bytes = fs::read(path).ok()?;
    if bytes.len() > 64 * 1024 {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

fn write_book(path: &Path, book: &AccountBook) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("json.tmp");
    let mut f = fs::File::create(&tmp).map_err(|e| e.to_string())?;
    serde_json::to_writer_pretty(&mut f, book).map_err(|e| e.to_string())?;
    f.write_all(b"\n").map_err(|e| e.to_string())?;
    f.sync_all().map_err(|e| e.to_string())?;
    fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use quota_core::types::ProviderId;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn add_list_remove_roundtrip() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quota-accounts-{stamp}.json"));
        let mut store = AccountStore::load(path.clone());
        let rec = store
            .add(AccountsAddParams {
                id: Some("acct_test".into()),
                provider: ProviderId::Codex,
                email: Some("openai@ctx.op0.dev".into()),
                workspace_label: Some("Personal".into()),
                login_method: Some("pro".into()),
                workspace_account_id: None,
                secret_ref: None,
                home_path: None,
                select: true,
            })
            .unwrap();
        assert_eq!(rec.id, "acct_test");
        assert_eq!(store.list().active_id.as_deref(), Some("acct_test"));
        assert!(store.remove("acct_test").unwrap());
        assert!(store.list().accounts.is_empty());
        let _ = fs::remove_file(&path);
    }
}
