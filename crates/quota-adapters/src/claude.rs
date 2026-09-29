//! Claude Code usage adapter.
//!
//! # Hypothesis (labeled)
//! Claude Code stores OAuth on Linux at `~/.claude/.credentials.json` (mode
//! 0600) and on macOS primarily in Keychain service `Claude Code-credentials`
//! with the same file as a fallback. The `/usage` UI is powered by an
//! **undocumented** `GET https://api.anthropic.com/api/oauth/usage` using
//! `Authorization: Bearer <claudeAiOauth.accessToken>` and
//! `anthropic-beta: oauth-2025-04-20`.
//!
//! On macOS the default account reads the Keychain item first (through
//! `quota-secrets`), because the file there goes stale. An isolated config dir
//! (`CLAUDE_CONFIG_DIR` or an account `home_path`) reads only its own file.
//! We never write either. API-key mode cannot use this endpoint — we report
//! `unavailable`, not fake percents.
//!
//! A fresh statusline push (see the `quota-source-claude-statusline` crate)
//! outranks this poll; the daemon skips the poll while a push is current.
//!
//! This endpoint is widely reported as aggressively rate-limited. The daemon
//! backs off; a 429 is `unavailable`, never a guessed number.

use std::path::Path;

use quota_core::types::{
    AdapterError, Credits, ProviderId, ProviderObservation, ProviderPermission, ProviderSnapshot,
    Source, UsageWindow, WindowKind, DEFAULT_READING_MAX_AGE_SECS,
};

use quota_secrets::claude_code::keychain_disabled;
use quota_secrets::{ClaudeCodeKeychain, SecretsBackend};

use crate::creds::{load_claude_creds, ClaudeCreds, CredsError};
use crate::http::Transport;
use crate::provider::{ProbeCtx, Provider};

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const OAUTH_BETA: &str = "oauth-2025-04-20";

#[derive(Default)]
pub struct ClaudeAdapter {
    /// When set, only this Claude config dir is consulted (`home_path` isolation).
    pub config_dir: Option<std::path::PathBuf>,
    /// Consulted before the credentials file. Left `None` for isolated homes.
    pub keychain: Option<Box<dyn SecretsBackend>>,
}

impl ClaudeAdapter {
    /// The default account on macOS reads Claude Code's Keychain item first;
    /// an explicit home, `CLAUDE_CONFIG_DIR`, other platforms, or
    /// `QUOTA_NO_KEYCHAIN=1` keep to the file.
    pub fn for_account(config_dir: Option<std::path::PathBuf>) -> Self {
        let isolated = config_dir.is_some()
            || std::env::var("CLAUDE_CONFIG_DIR").is_ok_and(|dir| !dir.trim().is_empty());
        let keychain: Option<Box<dyn SecretsBackend>> =
            if cfg!(target_os = "macos") && !isolated && !keychain_disabled() {
                Some(Box::new(ClaudeCodeKeychain))
            } else {
                None
            };
        Self {
            config_dir,
            keychain,
        }
    }

    fn load_creds(&self) -> (Result<ClaudeCreds, CredsError>, Option<String>) {
        let mut keychain_note = None;
        if let Some(keychain) = &self.keychain {
            match keychain.get("claude") {
                Ok(Some(record)) => {
                    return (
                        Ok(ClaudeCreds {
                            access_token: record.value,
                            path: std::path::PathBuf::from(record.path),
                            expires_at: None,
                        }),
                        None,
                    );
                }
                Ok(None) => {}
                Err(e) => keychain_note = Some(format!("keychain: {e}")),
            }
        }
        (load_claude_creds(self.config_dir.as_deref()), keychain_note)
    }
}

impl Provider for ClaudeAdapter {
    fn id(&self) -> ProviderId {
        ProviderId::Claude
    }

