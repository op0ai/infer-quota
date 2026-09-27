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

const SERVICE: &str = "infer-quota";
const MAX_VALUE: usize = 32 * 1024;

#[derive(Debug, Default, Clone)]
pub struct KeychainBackend;

impl SecretsBackend for KeychainBackend {
    fn name(&self) -> &'static str {
        "keychain"
    }

    fn get(&self, path: &str) -> Result<Option<SecretRecord>, SecretsError> {
        let path = check_path(path)?;
        platform::get(path)
    }

    fn put(&self, path: &str, value: &str) -> Result<(), SecretsError> {
        let path = check_path(path)?;
        check_value(value)?;
        platform::put(path, value)
    }

    fn delete(&self, path: &str) -> Result<(), SecretsError> {
        let path = check_path(path)?;
        platform::delete(path)
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

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "macos")]
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

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_path_and_value_are_refused() {
        let k = KeychainBackend;
        assert!(matches!(k.get("  "), Err(SecretsError::Config(_))));
        assert!(matches!(k.put("ok", ""), Err(SecretsError::Config(_))));
    }

    #[test]
    fn roundtrip_or_skip_without_session() {
        let k = KeychainBackend;
        let path = format!("quota/unit-{}", std::process::id());
        let material = "keychain-unit-material";
        match k.put(&path, material) {
            Err(SecretsError::Unavailable(_)) => return,
            Err(err) => panic!("put failed: {err}"),
            Ok(()) => {}
        }
        let rec = k.get(&path).unwrap().expect("stored item");
        assert_eq!(rec.backend, "keychain");
        assert_eq!(rec.value, material);
        assert!(!format!("{rec:?}").contains(material));
        k.delete(&path).unwrap();
        assert!(k.get(&path).unwrap().is_none());
    }
}
