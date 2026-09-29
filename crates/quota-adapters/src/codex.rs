//! Codex / ChatGPT usage adapter.
//!
//! # Hypothesis (labeled)
//! CodexBar and the open-source Codex client read OAuth from
//! `~/.codex/auth.json` (or `$CODEX_HOME/auth.json`) and GET
//! `https://chatgpt.com/backend-api/wham/usage` with
//! `Authorization: Bearer <access_token>`.
//! The endpoint is **not** a documented public OpenAI API. When it moves or
//! rejects us we return `unavailable` — we do not invent windows.
//!
//! We never write `auth.json`. Token refresh is owned by `codex login`.

use std::path::{Path, PathBuf};

use quota_core::types::{
    AdapterError, Availability, Credits, ProviderId, ProviderPermission, ProviderSnapshot, Source,
    UsageWindow, DEFAULT_READING_MAX_AGE_SECS,
};

use crate::codexbar;
use crate::creds::{load_codex_creds, parse_chatgpt_base_url, CodexCreds, CredsError};
use crate::http::Transport;
use crate::provider::{ProbeCtx, Provider};

const DEFAULT_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

pub struct CodexAdapter {
    /// When set, only this home is consulted (`CODEX_HOME` isolation).
    pub home: Option<PathBuf>,
    /// CodexBar support dir. `None` → [`quota_core::default_codexbar_dir`].
    pub codexbar_dir: Option<PathBuf>,
    /// Read local CodexBar snapshot/history when the API is unavailable.
    pub enable_codexbar_files: bool,
}

impl Default for CodexAdapter {
    fn default() -> Self {
        Self {
            home: None,
            codexbar_dir: None,
            enable_codexbar_files: true,
        }
    }
}

impl Provider for CodexAdapter {
    fn id(&self) -> ProviderId {
        ProviderId::Codex
    }

    fn probe(&self, ctx: &ProbeCtx<'_>) -> ProviderSnapshot {
        let api = match load_codex_creds(self.home.as_deref()) {
            Ok(creds) => fetch_usage(ctx.transport, &creds, self.home.as_deref(), ctx.now),
            Err(e) => {
                let mut snap = ProviderSnapshot::unavailable(
                    ProviderId::Codex,
                    AdapterError::new(creds_code(&e), e.to_string()),
                );
                snap.source = Some(Source::Oauth);
                snap.credential_path = cred_path(&e);
                snap
            }
        };
        if api.status == Availability::Ok || !self.enable_codexbar_files {
            return api;
        }
        match self.try_codexbar_file() {
            Some(mut file) => {
                // A file snapshot can provide fallback usage, but it must not
                // erase the API's per-provider rate-limit deadline.
                file.retry_after_secs = api.retry_after_secs;
                // Permission is independent evidence from the API response.
                // Keep an explicit decision when the file only contributes usage.
                if api.permission != ProviderPermission::Unknown {
                    file.permission = api.permission;
                }
                file
            }
            None => api,
        }
    }
}

impl CodexAdapter {
    fn try_codexbar_file(&self) -> Option<ProviderSnapshot> {
        let dir = self
            .codexbar_dir
            .clone()
            .unwrap_or_else(quota_core::default_codexbar_dir);
        let (snap, path) = codexbar::load_snapshot_from_dir(&dir)?;
        let mut snap = snap;
        snap.source = Some(Source::File);
        snap.credential_path = Some(path.display().to_string());
        Some(snap)
    }
}

fn creds_code(e: &CredsError) -> &'static str {
    match e {
        CredsError::NotFound(_) => "no_credentials",
        CredsError::NoToken(_) => "no_access_token",
        CredsError::TooLarge => "creds_too_large",
        CredsError::Io(_) => "creds_io",
        CredsError::Parse(_) => "creds_parse",
        CredsError::Symlink(_) => "creds_symlink",
    }
}

fn cred_path(e: &CredsError) -> Option<String> {
    match e {
        CredsError::NotFound(p) | CredsError::NoToken(p) => Some(p.clone()),
        _ => None,
    }
}

