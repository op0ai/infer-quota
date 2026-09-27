//! Persist account metadata (no secrets) next to daemon state.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use quota_core::accounts::{AccountBook, AccountRecord};
use quota_core::fsutil::{
    chmod_private_file, ensure_private_dir, path_has_parent_dir, read_file_capped,
};
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
            home_path: sanitize_home_path(params.home_path)?,
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

/// Empty / omitted is fine. A *provided* path must be absolute and `..`-free.
fn sanitize_home_path(raw: Option<String>) -> Result<Option<String>, String> {
    let Some(s) = raw.filter(|s| !s.trim().is_empty()) else {
        return Ok(None);
    };
    let p = PathBuf::from(&s);
    if !p.is_absolute() || path_has_parent_dir(&p) {
        return Err("home_path must be an absolute path without '..'".into());
    }
    Ok(Some(s))
}

fn generate_id(email: Option<&str>, now: i64) -> String {
    match email {
        Some(e) if !e.is_empty() => format!("acct_{}", e.replace(['@', '.'], "_")),
        _ => format!("acct_{now}"),
    }
}

fn read_book(path: &Path) -> Option<AccountBook> {
    let bytes = read_file_capped(path, 64 * 1024).ok()?;
    let mut book: AccountBook = serde_json::from_slice(&bytes).ok()?;
    for rec in &mut book.accounts {
        rec.home_path = sanitize_home_path(rec.home_path.take()).ok().flatten();
    }
    Some(book)
}

fn write_book(path: &Path, book: &AccountBook) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        ensure_private_dir(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("json.tmp");
    let mut f = quota_core::create_private_file(&tmp).map_err(|e| e.to_string())?;
    serde_json::to_writer_pretty(&mut f, book).map_err(|e| e.to_string())?;
    f.write_all(b"\n").map_err(|e| e.to_string())?;
    f.sync_all().map_err(|e| e.to_string())?;
    fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    chmod_private_file(path).map_err(|e| e.to_string())?;
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

    #[test]
    fn relative_or_dotdot_home_path_is_rejected() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quota-accounts-home-{stamp}.json"));
        let mut store = AccountStore::load(path.clone());
        let err = store
            .add(AccountsAddParams {
                id: Some("acct_bad_home".into()),
                provider: ProviderId::Codex,
                email: None,
                workspace_label: None,
                login_method: None,
                workspace_account_id: None,
                secret_ref: None,
                home_path: Some("../etc".into()),
                select: false,
            })
            .unwrap_err();
        assert!(err.contains("absolute"));
        let err = store
            .add(AccountsAddParams {
                id: Some("acct_rel_home".into()),
                provider: ProviderId::Claude,
                email: None,
                workspace_label: None,
                login_method: None,
                workspace_account_id: None,
                secret_ref: None,
                home_path: Some("relative/claude".into()),
                select: false,
            })
            .unwrap_err();
        assert!(err.contains("absolute"));
        let rec = store
            .add(AccountsAddParams {
                id: Some("acct_ok_home".into()),
                provider: ProviderId::Claude,
                email: None,
                workspace_label: None,
                login_method: None,
                workspace_account_id: None,
                secret_ref: None,
                home_path: Some("/tmp/isolated-claude".into()),
                select: false,
            })
            .unwrap();
        assert_eq!(rec.home_path.as_deref(), Some("/tmp/isolated-claude"));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn persisted_relative_home_path_is_dropped_on_load() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quota-accounts-load-{stamp}.json"));
        fs::write(
            &path,
            r#"{"version":1,"active_id":"legacy","accounts":[{"id":"legacy","provider":"codex","home_path":"../etc","created_at":1,"updated_at":1}]}"#,
        )
        .unwrap();
        let store = AccountStore::load(path.clone());
        let rec = store.book().get("legacy").expect("legacy account");
        assert!(rec.home_path.is_none());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn persisted_book_is_metadata_only() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quota-accounts-meta-{stamp}.json"));
        let mut store = AccountStore::load(path.clone());
        store
            .add(AccountsAddParams {
                id: Some("acct_meta".into()),
                provider: ProviderId::Claude,
                email: Some("human@example.com".into()),
                workspace_label: None,
                login_method: None,
                workspace_account_id: None,
                secret_ref: Some(quota_core::SecretRef {
                    backend: "file".into(),
                    path: "/tmp/not-a-token".into(),
                }),
                home_path: Some("/tmp/isolated-home".into()),
                select: false,
            })
            .unwrap();
        let on_disk = fs::read_to_string(&path).unwrap();
        for needle in [
            "access_token",
            "refresh_token",
            "password",
            "eyJhbGci",
            "Bearer ",
        ] {
            assert!(!on_disk.contains(needle), "book leaked {needle}");
        }
        assert!(on_disk.contains("secret_ref"));
        assert!(on_disk.contains("/tmp/not-a-token"));
        let _ = fs::remove_file(&path);
    }
}
