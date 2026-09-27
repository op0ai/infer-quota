//! Unified secrets backends for `quota-ctl`.
//!
//! Lookup order (first hit wins):
//! 1. OpenBao KV (feature `openbao`; env-configured; rustls for `https://`)
//! 2. OS keychain (Linux secret-service, macOS Security.framework)
//! 3. Existing CLI OAuth files (read-only)
//!
//! `cargo test` never dials a live OpenBao. Plain-HTTP and TLS tests use an
//! in-process listener.

#![forbid(unsafe_code)]

pub mod chain;
pub mod file;
pub mod keychain;
pub mod memory;
#[cfg(feature = "openbao")]
pub mod openbao;
pub mod types;

pub use chain::{from_env, SecretChain};
pub use file::FileOauthBackend;
pub use keychain::KeychainBackend;
pub use memory::MemoryBackend;
#[cfg(feature = "openbao")]
pub use openbao::OpenBaoBackend;
pub use types::{SecretRecord, SecretsBackend, SecretsError};

/// Env knobs for the optional OpenBao backend.
pub const ENV_OPENBAO_ADDR: &str = "QUOTA_OPENBAO_ADDR";
pub const ENV_OPENBAO_TOKEN: &str = "QUOTA_OPENBAO_TOKEN";
pub const ENV_OPENBAO_MOUNT: &str = "QUOTA_OPENBAO_MOUNT";
pub const ENV_OPENBAO_PREFIX: &str = "QUOTA_OPENBAO_PREFIX";
/// PEM file of extra CA certificates for a private OpenBao TLS endpoint.
pub const ENV_OPENBAO_CA_FILE: &str = "QUOTA_OPENBAO_CA_FILE";
/// Set to `1` to allow plain HTTP to a non-loopback OpenBao (local-dev only).
pub const ENV_OPENBAO_ALLOW_PLAINTEXT: &str = "QUOTA_OPENBAO_ALLOW_PLAINTEXT";