    fn probe(&self, ctx: &ProbeCtx<'_>) -> ProviderSnapshot {
        match self.load_creds() {
            (Ok(creds), _) => fetch_usage(ctx.transport, &creds, ctx.now),
            (Err(e), keychain_note) => {
                let message = match keychain_note {
                    Some(note) => format!("{e} ({note})"),
                    None => e.to_string(),
                };
                let mut snap = ProviderSnapshot::unavailable(
                    ProviderId::Claude,
                    AdapterError::new(creds_code(&e), message),
                );
                snap.source = Some(Source::Oauth);
                snap.credential_path = match &e {
                    CredsError::NotFound(p) | CredsError::NoToken(p) => Some(p.clone()),
                    _ => None,
                };
                snap
            }
        }
    }
}

fn creds_code(e: &CredsError) -> &'static str {
    match e {
        CredsError::NotFound(_) => "no_credentials",
        CredsError::NoToken(_) => "no_oauth_token",
        CredsError::TooLarge => "creds_too_large",
        CredsError::Io(_) => "creds_io",
        CredsError::Parse(_) => "creds_parse",
        CredsError::Symlink(_) => "creds_symlink",
    }
}

fn fetch_usage(transport: &dyn Transport, creds: &ClaudeCreds, now: i64) -> ProviderSnapshot {
    let expired = creds.expires_at.is_some_and(|exp| exp + 30 < now);
    let auth = format!("Bearer {}", creds.access_token);
    let headers = [
        ("Authorization", auth.as_str()),
        ("anthropic-beta", OAUTH_BETA),
        ("Accept", "application/json"),
        ("User-Agent", "quota/0.1.0"),
    ];
    match transport.get(USAGE_URL, &headers) {
        Ok(resp) => {
            let mut snap = parse_usage_http_with_meta(
                resp.status,
                &resp.body,
                &creds.path,
                now,
                resp.retry_after_secs,
            );
            if expired {
                if let Some(err) = snap.error.as_mut() {
                    err.message.push_str(" (local expiresAt is in the past)");
                }
            }
            snap
        }
        Err(e) => {
            let extra = if expired {
                " (local expiresAt is in the past; re-login via Claude Code)"
            } else {
                ""
            };
            let mut snap = ProviderSnapshot::unavailable(
                ProviderId::Claude,
                AdapterError::new("network", format!("usage probe failed: {e}{extra}")),
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
            ProviderId::Claude,
            AdapterError::new(
                "unauthorized",
                format!(
                    "HTTP {status}: re-login with Claude Code `/login` (we do not refresh tokens)"
                ),
            ),
        );
        snap.source = Some(Source::Oauth);
        snap.credential_path = Some(path);
        snap.retry_after_secs = retry_after_secs;
        return snap;
    }
    if status == 429 {
        let mut snap = ProviderSnapshot::unavailable(
            ProviderId::Claude,
            AdapterError::new(
                "rate_limited",
                "HTTP 429 from /api/oauth/usage (undocumented, often aggressive)",
            ),
        );
        snap.source = Some(Source::Oauth);
        snap.credential_path = Some(path);
        snap.retry_after_secs = retry_after_secs;
        return snap;
    }
    if !(200..300).contains(&status) {
        let _ = body;
        let mut snap = ProviderSnapshot::unavailable(
            ProviderId::Claude,
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
                ProviderId::Claude,
                AdapterError::new("parse", format!("usage JSON: {e}")),
            );
            snap.source = Some(Source::Oauth);
            snap.credential_path = Some(path.to_string());
            return snap;
        }
    };

    let mut windows = Vec::new();
    push_bucket(
        &mut windows,
        &v,
        "five_hour",
        WindowKind::FiveHour,
        "5h",
        observed_at,
    );
    push_bucket(
        &mut windows,
        &v,
        "seven_day",
        WindowKind::Weekly,
        "weekly",
        observed_at,
    );
    push_bucket(
        &mut windows,
        &v,
        "seven_day_opus",
        WindowKind::Extra,
        "opus weekly",
        observed_at,
    );
    push_bucket(
        &mut windows,
        &v,
        "seven_day_sonnet",
        WindowKind::Extra,
        "sonnet weekly",
        observed_at,
    );

    let credits = v.get("extra_usage").and_then(map_extra_usage);
    if let Some(extra) = v.get("extra_usage") {
        if let Some(node) = extra.get("utilization").filter(|node| !node.is_null()) {
            windows.push(percent_window(
                node.as_f64(),
                WindowKind::Monthly,
                "extra usage",
                None,
                observed_at,
            ));
        }
    }

    if windows.is_empty() && credits.is_none() {
        let mut snap = ProviderSnapshot::unavailable(
            ProviderId::Claude,
            AdapterError::new(
                "empty",
                "usage response had no five_hour/seven_day buckets (shape may have changed)",
            ),
        );
        snap.source = Some(Source::Oauth);
        snap.credential_path = Some(path.to_string());
        return snap;
    }

    ProviderSnapshot::observed(ProviderObservation {
        provider: ProviderId::Claude,
        source: Some(Source::Oauth),
        windows,
        credits,
        plan: None,
        credential_path: Some(path.to_string()),
        observed_at: Some(observed_at),
        max_age_secs: DEFAULT_READING_MAX_AGE_SECS,
        permission: ProviderPermission::Unknown,
    })
}

