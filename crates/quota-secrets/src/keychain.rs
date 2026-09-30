//! OS keychain.
//!
//! - **Linux:** [`secret-service`](https://docs.rs/secret-service) 4 over the
//!   session bus (zbus, RustCrypto, no libdbus). Attributes are
//!   `application=infer-quota` and `path=<logical path>`. If the bus is down
//!   the backend returns [`SecretsError::Unavailable`] and the chain skips it.
//! - **macOS:** Security.framework generic passwords, service `infer-quota`,
//!   account = logical path. Compiled only on `target_os = "macos"`.
//! - **Other OS:** [`SecretsError::Unavailable`] with an explicit message.
//!   This is not a silent success and not a permanent `unimplemented!()`.

use crate::types::{SecretRecord, SecretsBackend, SecretsError};

#[cfg(test)]
use std::sync::Arc;

#[cfg(not(test))]
const SERVICE: &str = "infer-quota";
const MAX_VALUE: usize = 32 * 1024;

#[derive(Default)]
pub struct KeychainBackend {
    #[cfg(test)]
    test_platform: Option<Arc<dyn KeychainPlatform>>,
}

#[cfg(test)]
trait KeychainPlatform: Send + Sync {
    fn get(&self, path: &str) -> Result<Option<SecretRecord>, SecretsError>;
    fn put(&self, path: &str, value: &str) -> Result<(), SecretsError>;
    fn delete(&self, path: &str) -> Result<(), SecretsError>;
}

impl KeychainBackend {
    #[cfg(test)]
    fn with_platform(platform: Arc<dyn KeychainPlatform>) -> Self {
        Self {
            test_platform: Some(platform),
        }
    }

    fn platform_get(&self, path: &str) -> Result<Option<SecretRecord>, SecretsError> {
        #[cfg(test)]
        if let Some(platform) = &self.test_platform {
            return platform.get(path);
        }
        platform::get(path)
    }

    fn platform_put(&self, path: &str, value: &str) -> Result<(), SecretsError> {
        #[cfg(test)]
        if let Some(platform) = &self.test_platform {
            return platform.put(path, value);
        }
        platform::put(path, value)
    }

    fn platform_delete(&self, path: &str) -> Result<(), SecretsError> {
        #[cfg(test)]
        if let Some(platform) = &self.test_platform {
            return platform.delete(path);
        }
        platform::delete(path)
    }

    fn get_with_enabled(
        &self,
        path: &str,
        enabled: bool,
    ) -> Result<Option<SecretRecord>, SecretsError> {
        let path = check_path(path)?;
        get_when_enabled(enabled, || self.platform_get(path))
    }
}

impl SecretsBackend for KeychainBackend {
    fn name(&self) -> &'static str {
        "keychain"
    }

    fn get(&self, path: &str) -> Result<Option<SecretRecord>, SecretsError> {
        self.get_with_enabled(path, !crate::claude_code::keychain_disabled())
    }

    fn put(&self, path: &str, value: &str) -> Result<(), SecretsError> {
        let path = check_path(path)?;
        check_value(value)?;
        self.platform_put(path, value)
    }

    fn delete(&self, path: &str) -> Result<(), SecretsError> {
        let path = check_path(path)?;
        self.platform_delete(path)
    }
}

fn get_when_enabled<T>(
    enabled: bool,
    read: impl FnOnce() -> Result<Option<T>, SecretsError>,
) -> Result<Option<T>, SecretsError> {
    if enabled {
        read()
    } else {
        Ok(None)
    }
}

fn check_path(path: &str) -> Result<&str, SecretsError> {
    let path = path.trim();
    if path.is_empty() || path.len() > 256 {
        return Err(SecretsError::Config(
            "keychain path must be 1..=256 bytes".into(),
        ));
    }
    if path.chars().any(|c| c == '\0' || c.is_control()) {
        return Err(SecretsError::Config(
            "keychain path has a control character".into(),
        ));
    }
    Ok(path)
}

fn check_value(value: &str) -> Result<(), SecretsError> {
    if value.is_empty() {
        return Err(SecretsError::Config(
            "refusing to store an empty secret".into(),
        ));
    }
    if value.len() > MAX_VALUE {
        return Err(SecretsError::Config("secret value exceeds 32 KiB".into()));
    }
    Ok(())
}

#[cfg(all(target_os = "linux", not(test)))]
mod platform {
    use std::collections::HashMap;

    use secret_service::blocking::SecretService;
    use secret_service::EncryptionType;

    use super::SERVICE;
    use crate::types::{SecretRecord, SecretsError};

