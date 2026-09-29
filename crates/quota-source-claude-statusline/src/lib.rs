//! Claude Code pushes a JSON object to its statusline command on stdin. That
//! object carries the account's live `rate_limits`, so a statusline hook is a
//! zero-credential source: nothing here reads a token, a keychain, or the
//! network.
//!
//! Pinned field names (see `fixtures/claude-statusline/`):
//!
//! ```text
//! rate_limits.five_hour.used_percentage     number, percent used, 0-100   2.1.80
//! rate_limits.five_hour.resets_at           Unix epoch seconds             2.1.80
//! rate_limits.seven_day.used_percentage                                    2.1.80
//! rate_limits.seven_day.resets_at                                          2.1.80
//! rate_limits.spend_limit.used_percentage   0-100, above 100 when over     2.1.251
//! rate_limits.spend_limit.resets_at                                        2.1.251
//! rate_limits.spend_limit.used_usd                                         2.1.284
//! rate_limits.spend_limit.limit_usd                                        2.1.284
//! rate_limits.spend_limit.period                                           2.1.284
//! ```
//!
//! Sources, at an immutable commit:
//! - <https://github.com/anthropics/claude-code/blob/8364969e9f5234ef3d9743cf7c790e9aab0ac3b1/CHANGELOG.md#L5334-L5336> (2.1.80: `rate_limits`, 5-hour and 7-day, `used_percentage`, `resets_at`)
//! - <https://github.com/anthropics/claude-code/blob/8364969e9f5234ef3d9743cf7c790e9aab0ac3b1/CHANGELOG.md#L1686-L1690> (2.1.251: `rate_limits.spend_limit`)
//! - <https://github.com/anthropics/claude-code/blob/8364969e9f5234ef3d9743cf7c790e9aab0ac3b1/CHANGELOG.md#L3-L7> (2.1.284: `spend_limit` gains `used_usd`, `limit_usd`, `period`)
//! - <https://code.claude.com/docs/en/statusline> as read on 2026-09-29
//!   (sha256 `6ddde6b55a53ae3a10c1c105729cb2c112ed19b30ff343c37f4a5241bce20080`):
//!   field table lines 193-195, example lines 293-305, absence rules line 339.
//!   The page documents the 2.1.80 and 2.1.251 fields; the 2.1.284 dollar
//!   fields are in the changelog only.
//!
//! `rate_limits` appears only for claude.ai Pro/Max subscribers or behind a
//! Claude apps gateway with a spend limit, and only after the first API
//! response of the session. Each window may be absent on its own, and Claude
//! Code drops a window once its `resets_at` passes. Absence yields no window,
//! never a guessed number; a window that is present but unreadable stays in
//! the push as unreadable.

#![forbid(unsafe_code)]

use quota_core::protocol::{ObserveParams, ObservedWindow, OBSERVE_SCHEMA_VERSION};
use quota_core::timeutil::parse_reset_at;
use quota_core::types::{ProviderId, Source, WindowKind};
use quota_core::windows::{FIVE_HOUR_SECS, WEEK_SECS};
use serde_json::Value;
use thiserror::Error;

/// Statusline payloads carry a transcript path and a few counters; anything
/// this large is not one.
pub const MAX_INPUT_BYTES: usize = 256 * 1024;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum StatuslineError {
    #[error("statusline input is empty")]
    Empty,
    #[error("statusline input is larger than {MAX_INPUT_BYTES} bytes")]
    TooLarge,
    #[error("statusline input is not valid JSON: {0}")]
    Json(String),
    #[error("statusline input is not a JSON object")]
    NotObject,
    #[error("rate_limits is present but is not an object")]
    RateLimitsShape,
}

/// What one statusline invocation told us about the account.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Statusline {
    pub windows: Vec<ObservedWindow>,
    /// `spend_limit.period` as Claude Code reports it (e.g. `"month"`).
    pub spend_period: Option<String>,
}

