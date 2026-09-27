//! Socket request/response types. Schema version is [`PROTOCOL_VERSION`].
//!
//! A client writes one framed [`Request`] and reads one framed [`Response`],
//! except `watch`, which keeps the connection open and streams [`Response`]s
//! with the same `id` after each refresh.

use serde::{Deserialize, Serialize};

use crate::accounts::{AccountBook, AccountRecord};
use crate::types::{CanStartAnswer, PaceReport, ProviderId, Snapshot, PACKAGE_VERSION};

/// Bump when adding a breaking field. Additive optional fields do not require a bump.
pub const PROTOCOL_VERSION: u32 = 1;

pub const METHOD_PING: &str = "ping";
pub const METHOD_VERSION: &str = "version";
pub const METHOD_STATUS: &str = "status";
pub const METHOD_PACE: &str = "pace";
pub const METHOD_CAN_START: &str = "can_start";
pub const METHOD_WATCH: &str = "watch";
/// Immediate provider probe (same as the timer). Additive in protocol 1.
pub const METHOD_REFRESH: &str = "refresh";
pub const METHOD_ACCOUNTS_LIST: &str = "accounts.list";
pub const METHOD_ACCOUNTS_ADD: &str = "accounts.add";
pub const METHOD_ACCOUNTS_REMOVE: &str = "accounts.remove";
pub const METHOD_ACCOUNTS_SELECT: &str = "accounts.select";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ProviderFilter {
    #[default]
    All,
    Codex,
    Claude,
}

impl ProviderFilter {
    pub fn from_str_loose(s: &str) -> Option<Self> {
        match s {
            "all" => Some(Self::All),
            "codex" => Some(Self::Codex),
            "claude" => Some(Self::Claude),
            _ => None,
        }
    }

    pub fn as_ids(self) -> Vec<ProviderId> {
        match self {
            Self::All => vec![ProviderId::Codex, ProviderId::Claude],
            Self::Codex => vec![ProviderId::Codex],
            Self::Claude => vec![ProviderId::Claude],
        }
    }

    pub fn matches(self, id: ProviderId) -> bool {
        match self {
            Self::All => true,
            Self::Codex => id == ProviderId::Codex,
            Self::Claude => id == ProviderId::Claude,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct StatusParams {
    #[serde(default = "default_all")]
    pub provider: ProviderFilter,
}

fn default_all() -> ProviderFilter {
    ProviderFilter::All
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PaceParams {
    #[serde(default = "default_all")]
    pub provider: ProviderFilter,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanStartParams {
    pub tokens: u64,
    /// UTC unix seconds. When omitted, the binding deadline is the window reset.
    #[serde(default)]
    pub deadline: Option<i64>,
    #[serde(default = "default_all")]
    pub provider: ProviderFilter,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WatchParams {
    #[serde(default = "default_all")]
    pub provider: ProviderFilter,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RefreshParams {
    #[serde(default = "default_all")]
    pub provider: ProviderFilter,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AccountsListParams {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountsAddParams {
    /// Caller-supplied id. Daemon generates one when omitted.
    #[serde(default)]
    pub id: Option<String>,
    pub provider: ProviderId,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub workspace_label: Option<String>,
    #[serde(default)]
    pub login_method: Option<String>,
    #[serde(default)]
    pub workspace_account_id: Option<String>,
    #[serde(default)]
    pub secret_ref: Option<crate::accounts::SecretRef>,
    #[serde(default)]
    pub home_path: Option<String>,
    /// When true, this account becomes the active selection.
    #[serde(default)]
    pub select: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountsRemoveParams {
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountsSelectParams {
    /// `null` / omitted clears the selection.
    #[serde(default)]
    pub id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountsListResult {
    pub version: u32,
    pub active_id: Option<String>,
    pub accounts: Vec<AccountRecord>,
}

impl From<&AccountBook> for AccountsListResult {
    fn from(book: &AccountBook) -> Self {
        Self {
            version: book.version,
            active_id: book.active_id.clone(),
            accounts: book.accounts.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountMutationResult {
    pub account: AccountRecord,
    pub active_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

impl Request {
    pub fn new(id: u64, method: impl Into<String>) -> Self {
        Self {
            id,
            method: method.into(),
            params: serde_json::Value::Null,
        }
    }

    pub fn with_params(id: u64, method: impl Into<String>, params: impl Serialize) -> Self {
        Self {
            id,
            method: method.into(),
            params: serde_json::to_value(params).unwrap_or(serde_json::Value::Null),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
}

impl Response {
    pub fn result(id: u64, value: impl Serialize) -> Self {
        Self {
            id,
            ok: true,
            result: Some(serde_json::to_value(value).unwrap_or(serde_json::Value::Null)),
            error: None,
        }
    }

    pub fn err(id: u64, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            id,
            ok: false,
            result: None,
            error: Some(ErrorBody {
                code: code.into(),
                message: message.into(),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionInfo {
    pub name: String,
    pub version: String,
    pub protocol: u32,
}

impl VersionInfo {
    pub fn current() -> Self {
        Self {
            name: "quotad".to_string(),
            version: PACKAGE_VERSION.to_string(),
            protocol: PROTOCOL_VERSION,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pong {
    pub pong: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusResult {
    pub snapshot: Snapshot,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaceResult {
    pub reports: Vec<PaceReport>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanStartResult {
    /// True iff every *available* selected provider answered `ok: true`.
    /// If none are available, this is false.
    pub ok: bool,
    pub answers: Vec<CanStartAnswer>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrip() {
        let req = Request::with_params(
            7,
            METHOD_CAN_START,
            CanStartParams {
                tokens: 50_000,
                deadline: None,
                provider: ProviderFilter::Codex,
            },
        );
        let bytes = serde_json::to_vec(&req).unwrap();
        let back: Request = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.id, 7);
        assert_eq!(back.method, METHOD_CAN_START);
        let p: CanStartParams = serde_json::from_value(back.params).unwrap();
        assert_eq!(p.tokens, 50_000);
    }
}
