//! Library surface for menu-bar / tmux / MCP clients that want the same
//! control-plane RPCs as the `quota-ctl` CLI. This crate never probes
//! providers.

#![forbid(unsafe_code)]

use std::path::Path;

use quota_core::accounts::SecretRef;
use quota_core::protocol::{
    AccountMutationResult, AccountsAddParams, AccountsListResult, AccountsRemoveParams,
    AccountsSelectParams, RefreshParams, StatusResult, METHOD_ACCOUNTS_ADD, METHOD_ACCOUNTS_LIST,
    METHOD_ACCOUNTS_REMOVE, METHOD_ACCOUNTS_SELECT, METHOD_PING, METHOD_REFRESH,
};
use quota_core::rpc::{decode_result, rpc, RpcError};
use quota_core::ProviderFilter;

pub use quota_core::rpc::RpcError as ClientError;

pub fn ping(socket: &Path) -> Result<(), RpcError> {
    let resp = rpc(socket, 1, METHOD_PING, serde_json::Value::Null)?;
    if resp.ok {
        Ok(())
    } else {
        Err(RpcError::Rpc(quota_core::rpc::err_msg(&resp)))
    }
}

pub fn list_accounts(socket: &Path) -> Result<AccountsListResult, RpcError> {
    let resp = rpc(socket, 1, METHOD_ACCOUNTS_LIST, serde_json::Value::Null)?;
    decode_result(&resp)
}

pub fn add_account(
    socket: &Path,
    params: AccountsAddParams,
) -> Result<AccountMutationResult, RpcError> {
    let resp = rpc(socket, 1, METHOD_ACCOUNTS_ADD, params)?;
    decode_result(&resp)
}

pub fn remove_account(socket: &Path, id: &str) -> Result<AccountsListResult, RpcError> {
    let resp = rpc(
        socket,
        1,
        METHOD_ACCOUNTS_REMOVE,
        AccountsRemoveParams { id: id.to_string() },
    )?;
    decode_result(&resp)
}

pub fn select_account(socket: &Path, id: Option<&str>) -> Result<AccountsListResult, RpcError> {
    let resp = rpc(
        socket,
        1,
        METHOD_ACCOUNTS_SELECT,
        AccountsSelectParams {
            id: id.map(|s| s.to_string()),
        },
    )?;
    decode_result(&resp)
}

pub fn refresh(socket: &Path, provider: ProviderFilter) -> Result<StatusResult, RpcError> {
    let resp = rpc(socket, 1, METHOD_REFRESH, RefreshParams { provider })?;
    decode_result(&resp)
}

pub fn secret_ref(backend: Option<String>, path: Option<String>) -> Option<SecretRef> {
    match (backend, path) {
        (Some(backend), Some(path)) => Some(SecretRef { backend, path }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_ref_needs_both() {
        let p = secret_ref(Some("openbao".into()), Some("codex/work".into())).unwrap();
        assert_eq!(p.backend, "openbao");
        assert!(secret_ref(Some("openbao".into()), None).is_none());
    }
}