fn usage_url(home: Option<&Path>) -> String {
    let home = home
        .map(PathBuf::from)
        .unwrap_or_else(quota_core::codex_home);
    let cfg = home.join("config.toml");
    if let Ok(bytes) = quota_core::read_file_capped(&cfg, 64 * 1024) {
        if let Ok(text) = std::str::from_utf8(&bytes) {
            if let Some(base) = parse_chatgpt_base_url(text) {
                return join_usage_url(&base);
            }
        }
    }
    DEFAULT_USAGE_URL.to_string()
}

fn join_usage_url(base: &str) -> String {
    let base = base.trim_end_matches('/');
    if (base.contains("chatgpt.com") && base.contains("backend-api"))
        || base.ends_with("/backend-api")
    {
        format!("{base}/wham/usage")
    } else {
        // Hypothesis (OpenUsage): non-ChatGPT bases use /api/codex/usage.
        format!("{base}/api/codex/usage")
    }
}

fn fetch_usage(
    transport: &dyn Transport,
    creds: &CodexCreds,
    home: Option<&Path>,
    now: i64,
) -> ProviderSnapshot {
    let url = usage_url(home);
    let auth = format!("Bearer {}", creds.access_token);
    let account = creds.account_id.clone();
    let mut headers: Vec<(&str, &str)> = vec![
        ("Authorization", auth.as_str()),
        ("Accept", "application/json"),
        ("User-Agent", "quota/0.1.0"),
    ];
    if let Some(id) = account.as_deref() {
        headers.push(("ChatGPT-Account-Id", id));
    }

    match transport.get(&url, &headers) {
        Ok(resp) => parse_usage_http_with_meta(
            resp.status,
            &resp.body,
            &creds.path,
            now,
            resp.retry_after_secs,
        ),
        Err(e) => {
            let mut snap = ProviderSnapshot::unavailable(
                ProviderId::Codex,
                AdapterError::new("network", format!("usage probe failed: {e}")),
            );
            snap.source = Some(Source::Oauth);
            snap.credential_path = Some(creds.path.display().to_string());
            snap
        }
    }
}

pub fn parse_usage_http(status: u16, body: &[u8], cred_path: &Path) -> ProviderSnapshot {
    parse_usage_http_with_meta(
        status,
        body,
        cred_path,
        quota_core::timeutil::now_unix(),
        None,
    )
}

fn parse_usage_http_with_meta(
    status: u16,
    body: &[u8],
    cred_path: &Path,
    observed_at: i64,
    retry_after_secs: Option<u64>,
) -> ProviderSnapshot {
    let path = cred_path.display().to_string();
    if status == 401 || status == 403 {
        let mut snap = ProviderSnapshot::unavailable(
            ProviderId::Codex,
            AdapterError::new(
                "unauthorized",
                format!("HTTP {status}: re-login with `codex login` (we do not refresh tokens)"),
            ),
        );
        snap.source = Some(Source::Oauth);
        snap.credential_path = Some(path);
        snap.retry_after_secs = retry_after_secs;
        return snap;
    }
    if status == 429 {
        let mut snap = ProviderSnapshot::unavailable(
            ProviderId::Codex,
            AdapterError::new("rate_limited", format!("HTTP {status} from usage endpoint")),
        );
        snap.source = Some(Source::Oauth);
        snap.credential_path = Some(path);
        snap.retry_after_secs = retry_after_secs;
        return snap;
    }
    if !(200..300).contains(&status) {
        let _ = body;
        let mut snap = ProviderSnapshot::unavailable(
            ProviderId::Codex,
            AdapterError::new("http", format!("HTTP {status}")),
        );
        snap.source = Some(Source::Oauth);
        snap.credential_path = Some(path);
        snap.retry_after_secs = retry_after_secs;
        return snap;
    }
    parse_usage_json(body, &path, observed_at)
}

