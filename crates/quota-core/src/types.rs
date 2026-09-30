//! Public snapshot schema. This is the source of truth for `quota`, `quotad`,
//! and any later thin client (menu bar, tmux, MCP).
//!
//! Field names are stable. Unknown provider fields must be ignored by clients
//! (`#[serde(default)]` / deny_unknown_fields is intentionally *not* used).

use serde::{Deserialize, Serialize};

/// Crate / binary version string (workspace version).
pub const PACKAGE_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Evidence older than this is not presented as current by the collector.
pub const DEFAULT_READING_MAX_AGE_SECS: u64 = 300;
/// Longest wait any provider may impose. A larger `Retry-After` is clamped so
/// every backoff deadline stays finite.
pub const MAX_RETRY_AFTER_SECS: u64 = 86_400;
/// Error code of a reading presented for a provider account other than the
/// one it was taken for.
pub const ACCOUNT_CHANGED: &str = "account_changed";

/// The provider account the local credentials name at the moment of use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActiveIdentity {
    /// Credentials name this account: the SHA-256 hex of its id.
    Named(String),
    /// Credentials are present but name no account.
    Unnamed,
    /// No usable credentials.
    Absent,
}

impl ActiveIdentity {
    pub fn digest(&self) -> Option<&str> {
        match self {
            Self::Named(digest) => Some(digest),
            Self::Unnamed | Self::Absent => None,
        }
    }
}

