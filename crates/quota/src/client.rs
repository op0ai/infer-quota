//! Thin re-export so the CLI keeps a local `client` module.
pub use quota_core::rpc::{decode_result, err_msg, rpc, rpc_watch, RpcError as ClientError};
