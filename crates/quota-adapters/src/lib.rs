//! Best-effort Codex + Claude adapters.
//!
//! These modules reuse *already logged-in* local sessions. They never store
//! passwords, never write credential files, and never invent quota numbers.
//! Network and parse failures become `status: unavailable` with a reason.

#![forbid(unsafe_code)]

pub mod claude;
pub mod codex;
pub mod creds;
pub mod http;
pub mod provider;

pub use claude::ClaudeAdapter;
pub use codex::CodexAdapter;
pub use http::{HttpResponse, TlsTransport, Transport, TransportError};
pub use provider::{probe_all, AdapterSet, ProbeCtx, Provider};