pub fn parse(input: &[u8]) -> Result<Statusline, StatuslineError> {
    if input.len() > MAX_INPUT_BYTES {
        return Err(StatuslineError::TooLarge);
    }
    if input.iter().all(u8::is_ascii_whitespace) {
        return Err(StatuslineError::Empty);
    }
    let root: Value =
        serde_json::from_slice(input).map_err(|e| StatuslineError::Json(e.to_string()))?;
    let Some(root) = root.as_object() else {
        return Err(StatuslineError::NotObject);
    };
    let limits = match root.get("rate_limits") {
        None | Some(Value::Null) => return Ok(Statusline::default()),
        Some(Value::Object(limits)) => limits,
        Some(_) => return Err(StatuslineError::RateLimitsShape),
    };
    let mut out = Statusline::default();
    for (key, kind, label, seconds) in [
        (
            "five_hour",
            WindowKind::FiveHour,
            "5h",
            Some(FIVE_HOUR_SECS),
        ),
        ("seven_day", WindowKind::Weekly, "weekly", Some(WEEK_SECS)),
        ("spend_limit", WindowKind::Spend, "spend", None),
    ] {
        // A bucket Claude Code reported but we cannot read stays in the push as
        // an unreadable window; dropping it would let the readable ones speak
        // for the whole account.
        let bucket = match limits.get(key) {
            None | Some(Value::Null) => continue,
            Some(bucket) => bucket,
        };
        out.windows.push(ObservedWindow {
            kind,
            label: label.to_string(),
            used_percent: finite_non_negative(bucket.get("used_percentage")),
            reset_at: bucket.get("resets_at").and_then(parse_reset_at),
            limit_window_seconds: seconds,
            used_usd: finite_non_negative(bucket.get("used_usd")),
            limit_usd: finite_non_negative(bucket.get("limit_usd")),
        });
        if key == "spend_limit" {
            out.spend_period = bucket
                .get("period")
                .and_then(Value::as_str)
                .map(sanitize_period);
        }
    }
    Ok(out)
}

/// A reported bucket whose number is missing, negative or not finite stays
/// unreadable downstream instead of becoming zero.
fn finite_non_negative(value: Option<&Value>) -> Option<f64> {
    value?.as_f64().filter(|n| n.is_finite() && *n >= 0.0)
}

/// The period lands in a terminal line; keep it to a short plain word.
fn sanitize_period(raw: &str) -> String {
    raw.chars()
        .filter(char::is_ascii_alphanumeric)
        .take(12)
        .collect()
}

impl Statusline {
    /// The observation to push, or `None` when Claude Code sent no rate limits.
    pub fn observe_params(&self) -> Option<ObserveParams> {
        (!self.windows.is_empty()).then(|| ObserveParams {
            schema: OBSERVE_SCHEMA_VERSION,
            provider: ProviderId::Claude,
            source: Source::Statusline,
            plan: None,
            windows: self.windows.clone(),
        })
    }

    /// One line for the status bar, e.g. `5h 34% ↺2h13m · wk 12% ↺3d4h`.
    /// Empty when there is nothing to show. Computed from the input alone so it
    /// still prints when `quotad` is down.
    pub fn segment(&self, now: i64) -> String {
        let mut parts = Vec::with_capacity(self.windows.len());
        for window in &self.windows {
            let name = match window.kind {
                WindowKind::FiveHour => "5h",
                WindowKind::Weekly => "wk",
                _ => "spend",
            };
            let mut part = match (window.kind.clone(), window.used_usd, window.limit_usd) {
                (WindowKind::Spend, Some(used), Some(limit)) => {
                    let period = self.spend_period.as_deref().unwrap_or("");
                    format!("${used:.0}/${limit:.0} {period}")
                        .trim_end()
                        .to_string()
                }
                _ => match window.used_percent {
                    Some(used) => format!("{name} {:.0}%", used.min(100.0)),
                    None => format!("{name} ?"),
                },
            };
            if let Some(reset) = window.reset_at.filter(|r| *r > now) {
                part.push_str(&format!(" ↺{}", format_countdown(reset - now)));
            }
            parts.push(part);
        }
        parts.join(" · ")
    }
}

