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
//! We only implement the file-based path. We do not talk to Keychain in v0
//! (document it). We never write the credentials file. API-key mode cannot
//! use this endpoint — we report `unavailable`, not fake percents.
//!
//! This endpoint is widely reported as aggressively rate-limited. The daemon
//! backs off; a 429 is `unavailable`, never a guessed number.

use std::path::Path;

use quota_core::types::{
    AdapterError, Availability, Credits, ProviderId, ProviderSnapshot, Source, UsageWindow,
    WindowKind,
};

use crate::creds::{load_claude_creds, ClaudeCreds, CredsError};
use crate::http::Transport;
use crate::provider::{ProbeCtx, Provider};

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const OAUTH_BETA: &str = "oauth-2025-04-20";

#[derive(Default)]
pub struct ClaudeAdapter;

impl Provider for ClaudeAdapter {
    fn id(&self) -> ProviderId {
        ProviderId::Claude
    }

    fn probe(&self, ctx: &ProbeCtx<'_>) -> ProviderSnapshot {
        match load_claude_creds() {
            Ok(creds) => fetch_usage(ctx.transport, &creds, ctx.now),
            Err(e) => {
                let mut snap = ProviderSnapshot::unavailable(
                    ProviderId::Claude,
                    AdapterError::new(creds_code(&e), e.to_string()),
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
            let mut snap = parse_usage_http(resp.status, &resp.body, &creds.path);
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
        return snap;
    }
    if !(200..300).contains(&status) {
        let hint = String::from_utf8_lossy(body);
        let hint = hint.chars().take(160).collect::<String>();
        let mut snap = ProviderSnapshot::unavailable(
            ProviderId::Claude,
            AdapterError::new("http", format!("HTTP {status}: {hint}")),
        );
        snap.source = Some(Source::Oauth);
        snap.credential_path = Some(path);
        return snap;
    }
    parse_usage_json(body, &path)
}

fn parse_usage_json(body: &[u8], path: &str) -> ProviderSnapshot {
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
    push_bucket(&mut windows, &v, "five_hour", WindowKind::FiveHour, "5h");
    push_bucket(&mut windows, &v, "seven_day", WindowKind::Weekly, "weekly");
    push_bucket(
        &mut windows,
        &v,
        "seven_day_opus",
        WindowKind::Extra,
        "opus weekly",
    );
    push_bucket(
        &mut windows,
        &v,
        "seven_day_sonnet",
        WindowKind::Extra,
        "sonnet weekly",
    );

    let credits = v.get("extra_usage").and_then(map_extra_usage);
    if let Some(extra) = v.get("extra_usage") {
        if let Some(util) = extra.get("utilization").and_then(|x| x.as_f64()) {
            windows.push(UsageWindow::from_percent(
                WindowKind::Monthly,
                "extra usage",
                util,
                None,
                None,
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

    ProviderSnapshot {
        provider: ProviderId::Claude,
        status: Availability::Ok,
        source: Some(Source::Oauth),
        windows,
        credits,
        plan: None,
        error: None,
        credential_path: Some(path.to_string()),
    }
}

fn push_bucket(
    windows: &mut Vec<UsageWindow>,
    root: &serde_json::Value,
    key: &str,
    kind: WindowKind,
    label: &str,
) {
    let Some(node) = root.get(key) else {
        return;
    };
    if node.is_null() {
        return;
    }
    let Some(used) = node.get("utilization").and_then(|x| x.as_f64()) else {
        return;
    };
    let reset = node
        .get("resets_at")
        .and_then(quota_core::timeutil::parse_reset_at);
    windows.push(UsageWindow::from_percent(kind, label, used, reset, None));
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
    fn rate_limited() {
        let snap = parse_usage_http(429, b"{}", Path::new("c.json"));
        assert_eq!(snap.status, Availability::Unavailable);
        assert_eq!(snap.error.as_ref().unwrap().code, "rate_limited");
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
}
