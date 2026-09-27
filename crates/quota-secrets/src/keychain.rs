//! OS-native keychain / secret-service. Scaffold only.
//!
//! Real Security.framework calls are macOS-gated and not linked in v0.
//! Linux secret-service is documented, not implemented.

use crate::types::{SecretRecord, SecretsBackend, SecretsError};

#[derive(Debug, Default, Clone)]
pub struct KeychainBackend;

impl SecretsBackend for KeychainBackend {
    fn name(&self) -> &'static str {
        "keychain"
    }

    fn get(&self, path: &str) -> Result<Option<SecretRecord>, SecretsError> {
        let _ = path;
        Err(unimplemented_error())
    }

    fn put(&self, path: &str, _value: &str) -> Result<(), SecretsError> {
        let _ = path;
        Err(unimplemented_error())
    }

    fn delete(&self, path: &str) -> Result<(), SecretsError> {
        let _ = path;
        Err(unimplemented_error())
    }
}

fn unimplemented_error() -> SecretsError {
    #[cfg(target_os = "macos")]
    {
        SecretsError::Unimplemented(
            "macOS Keychain is a documented fallback; Security.framework is not linked in v0"
                .into(),
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        SecretsError::Unimplemented(
            "libsecret/secret-service is a documented fallback; not implemented on this OS".into(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_does_not_panic() {
        let k = KeychainBackend;
        match k.get("quota/test") {
            Err(SecretsError::Unimplemented(_)) => {}
            other => panic!("expected unimplemented, got {other:?}"),
        }
    }
}