fn format_countdown(secs: i64) -> String {
    let (days, hours, minutes) = (secs / 86_400, (secs % 86_400) / 3_600, (secs % 3_600) / 60);
    match (days, hours) {
        (0, 0) => format!("{minutes}m"),
        (0, _) => format!("{hours}h{minutes:02}m"),
        _ => format!("{days}d{hours}h"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// Fixed "now" that sits before every reset in the fixtures.
    const NOW: i64 = 1_746_536_400;

    fn fixtures() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/claude-statusline")
    }

    fn read(name: &str) -> Vec<u8> {
        std::fs::read(fixtures().join(name)).unwrap()
    }

    /// Compare against `<name>.golden.json`; `UPDATE_GOLDEN=1` rewrites it.
    fn assert_golden(name: &str) {
        let parsed = parse(&read(&format!("{name}.json"))).unwrap();
        let actual = serde_json::json!({
            "observe": parsed.observe_params(),
            "segment": parsed.segment(NOW),
        });
        let golden = fixtures().join(format!("{name}.golden.json"));
        if std::env::var("UPDATE_GOLDEN").is_ok_and(|v| v == "1") {
            let mut text = serde_json::to_string_pretty(&actual).unwrap();
            text.push('\n');
            std::fs::write(&golden, text).unwrap();
        }
        let expected: Value = serde_json::from_slice(&std::fs::read(&golden).unwrap()).unwrap();
        assert_eq!(actual, expected, "golden mismatch for {name}");
    }

    #[test]
    fn five_hour_and_seven_day_golden() {
        assert_golden("full-2.1.80");
    }

    #[test]
    fn spend_limit_golden() {
        assert_golden("spend-2.1.284");
    }

    #[test]
    fn partial_payload_golden() {
        assert_golden("five-hour-only");
    }

    #[test]
    fn fresh_session_has_no_rate_limits_and_pushes_nothing() {
        let parsed = parse(&read("fresh-session.json")).unwrap();
        assert!(parsed.windows.is_empty());
        assert!(parsed.observe_params().is_none());
        assert_eq!(parsed.segment(NOW), "");
    }

    #[test]
    fn pinned_field_names_decode_to_the_documented_values() {
        let parsed = parse(&read("full-2.1.80.json")).unwrap();
        let five = &parsed.windows[0];
        assert_eq!(five.kind, WindowKind::FiveHour);
        assert_eq!(five.used_percent, Some(34.0));
        assert_eq!(five.reset_at, Some(1_746_540_000));
        assert_eq!(five.limit_window_seconds, Some(18_000));
        let week = &parsed.windows[1];
        assert_eq!(week.kind, WindowKind::Weekly);
        assert_eq!(week.used_percent, Some(12.0));
        assert_eq!(week.limit_window_seconds, Some(604_800));
    }

    #[test]
    fn decode_errors_are_typed() {
        assert_eq!(parse(b"  \n"), Err(StatuslineError::Empty));
        assert_eq!(parse(b"[]"), Err(StatuslineError::NotObject));
        assert_eq!(parse(b"3"), Err(StatuslineError::NotObject));
        assert_eq!(
            parse(br#"{"rate_limits": "soon"}"#),
            Err(StatuslineError::RateLimitsShape)
        );
        assert!(matches!(parse(b"{ nope"), Err(StatuslineError::Json(_))));
        let huge = vec![b' '; MAX_INPUT_BYTES + 1];
        assert_eq!(parse(&huge), Err(StatuslineError::TooLarge));
    }

    #[test]
    fn json_errors_do_not_echo_the_input() {
        let Err(StatuslineError::Json(message)) = parse(br#"{"session_id": "s3cret-abc" nope}"#)
        else {
            panic!("expected a JSON error");
        };
        assert!(!message.contains("s3cret-abc"), "{message}");
    }

    #[test]
    fn unreadable_numbers_stay_unknown_not_zero() {
        let parsed = parse(
            br#"{"rate_limits":{"five_hour":{"used_percentage":"lots","resets_at":1746540000},
                "seven_day":{"used_percentage":-4}}}"#,
        )
        .unwrap();
        assert_eq!(parsed.windows.len(), 2);
        assert!(parsed.windows.iter().all(|w| w.used_percent.is_none()));
        assert_eq!(parsed.segment(NOW), "5h ? ↺1h00m · wk ?");
    }

    #[test]
    fn null_buckets_are_absent_but_malformed_ones_stay_unreadable() {
        let parsed = parse(
            br#"{"rate_limits":{"five_hour":null,"seven_day":7,"spend_limit":{"used_percentage":1}}}"#,
        )
        .unwrap();
        assert_eq!(parsed.windows.len(), 2);
        assert_eq!(parsed.windows[0].kind, WindowKind::Weekly);
        assert_eq!(parsed.windows[0].used_percent, None);
        assert_eq!(parsed.windows[1].kind, WindowKind::Spend);
        assert_eq!(parsed.observe_params().unwrap().windows.len(), 2);
    }

    #[test]
    fn a_malformed_weekly_bucket_is_pushed_beside_a_valid_five_hour() {
        for weekly in [r#""soon""#, "[]", "true", r#"{"used_percentage":"lots"}"#] {
            let input = format!(
                r#"{{"rate_limits":{{"five_hour":{{"used_percentage":10,"resets_at":1746540000}},"seven_day":{weekly}}}}}"#
            );
            let parsed = parse(input.as_bytes()).unwrap();
            let pushed = parsed.observe_params().unwrap().windows;
            assert_eq!(pushed.len(), 2, "{weekly}");
            assert_eq!(pushed[1].kind, WindowKind::Weekly);
            assert_eq!(pushed[1].used_percent, None, "{weekly}");
            assert!(parsed.segment(NOW).ends_with("wk ?"), "{weekly}");
        }
    }

    #[test]
    fn segment_drops_elapsed_resets_and_clamps_over_100() {
        let parsed =
            parse(br#"{"rate_limits":{"five_hour":{"used_percentage":130.0,"resets_at":100}}}"#)
                .unwrap();
        assert_eq!(parsed.segment(NOW), "5h 100%");
    }

    #[test]
    fn period_is_sanitized_before_it_reaches_a_terminal() {
        let parsed = parse(
            br#"{"rate_limits":{"spend_limit":{"used_usd":1.0,"limit_usd":5.0,"period":"mo\u001b[31mnth!!"}}}"#,
        )
        .unwrap();
        assert_eq!(parsed.spend_period.as_deref(), Some("mo31mnth"));
    }
}
