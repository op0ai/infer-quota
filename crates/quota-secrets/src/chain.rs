use crate::claude_code::ClaudeCodeKeychain;
use crate::file::FileOauthBackend;
use crate::keychain::KeychainBackend;
#[cfg(feature = "openbao")]
use crate::openbao::OpenBaoBackend;
use crate::types::{SecretRecord, SecretsBackend, SecretsError};

/// Ordered fallback, first hit wins. [`from_env`] and
/// [`keychain_first_from_env`] fix the two orders the crate hands out.
pub struct SecretChain {
    backends: Vec<Box<dyn SecretsBackend>>,
}

impl SecretChain {
    pub fn new(backends: Vec<Box<dyn SecretsBackend>>) -> Self {
        Self { backends }
    }

    pub fn backend_names(&self) -> Vec<&'static str> {
        self.backends.iter().map(|b| b.name()).collect()
    }
}

impl SecretsBackend for SecretChain {
    fn name(&self) -> &'static str {
        "chain"
    }

    fn get(&self, path: &str) -> Result<Option<SecretRecord>, SecretsError> {
        let mut last_unimpl: Option<SecretsError> = None;
        for b in &self.backends {
            match b.get(path) {
                Ok(Some(rec)) => return Ok(Some(rec)),
                Ok(None) => {}
                Err(SecretsError::Unimplemented(_)) | Err(SecretsError::Unavailable(_)) => {
                    last_unimpl = Some(SecretsError::Unimplemented(b.name().into()));
                }
                Err(SecretsError::NotFound(_)) | Err(SecretsError::ReadOnly) => {}
                Err(e) => return Err(e),
            }
        }
        if last_unimpl.is_some() {
            return Ok(None);
        }
        Ok(None)
    }

    fn put(&self, path: &str, value: &str) -> Result<(), SecretsError> {
        for b in &self.backends {
            match b.put(path, value) {
                Ok(()) => return Ok(()),
                Err(SecretsError::ReadOnly)
                | Err(SecretsError::Unimplemented(_))
                | Err(SecretsError::Unavailable(_)) => {}
                Err(e) => return Err(e),
            }
        }
        Err(SecretsError::Unavailable(
            "no writable secrets backend (configure OpenBao or use a memory store in tests)".into(),
        ))
    }

    fn delete(&self, path: &str) -> Result<(), SecretsError> {
        for b in &self.backends {
            match b.delete(path) {
                Ok(()) => return Ok(()),
                Err(SecretsError::ReadOnly)
                | Err(SecretsError::Unimplemented(_))
                | Err(SecretsError::Unavailable(_)) => {}
                Err(e) => return Err(e),
            }
        }
        Err(SecretsError::Unavailable(
            "no writable secrets backend".into(),
        ))
    }
}

/// OpenBao is configured only when the `openbao` feature is enabled and
/// `QUOTA_OPENBAO_ADDR` is set.
fn openbao_from_env() -> Result<Option<Box<dyn SecretsBackend>>, SecretsError> {
    #[cfg(feature = "openbao")]
    let openbao = OpenBaoBackend::from_env()?.map(|bao| Box::new(bao) as Box<dyn SecretsBackend>);
    #[cfg(not(feature = "openbao"))]
    let openbao = None;
    Ok(openbao)
}

/// Build the default ordered chain: OpenBao, then the OS keychain and Claude
/// Code's own item, then the read-only CLI files.
pub fn from_env() -> Result<SecretChain, SecretsError> {
    let local: [Box<dyn SecretsBackend>; 3] = [
        Box::new(KeychainBackend),
        Box::new(ClaudeCodeKeychain),
        Box::new(FileOauthBackend::default()),
    ];
    Ok(SecretChain::new(
        openbao_from_env()?.into_iter().chain(local).collect(),
    ))
}

/// The chain for a session cookie the user copies in by hand (Cursor): the OS
/// keychain, then OpenBao when configured, then the private read-only file.
pub fn keychain_first_from_env() -> Result<SecretChain, SecretsError> {
    Ok(keychain_first(openbao_from_env()?))
}

/// [`keychain_first_from_env`] with the OpenBao slot supplied by the caller.
pub fn keychain_first(openbao: Option<Box<dyn SecretsBackend>>) -> SecretChain {
    let keychain: Box<dyn SecretsBackend> = Box::new(KeychainBackend);
    let file: Box<dyn SecretsBackend> = Box::new(FileOauthBackend::default());
    SecretChain::new(
        std::iter::once(keychain)
            .chain(openbao)
            .chain(std::iter::once(file))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryBackend;

    #[test]
    fn get_falls_through_then_hits_memory() {
        let mem = MemoryBackend::new();
        mem.put("k", "secret-value").unwrap();
        let chain = SecretChain::new(vec![Box::new(KeychainBackend), Box::new(mem)]);
        let rec = chain.get("k").unwrap().unwrap();
        assert_eq!(rec.value, "secret-value");
        assert_eq!(rec.backend, "memory");
    }

    #[test]
    fn put_skips_readonly() {
        let mem = MemoryBackend::new();
        let chain = SecretChain::new(vec![Box::new(FileOauthBackend::default()), Box::new(mem)]);
        chain.put("n", "v").unwrap();
        assert_eq!(chain.get("n").unwrap().unwrap().value, "v");
    }

    #[test]
    fn default_from_env_has_file_backend() {
        let chain = from_env().unwrap();
        let names = chain.backend_names();
        assert!(names.contains(&"file"));
        assert!(names.contains(&"keychain"));
        #[cfg(not(feature = "openbao"))]
        assert!(
            !names.contains(&"openbao"),
            "openbao must stay feature-gated out of default tests"
        );
    }

    #[test]
    fn keychain_first_orders_keychain_then_openbao_then_file() {
        let chain = keychain_first(Some(Box::new(MemoryBackend::new())));
        assert_eq!(chain.backend_names(), ["keychain", "memory", "file"]);
        assert_eq!(keychain_first(None).backend_names(), ["keychain", "file"]);
    }

    #[test]
    fn keychain_first_from_env_starts_at_the_keychain_and_ends_at_the_file() {
        let names = keychain_first_from_env().unwrap().backend_names();
        assert_eq!(names.first(), Some(&"keychain"));
        assert_eq!(names.last(), Some(&"file"));
        assert!(!names.contains(&"claude-code-keychain"), "{names:?}");
        #[cfg(not(feature = "openbao"))]
        assert_eq!(names, ["keychain", "file"]);
    }

    #[test]
    fn secret_record_debug_never_prints_value() {
        let rec = SecretRecord {
            backend: "memory",
            path: "codex/work".into(),
            value: "sk-must-not-appear-in-debug".into(),
        };
        let dumped = format!("{rec:?}");
        assert!(dumped.contains("<redacted>"));
        assert!(!dumped.contains("sk-must-not-appear-in-debug"));
    }
}