fn parse_usage_json(body: &[u8], path: &str, observed_at: i64) -> ProviderSnapshot {
    let v: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => {
            let mut snap = ProviderSnapshot::unavailable(
                ProviderId::Codex,
                AdapterError::new("parse", format!("usage JSON: {e}")),
            );
            snap.source = Some(Source::Oauth);
            snap.credential_path = Some(path.to_string());
            return snap;
        }
    };

    let rate = v.get("rate_limit");
    let permission = rate
        .map(parse_permission)
        .unwrap_or(ProviderPermission::Unknown);
    let mut windows = Vec::new();
    if let Some(rate) = rate {
        if let Some(w) = map_codex_window(rate.get("primary_window"), "primary", observed_at) {
            windows.push(w);
        }
        if let Some(w) = map_codex_window(rate.get("secondary_window"), "secondary", observed_at) {
            windows.push(w);
        }
        if let Some(extra) = rate
            .get("additional_rate_limits")
            .and_then(|x| x.as_array())
        {
            for item in extra {
                let label = item
                    .get("name")
                    .or_else(|| item.get("id"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("extra")
                    .to_string();
                if let Some(mut w) = map_codex_window(Some(item), "tertiary", observed_at) {
                    w.label = label;
                    windows.push(w);
                }
            }
        }
    }

    let credits = v.get("credits").and_then(map_credits);
    let plan = v
        .get("plan_type")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());

    if windows.is_empty() && credits.is_none() {
        let mut snap = ProviderSnapshot::unavailable(
            ProviderId::Codex,
            AdapterError::new(
                "empty",
                "usage response had no rate_limit windows (shape may have changed)",
            ),
        );
        snap.source = Some(Source::Oauth);
        snap.credential_path = Some(path.to_string());
        snap.plan = plan;
        snap.permission = permission;
        snap.observed_at = Some(observed_at);
        snap.max_age_secs = DEFAULT_READING_MAX_AGE_SECS;
        snap.refresh_freshness(quota_core::timeutil::now_unix());
        return snap;
    }

    ProviderSnapshot::observed(
        ProviderId::Codex,
        Some(Source::Oauth),
        windows,
        credits,
        plan,
        Some(path.to_string()),
        Some(observed_at),
        DEFAULT_READING_MAX_AGE_SECS,
        permission,
    )
}

fn parse_permission(rate: &serde_json::Value) -> ProviderPermission {
    let allowed = rate.get("allowed").and_then(serde_json::Value::as_bool);
    let limit_reached = rate
        .get("limit_reached")
        .and_then(serde_json::Value::as_bool);
    if allowed == Some(false) || limit_reached == Some(true) {
        ProviderPermission::LimitReached
    } else if allowed == Some(true) || limit_reached == Some(false) {
        ProviderPermission::Allowed
    } else {
        ProviderPermission::Unknown
    }
}

fn map_codex_window(
    node: Option<&serde_json::Value>,
    slot: &str,
    observed_at: i64,
) -> Option<UsageWindow> {
    let node = node?;
    if node.is_null() {
        return None;
    }
    let reset = node
        .get("reset_at")
        .and_then(quota_core::timeutil::parse_reset_at);
    let limit_window_seconds = node.get("limit_window_seconds").and_then(|x| x.as_i64());
    let minutes = node.get("window_minutes").and_then(|x| x.as_i64());
    let (kind, label) =
        quota_core::classify_codex_window(Some(slot), limit_window_seconds, minutes);
    Some(match node.get("used_percent").and_then(|x| x.as_f64()) {
        Some(used) if used.is_finite() && used >= 0.0 => UsageWindow::from_percent_at(
            kind,
            label,
            used,
            reset,
            limit_window_seconds,
            Some(observed_at),
            DEFAULT_READING_MAX_AGE_SECS,
        ),
        _ => UsageWindow::unreadable(
            kind,
            label,
            reset,
            limit_window_seconds,
            Some(observed_at),
            DEFAULT_READING_MAX_AGE_SECS,
        ),
    })
}

