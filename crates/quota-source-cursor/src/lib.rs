//! Cursor plan usage and on-demand spend.
//!
//! # Source
//! `GET https://cursor.com/api/usage-summary` with the dashboard session
//! cookie (`WorkosCursorSessionToken`). This is the endpoint cursor.com's own
//! usage page calls; it is not a versioned public API, so every field is
//! optional and a shape change becomes a typed error, never a guessed number.
//! The request and response shapes follow CodexBar's open-source collector
//! at the immutable commit `steipete/CodexBar@25bba9b7`:
//! - <https://github.com/steipete/CodexBar/blob/25bba9b7fd9ce83c33053958f7366e23b2dc8a82/Sources/CodexBarCore/Providers/Cursor/CursorStatusProbe.swift#L1404-L1438>
//!   (`GET /api/usage-summary`, `Cookie` header, 401/403 = not logged in)
//! - <https://github.com/steipete/CodexBar/blob/25bba9b7fd9ce83c33053958f7366e23b2dc8a82/Sources/CodexBarCore/Providers/Cursor/CursorStatusProbe.swift#L201-L278>
//!   (`CursorUsageSummary`, `CursorPlanUsage`, `CursorOnDemandUsage`: `used`,
//!   `limit`, `remaining` in **cents**, `totalPercentUsed`, ISO-8601
//!   `billingCycleStart` / `billingCycleEnd`)
//! - <https://github.com/steipete/CodexBar/blob/25bba9b7fd9ce83c33053958f7366e23b2dc8a82/Sources/CodexBarCore/Providers/Cursor/CursorStatusProbe+UsageSummary.swift#L43-L61>
//!   (CodexBar's percent precedence; we prefer cents, see `docs/SOURCES.md`)
//!
//! # Credentials
//! The cookie comes only from `quota-secrets`, first hit wins: the OS keychain,
//! then OpenBao when configured, then a private read-only file
//! ([`quota_secrets::keychain_first_from_env`]), at the logical path the user
//! configured. It goes
//! into one request header. It is never logged, never put in an error, and
//! never echoed with a response.

#![forbid(unsafe_code)]

use quota_adapters::provider::{ProbeCtx, Provider};
use quota_core::timeutil::parse_reset_at_str;
use quota_core::types::{
    AdapterError, Credits, ProviderId, ProviderObservation, ProviderPermission, ProviderSnapshot,
    Source, UsageWindow, WindowKind, WindowState,
};
use quota_secrets::{SecretsBackend, SecretsError};
use serde::Deserialize;
use thiserror::Error;

pub const USAGE_URL: &str = "https://cursor.com/api/usage-summary";
const SESSION_COOKIE: &str = "WorkosCursorSessionToken";

