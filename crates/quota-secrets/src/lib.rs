//! Unified secrets backends for `quota-ctl`.
//!
//! Lookup order (first hit wins):
//! 1. OpenBao KV (optional; env-configured)
//! 2. OS keychain / secret-service (scaffold; macOS-gated)
//! 3. Existing CLI OAuth files (read-only)
//!
//! Default `cargo test` never talks to OpenBao or the network.

#![forbid(unsafe_code)]

pub mod chain;
pub mod file;
pub mod keychain;
pub mod memory;
pub mod openbao;
pub mod types;

pub use chain::{from_env, SecretChain};
pub use file::FileOauthBackend;
pub use keychain::KeychainBackend;
pub use memory::MemoryBackend;
pub use openbao::OpenBaoBackend;
pub use types::{SecretRecord, SecretsBackend, SecretsError};

/// Env knobs for the optional OpenBao backend.
pub const ENV_OPENBAO_ADDR: &str = "QUOTA_OPENBAO_ADDR";
pub const ENV_OPENBAO_TOKEN: &str = "QUOTA_OPENBAO_TOKEN";
pub const ENV_OPENBAO_MOUNT: &str = "QUOTA_OPENBAO_MOUNT";
pub const ENV_OPENBAO_PREFIX: &str = "QUOTA_OPENBAO_PREFIX";
/// Set to `1` to allow plain HTTP to a non-loopback OpenBao (local-dev only).
pub const ENV_OPENBAO_ALLOW_PLAINTEXT: &str = "QUOTA_OPENBAO_ALLOW_PLAINTEXT";