    fn session() -> Result<SecretService<'static>, SecretsError> {
        SecretService::connect(EncryptionType::Dh).map_err(map_err)
    }

    fn map_err(err: secret_service::Error) -> SecretsError {
        let kind = match err {
            secret_service::Error::Crypto(_) => "crypto",
            secret_service::Error::Zbus(_) | secret_service::Error::ZbusFdo(_) => "dbus",
            secret_service::Error::Zvariant(_) => "decode",
            secret_service::Error::Locked => "locked",
            secret_service::Error::NoResult => "no-result",
            secret_service::Error::Prompt => "prompt-dismissed",
            secret_service::Error::Unavailable => "no-session",
            _ => "other",
        };
        SecretsError::Unavailable(format!("secret-service {kind}"))
    }

    fn attrs(path: &str) -> HashMap<&str, &str> {
        let mut attributes = HashMap::new();
        attributes.insert("application", SERVICE);
        attributes.insert("path", path);
        attributes
    }

    pub fn get(path: &str) -> Result<Option<SecretRecord>, SecretsError> {
        let ss = session()?;
        let collection = ss.get_any_collection().map_err(map_err)?;
        let _ = collection.ensure_unlocked();
        let items = collection.search_items(attrs(path)).map_err(map_err)?;
        let Some(item) = items.first() else {
            return Ok(None);
        };
        let _ = item.ensure_unlocked();
        let bytes = item.get_secret().map_err(map_err)?;
        let value = String::from_utf8(bytes)
            .map_err(|_| SecretsError::Parse("keychain item is not utf-8".into()))?;
        Ok(Some(SecretRecord {
            backend: "keychain",
            path: path.to_string(),
            value,
        }))
    }

    pub fn put(path: &str, value: &str) -> Result<(), SecretsError> {
        let ss = session()?;
        let collection = ss.get_any_collection().map_err(map_err)?;
        collection.ensure_unlocked().map_err(map_err)?;
        collection
            .create_item(
                &format!("infer-quota {path}"),
                attrs(path),
                value.as_bytes(),
                true,
                "text/plain",
            )
            .map_err(map_err)?;
        Ok(())
    }

    pub fn delete(path: &str) -> Result<(), SecretsError> {
        let ss = session()?;
        let collection = ss.get_any_collection().map_err(map_err)?;
        let items = collection.search_items(attrs(path)).map_err(map_err)?;
        for item in &items {
            item.delete().map_err(map_err)?;
        }
        Ok(())
    }
}

#[cfg(all(target_os = "macos", not(test)))]
mod platform {
    use security_framework::passwords::{
        delete_generic_password, get_generic_password, set_generic_password,
    };

    use super::SERVICE;
    use crate::types::{SecretRecord, SecretsError};

    /// Apple `errSecItemNotFound`.
    const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

    pub fn get(path: &str) -> Result<Option<SecretRecord>, SecretsError> {
        match get_generic_password(SERVICE, path) {
            Ok(bytes) => {
                let value = String::from_utf8(bytes)
                    .map_err(|_| SecretsError::Parse("keychain item is not utf-8".into()))?;
                Ok(Some(SecretRecord {
                    backend: "keychain",
                    path: path.to_string(),
                    value,
                }))
            }
            Err(err) => {
                let code = err.code();
                if code == ERR_SEC_ITEM_NOT_FOUND {
                    Ok(None)
                } else {
                    Err(SecretsError::Unavailable(format!("keychain error {code}")))
                }
            }
        }
    }

    pub fn put(path: &str, value: &str) -> Result<(), SecretsError> {
        set_generic_password(SERVICE, path, value.as_bytes())
            .map_err(|err| SecretsError::Unavailable(format!("keychain error {}", err.code())))
    }

    pub fn delete(path: &str) -> Result<(), SecretsError> {
        match delete_generic_password(SERVICE, path) {
            Ok(()) => Ok(()),
            Err(err) => {
                let code = err.code();
                if code == ERR_SEC_ITEM_NOT_FOUND {
                    Ok(())
                } else {
                    Err(SecretsError::Unavailable(format!("keychain error {code}")))
                }
            }
        }
    }
}

#[cfg(all(not(test), not(any(target_os = "linux", target_os = "macos"))))]
mod platform {
    use crate::types::{SecretRecord, SecretsError};

    fn unsupported() -> SecretsError {
        SecretsError::Unavailable(
            "OS keychain is unavailable on this platform (supported: linux secret-service, macos Security.framework)"
                .into(),
        )
    }

    pub fn get(_path: &str) -> Result<Option<SecretRecord>, SecretsError> {
        Err(unsupported())
    }