/// Whether a reading taken for provider account `reading` may answer for
/// `active`, the account the provider's credentials name at the moment of
/// use. Two named accounts must be the same one. Otherwise nothing ties the
/// reading to the active account except there being exactly one, so it
/// answers only while exactly one account is known. The count takes the
/// reading's own account, every account in `others()` (the provider's other
/// local sources; `None` for one they cannot name) and the active one: a
/// `Named` or `Unnamed` credential is one account, which an unnamed one may
/// share with a named account, and `Absent` is none. A reading's own name
/// shows which account it was, not that one is still here, so with absent
/// credentials and no other source it refuses.
pub fn reading_answers_for_active_account<F>(
    reading: Option<&str>,
    active: &ActiveIdentity,
    others: F,
) -> bool
where
    F: FnOnce() -> Vec<Option<String>>,
{
    if let (Some(reading), ActiveIdentity::Named(active)) = (reading, active) {
        return reading == active;
    }
    let others = others();
    let mut named: Vec<&str> = reading
        .into_iter()
        .chain(active.digest())
        .chain(others.iter().filter_map(Option::as_deref))
        .collect();
    named.sort_unstable();
    named.dedup();
    let unnamed = others.iter().filter(|account| account.is_none()).count();
    let present = *active != ActiveIdentity::Absent || !others.is_empty();
    present && named.len() + unnamed <= 1
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderId {
    Codex,
    Claude,
    Cursor,
}

impl ProviderId {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Cursor => "cursor",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "codex" => Some(Self::Codex),
            "claude" => Some(Self::Claude),
            "cursor" => Some(Self::Cursor),
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
    /// Pushed by Claude Code's statusline hook through `quota statusline`.
    Statusline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Availability {
    Ok,
    Stale,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Freshness {
    Current,
    Stale,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProviderPermission {
    Allowed,
    LimitReached,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WindowState {
    Ok,
    Exhausted,
    #[default]
    Unknown,
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
    /// Money against a cap (`used_usd` / `limit_usd`), e.g. Cursor on-demand.
    Spend,
}

impl WindowKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::FiveHour => "five_hour",
            Self::Weekly => "weekly",
            Self::Monthly => "monthly",
            Self::Extra => "extra",
            Self::Spend => "spend",
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
    /// State of this individual window. Missing/unreadable measurements stay unknown.
    #[serde(default)]
    pub state: WindowState,
    /// The source measurement. Null means the provider reported a window but
    /// did not provide a readable measurement.
    pub reading: Option<WindowReading>,
    /// Time the source says this measurement was observed, in UTC Unix seconds.
    #[serde(default)]
    pub observed_at: Option<i64>,
    /// Maximum age at which this evidence may be presented as current.
    #[serde(default = "default_max_age")]
    pub max_age_secs: u64,
    #[serde(default)]
    pub freshness: Freshness,
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
    /// `"percent"`, `"tokens"`, `"credits"`, or `"usd"` — only set when a number exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// Dollars already spent, for `spend` windows and money-denominated plans.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub used_usd: Option<f64>,
    /// Dollar cap for the window, when the source publishes one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_usd: Option<f64>,
    /// UTC unix seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_at_rfc3339: Option<String>,
    /// Window length in seconds when the source publishes it (e.g. 18000).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_window_seconds: Option<i64>,
}

fn default_max_age() -> u64 {
    DEFAULT_READING_MAX_AGE_SECS
}

/// Structured measurement. The legacy flat numeric fields on `UsageWindow`
/// remain available for existing clients and are kept in sync by constructors.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowReading {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub used_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub used_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_usd: Option<f64>,
}

impl UsageWindow {
    pub fn from_percent(
        kind: WindowKind,
        label: impl Into<String>,
        used_percent: f64,
        reset_at: Option<i64>,
        limit_window_seconds: Option<i64>,
    ) -> Self {
        Self::from_percent_at(
            kind,
            label,
            used_percent,
            reset_at,
            limit_window_seconds,
            Some(crate::timeutil::now_unix()),
            DEFAULT_READING_MAX_AGE_SECS,
        )
    }

    pub fn from_percent_at(
        kind: WindowKind,
        label: impl Into<String>,
        used_percent: f64,
        reset_at: Option<i64>,
        limit_window_seconds: Option<i64>,
        observed_at: Option<i64>,
        max_age_secs: u64,
    ) -> Self {
        let remaining_percent = (100.0 - used_percent).clamp(0.0, 100.0);
        let state = if used_percent >= 100.0 {
            WindowState::Exhausted
        } else {
            WindowState::Ok
        };
        let reading = WindowReading {
            used_percent: Some(used_percent),
            remaining_percent: Some(remaining_percent),
            remaining: None,
            limit: None,
            unit: Some("percent".to_string()),
            used_usd: None,
            limit_usd: None,
        };
        let freshness = freshness_for(observed_at, max_age_secs, crate::timeutil::now_unix());
        Self {
            kind,
            label: label.into(),
            state,
            reading: Some(reading),
            observed_at,
            max_age_secs,
            freshness,
            used_percent: Some(used_percent),
            remaining_percent: Some(remaining_percent),
            remaining: None,
            limit: None,
            unit: Some("percent".to_string()),
            used_usd: None,
            limit_usd: None,
            reset_at,
            reset_at_rfc3339: reset_at.map(crate::timeutil::format_rfc3339),
            limit_window_seconds,
        }
    }

    /// A money window. With a positive cap the percent fields are derived from
    /// dollars; without one the window records spend but publishes no percent,
    /// so admission never treats an uncapped counter as headroom.
    #[allow(clippy::too_many_arguments)]
    pub fn from_spend_at(
        kind: WindowKind,
        label: impl Into<String>,
        used_usd: f64,
        limit_usd: Option<f64>,
        reset_at: Option<i64>,
        limit_window_seconds: Option<i64>,
        observed_at: Option<i64>,
        max_age_secs: u64,
    ) -> Self {
        let cap = limit_usd.filter(|limit| limit.is_finite() && *limit > 0.0);
        let mut window = match cap {
            Some(limit) => Self::from_percent_at(
                kind,
                label,
                used_usd / limit * 100.0,
                reset_at,
                limit_window_seconds,
                observed_at,
                max_age_secs,
            ),
            None => {
                let mut window = Self::unreadable(
                    kind,
                    label,
                    reset_at,
                    limit_window_seconds,
                    observed_at,
                    max_age_secs,
                );
                window.state = WindowState::Ok;
                window.reading = Some(WindowReading {
                    used_percent: None,
                    remaining_percent: None,
                    remaining: None,
                    limit: None,
                    unit: Some("usd".to_string()),
                    used_usd: None,
                    limit_usd: None,
                });
                window
            }
        };
        window.unit = Some("usd".to_string());
        window.used_usd = Some(used_usd);
        window.limit_usd = cap;
        window.remaining = cap.map(|limit| (limit - used_usd).max(0.0));
        window.limit = cap;
        if let Some(reading) = window.reading.as_mut() {
            reading.unit = Some("usd".to_string());
            reading.used_usd = Some(used_usd);
            reading.limit_usd = cap;
            reading.remaining = window.remaining;
            reading.limit = cap;
        }
        window
    }

    pub fn unreadable(
        kind: WindowKind,
        label: impl Into<String>,
        reset_at: Option<i64>,
        limit_window_seconds: Option<i64>,
        observed_at: Option<i64>,
        max_age_secs: u64,
    ) -> Self {
        let freshness = freshness_for(observed_at, max_age_secs, crate::timeutil::now_unix());
        Self {
            kind,
            label: label.into(),
            state: WindowState::Unknown,
            reading: None,
            observed_at,
            max_age_secs,
            freshness,
            used_percent: None,
            remaining_percent: None,
            remaining: None,
            limit: None,
            unit: None,
            used_usd: None,
            limit_usd: None,
            reset_at,
            reset_at_rfc3339: reset_at.map(crate::timeutil::format_rfc3339),
            limit_window_seconds,
        }
    }

    fn refresh_freshness(&mut self, now: i64) {
        self.freshness = freshness_for(self.observed_at, self.max_age_secs, now);
    }
}

/// The one freshness rule: a reading is current while its age is at most `max_age_secs`; a future timestamp is stale.
pub fn freshness_for(observed_at: Option<i64>, max_age_secs: u64, now: i64) -> Freshness {
    match observed_at {
        None => Freshness::Unknown,
        Some(at) if at > now || now.saturating_sub(at) as u64 > max_age_secs => Freshness::Stale,
        Some(_) => Freshness::Current,
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
    #[serde(default)]
    pub permission: ProviderPermission,
    /// Labels for every window whose observed state is exhausted.
    #[serde(default)]
    pub exhausted_windows: Vec<String>,
    #[serde(default)]
    pub observed_at: Option<i64>,
    #[serde(default = "default_max_age")]
    pub max_age_secs: u64,
    #[serde(default)]
    pub freshness: Freshness,
    /// Seconds left until the provider may be probed again. Recomputed from
    /// `retry_after_until` every time freshness is refreshed.
    #[serde(default)]
    pub retry_after_secs: Option<u64>,
    /// UTC Unix second at which the provider's retry deadline elapses.
    #[serde(default)]
    pub retry_after_until: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    /// Lowercase SHA-256 hex of the provider account id this reading belongs
    /// to. Never the raw id or an email. `None` when the account is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_digest: Option<String>,
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

/// Everything a collector learned from one successful provider response.
pub struct ProviderObservation {
    pub provider: ProviderId,
    pub source: Option<Source>,
    pub windows: Vec<UsageWindow>,
    pub credits: Option<Credits>,
    pub plan: Option<String>,
    pub credential_path: Option<String>,
    pub observed_at: Option<i64>,
    pub max_age_secs: u64,
    pub permission: ProviderPermission,
}

impl ProviderSnapshot {
    pub fn observed(observation: ProviderObservation) -> Self {
        let ProviderObservation {
            provider,
            source,
            windows,
            credits,
            plan,
            credential_path,
            observed_at,
            max_age_secs,
            permission,
        } = observation;
        let readable = windows
            .iter()
            .any(|window| window.state != WindowState::Unknown);
        let error = (!readable && credits.is_none()).then(|| {
            AdapterError::new(
                "unreadable",
                "provider reported windows but none had a readable measurement",
            )
        });
        let mut snapshot = Self {
            provider,
            status: if error.is_some() {
                Availability::Unavailable
            } else {
                Availability::Ok
            },
            permission,
            exhausted_windows: Vec::new(),
            observed_at,
            max_age_secs,
            freshness: Freshness::Unknown,
            retry_after_secs: None,
            retry_after_until: None,
            source,
            account_digest: None,
            windows,
            credits,
            plan,
            error,
            credential_path,
        };
        for window in &mut snapshot.windows {
            if window.observed_at.is_none() {
                window.observed_at = observed_at;
                window.max_age_secs = max_age_secs;
            }
        }
        snapshot.refresh_freshness(crate::timeutil::now_unix());
        snapshot
    }

    pub fn unavailable(provider: ProviderId, error: AdapterError) -> Self {
        Self {
            provider,
            status: Availability::Unavailable,
            permission: ProviderPermission::Unknown,
            exhausted_windows: Vec::new(),
            observed_at: None,
            max_age_secs: DEFAULT_READING_MAX_AGE_SECS,
            freshness: Freshness::Unknown,
            retry_after_secs: None,
            retry_after_until: None,
            source: None,
            account_digest: None,
            windows: Vec::new(),
            credits: None,
            plan: None,
            error: Some(error),
            credential_path: None,
        }
    }

    /// This reading as presented for an account it was not taken for: its
    /// usage, permission and deadline are unknown there. It still names the
    /// account it belongs to.
    pub fn for_another_account(&self) -> Self {
        let mut other = Self::unavailable(
            self.provider,
            AdapterError::new(
                ACCOUNT_CHANGED,
                "account changed since reading; awaiting a refresh for the active account",
            ),
        );
        other.source = self.source;
        other.account_digest = self.account_digest.clone();
        other.observed_at = self.observed_at;
        other.max_age_secs = self.max_age_secs;
        other.credential_path = self.credential_path.clone();
        other.refresh_freshness(crate::timeutil::now_unix());
        other
    }

    /// Whether this reading says anything about an account's quota. One that
    /// says nothing cannot answer for the wrong account.
    pub fn holds_quota_evidence(&self) -> bool {
        !self.windows.is_empty()
            || !self.exhausted_windows.is_empty()
            || self.permission != ProviderPermission::Unknown
            || self.retry_after_secs.is_some()
            || self.retry_after_until.is_some()
            || self.credits.is_some()
            || self.plan.is_some()
    }

    /// A placeholder never tied to an account: it names none and holds no
    /// quota evidence. Every other reading, an error one included, was taken
    /// for an account and answers only for that account.
    pub fn is_unattributed_placeholder(&self) -> bool {
        self.account_digest.is_none() && !self.holds_quota_evidence()
    }

    pub fn is_for_another_account(&self) -> bool {
        self.error
            .as_ref()
            .is_some_and(|error| error.code == ACCOUNT_CHANGED)
    }

    pub fn window(&self, kind: &WindowKind) -> Option<&UsageWindow> {
        self.windows.iter().find(|w| w.kind == *kind)
    }

    pub fn refresh_freshness(&mut self, now: i64) {
        if self.retry_after_until.is_none() {
            self.retry_after_until = self
                .retry_after_secs
                .filter(|seconds| *seconds > 0)
                .map(|seconds| now.saturating_add(seconds.min(MAX_RETRY_AFTER_SECS) as i64));
        }
        if let Some(until) = self.retry_after_until {
            if until > now {
                self.retry_after_secs = Some(until.abs_diff(now));
            } else {
                self.retry_after_secs = None;
                self.retry_after_until = None;
            }
        }
        for window in &mut self.windows {
            window.refresh_freshness(now);
        }
        self.exhausted_windows = self
            .windows
            .iter()
            .filter(|window| window.state == WindowState::Exhausted)
            .map(|window| window.label.clone())
            .collect();
        self.freshness = freshness_for(self.observed_at, self.max_age_secs, now);
        if self
            .windows
            .iter()
            .any(|window| window.freshness != Freshness::Current)
        {
            self.freshness = Freshness::Stale;
        }
        if self.status != Availability::Unavailable {
            self.status = if self.freshness == Freshness::Current {
                Availability::Ok
            } else {
                Availability::Stale
            };
        }
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

    pub fn refreshed_at(&self, now: i64) -> Self {
        let mut snapshot = self.clone();
        for provider in &mut snapshot.providers {
            provider.refresh_freshness(now);
        }
        snapshot
    }

    /// Seconds until the first reading in this snapshot stops being current,
    /// or `None` when nothing is still current.
    pub fn secs_until_next_expiry(&self, now: i64) -> Option<u64> {
        self.providers
            .iter()
            .flat_map(|provider| {
                std::iter::once((provider.observed_at, provider.max_age_secs)).chain(
                    provider
                        .windows
                        .iter()
                        .map(|window| (window.observed_at, window.max_age_secs)),
                )
            })
            .filter_map(|(observed_at, max_age_secs)| {
                let expires_at =
                    observed_at?.saturating_add(max_age_secs.min(i64::MAX as u64) as i64);
                (expires_at >= now).then(|| expires_at.abs_diff(now).saturating_add(1))
            })
            .min()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CanStartBasis {
    TokenBudget,
    /// `--percent` admission: headroom above a reserve plus a pace projection.
    PercentBudget,
    PercentOnly,
    Unavailable,
    /// The provider reported a window whose measurement could not be read, so
    /// that limit may already be exhausted.
    UnknownWindow,
    /// The reading was taken for a provider account other than the one the
    /// credentials name now.
    AccountChanged,
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
    /// Present only for `--percent` admission; names each test that was applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission: Option<PercentAdmission>,
}

/// The tests behind a percent admission: `remaining - requested >= reserve`,
/// the pace projection not emptying the window before it resets, and (when one
/// was asked for) the job fitting before the deadline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PercentAdmission {
    pub requested_percent: f64,
    pub reserve_percent: f64,
    pub remaining_after_percent: f64,
    pub headroom_ok: bool,
    pub pace_ok: bool,
    /// False when no burn rate or no published reset existed, so pace could not veto.
    pub pace_checked: bool,
    /// True when no deadline was requested or the job fits before it. Absent in
    /// answers from older daemons, which read as true.
    #[serde(default = "default_true")]
    pub deadline_ok: bool,
}

fn default_true() -> bool {
    true
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

#[cfg(test)]
mod tests {
    use super::ActiveIdentity::{Absent, Named, Unnamed};
    use super::*;

    #[test]
    fn old_evidence_is_never_serialized_as_current() {
        let window = UsageWindow::from_percent_at(
            WindowKind::Weekly,
            "weekly",
            20.0,
            None,
            Some(604_800),
            Some(1_000),
            60,
        );
        let provider = ProviderSnapshot::observed(ProviderObservation {
            provider: ProviderId::Codex,
            source: Some(Source::Oauth),
            windows: vec![window],
            credits: None,
            plan: None,
            credential_path: None,
            observed_at: Some(1_000),
            max_age_secs: 60,
            permission: ProviderPermission::Allowed,
        });
        let snapshot = Snapshot::new(1_000, vec![provider]).refreshed_at(1_061);
        let codex = snapshot.by_id(ProviderId::Codex).unwrap();
        assert_eq!(codex.status, Availability::Stale);
        assert_eq!(codex.freshness, Freshness::Stale);
        assert_eq!(codex.observed_at, Some(1_000));
        assert_eq!(codex.windows[0].freshness, Freshness::Stale);
        assert_eq!(codex.windows[0].observed_at, Some(1_000));
        assert_eq!(codex.windows[0].max_age_secs, 60);
    }

    #[test]
    fn evidence_within_max_age_is_current() {
        let mut provider = ProviderSnapshot::observed(ProviderObservation {
            provider: ProviderId::Codex,
            source: Some(Source::Oauth),
            windows: vec![UsageWindow::from_percent_at(
                WindowKind::Weekly,
                "weekly",
                20.0,
                None,
                None,
                Some(1_000),
                60,
            )],
            credits: None,
            plan: None,
            credential_path: None,
            observed_at: Some(1_000),
            max_age_secs: 60,
            permission: ProviderPermission::Allowed,
        });
        provider.refresh_freshness(1_060);
        assert_eq!(provider.status, Availability::Ok);
        assert_eq!(provider.freshness, Freshness::Current);
    }

    fn observation(windows: Vec<UsageWindow>) -> ProviderObservation {
        ProviderObservation {
            provider: ProviderId::Claude,
            source: Some(Source::Oauth),
            windows,
            credits: None,
            plan: None,
            credential_path: None,
            observed_at: Some(1_000),
            max_age_secs: 60,
            permission: ProviderPermission::Unknown,
        }
    }

    #[test]
    fn only_unreadable_windows_are_not_a_healthy_provider() {
        let unreadable =
            UsageWindow::unreadable(WindowKind::FiveHour, "5h", None, None, Some(1_000), 60);
        let provider = ProviderSnapshot::observed(observation(vec![unreadable.clone()]));
        assert_eq!(provider.status, Availability::Unavailable);
        assert_eq!(provider.error.as_ref().unwrap().code, "unreadable");
        assert_eq!(provider.windows.len(), 1);

        let readable = UsageWindow::from_percent_at(
            WindowKind::Weekly,
            "weekly",
            10.0,
            None,
            None,
            Some(1_000),
            60,
        );
        let mixed = ProviderSnapshot::observed(observation(vec![unreadable, readable]));
        assert!(mixed.error.is_none());
    }

    #[test]
    fn retry_after_counts_down_from_a_fixed_deadline() {
        let mut provider = ProviderSnapshot::unavailable(
            ProviderId::Claude,
            AdapterError::new("rate_limited", "HTTP 429"),
        );
        provider.retry_after_secs = Some(120);
        provider.refresh_freshness(1_000);
        assert_eq!(provider.retry_after_until, Some(1_120));
        assert_eq!(provider.retry_after_secs, Some(120));
        provider.refresh_freshness(1_100);
        assert_eq!(provider.retry_after_secs, Some(20));
        provider.refresh_freshness(1_120);
        assert_eq!(provider.retry_after_secs, None);
    }

    #[test]
    fn coderabbit_elapsed_retry_deadline_is_cleared_so_a_new_one_can_start() {
        let mut provider = ProviderSnapshot::unavailable(
            ProviderId::Claude,
            AdapterError::new("rate_limited", "HTTP 429"),
        );
        provider.retry_after_secs = Some(120);
        provider.refresh_freshness(1_000);
        provider.refresh_freshness(1_120);
        assert_eq!(provider.retry_after_secs, None);
        assert_eq!(provider.retry_after_until, None);

        provider.retry_after_secs = Some(30);
        provider.refresh_freshness(2_000);
        assert_eq!(provider.retry_after_until, Some(2_030));
        assert_eq!(provider.retry_after_secs, Some(30));
    }

    #[test]
    fn coderabbit_every_retry_deadline_is_finite() {
        let mut provider = ProviderSnapshot::unavailable(
            ProviderId::Codex,
            AdapterError::new("rate_limited", "HTTP 429"),
        );
        provider.retry_after_secs = Some(u64::MAX);
        provider.refresh_freshness(1_000);
        assert_eq!(
            provider.retry_after_until,
            Some(1_000 + MAX_RETRY_AFTER_SECS as i64)
        );
        assert_eq!(provider.retry_after_secs, Some(MAX_RETRY_AFTER_SECS));
    }

    #[test]
    fn account_digest_is_omitted_when_unknown_and_read_back_when_present() {
        let mut provider =
            ProviderSnapshot::unavailable(ProviderId::Codex, AdapterError::new("x", "y"));
        let json = serde_json::to_value(&provider).unwrap();
        assert!(json.get("account_digest").is_none());
        provider.account_digest = Some("ab".repeat(32));
        let back: ProviderSnapshot =
            serde_json::from_value(serde_json::to_value(&provider).unwrap()).unwrap();
        assert_eq!(back.account_digest, provider.account_digest);
    }

    #[test]
    fn next_expiry_names_the_first_reading_to_go_stale() {
        let window = UsageWindow::from_percent_at(
            WindowKind::Weekly,
            "weekly",
            10.0,
            None,
            None,
            Some(1_000),
            60,
        );
        let provider = ProviderSnapshot::observed(observation(vec![window]));
        let snapshot = Snapshot::new(1_000, vec![provider]);
        assert_eq!(snapshot.secs_until_next_expiry(1_000), Some(61));
        assert_eq!(snapshot.secs_until_next_expiry(1_060), Some(1));
        assert_eq!(snapshot.secs_until_next_expiry(1_061), None);
    }

    #[test]
    fn round4_named_readings_answer_only_for_the_same_named_account() {
        let (a, b) = ("a".repeat(64), "b".repeat(64));
        let none = Vec::new;
        let (named_a, named_b) = (Named(a.clone()), Named(b.clone()));
        assert!(reading_answers_for_active_account(Some(&a), &named_a, none));
        assert!(!reading_answers_for_active_account(
            Some(&a),
            &named_b,
            none
        ));
        // Two named accounts decide alone; other sources are never consulted.
        assert!(reading_answers_for_active_account(
            Some(&a),
            &named_a,
            || -> Vec<Option<String>> { panic!("not consulted") }
        ));
    }

    #[test]
    fn round4_an_unnamed_side_answers_only_while_one_account_is_known() {
        let (a, b) = ("a".repeat(64), "b".repeat(64));
        let (named_a, named_b) = (Named(a.clone()), Named(b.clone()));
        let only_a = || vec![Some("a".repeat(64))];
        let a_and_b = || vec![Some("a".repeat(64)), Some("b".repeat(64))];
        let unnamed = || vec![None];

        assert!(reading_answers_for_active_account(None, &Unnamed, Vec::new));
        assert!(reading_answers_for_active_account(
            Some(&a),
            &Unnamed,
            Vec::new
        ));
        assert!(reading_answers_for_active_account(
            Some(&a),
            &Unnamed,
            only_a
        ));
        assert!(reading_answers_for_active_account(None, &named_a, only_a));
        assert!(reading_answers_for_active_account(None, &Unnamed, unnamed));

        assert!(!reading_answers_for_active_account(
            Some(&a),
            &Unnamed,
            a_and_b
        ));
        assert!(!reading_answers_for_active_account(None, &named_b, only_a));
        assert!(!reading_answers_for_active_account(None, &Unnamed, a_and_b));
        assert!(!reading_answers_for_active_account(
            Some(&a),
            &Unnamed,
            unnamed
        ));
    }

    #[test]
    fn round5_absent_credentials_with_no_other_source_refuse() {
        let a = "a".repeat(64);
        assert!(!reading_answers_for_active_account(None, &Absent, Vec::new));
        // Credentials removed after a named reading: nothing ties it to a
        // live account.
        assert!(!reading_answers_for_active_account(
            Some(&a),
            &Absent,
            Vec::new
        ));
    }

    #[test]
    fn round5_absent_credentials_answer_only_through_one_other_source() {
        let a = "a".repeat(64);
        let only_a = || vec![Some("a".repeat(64))];
        let only_b = || vec![Some("b".repeat(64))];
        let a_and_b = || vec![Some("a".repeat(64)), Some("b".repeat(64))];
        assert!(reading_answers_for_active_account(
            Some(&a),
            &Absent,
            only_a
        ));
        assert!(reading_answers_for_active_account(None, &Absent, only_a));
        assert!(!reading_answers_for_active_account(
            Some(&a),
            &Absent,
            only_b
        ));
        assert!(!reading_answers_for_active_account(None, &Absent, a_and_b));
    }

    #[test]
    fn round5_unnamed_credentials_with_no_other_source_answer() {
        assert!(reading_answers_for_active_account(None, &Unnamed, Vec::new));
    }

    #[test]
    fn round5_unnamed_credentials_with_two_codexbar_accounts_refuse() {
        let two = || vec![Some("a".repeat(64)), Some("b".repeat(64))];
        let two_unnamed = || vec![None, None];
        assert!(!reading_answers_for_active_account(None, &Unnamed, two));
        assert!(!reading_answers_for_active_account(
            None,
            &Unnamed,
            two_unnamed
        ));
    }

    #[test]
    fn round5_a_reading_without_quota_evidence_has_nothing_to_misattribute() {
        let no_credentials = ProviderSnapshot::unavailable(
            ProviderId::Codex,
            AdapterError::new("no_credentials", "missing"),
        );
        assert!(!no_credentials.holds_quota_evidence());

        let mut refused = no_credentials.clone();
        refused.permission = ProviderPermission::LimitReached;
        assert!(refused.holds_quota_evidence());
        let mut deadline = no_credentials.clone();
        deadline.retry_after_secs = Some(30);
        assert!(deadline.holds_quota_evidence());
        let mut windowed = no_credentials;
        windowed.windows = vec![UsageWindow::from_percent(
            WindowKind::Weekly,
            "weekly",
            40.0,
            None,
            None,
        )];
        assert!(windowed.holds_quota_evidence());
    }

    #[test]
    fn round4_a_reading_for_another_account_keeps_only_its_owner() {
        let mut reading = ProviderSnapshot::unavailable(
            ProviderId::Codex,
            AdapterError::new("rate_limited", "HTTP 429"),
        );
        reading.permission = ProviderPermission::LimitReached;
        reading.retry_after_secs = Some(120);
        reading.account_digest = Some("a".repeat(64));
        reading.windows = vec![UsageWindow::from_percent(
            WindowKind::Weekly,
            "weekly",
            40.0,
            None,
            None,
        )];

        let elsewhere = reading.for_another_account();

        assert!(elsewhere.is_for_another_account());
        assert!(!reading.is_for_another_account());
        assert_eq!(elsewhere.account_digest, reading.account_digest);
        assert_eq!(elsewhere.status, Availability::Unavailable);
        assert_eq!(elsewhere.permission, ProviderPermission::Unknown);
        assert!(elsewhere.windows.is_empty());
        assert_eq!(elsewhere.retry_after_secs, None);
        assert_eq!(elsewhere.retry_after_until, None);
    }
}
