//! Public snapshot schema. This is the source of truth for `quota`, `quotad`,
//! and any later thin client (menu bar, tmux, MCP).
//!
//! Field names are stable. Unknown provider fields must be ignored by clients
//! (`#[serde(default)]` / deny_unknown_fields is intentionally *not* used).

use serde::{Deserialize, Serialize};

/// Crate / binary version string (workspace version).
pub const PACKAGE_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderId {
    Codex,
    Claude,
}

impl ProviderId {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "codex" => Some(Self::Codex),
            "claude" => Some(Self::Claude),
            _ => None,
        }
    }
}

impl std::fmt::Display for ProviderId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How credentials were located. Passwords are never stored by this project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Oauth,
    Cli,
    Cookie,
    File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Availability {
    Ok,
    Unavailable,
}

/// Quota window kinds advertised by providers. Not every provider has all of them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowKind {
    Session,
    FiveHour,
    Weekly,
    Monthly,
    Extra,
}

impl WindowKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::FiveHour => "five_hour",
            Self::Weekly => "weekly",
            Self::Monthly => "monthly",
            Self::Extra => "extra",
        }
    }
}

/// One usage window. Absolute token remaining/limit are optional because both
/// Codex `/wham/usage` and Claude `/api/oauth/usage` typically publish *percent
/// used* only. Never invent a token budget from a percentage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageWindow {
    pub kind: WindowKind,
    /// Stable display label, e.g. `"5h"`, `"weekly"`, `"opus weekly"`.
    pub label: String,
    /// Percent already consumed in `[0, 100]` when the source provides it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub used_percent: Option<f64>,
    /// `100 - used_percent` when `used_percent` is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining_percent: Option<f64>,
    /// Absolute remaining when the source publishes a real unit (tokens/credits).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining: Option<f64>,
    /// Absolute limit when the source publishes a real unit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<f64>,
    /// `"percent"`, `"tokens"`, or `"credits"` — only set when a number exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// UTC unix seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_at_rfc3339: Option<String>,
    /// Window length in seconds when the source publishes it (e.g. 18000).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_window_seconds: Option<i64>,
}

impl UsageWindow {
    pub fn from_percent(
        kind: WindowKind,
        label: impl Into<String>,
        used_percent: f64,
        reset_at: Option<i64>,
        limit_window_seconds: Option<i64>,
    ) -> Self {
        let remaining_percent = (100.0 - used_percent).clamp(0.0, 100.0);
        Self {
            kind,
            label: label.into(),
            used_percent: Some(used_percent),
            remaining_percent: Some(remaining_percent),
            remaining: None,
            limit: None,
            unit: Some("percent".to_string()),
            reset_at,
            reset_at_rfc3339: reset_at.map(crate::timeutil::format_rfc3339),
            limit_window_seconds,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Credits {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub balance: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unlimited: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_credits: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterError {
    pub code: String,
    pub message: String,
}

impl AdapterError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderSnapshot {
    pub provider: ProviderId,
    pub status: Availability,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    pub windows: Vec<UsageWindow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credits: Option<Credits>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<AdapterError>,
    /// Credential or config path that was consulted (never includes secrets).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential_path: Option<String>,
}

impl ProviderSnapshot {
    pub fn unavailable(provider: ProviderId, error: AdapterError) -> Self {
        Self {
            provider,
            status: Availability::Unavailable,
            source: None,
            windows: Vec::new(),
            credits: None,
            plan: None,
            error: Some(error),
            credential_path: None,
        }
    }

    pub fn window(&self, kind: &WindowKind) -> Option<&UsageWindow> {
        self.windows.iter().find(|w| w.kind == *kind)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub fetched_at: i64,
    pub fetched_at_rfc3339: String,
    pub providers: Vec<ProviderSnapshot>,
}

impl Snapshot {
    pub fn new(fetched_at: i64, providers: Vec<ProviderSnapshot>) -> Self {
        Self {
            fetched_at,
            fetched_at_rfc3339: crate::timeutil::format_rfc3339(fetched_at),
            providers,
        }
    }

    pub fn by_id(&self, id: ProviderId) -> Option<&ProviderSnapshot> {
        self.providers.iter().find(|p| p.provider == id)
    }

    pub fn filtered(&self, filter: crate::protocol::ProviderFilter) -> Vec<&ProviderSnapshot> {
        self.providers
            .iter()
            .filter(|p| filter.matches(p.provider))
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CanStartBasis {
    TokenBudget,
    PercentOnly,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanStartAnswer {
    pub provider: ProviderId,
    pub ok: bool,
    pub basis: CanStartBasis,
    pub explanation: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_kind: Option<WindowKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining_tokens: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eta_empty_secs: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub burn_percent_per_hour: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaceReport {
    pub provider: ProviderId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_kind: Option<WindowKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub used_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub burn_percent_per_hour: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eta_empty_secs: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<i64>,
    pub samples: u32,
    pub explanation: String,
}
