use std::collections::HashMap;
use std::sync::Mutex;

use crate::types::{SecretRecord, SecretsBackend, SecretsError};

/// In-process map for tests. Not a production store.
#[derive(Debug, Default)]
pub struct MemoryBackend {
    inner: Mutex<HashMap<String, String>>,
}

impl MemoryBackend {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SecretsBackend for MemoryBackend {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn get(&self, path: &str) -> Result<Option<SecretRecord>, SecretsError> {
        let g = self
            .inner
            .lock()
            .map_err(|e| SecretsError::Io(e.to_string()))?;
        Ok(g.get(path).map(|value| SecretRecord {
            backend: "memory",
            path: path.to_string(),
            value: value.clone(),
        }))
    }

    fn put(&self, path: &str, value: &str) -> Result<(), SecretsError> {
        let mut g = self
            .inner
            .lock()
            .map_err(|e| SecretsError::Io(e.to_string()))?;
        g.insert(path.to_string(), value.to_string());
        Ok(())
    }

    fn delete(&self, path: &str) -> Result<(), SecretsError> {
        let mut g = self
            .inner
            .lock()
            .map_err(|e| SecretsError::Io(e.to_string()))?;
        g.remove(path);
        Ok(())
    }
}