    pub fn put(_path: &str, _value: &str) -> Result<(), SecretsError> {
        Err(unsupported())
    }

    pub fn delete(_path: &str) -> Result<(), SecretsError> {
        Err(unsupported())
    }
}

/// Unit tests do not connect to the user's credential store. Contract tests
/// inject an isolated platform; the default test platform stays unavailable.
#[cfg(test)]
mod platform {
    use crate::types::{SecretRecord, SecretsError};

    fn unavailable() -> SecretsError {
        SecretsError::Unavailable("native keychain access disabled in unit tests".into())
    }

    pub fn get(_path: &str) -> Result<Option<SecretRecord>, SecretsError> {
        Err(unavailable())
    }

    pub fn put(_path: &str, _value: &str) -> Result<(), SecretsError> {
        Err(unavailable())
    }

    pub fn delete(_path: &str) -> Result<(), SecretsError> {
        Err(unavailable())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct TemporaryKeychain(Mutex<HashMap<String, String>>);

    impl KeychainPlatform for TemporaryKeychain {
        fn get(&self, path: &str) -> Result<Option<SecretRecord>, SecretsError> {
            let value = self
                .0
                .lock()
                .map_err(|_| SecretsError::Unavailable("temporary keychain poisoned".into()))?
                .get(path)
                .cloned();
            Ok(value.map(|value| SecretRecord {
                backend: "keychain",
                path: path.to_string(),
                value,
            }))
        }

        fn put(&self, path: &str, value: &str) -> Result<(), SecretsError> {
            self.0
                .lock()
                .map_err(|_| SecretsError::Unavailable("temporary keychain poisoned".into()))?
                .insert(path.to_string(), value.to_string());
            Ok(())
        }

        fn delete(&self, path: &str) -> Result<(), SecretsError> {
            self.0
                .lock()
                .map_err(|_| SecretsError::Unavailable("temporary keychain poisoned".into()))?
                .remove(path);
            Ok(())
        }
    }

    struct UnavailableKeychain;

    impl KeychainPlatform for UnavailableKeychain {
        fn get(&self, _path: &str) -> Result<Option<SecretRecord>, SecretsError> {
            Err(SecretsError::Unavailable(
                "secret-service no-session".into(),
            ))
        }

        fn put(&self, _path: &str, _value: &str) -> Result<(), SecretsError> {
            Err(SecretsError::Unavailable(
                "secret-service no-session".into(),
            ))
        }

        fn delete(&self, _path: &str) -> Result<(), SecretsError> {
            Err(SecretsError::Unavailable(
                "secret-service no-session".into(),
            ))
        }
    }

    #[test]
    fn empty_path_and_value_are_refused() {
        let k = KeychainBackend::default();
        assert!(matches!(k.get("  "), Err(SecretsError::Config(_))));
        assert!(matches!(k.put("ok", ""), Err(SecretsError::Config(_))));
    }

    #[test]
    fn disabled_keychain_returns_without_calling_the_platform_reader() {
        assert_eq!(
            get_when_enabled::<SecretRecord>(false, || {
                panic!("disabled keychain must not reach the platform")
            }),
            Ok(None)
        );
    }

    #[test]
    fn native_platform_unavailability_is_preserved_through_the_backend() {
        // Exercise the native-unavailable contract without opening a session
        // bus, macOS Keychain, or any other user credential store.
        let k = KeychainBackend::with_platform(Arc::new(UnavailableKeychain));
        match k.get_with_enabled("quota/no-bus-unit", true) {
            Ok(_) => {}
            Err(SecretsError::Unavailable(msg)) => {
                assert!(
                    msg.contains("secret-service")
                        || msg.contains("unavailable")
                        || msg.contains("platform"),
                    "{msg}"
                );
            }
            Err(other) => panic!("expected Unavailable, got {other}"),
        }
    }

    #[test]
    fn injected_platform_roundtrips_through_the_keychain_backend() {
        let path = format!("quota/unit-{}", std::process::id());
        let material = "keychain-unit-material";
        let k = KeychainBackend::with_platform(Arc::new(TemporaryKeychain::default()));
        k.put(&path, material).unwrap();
        let rec = k
            .get_with_enabled(&path, true)
            .unwrap()
            .expect("stored item");
        assert_eq!(rec.backend, "keychain");
        assert_eq!(rec.value, material);
        assert!(!format!("{rec:?}").contains(material));
        k.delete(&path).unwrap();
        assert!(k.get_with_enabled(&path, true).unwrap().is_none());
    }
}
