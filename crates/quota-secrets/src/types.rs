use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SecretsError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("backend unavailable: {0}")]
    Unavailable(String),
    #[error("read-only backend")]
    ReadOnly,
    #[error("not implemented: {0}")]
    Unimplemented(String),
    #[error("io: {0}")]
    Io(String),
    #[error("parse: {0}")]
    Parse(String),
    #[error("config: {0}")]
    Config(String),
}

/// A retrieved secret. Never log `value`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretRecord {
    pub backend: &'static str,
    pub path: String,
    pub value: String,
}

pub trait SecretsBackend: Send + Sync {
    fn name(&self) -> &'static str;
    fn get(&self, path: &str) -> Result<Option<SecretRecord>, SecretsError>;
    fn put(&self, path: &str, value: &str) -> Result<(), SecretsError>;
    fn delete(&self, path: &str) -> Result<(), SecretsError>;
}