fn push_bucket(
    windows: &mut Vec<UsageWindow>,
    root: &serde_json::Value,
    key: &str,
    kind: WindowKind,
    label: &str,
    observed_at: i64,
) {
    let Some(node) = root.get(key) else {
        return;
    };
    if node.is_null() {
        return;
    }
    let reset = node
        .get("resets_at")
        .and_then(quota_core::timeutil::parse_reset_at);
    windows.push(percent_window(
        node.get("utilization").and_then(|x| x.as_f64()),
        kind,
        label,
        reset,
        observed_at,
    ));
}

fn percent_window(
    used: Option<f64>,
    kind: WindowKind,
    label: &str,
    reset: Option<i64>,
    observed_at: i64,
) -> UsageWindow {
    match used {
        Some(used) if used.is_finite() && used >= 0.0 => UsageWindow::from_percent_at(
            kind,
            label,
            used,
            reset,
            None,
            Some(observed_at),
            DEFAULT_READING_MAX_AGE_SECS,
        ),
        _ => UsageWindow::unreadable(
            kind,
            label,
            reset,
            None,
            Some(observed_at),
            DEFAULT_READING_MAX_AGE_SECS,
        ),
    }
}

fn map_extra_usage(node: &serde_json::Value) -> Option<Credits> {
    if node.is_null() {
        return None;
    }
    let enabled = node.get("is_enabled").and_then(|x| x.as_bool());
    let used = node.get("used_credits").and_then(|x| x.as_f64());
    let limit = node.get("monthly_limit").and_then(|x| x.as_f64());
    if enabled.is_none() && used.is_none() && limit.is_none() {
        return None;
    }
    Some(Credits {
        balance: match (used, limit) {
            (Some(u), Some(l)) => Some((l - u).max(0.0)),
            _ => used,
        },
        unlimited: Some(enabled == Some(false) && used.is_none()),
        has_credits: enabled,
        unit: Some("credits".into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{MockTransport, TransportError};
    use quota_core::types::Availability;
    use quota_secrets::SecretsError;
    use std::path::PathBuf;

    const FIXTURE: &str = r#"{
        "five_hour": {"utilization": 33.0, "resets_at": "2026-04-11T07:00:00Z"},
        "seven_day": {"utilization": 13.0, "resets_at": "2026-04-17T00:59:59Z"},
        "seven_day_opus": null,
        "seven_day_sonnet": {"utilization": 1.0, "resets_at": "2026-04-16T03:00:00Z"},
        "extra_usage": {
            "is_enabled": false,
            "monthly_limit": null,
            "used_credits": null,
            "utilization": null
        }
    }"#;

    #[test]
    fn parses_oauth_usage_fixture() {
        let snap = parse_usage_http(200, FIXTURE.as_bytes(), Path::new("/tmp/.credentials.json"));
        assert_eq!(snap.status, Availability::Ok);
        assert_eq!(snap.windows.len(), 3);
        assert_eq!(snap.windows[0].kind, WindowKind::FiveHour);
        assert_eq!(snap.windows[0].used_percent, Some(33.0));
        assert!(snap.windows[0].reset_at.is_some());
        assert_eq!(snap.windows[1].kind, WindowKind::Weekly);
        assert_eq!(snap.windows[2].label, "sonnet weekly");
        assert_eq!(snap.permission, ProviderPermission::Unknown);
        assert!(snap.observed_at.is_some());
    }

    #[test]
    fn null_buckets_skipped() {
        let snap = parse_usage_http(
            200,
            br#"{"five_hour":null,"seven_day":null}"#,
            Path::new("c.json"),
        );
        assert_eq!(snap.status, Availability::Unavailable);
        assert_eq!(snap.error.as_ref().unwrap().code, "empty");
    }

    #[test]
    fn reported_bucket_without_utilization_is_unknown_not_zero() {
        let snap = parse_usage_http(
            200,
            br#"{"five_hour":{"resets_at":"2026-10-01T00:00:00Z"}}"#,
            Path::new("c.json"),
        );
        assert_eq!(snap.status, Availability::Unavailable);
        assert_eq!(snap.error.as_ref().unwrap().code, "unreadable");
        assert_eq!(snap.windows.len(), 1);
        assert_eq!(
            snap.windows[0].state,
            quota_core::types::WindowState::Unknown
        );
        assert!(snap.windows[0].reading.is_none());
        assert!(serde_json::to_value(&snap).unwrap()["windows"][0]["reading"].is_null());
    }

    #[test]
    fn negative_extra_usage_is_unknown_and_not_healthy() {
        let snap = parse_usage_http(
            200,
            br#"{"extra_usage":{"utilization":-5.0}}"#,
            Path::new("c.json"),
        );
        assert_eq!(snap.windows.len(), 1);
        assert_eq!(snap.windows[0].label, "extra usage");
        assert_eq!(
            snap.windows[0].state,
            quota_core::types::WindowState::Unknown
        );
        assert!(snap.windows[0].reading.is_none());
        assert_eq!(snap.status, Availability::Unavailable);
    }

    #[test]
    fn rate_limited() {
        let snap = parse_usage_http(429, b"{}", Path::new("c.json"));
        assert_eq!(snap.status, Availability::Unavailable);
        assert_eq!(snap.error.as_ref().unwrap().code, "rate_limited");
    }

    #[test]
    fn retry_after_is_retained_from_rate_limit_response() {
        let snap = parse_usage_http_with_meta(429, b"{}", Path::new("c.json"), 100, Some(45));
        assert_eq!(snap.retry_after_secs, Some(45));
    }

    #[test]
    fn unauthorized() {
        let snap = parse_usage_http(401, br#"{"token":"should-not-leak"}"#, Path::new("c.json"));
        assert_eq!(snap.status, Availability::Unavailable);
        assert_eq!(snap.error.as_ref().unwrap().code, "unauthorized");
        assert!(!snap
            .error
            .as_ref()
            .unwrap()
            .message
            .contains("should-not-leak"));
    }

    #[test]
    fn http_error_omits_body() {
        let snap = parse_usage_http(500, b"internal secret xyz", Path::new("c.json"));
        assert_eq!(snap.error.as_ref().unwrap().code, "http");
        assert_eq!(snap.error.as_ref().unwrap().message, "HTTP 500");
    }

    #[test]
    fn isolated_config_dir_missing_does_not_panic() {
        let t = MockTransport {
            next: Some(Err(TransportError::Message("offline".into()))),
            last_url: std::sync::Mutex::new(None),
        };
        let adapter = ClaudeAdapter {
            config_dir: Some(PathBuf::from("/no/such/claude-home-quota-test")),
            keychain: None,
        };
        let ctx = crate::provider::ProbeCtx {
            transport: &t,
            now: 1,
        };
        let snap = adapter.probe(&ctx);
        assert_eq!(snap.status, Availability::Unavailable);
        assert_eq!(snap.error.as_ref().unwrap().code, "no_credentials");
    }

    enum FakeKeychain {
        Found(&'static str),
        Fails(&'static str),
    }

    impl SecretsBackend for FakeKeychain {
        fn name(&self) -> &'static str {
            "fake-keychain"
        }
        fn get(&self, _path: &str) -> Result<Option<quota_secrets::SecretRecord>, SecretsError> {
            match self {
                Self::Found(value) => Ok(Some(quota_secrets::SecretRecord {
                    backend: "fake-keychain",
                    path: "keychain:Claude Code-credentials".into(),
                    value: (*value).into(),
                })),
                Self::Fails(reason) => Err(SecretsError::Unavailable((*reason).into())),
            }
        }
        fn put(&self, _: &str, _: &str) -> Result<(), SecretsError> {
            Err(SecretsError::ReadOnly)
        }
        fn delete(&self, _: &str) -> Result<(), SecretsError> {
            Err(SecretsError::ReadOnly)
        }
    }

    fn adapter_with(keychain: FakeKeychain) -> ClaudeAdapter {
        ClaudeAdapter {
            config_dir: Some(PathBuf::from("/no/such/claude-home-quota-test")),
            keychain: Some(Box::new(keychain)),
        }
    }

    #[test]
    fn keychain_token_is_used_before_the_file_and_is_what_gets_sent() {
        let t = MockTransport::ok_json(200, FIXTURE);
        let ctx = crate::provider::ProbeCtx {
            transport: &t,
            now: quota_core::timeutil::now_unix(),
        };
        let snap = adapter_with(FakeKeychain::Found("kc-token")).probe(&ctx);
        assert_eq!(snap.status, Availability::Ok);
        assert_eq!(
            snap.credential_path.as_deref(),
            Some("keychain:Claude Code-credentials")
        );
        assert!(!serde_json::to_string(&snap).unwrap().contains("kc-token"));
    }

    #[test]
    fn keychain_failure_is_named_when_the_file_fallback_is_missing_too() {
        let t = MockTransport {
            next: None,
            last_url: std::sync::Mutex::new(None),
        };
        let ctx = crate::provider::ProbeCtx {
            transport: &t,
            now: 1,
        };
        let snap = adapter_with(FakeKeychain::Fails("keychain error -128")).probe(&ctx);
        let error = snap.error.unwrap();
        assert_eq!(error.code, "no_credentials");
        assert!(error.message.contains("keychain error -128"));
    }

    #[test]
    fn isolated_accounts_never_get_a_keychain_reader() {
        let adapter = ClaudeAdapter::for_account(Some(PathBuf::from("/some/home")));
        assert!(adapter.keychain.is_none());
    }

    #[test]
    fn fetch_network_error() {
        let t = MockTransport {
            next: Some(Err(TransportError::Message("offline".into()))),
            last_url: std::sync::Mutex::new(None),
        };
        let creds = ClaudeCreds {
            access_token: "x".into(),
            path: PathBuf::from("/tmp/c.json"),
            expires_at: None,
        };
        let snap = fetch_usage(&t, &creds, 0);
        assert_eq!(snap.status, Availability::Unavailable);
        assert_eq!(snap.error.as_ref().unwrap().code, "network");
    }

    #[test]
    fn http_error_does_not_echo_body() {
        let snap = parse_usage_http(
            502,
            br#"{"access_token":"sk-ant-oat01-leaked"}"#,
            Path::new("/tmp/.credentials.json"),
        );
        let msg = &snap.error.as_ref().unwrap().message;
        assert!(msg.contains("HTTP 502"));
        assert!(!msg.contains("sk-ant"));
    }
}
