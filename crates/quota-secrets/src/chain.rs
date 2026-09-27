use crate::file::FileOauthBackend;
use crate::keychain::KeychainBackend;
use crate::openbao::OpenBaoBackend;
use crate::types::{SecretRecord, SecretsBackend, SecretsError};

/// Ordered fallback: OpenBao (if configured) → keychain stub → file OAuth.
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

/// Build the default ordered chain. OpenBao is included only when env is set.
pub fn from_env() -> Result<SecretChain, SecretsError> {
    let mut backends: Vec<Box<dyn SecretsBackend>> = Vec::new();
    if let Some(bao) = OpenBaoBackend::from_env()? {
        backends.push(Box::new(bao));
    }
    backends.push(Box::new(KeychainBackend));
    backends.push(Box::new(FileOauthBackend::default()));
    Ok(SecretChain::new(backends))
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
    }
}