/// Usage moves slowly and the endpoint is a dashboard route: evidence stays
/// current for 15 minutes and the daemon polls it no more often than 5.
pub const EVIDENCE_MAX_AGE_SECS: u64 = 900;
pub const MIN_POLL_SECS: u64 = 300;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CursorError {
    #[error("usage-summary JSON: {0}")]
    Json(String),
    #[error("usage-summary carried no readable plan or on-demand usage (shape may have changed)")]
    Empty,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CookieError {
    #[error("the stored Cursor credential is empty")]
    Empty,
    #[error("the stored Cursor credential has characters that cannot go in an HTTP header")]
    Unsafe,
    #[error("the stored Cursor credential is neither a bare session token nor a Cookie header with a WorkosCursorSessionToken")]
    Shape,
}

pub struct CursorAdapter {
    secrets: Box<dyn SecretsBackend>,
    secret_path: String,
}

impl CursorAdapter {
    pub fn new(secrets: Box<dyn SecretsBackend>, secret_path: impl Into<String>) -> Self {
        Self {
            secrets,
            secret_path: secret_path.into(),
        }
    }

    /// Looks the cookie up in the OS keychain, then OpenBao (built with the
    /// `openbao` feature and configured), then the read-only cookie file.
    pub fn from_keychain_first_chain(secret_path: impl Into<String>) -> Result<Self, SecretsError> {
        let secret_path = secret_path.into();
        Ok(Self::new(
            Box::new(quota_secrets::keychain_first_from_env_for_path(
                secret_path.clone(),
            )?),
            secret_path,
        ))
    }

    fn unavailable(&self, code: &str, message: impl Into<String>) -> ProviderSnapshot {
        let mut snap =
            ProviderSnapshot::unavailable(ProviderId::Cursor, AdapterError::new(code, message));
        snap.source = Some(Source::Cookie);
        snap.credential_path = Some(self.secret_path.clone());
        snap
    }
}

impl Provider for CursorAdapter {
    fn id(&self) -> ProviderId {
        ProviderId::Cursor
    }

    fn probe(&self, ctx: &ProbeCtx<'_>) -> ProviderSnapshot {
        let secret = match self.secrets.get(&self.secret_path) {
            Ok(Some(record)) => record.value,
            Ok(None) => {
                return self.unavailable(
                    "no_credentials",
                    format!(
                        "no Cursor session cookie stored at {} (store WorkosCursorSessionToken with `quota-ctl secret put`)",
                        self.secret_path
                    ),
                );
            }
            Err(e) => return self.unavailable("secrets", e.to_string()),
        };
        let cookie = match cookie_header(&secret) {
            Ok(cookie) => cookie,
            Err(e) => return self.unavailable("bad_credential", e.to_string()),
        };
        let headers = [
            ("Cookie", cookie.as_str()),
            ("Accept", "application/json"),
            ("User-Agent", "quota/0.1.0"),
        ];
        match ctx.transport.get(USAGE_URL, &headers) {
            Ok(resp) => {
                self.snapshot_for_response(resp.status, &resp.body, resp.retry_after_secs, ctx.now)
            }
            Err(e) => self.unavailable("network", format!("usage probe failed: {e}")),
        }
    }
}

impl CursorAdapter {
    fn snapshot_for_response(
        &self,
        status: u16,
        body: &[u8],
        retry_after_secs: Option<u64>,
        now: i64,
    ) -> ProviderSnapshot {
        let mut failure = match status {
            401 | 403 => self.unavailable(
                "unauthorized",
                format!("HTTP {status}: Cursor rejected the session cookie; copy a fresh WorkosCursorSessionToken"),
            ),
            429 => self.unavailable("rate_limited", "HTTP 429 from cursor.com/api/usage-summary"),
            s if !(200..300).contains(&s) => self.unavailable("http", format!("HTTP {s}")),
            _ => {
                return match parse_usage_summary(body, now) {
                    Ok(summary) => self.observed(summary, now),
                    Err(e) => {
                        let code = match e {
                            CursorError::Json(_) => "parse",
                            CursorError::Empty => "empty",
                        };
                        self.unavailable(code, e.to_string())
                    }
                };
            }
        };
        failure.retry_after_secs = retry_after_secs;
        failure
    }

    fn observed(&self, summary: Summary, now: i64) -> ProviderSnapshot {
        ProviderSnapshot::observed(ProviderObservation {
            provider: ProviderId::Cursor,
            source: Some(Source::Cookie),
            windows: summary.windows,
            credits: summary.unlimited.then(|| Credits {
                balance: None,
                unlimited: Some(true),
                has_credits: None,
                unit: Some("usd".into()),
            }),
            plan: summary.plan,
            credential_path: Some(self.secret_path.clone()),
            observed_at: Some(now),
            max_age_secs: EVIDENCE_MAX_AGE_SECS,
            permission: ProviderPermission::Unknown,
        })
    }
}

/// The stored value is one of two shapes, told apart by its grammar:
/// - a bare `WorkosCursorSessionToken` value: no `=`, `;`, `,`, quote,
///   backslash or whitespace. Its `::` separator is percent-encoded the way
///   the browser stores it.
/// - a whole `Cookie:` header value: `name=value` pairs joined by `;`, one of
///   them a non-empty `WorkosCursorSessionToken`. It is sent as stored.
///
/// Anything else is refused before a request is made.
pub fn cookie_header(secret: &str) -> Result<String, CookieError> {
    let secret = secret.trim();
    if secret.is_empty() {
        return Err(CookieError::Empty);
    }
    if !secret.bytes().all(|b| (0x20..0x7f).contains(&b)) {
        return Err(CookieError::Unsafe);
    }
    if secret.bytes().all(is_bare_token_byte) {
        return Ok(format!(
            "{SESSION_COOKIE}={}",
            secret.replace("::", "%3A%3A")
        ));
    }
    if is_session_cookie_header(secret) {
        return Ok(secret.to_string());
    }
    Err(CookieError::Shape)
}

fn is_bare_token_byte(b: u8) -> bool {
    b.is_ascii_graphic() && !matches!(b, b'=' | b';' | b',' | b'"' | b'\\')
}

fn is_session_cookie_header(header: &str) -> bool {
    let mut has_session = false;
    for pair in header.split(';').map(str::trim).filter(|p| !p.is_empty()) {
        let Some((name, value)) = pair.split_once('=') else {
            return false;
        };
        let name_ok = !name.is_empty() && name.bytes().all(is_bare_token_byte);
        let value_ok = value
            .bytes()
            .all(|b| b.is_ascii_graphic() && !matches!(b, b';' | b',' | b'\\'));
        if !name_ok || !value_ok {
            return false;
        }
        has_session |= name == SESSION_COOKIE && !value.is_empty();
    }
    has_session
}

#[derive(Debug, PartialEq)]
pub struct Summary {
    pub windows: Vec<UsageWindow>,
    pub plan: Option<String>,
    pub unlimited: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageSummary {
    billing_cycle_start: Option<String>,
    billing_cycle_end: Option<String>,
    membership_type: Option<String>,
    is_unlimited: Option<bool>,
    individual_usage: Option<IndividualUsage>,
}

#[derive(Deserialize)]
struct IndividualUsage {
    plan: Option<Bucket>,
    #[serde(rename = "onDemand")]
    on_demand: Option<Bucket>,
}

/// Shared shape of `plan` and `onDemand`: `used` / `limit` / `remaining` are
/// cents. `totalPercentUsed` exists on `plan` only.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Bucket {
    enabled: Option<bool>,
    used: Option<f64>,
    limit: Option<f64>,
    total_percent_used: Option<f64>,
}

fn cents_to_usd(cents: f64) -> Option<f64> {
    (cents.is_finite() && cents >= 0.0).then(|| cents / 100.0)
}

pub fn parse_usage_summary(body: &[u8], now: i64) -> Result<Summary, CursorError> {
    let summary: UsageSummary =
        serde_json::from_slice(body).map_err(|e| CursorError::Json(e.to_string()))?;
    let reset = summary
        .billing_cycle_end
        .as_deref()
        .and_then(parse_reset_at_str);
    let cycle_secs = match (
        summary
            .billing_cycle_start
            .as_deref()
            .and_then(parse_reset_at_str),
        reset,
    ) {
        (Some(start), Some(end)) if end > start => Some(end - start),
        _ => None,
    };
    let mut windows = Vec::new();
    if let Some(usage) = summary.individual_usage {
        for (label, bucket, percent_fallback) in [
            ("included", usage.plan, true),
            ("on-demand", usage.on_demand, false),
        ] {
            if let Some(bucket) = bucket.filter(|b| b.enabled != Some(false)) {
                windows.push(window(
                    label,
                    &bucket,
                    reset,
                    cycle_secs,
                    now,
                    percent_fallback,
                ));
            }
        }
    }
    let unlimited = summary.is_unlimited == Some(true);
    if !unlimited && windows.iter().all(|w| w.state == WindowState::Unknown) {
        return Err(CursorError::Empty);
    }
    Ok(Summary {
        windows,
        plan: summary
            .membership_type
            .map(|m| m.chars().filter(|c| !c.is_control()).take(32).collect()),
        unlimited,
    })
}

/// Every enabled bucket becomes a window. Dollars when the bucket has cents,
/// the published percent otherwise, and an `unknown` window when it has
/// neither: a reported bucket never disappears, and percent admission refuses
/// while it is unknown.
fn window(
    label: &str,
    bucket: &Bucket,
    reset: Option<i64>,
    cycle_secs: Option<i64>,
    now: i64,
    percent_fallback: bool,
) -> UsageWindow {
    let observed = Some(now);
    let limit_usd = bucket.limit.and_then(cents_to_usd);
    let percent = bucket
        .total_percent_used
        .filter(|p| percent_fallback && p.is_finite() && *p >= 0.0);
    let used_usd = bucket.used.and_then(cents_to_usd);
    match (used_usd, percent, limit_usd) {
        (Some(used_usd), _, Some(limit_usd)) => UsageWindow::from_spend_at(
            WindowKind::Spend,
            label,
            used_usd,
            Some(limit_usd),
            reset,
            cycle_secs,
            observed,
            EVIDENCE_MAX_AGE_SECS,
        ),
        (Some(used_usd), Some(percent), None) => {
            let mut window = UsageWindow::from_percent_at(
                WindowKind::Spend,
                label,
                percent,
                reset,
                cycle_secs,
                observed,
                EVIDENCE_MAX_AGE_SECS,
            );
            window.unit = Some("usd".to_string());
            window.used_usd = Some(used_usd);
            if let Some(reading) = window.reading.as_mut() {
                reading.unit = Some("usd".to_string());
                reading.used_usd = Some(used_usd);
            }
            window
        }
        (_, Some(percent), _) => UsageWindow::from_percent_at(
            WindowKind::Spend,
            label,
            percent,
            reset,
            cycle_secs,
            observed,
            EVIDENCE_MAX_AGE_SECS,
        ),
        (Some(used_usd), None, None) => UsageWindow::from_spend_at(
            WindowKind::Spend,
            label,
            used_usd,
            None,
            reset,
            cycle_secs,
            observed,
            EVIDENCE_MAX_AGE_SECS,
        ),
        (None, None, _) => UsageWindow::unreadable(
            WindowKind::Spend,
            label,
            reset,
            cycle_secs,
            observed,
            EVIDENCE_MAX_AGE_SECS,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quota_adapters::http::{HttpResponse, Transport, TransportError};
    use quota_core::math::can_start_percent;
    use quota_core::types::{Availability, CanStartBasis};
    use quota_secrets::MemoryBackend;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    /// After `billingCycleStart`, before `billingCycleEnd` in the fixtures.
    const NOW: i64 = 1_759_500_000;

    fn fixture(name: &str) -> Vec<u8> {
        let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/cursor")
            .join(name);
        std::fs::read(path).unwrap()
    }

    struct Recording {
        response: Result<HttpResponse, TransportError>,
        seen: Mutex<Vec<(String, String)>>,
    }

    impl Recording {
        fn ok(status: u16, body: Vec<u8>) -> Self {
            Self {
                response: Ok(HttpResponse {
                    status,
                    body,
                    retry_after_secs: None,
                }),
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    impl Transport for Recording {
        fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<HttpResponse, TransportError> {
            let mut seen = self.seen.lock().unwrap();
            seen.push((url.to_string(), String::new()));
            for (k, v) in headers {
                if k.eq_ignore_ascii_case("cookie") {
                    seen.last_mut().unwrap().1 = (*v).to_string();
                }
            }
            self.response.clone()
        }
    }

    fn adapter_with(secret: Option<&str>) -> CursorAdapter {
        let mem = MemoryBackend::new();
        if let Some(secret) = secret {
            mem.put("cursor/session", secret).unwrap();
        }
        CursorAdapter::new(Box::new(mem), "cursor/session")
    }

    fn probe(adapter: &CursorAdapter, transport: &Recording) -> ProviderSnapshot {
        adapter.probe(&ProbeCtx {
            transport,
            now: quota_core::timeutil::now_unix(),
        })
    }

    #[test]
    fn plan_and_on_demand_map_to_dollar_windows_golden() {
        let summary = parse_usage_summary(&fixture("usage-summary.pro-plus.json"), NOW).unwrap();
        assert_eq!(summary.plan.as_deref(), Some("pro_plus"));
        assert_eq!(summary.windows.len(), 2);

        let included = &summary.windows[0];
        assert_eq!(included.kind, WindowKind::Spend);
        assert_eq!(included.label, "included");
        assert_eq!(included.used_usd, Some(42.5));
        assert_eq!(included.limit_usd, Some(60.0));
        assert_eq!(included.remaining, Some(17.5));
        assert!((included.used_percent.unwrap() - 70.8333333).abs() < 1e-4);
        assert_eq!(included.reset_at, Some(1_761_955_200));
        assert_eq!(included.limit_window_seconds, Some(2_678_400));

        let on_demand = &summary.windows[1];
        assert_eq!(on_demand.label, "on-demand");
        assert_eq!(on_demand.used_usd, Some(3.0));
        assert_eq!(on_demand.limit_usd, Some(20.0));
        assert_eq!(on_demand.used_percent, Some(15.0));
        assert_eq!(on_demand.state, WindowState::Ok);
    }

    #[test]
    fn on_demand_without_a_cap_records_spend_and_no_headroom() {
        let summary = parse_usage_summary(&fixture("usage-summary.uncapped.json"), NOW).unwrap();
        let on_demand = summary
            .windows
            .iter()
            .find(|w| w.label == "on-demand")
            .unwrap();
        assert_eq!(on_demand.used_usd, Some(7.25));
        assert_eq!(on_demand.limit_usd, None);
        assert_eq!(on_demand.remaining_percent, None);
        assert_eq!(on_demand.state, WindowState::Ok);
    }

    #[test]
    fn disabled_buckets_are_skipped_and_percent_backs_up_missing_cents() {
        let summary =
            parse_usage_summary(&fixture("usage-summary.percent-only.json"), NOW).unwrap();
        assert_eq!(summary.windows.len(), 1);
        assert_eq!(summary.windows[0].used_percent, Some(64.5));
        assert_eq!(summary.windows[0].used_usd, None);
    }

    #[test]
    fn published_plan_percent_survives_when_used_cents_have_no_limit() {
        let body = br#"{"individualUsage":{"plan":{"used":1250,"totalPercentUsed":135}}}"#;
        let snap = probe(
            &adapter_with(Some("test-session")),
            &Recording::ok(200, body.to_vec()),
        );
        let included = &snap.windows[0];
        assert_eq!(included.used_percent, Some(135.0));
        assert_eq!(included.remaining_percent, Some(0.0));
        assert_eq!(included.used_usd, Some(12.5));
        assert_eq!(included.limit_usd, None);

        let answer = can_start_percent(&snap, &[], 1.0, 2.0, None, snap.observed_at.unwrap());
        assert!(!answer.ok);
        assert_eq!(answer.basis, CanStartBasis::PercentBudget);
    }

    #[test]
    fn unlimited_plan_without_windows_is_credits_not_an_error() {
        let summary = parse_usage_summary(&fixture("usage-summary.unlimited.json"), NOW).unwrap();
        assert!(summary.unlimited && summary.windows.is_empty());
    }

    #[test]
    fn decode_errors_are_typed() {
        assert!(matches!(
            parse_usage_summary(b"<html>", NOW),
            Err(CursorError::Json(_))
        ));
        assert_eq!(parse_usage_summary(b"{}", NOW), Err(CursorError::Empty));
        assert_eq!(
            parse_usage_summary(br#"{"individualUsage":{"plan":{"enabled":false}}}"#, NOW),
            Err(CursorError::Empty)
        );
        assert!(matches!(
            parse_usage_summary(br#"{"individualUsage":{"plan":{"used":"lots"}}}"#, NOW),
            Err(CursorError::Json(_))
        ));
    }

    #[test]
    fn negative_or_nonfinite_cents_are_not_readings() {
        let body = br#"{"individualUsage":{"plan":{"used":-5,"limit":6000}}}"#;
        assert_eq!(parse_usage_summary(body, NOW), Err(CursorError::Empty));
    }

    fn assert_kept_unknown(summary: &Summary, label: &str) {
        let kept = summary
            .windows
            .iter()
            .find(|w| w.label == label)
            .unwrap_or_else(|| panic!("{label} was dropped: {:?}", summary.windows));
        assert_eq!(kept.state, WindowState::Unknown);
        assert!(kept.reading.is_none());
        assert_eq!(kept.used_usd, None);
        assert_eq!(kept.remaining_percent, None);
    }

    #[test]
    fn an_unreadable_bucket_is_not_dropped_beside_a_readable_one() {
        let body = br#"{"individualUsage":{"plan":{"used":-5,"limit":6000},
            "onDemand":{"enabled":true,"used":100,"limit":5000}}}"#;
        let summary = parse_usage_summary(body, NOW).unwrap();
        assert_eq!(summary.windows.len(), 2);
        assert_kept_unknown(&summary, "included");
        let body = br#"{"individualUsage":{"plan":{"used":100,"limit":6000},
            "onDemand":{"enabled":true,"used":-1,"limit":5000}}}"#;
        let summary = parse_usage_summary(body, NOW).unwrap();
        assert_eq!(summary.windows.len(), 2);
        assert_kept_unknown(&summary, "on-demand");
    }

    #[test]
    fn an_enabled_bucket_without_usage_is_kept_unknown_beside_a_readable_one() {
        for on_demand in [
            r#"{"enabled":true}"#,
            r#"{"enabled":true,"used":null,"limit":5000}"#,
            r#"{}"#,
        ] {
            let body = format!(
                r#"{{"individualUsage":{{"plan":{{"used":100,"limit":6000}},"onDemand":{on_demand}}}}}"#
            );
            let summary = parse_usage_summary(body.as_bytes(), NOW).unwrap();
            assert_eq!(summary.windows.len(), 2, "{on_demand}");
            assert_kept_unknown(&summary, "on-demand");
        }
        let body = br#"{"individualUsage":{"plan":{"enabled":true,"limit":6000},
            "onDemand":{"enabled":true,"used":100,"limit":5000}}}"#;
        assert_kept_unknown(&parse_usage_summary(body, NOW).unwrap(), "included");
    }

    #[test]
    fn a_kept_unknown_bucket_refuses_percent_admission_end_to_end() {
        let body = br#"{"billingCycleEnd":"2099-01-01T00:00:00.000Z",
            "individualUsage":{"plan":{"used":100,"limit":6000},"onDemand":{"enabled":true}}}"#;
        let snap = probe(
            &adapter_with(Some("tok")),
            &Recording::ok(200, body.to_vec()),
        );
        assert_eq!(snap.status, Availability::Ok);
        let answer = can_start_percent(&snap, &[], 1.0, 2.0, None, snap.observed_at.unwrap());
        assert!(!answer.ok, "{}", answer.explanation);
        assert_eq!(answer.basis, CanStartBasis::UnknownWindow);
        assert!(
            answer
                .explanation
                .contains("on-demand has no readable measurement"),
            "{}",
            answer.explanation
        );
    }

    #[test]
    fn probe_sends_the_cookie_to_the_usage_endpoint_and_maps_the_reading() {
        let transport = Recording::ok(200, fixture("usage-summary.pro-plus.json"));
        let snap = probe(&adapter_with(Some("user_01ABC::jwt.part")), &transport);
        assert_eq!(snap.status, Availability::Ok);
        assert_eq!(snap.source, Some(Source::Cookie));
        assert_eq!(snap.max_age_secs, EVIDENCE_MAX_AGE_SECS);
        let seen = transport.seen.lock().unwrap();
        assert_eq!(seen[0].0, USAGE_URL);
        assert_eq!(
            seen[0].1,
            "WorkosCursorSessionToken=user_01ABC%3A%3Ajwt.part"
        );
    }

    #[test]
    fn the_cookie_never_reaches_a_snapshot_or_an_error() {
        let secret = "user_01SECRETVALUE::jwt-secret-part";
        for (status, body) in [
            (200, fixture("usage-summary.pro-plus.json")),
            (200, b"not json".to_vec()),
            (401, b"{}".to_vec()),
            (500, format!("echo {secret}").into_bytes()),
        ] {
            let snap = probe(&adapter_with(Some(secret)), &Recording::ok(status, body));
            let dumped = serde_json::to_string(&snap).unwrap();
            assert!(!dumped.contains("SECRETVALUE"), "{dumped}");
            assert!(!dumped.contains("jwt-secret-part"), "{dumped}");
        }
        let failing = Recording {
            response: Err(TransportError::Message("connect: refused".into())),
            seen: Mutex::new(Vec::new()),
        };
        let dumped = serde_json::to_string(&probe(&adapter_with(Some(secret)), &failing)).unwrap();
        assert!(!dumped.contains("SECRETVALUE"));
    }

    #[test]
    fn missing_credentials_and_bad_credentials_are_typed_and_make_no_request() {
        let transport = Recording::ok(200, Vec::new());
        let snap = probe(&adapter_with(None), &transport);
        assert_eq!(snap.error.as_ref().unwrap().code, "no_credentials");
        assert_eq!(snap.credential_path.as_deref(), Some("cursor/session"));

        let snap = probe(&adapter_with(Some("abc\r\nX-Evil: 1")), &transport);
        assert_eq!(snap.error.as_ref().unwrap().code, "bad_credential");
        assert!(transport.seen.lock().unwrap().is_empty());
    }

    #[test]
    fn http_failures_map_to_typed_codes() {
        for (status, code) in [
            (401, "unauthorized"),
            (403, "unauthorized"),
            (429, "rate_limited"),
            (503, "http"),
        ] {
            let snap = probe(
                &adapter_with(Some("tok")),
                &Recording::ok(status, b"{}".to_vec()),
            );
            assert_eq!(snap.status, Availability::Unavailable);
            assert_eq!(snap.error.as_ref().unwrap().code, code);
        }
    }

    #[test]
    fn cookie_header_accepts_bare_tokens_and_whole_headers_only() {
        assert_eq!(
            cookie_header(" abc ").unwrap(),
            "WorkosCursorSessionToken=abc"
        );
        assert_eq!(
            cookie_header("WorkosCursorSessionToken=a%3A%3Ab; other=1").unwrap(),
            "WorkosCursorSessionToken=a%3A%3Ab; other=1"
        );
        assert_eq!(cookie_header("  "), Err(CookieError::Empty));
        assert_eq!(cookie_header("a\nb"), Err(CookieError::Unsafe));
        assert_eq!(cookie_header("tökén"), Err(CookieError::Unsafe));
    }

    #[test]
    fn cookie_shapes_are_parsed_not_guessed_from_an_equals_sign() {
        assert_eq!(
            cookie_header("user_01A%3A%3Aeyj.x-y_z").unwrap(),
            "WorkosCursorSessionToken=user_01A%3A%3Aeyj.x-y_z"
        );
        assert_eq!(
            cookie_header("a=1; WorkosCursorSessionToken=tok==;").unwrap(),
            "a=1; WorkosCursorSessionToken=tok==;"
        );
        for bad in [
            "other=1",
            "WorkosCursorSessionToken=",
            "WorkosCursorSessionToken=a; broken",
            "WorkosCursorSessionToken=a, b=c",
            "=tok",
            "two words",
            "tok;",
            "tok,",
            "\"tok\"",
        ] {
            assert_eq!(cookie_header(bad), Err(CookieError::Shape), "{bad}");
        }
    }
}