fn map_credits(node: &serde_json::Value) -> Option<Credits> {
    if node.is_null() {
        return None;
    }
    let balance = node.get("balance").and_then(|b| match b {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.parse().ok(),
        _ => None,
    });
    let unlimited = node.get("unlimited").and_then(|x| x.as_bool());
    let has_credits = node.get("has_credits").and_then(|x| x.as_bool());
    if balance.is_none() && unlimited.is_none() && has_credits.is_none() {
        return None;
    }
    Some(Credits {
        balance,
        unlimited,
        has_credits,
        unit: Some("credits".into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::MockTransport;
    use crate::provider::ProbeCtx;
    use quota_core::types::WindowKind;
    use std::path::Path;

    fn unique_test_dir() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        for _ in 0..1_000 {
            let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "quota-codex-adapter-{}-{serial}",
                std::process::id()
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return path,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create {}: {error}", path.display()),
            }
        }
        panic!("could not allocate unique Codex adapter test directory");
    }

    const FIXTURE: &str = r#"{
        "plan_type": "plus",
        "rate_limit": {
            "allowed": true,
            "limit_reached": false,
            "primary_window": {
                "used_percent": 27,
                "limit_window_seconds": 18000,
                "reset_at": 1782770922
            },
            "secondary_window": {
                "used_percent": 4,
                "limit_window_seconds": 604800,
                "reset_at": 1783357722
            }
        },
        "credits": {"has_credits": false, "unlimited": false, "balance": "0"}
    }"#;

    #[test]
    fn parses_wham_fixture() {
        let snap = parse_usage_http(200, FIXTURE.as_bytes(), Path::new("/tmp/auth.json"));
        assert_eq!(snap.status, Availability::Ok);
        assert_eq!(snap.windows.len(), 2);
        assert_eq!(snap.windows[0].used_percent, Some(27.0));
        assert_eq!(snap.windows[0].remaining_percent, Some(73.0));
        assert_eq!(snap.windows[0].kind, WindowKind::Session);
        assert_eq!(snap.windows[0].label, "5h");
        assert_eq!(snap.windows[1].kind, WindowKind::Weekly);
        assert_eq!(snap.windows[1].label, "weekly");
        assert_eq!(snap.windows[0].reset_at, Some(1_782_770_922));
        assert_eq!(snap.plan.as_deref(), Some("plus"));
        assert_eq!(snap.credits.as_ref().and_then(|c| c.balance), Some(0.0));
        assert_eq!(snap.permission, ProviderPermission::Allowed);
        assert!(snap.observed_at.is_some());
        assert!(snap
            .windows
            .iter()
            .all(|w| w.observed_at == snap.observed_at));
    }

    #[test]
    fn unauthorized_is_unavailable() {
        let snap = parse_usage_http(401, br#"{"access_token":"leak"}"#, Path::new("auth.json"));
        assert_eq!(snap.status, Availability::Unavailable);
        assert_eq!(snap.error.as_ref().unwrap().code, "unauthorized");
        assert!(snap.windows.is_empty());
        assert!(!snap.error.as_ref().unwrap().message.contains("leak"));
    }

    #[test]
    fn http_error_omits_body() {
        let snap = parse_usage_http(503, b"upstream token=secret", Path::new("auth.json"));
        assert_eq!(snap.error.as_ref().unwrap().code, "http");
        assert_eq!(snap.error.as_ref().unwrap().message, "HTTP 503");
    }

    #[test]
    fn garbage_json_unavailable() {
        let snap = parse_usage_http(200, b"not-json", Path::new("auth.json"));
        assert_eq!(snap.status, Availability::Unavailable);
        assert_eq!(snap.error.as_ref().unwrap().code, "parse");
    }

    #[test]
    fn empty_windows_unavailable() {
        let snap = parse_usage_http(200, br#"{"plan_type":"plus"}"#, Path::new("auth.json"));
        assert_eq!(snap.status, Availability::Unavailable);
        assert_eq!(snap.error.as_ref().unwrap().code, "empty");
    }

    #[test]
    fn empty_success_preserves_explicit_limit_reached_permission() {
        let snap = parse_usage_http(
            200,
            br#"{"rate_limit":{"allowed":false,"limit_reached":true}}"#,
            Path::new("auth.json"),
        );
        assert_eq!(snap.status, Availability::Unavailable);
        assert_eq!(snap.permission, ProviderPermission::LimitReached);
        assert!(snap.observed_at.is_some());
    }

    #[test]
    fn refusal_is_preserved_even_with_low_usage() {
        let json = r#"{
            "rate_limit": {
                "allowed": false,
                "limit_reached": true,
                "primary_window": {"used_percent": 1}
            }
        }"#;
        let snap = parse_usage_http(200, json.as_bytes(), Path::new("auth.json"));
        assert_eq!(snap.windows[0].used_percent, Some(1.0));
        assert_eq!(snap.permission, ProviderPermission::LimitReached);
    }

    #[test]
    fn reported_window_with_missing_measurement_is_unknown_and_null() {
        let snap = parse_usage_http(
            200,
            br#"{"rate_limit":{"primary_window":{"reset_at":1800000000}}}"#,
            Path::new("auth.json"),
        );
        assert_eq!(snap.status, Availability::Ok);
        assert_eq!(snap.windows.len(), 1);
        assert_eq!(
            snap.windows[0].state,
            quota_core::types::WindowState::Unknown
        );
        assert!(snap.windows[0].reading.is_none());
        assert!(serde_json::to_value(&snap).unwrap()["windows"][0]["reading"].is_null());
    }

    #[test]
    fn retry_after_is_retained_from_rate_limit_response() {
        let snap = parse_usage_http_with_meta(429, b"{}", Path::new("auth.json"), 100, Some(90));
        assert_eq!(snap.retry_after_secs, Some(90));
    }

    #[test]
    fn mock_transport_network_error() {
        let t = MockTransport {
            next: Some(Err(crate::http::TransportError::Message("dns".into()))),
            last_url: std::sync::Mutex::new(None),
        };
        let creds = CodexCreds {
            access_token: "x".into(),
            account_id: None,
            path: PathBuf::from("/tmp/auth.json"),
        };
        let snap = fetch_usage(&t, &creds, None, quota_core::timeutil::now_unix());
        assert_eq!(snap.status, Availability::Unavailable);
        assert_eq!(snap.error.as_ref().unwrap().code, "network");
    }

    #[test]
    fn join_url_chatgpt() {
        assert_eq!(
            join_usage_url("https://chatgpt.com/backend-api"),
            "https://chatgpt.com/backend-api/wham/usage"
        );
        assert_eq!(
            join_usage_url("https://example.com"),
            "https://example.com/api/codex/usage"
        );
    }

    #[test]
    fn probe_without_creds_does_not_panic() {
        let t = MockTransport::ok_json(200, FIXTURE);
        let adapter = CodexAdapter {
            home: Some(PathBuf::from("/no/such/codex-home-quota-test")),
            enable_codexbar_files: false,
            ..CodexAdapter::default()
        };
        let ctx = ProbeCtx {
            transport: &t,
            now: 1,
        };
        let snap = adapter.probe(&ctx);
        assert_eq!(snap.status, Availability::Unavailable);
        assert_eq!(snap.error.as_ref().unwrap().code, "no_credentials");
    }

    /// Live dogfood 2026-09-27: 7d window arrived in `primary_window`.
    #[test]
    fn weekly_primary_window_is_not_labeled_5h() {
        let json = r#"{
            "plan_type": "pro",
            "rate_limit": {
                "primary_window": {
                    "used_percent": 59,
                    "limit_window_seconds": 604800,
                    "reset_at": 1759510689
                },
                "secondary_window": null
            }
        }"#;
        let snap = parse_usage_http(200, json.as_bytes(), Path::new("/tmp/auth.json"));
        assert_eq!(snap.status, Availability::Ok);
        assert_eq!(snap.plan.as_deref(), Some("pro"));
        assert_eq!(snap.windows.len(), 1);
        assert_eq!(snap.windows[0].kind, WindowKind::Weekly);
        assert_eq!(snap.windows[0].label, "weekly");
        assert_eq!(snap.windows[0].used_percent, Some(59.0));
        assert_eq!(snap.windows[0].remaining_percent, Some(41.0));
        assert_eq!(snap.windows[0].limit_window_seconds, Some(604_800));
        assert_ne!(snap.windows[0].kind, WindowKind::Session);
        assert_ne!(snap.windows[0].label, "5h");
    }

    #[test]
    fn file_fallback_when_api_unavailable() {
        let t = MockTransport::ok_json(200, FIXTURE);
        let adapter = CodexAdapter {
            home: Some(PathBuf::from("/no/such/codex-home-quota-test")),
            codexbar_dir: Some(crate::codexbar::workspace_fixtures_dir()),
            enable_codexbar_files: true,
        };
        let ctx = ProbeCtx {
            transport: &t,
            now: 1,
        };
        let snap = adapter.probe(&ctx);
        assert_eq!(snap.status, Availability::Stale);
        assert_eq!(snap.freshness, quota_core::types::Freshness::Stale);
        assert_eq!(snap.source, Some(Source::File));
        assert_eq!(snap.windows[0].kind, WindowKind::Weekly);
        assert_eq!(snap.windows[0].label, "weekly");
        assert_eq!(snap.windows[0].used_percent, Some(59.0));
    }

    #[test]
    fn file_fallback_preserves_api_retry_after() {
        let home = unique_test_dir();
        std::fs::write(home.join("auth.json"), br#"{"access_token":"test-token"}"#).unwrap();
        let t = MockTransport {
            next: Some(Ok(crate::http::HttpResponse {
                status: 429,
                body: b"{}".to_vec(),
                retry_after_secs: Some(120),
            })),
            last_url: std::sync::Mutex::new(None),
        };
        let adapter = CodexAdapter {
            home: Some(home.clone()),
            codexbar_dir: Some(crate::codexbar::workspace_fixtures_dir()),
            enable_codexbar_files: true,
        };
        let ctx = ProbeCtx {
            transport: &t,
            now: quota_core::timeutil::now_unix(),
        };

        let snap = adapter.probe(&ctx);

        assert_eq!(snap.source, Some(Source::File));
        assert_eq!(snap.retry_after_secs, Some(120));
        assert_eq!(
            t.last_url.lock().unwrap().as_deref(),
            Some("https://chatgpt.com/backend-api/wham/usage")
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn file_fallback_preserves_explicit_api_refusal_without_windows() {
        let home = unique_test_dir();
        std::fs::write(home.join("auth.json"), br#"{"access_token":"test-token"}"#).unwrap();
        let transport = MockTransport::ok_json(
            200,
            r#"{"rate_limit":{"allowed":false,"limit_reached":true}}"#,
        );
        let adapter = CodexAdapter {
            home: Some(home.clone()),
            codexbar_dir: Some(crate::codexbar::workspace_fixtures_dir()),
            enable_codexbar_files: true,
        };
        let ctx = ProbeCtx {
            transport: &transport,
            now: quota_core::timeutil::now_unix(),
        };

        let snapshot = adapter.probe(&ctx);

        assert_eq!(snapshot.source, Some(Source::File));
        assert_eq!(snapshot.permission, ProviderPermission::LimitReached);
        assert!(!snapshot.windows.is_empty());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn http_error_body_is_not_copied() {
        let body = br#"{"error":"bearer sk-secret-must-not-leak","token":"eyJhbGciOi"}"#;
        let snap = parse_usage_http(500, body, Path::new("/tmp/auth.json"));
        assert_eq!(snap.status, Availability::Unavailable);
        assert_eq!(snap.error.as_ref().unwrap().code, "http");
        let msg = &snap.error.as_ref().unwrap().message;
        assert!(msg.contains("HTTP 500"));
        assert!(!msg.contains("sk-secret"));
        assert!(!msg.contains("eyJ"));
        let encoded = serde_json::to_string(&snap).unwrap();
        assert!(!encoded.contains("sk-secret-must-not-leak"));
    }

    #[test]
    fn mock_does_not_invent_live_latency() {
        // Offline mock: no wall-clock HTTPS. Presence of a body is enough.
        let t = MockTransport::ok_json(200, FIXTURE);
        let creds = CodexCreds {
            access_token: "tok".into(),
            account_id: None,
            path: PathBuf::from("/tmp/auth.json"),
        };
        let snap = fetch_usage(&t, &creds, None, quota_core::timeutil::now_unix());
        assert_eq!(snap.status, Availability::Ok);
        assert!(t
            .last_url
            .lock()
            .unwrap()
            .as_deref()
            .unwrap()
            .contains("wham"));
    }
}
