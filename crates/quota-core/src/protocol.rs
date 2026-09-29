//! Socket request/response types. Schema version is [`PROTOCOL_VERSION`].
//!
//! A client writes one framed [`Request`] and reads one framed [`Response`],
//! except `watch`, which keeps the connection open and streams [`Response`]s
//! with the same `id` after each refresh.

use serde::{Deserialize, Serialize};

use crate::accounts::{AccountBook, AccountRecord};
use crate::types::{
    CanStartAnswer, PaceReport, ProviderId, Snapshot, Source, WindowKind, PACKAGE_VERSION,
};

/// Bump whenever a response may carry a value an older client cannot decode
/// (a new variant of a closed enum such as [`ProviderId`], [`Source`],
/// [`WindowKind`] or `CanStartBasis`) or a request field changes what a method
/// answers. A new method or a new optional response field does not bump it.
///
/// 2: `cursor` provider, `statusline` source, `spend` window kind,
/// `percent_budget` basis, `can_start.percent`/`reserve`, `observe`.
pub const PROTOCOL_VERSION: u32 = 2;

/// Lowest daemon protocol that understands `can_start.percent`. An older
/// daemon ignores the unknown field and answers a `tokens: 0` question, so a
/// percent client checks `version` first.
pub const PERCENT_ADMISSION_MIN_PROTOCOL: u32 = 2;

pub const METHOD_PING: &str = "ping";
pub const METHOD_VERSION: &str = "version";
pub const METHOD_STATUS: &str = "status";
pub const METHOD_PACE: &str = "pace";
pub const METHOD_CAN_START: &str = "can_start";
pub const METHOD_WATCH: &str = "watch";
/// Immediate provider probe (same as the timer). Added in protocol 1.
pub const METHOD_REFRESH: &str = "refresh";
/// Push one observation from a passive source (Claude Code's statusline).
/// Added in protocol 2; the payload carries its own [`OBSERVE_SCHEMA_VERSION`].
pub const METHOD_OBSERVE: &str = "observe";
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
    Cursor,
}

impl ProviderFilter {
    pub fn from_str_loose(s: &str) -> Option<Self> {
        match s {
            "all" => Some(Self::All),
            "codex" => Some(Self::Codex),
            "claude" => Some(Self::Claude),
            "cursor" => Some(Self::Cursor),
            _ => None,
        }
    }

    pub fn as_ids(self) -> Vec<ProviderId> {
        match self {
            Self::All => vec![ProviderId::Codex, ProviderId::Claude, ProviderId::Cursor],
            Self::Codex => vec![ProviderId::Codex],
            Self::Claude => vec![ProviderId::Claude],
            Self::Cursor => vec![ProviderId::Cursor],
        }
    }

    pub fn matches(self, id: ProviderId) -> bool {
        match self {
            Self::All => true,
            Self::Codex => id == ProviderId::Codex,
            Self::Claude => id == ProviderId::Claude,
            Self::Cursor => id == ProviderId::Cursor,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanStartParams {
    /// Token budget request. Zero (or absent) when `percent` is used.
    #[serde(default)]
    pub tokens: u64,
    /// Percent-of-window admission: admit when `remaining - percent >= reserve`,
    /// the pace projection does not empty the window before its reset, and,
    /// when `deadline` is set, the remainder also outlasts it. A deadline only
    /// adds a refusal. Mutually exclusive with a non-zero `tokens`.
    #[serde(default)]
    pub percent: Option<f64>,
    /// Percent that must stay untouched after `percent` is spent. Default 2.
    #[serde(default)]
    pub reserve: Option<f64>,
    /// UTC unix seconds. When omitted, the binding deadline is the window reset.
    #[serde(default)]
    pub deadline: Option<i64>,
    #[serde(default = "default_all")]
    pub provider: ProviderFilter,
}

/// Wire schema for [`METHOD_OBSERVE`]. Bump on a breaking change; the daemon
/// refuses a schema it does not know rather than guessing at fields.
pub const OBSERVE_SCHEMA_VERSION: u32 = 1;
pub const OBSERVE_MAX_WINDOWS: usize = 8;

/// One pushed measurement. The daemon stamps `observed_at` with its own clock
/// at receipt, so a client cannot backdate or future-date evidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservedWindow {
    pub kind: WindowKind,
    pub label: String,
    #[serde(default)]
    pub used_percent: Option<f64>,
    /// UTC unix seconds.
    #[serde(default)]
    pub reset_at: Option<i64>,
    #[serde(default)]
    pub limit_window_seconds: Option<i64>,
    #[serde(default)]
    pub used_usd: Option<f64>,
    #[serde(default)]
    pub limit_usd: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObserveParams {
    pub schema: u32,
    pub provider: ProviderId,
    pub source: Source,
    #[serde(default)]
    pub plan: Option<String>,
    pub windows: Vec<ObservedWindow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObserveResult {
    pub accepted: usize,
    /// Daemon receipt time; the evidence timestamp the snapshot carries.
    pub observed_at: i64,
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
                percent: None,
                reserve: None,
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

    #[test]
    fn accounts_add_ignores_token_fields() {
        let raw = serde_json::json!({
            "provider": "codex",
            "email": "openai@ctx.op0.dev",
            "access_token": "sk-must-not-deserialize",
            "refresh_token": "rt-must-not-deserialize",
            "password": "hunter2",
            "secret_ref": { "backend": "openbao", "path": "quota/codex/work" },
            "select": true
        });
        let p: AccountsAddParams = serde_json::from_value(raw).unwrap();
        let encoded = serde_json::to_string(&p).unwrap();
        assert!(encoded.contains("quota/codex/work"));
        assert!(!encoded.contains("sk-must-not-deserialize"));
        assert!(!encoded.contains("rt-must-not-deserialize"));
        assert!(!encoded.contains("hunter2"));
        assert!(!encoded.contains("access_token"));
        assert!(!encoded.contains("refresh_token"));
        assert!(!encoded.contains("password"));
    }

    #[test]
    fn can_start_params_accept_the_old_tokens_only_shape() {
        let p: CanStartParams = serde_json::from_value(serde_json::json!({"tokens": 5})).unwrap();
        assert_eq!((p.tokens, p.percent, p.reserve), (5, None, None));
        let p: CanStartParams =
            serde_json::from_value(serde_json::json!({"percent": 3.5, "reserve": 1.0})).unwrap();
        assert_eq!((p.tokens, p.percent, p.reserve), (0, Some(3.5), Some(1.0)));
    }

    #[test]
    fn observe_params_roundtrip_and_reject_unknown_source() {
        let raw = serde_json::json!({
            "schema": 1,
            "provider": "claude",
            "source": "statusline",
            "windows": [{"kind": "five_hour", "label": "5h", "used_percent": 12.0, "reset_at": 1_800_000_000}]
        });
        let p: ObserveParams = serde_json::from_value(raw).unwrap();
        assert_eq!(p.source, Source::Statusline);
        assert_eq!(p.windows[0].kind, WindowKind::FiveHour);
        let bad = serde_json::json!({"schema": 1, "provider": "claude", "source": "vibes", "windows": []});
        assert!(serde_json::from_value::<ObserveParams>(bad).is_err());
    }

    #[test]
    fn provider_filter_rejects_unknown() {
        assert!(ProviderFilter::from_str_loose("gemini").is_none());
        assert!(ProviderFilter::from_str_loose("openai").is_none());
        assert_eq!(
            ProviderFilter::from_str_loose("codex"),
            Some(ProviderFilter::Codex)
        );
    }
}
