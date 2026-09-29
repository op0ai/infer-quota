//! Pushed observations (`observe`): validation and the snapshot they become.
//!
//! A push is evidence from a passive source. The daemon stamps it with its own
//! clock, so a client can neither backdate a reading nor keep a stale one
//! looking fresh, and the normal evidence-age rules then apply to it.

use quota_core::protocol::OBSERVE_SCHEMA_VERSION;
use quota_core::protocol::{ObserveParams, ObservedWindow, OBSERVE_MAX_WINDOWS};
use quota_core::types::{
    ProviderId, ProviderObservation, ProviderPermission, ProviderSnapshot, Source, UsageWindow,
    WindowKind, WindowState, DEFAULT_READING_MAX_AGE_SECS,
};

/// While a push repeats the same numbers, the ring takes at most one entry per
/// this many seconds; the rest refresh the newest entry in place.
pub const PUSH_RING_MIN_GAP_SECS: i64 = 60;

#[derive(Debug, PartialEq, Eq)]
pub struct Rejection {
    pub code: &'static str,
    pub message: String,
}

fn reject(code: &'static str, message: impl Into<String>) -> Rejection {
    Rejection {
        code,
        message: message.into(),
    }
}

pub fn snapshot_from_push(params: &ObserveParams, now: i64) -> Result<ProviderSnapshot, Rejection> {
    if params.schema != OBSERVE_SCHEMA_VERSION {
        return Err(reject(
            "unsupported_schema",
            format!(
                "observe schema {} is not supported (this daemon speaks {OBSERVE_SCHEMA_VERSION})",
                params.schema
            ),
        ));
    }
    if params.source != Source::Statusline {
        return Err(reject(
            "unsupported_source",
            "only source=statusline may push observations",
        ));
    }
    if params.provider != ProviderId::Claude {
        return Err(reject(
            "unsupported_provider",
            "the statusline source pushes for claude only",
        ));
    }
    if params.windows.is_empty() || params.windows.len() > OBSERVE_MAX_WINDOWS {
        return Err(reject(
            "bad_params",
            format!("observe needs 1..={OBSERVE_MAX_WINDOWS} windows"),
        ));
    }
    if let Some(bad) = params.windows.iter().find(|w| !label_ok(&w.label)) {
        return Err(reject(
            "bad_params",
            format!(
                "window label {:?} is not 1..=32 printable characters",
                bad.label
            ),
        ));
    }
    let windows: Vec<UsageWindow> = params.windows.iter().map(|w| window_at(w, now)).collect();
    if windows.iter().all(|w| w.state == WindowState::Unknown) {
        return Err(reject(
            "unreadable",
            "no pushed window had a readable measurement",
        ));
    }
    Ok(ProviderSnapshot::observed(ProviderObservation {
        provider: params.provider,
        source: Some(Source::Statusline),
        windows,
        credits: None,
        plan: params
            .plan
            .as_ref()
            .map(|plan| plan.chars().filter(|c| !c.is_control()).take(32).collect()),
        credential_path: None,
        observed_at: Some(now),
        max_age_secs: DEFAULT_READING_MAX_AGE_SECS,
        permission: ProviderPermission::Unknown,
    }))
}

fn label_ok(label: &str) -> bool {
    (1..=32).contains(&label.chars().count()) && !label.chars().any(char::is_control)
}

fn valid(number: Option<f64>) -> Option<f64> {
    number.filter(|n| n.is_finite() && *n >= 0.0)
}

/// Exactly one constructor builds each window, so its unit and numbers always
/// agree. A `spend` window with dollars and a positive cap is a USD window
/// whose percent is derived from those dollars. Without a cap, the source's
/// percent wins and no dollars are attached; with neither, dollars alone make
/// an uncapped USD window. Dollars on any other kind are ignored.
fn window_at(w: &ObservedWindow, now: i64) -> UsageWindow {
    let reset = w.reset_at.filter(|r| *r > 0);
    let seconds = w.limit_window_seconds.filter(|s| *s > 0);
    let observed = Some(now);
    let (used_usd, cap) = match w.kind {
        WindowKind::Spend => (
            valid(w.used_usd),
            valid(w.limit_usd).filter(|limit| *limit > 0.0),
        ),
        _ => (None, None),
    };
    match (used_usd, cap, valid(w.used_percent)) {
        (Some(used), Some(_), _) | (Some(used), None, None) => UsageWindow::from_spend_at(
            w.kind.clone(),
            &w.label,
            used,
            cap,
            reset,
            seconds,
            observed,
            DEFAULT_READING_MAX_AGE_SECS,
        ),
        (_, _, Some(percent)) => UsageWindow::from_percent_at(
            w.kind.clone(),
            &w.label,
            percent,
            reset,
            seconds,
            observed,
            DEFAULT_READING_MAX_AGE_SECS,
        ),
        (None, _, None) => UsageWindow::unreadable(
            w.kind.clone(),
            &w.label,
            reset,
            seconds,
            observed,
            DEFAULT_READING_MAX_AGE_SECS,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quota_core::types::{Availability, Freshness};

    fn params(windows: Vec<ObservedWindow>) -> ObserveParams {
        ObserveParams {
            schema: OBSERVE_SCHEMA_VERSION,
            provider: ProviderId::Claude,
            source: Source::Statusline,
            plan: None,
            windows,
        }
    }

    fn five_hour(used: Option<f64>) -> ObservedWindow {
        ObservedWindow {
            kind: WindowKind::FiveHour,
            label: "5h".into(),
            used_percent: used,
            reset_at: Some(1_900_000_000),
            limit_window_seconds: Some(18_000),
            used_usd: None,
            limit_usd: None,
        }
    }

    #[test]
    fn push_is_stamped_with_the_daemon_clock_and_ages_like_any_evidence() {
        let now = 1_800_000_000;
        let snap = snapshot_from_push(&params(vec![five_hour(Some(34.0))]), now).unwrap();
        assert_eq!(snap.source, Some(Source::Statusline));
        assert_eq!(snap.observed_at, Some(now));
        assert_eq!(snap.windows[0].observed_at, Some(now));
        assert_eq!(snap.windows[0].used_percent, Some(34.0));
        let mut later = snap.clone();
        later.refresh_freshness(now + DEFAULT_READING_MAX_AGE_SECS as i64 + 1);
        assert_eq!(later.status, Availability::Stale);
        assert_eq!(later.freshness, Freshness::Stale);
    }

    #[test]
    fn unknown_schema_source_and_provider_are_refused_with_typed_codes() {
        let mut p = params(vec![five_hour(Some(1.0))]);
        p.schema = 2;
        assert_eq!(
            snapshot_from_push(&p, 1).unwrap_err().code,
            "unsupported_schema"
        );
        let mut p = params(vec![five_hour(Some(1.0))]);
        p.source = Source::Oauth;
        assert_eq!(
            snapshot_from_push(&p, 1).unwrap_err().code,
            "unsupported_source"
        );
        let mut p = params(vec![five_hour(Some(1.0))]);
        p.provider = ProviderId::Codex;
        assert_eq!(
            snapshot_from_push(&p, 1).unwrap_err().code,
            "unsupported_provider"
        );
    }

    #[test]
    fn window_count_and_labels_are_bounded() {
        assert_eq!(
            snapshot_from_push(&params(vec![]), 1).unwrap_err().code,
            "bad_params"
        );
        let many = vec![five_hour(Some(1.0)); OBSERVE_MAX_WINDOWS + 1];
        assert_eq!(
            snapshot_from_push(&params(many), 1).unwrap_err().code,
            "bad_params"
        );
        let mut w = five_hour(Some(1.0));
        w.label = "5h\u{1b}[31m".into();
        assert_eq!(
            snapshot_from_push(&params(vec![w]), 1).unwrap_err().code,
            "bad_params"
        );
    }

    #[test]
    fn a_push_with_nothing_readable_cannot_replace_good_evidence() {
        let rejection = snapshot_from_push(&params(vec![five_hour(None)]), 1).unwrap_err();
        assert_eq!(rejection.code, "unreadable");
        let negative = five_hour(Some(-3.0));
        assert_eq!(
            snapshot_from_push(&params(vec![negative]), 1)
                .unwrap_err()
                .code,
            "unreadable"
        );
    }

    #[test]
    fn one_unreadable_window_stays_unknown_beside_a_readable_one() {
        let mut weekly = five_hour(None);
        weekly.kind = WindowKind::Weekly;
        weekly.label = "weekly".into();
        let snap = snapshot_from_push(&params(vec![five_hour(Some(10.0)), weekly]), 1_800_000_000)
            .unwrap();
        assert_eq!(snap.windows[1].state, WindowState::Unknown);
        assert!(snap.windows[1].reading.is_none());
    }

    fn spend(
        used_percent: Option<f64>,
        used_usd: Option<f64>,
        limit_usd: Option<f64>,
    ) -> ObservedWindow {
        ObservedWindow {
            kind: WindowKind::Spend,
            label: "spend".into(),
            used_percent,
            reset_at: Some(1_900_000_000),
            limit_window_seconds: None,
            used_usd,
            limit_usd,
        }
    }

    fn only_window(w: ObservedWindow) -> UsageWindow {
        let snap = snapshot_from_push(&params(vec![w]), 1_800_000_000).unwrap();
        snap.windows.into_iter().next().unwrap()
    }

    /// Every money field on the window and on its reading agree with each other.
    fn assert_coherent_usd(w: &UsageWindow, used: f64, limit: f64) {
        let reading = w.reading.as_ref().unwrap();
        for (unit, used_usd, limit_usd, lim, remaining) in [
            (&w.unit, w.used_usd, w.limit_usd, w.limit, w.remaining),
            (
                &reading.unit,
                reading.used_usd,
                reading.limit_usd,
                reading.limit,
                reading.remaining,
            ),
        ] {
            assert_eq!(unit.as_deref(), Some("usd"));
            assert_eq!(used_usd, Some(used));
            assert_eq!(limit_usd, Some(limit));
            assert_eq!(lim, Some(limit));
            assert_eq!(remaining, Some((limit - used).max(0.0)));
        }
        let percent = used / limit * 100.0;
        assert_eq!(w.used_percent, Some(percent));
        assert_eq!(reading.used_percent, Some(percent));
        assert_eq!(
            w.remaining_percent,
            Some((100.0 - percent).clamp(0.0, 100.0))
        );
    }

    #[test]
    fn spend_windows_keep_their_dollars() {
        let w = only_window(spend(Some(54.28), Some(271.4), Some(500.0)));
        assert_eq!(w.used_usd, Some(271.4));
        assert_eq!(w.limit_usd, Some(500.0));
        // Derived from the dollars, so it agrees with them to the last bit
        // rather than echoing the pushed percent.
        assert!((w.used_percent.unwrap() - 54.28).abs() < 1e-9);
        assert_coherent_usd(&w, 271.4, 500.0);
        assert_eq!(w.state, WindowState::Ok);
    }

    #[test]
    fn spend_percent_is_derived_from_dollars_when_the_two_disagree() {
        let w = only_window(spend(Some(10.0), Some(400.0), Some(500.0)));
        assert_coherent_usd(&w, 400.0, 500.0);
        assert_eq!(w.used_percent, Some(80.0));
    }

    #[test]
    fn spend_over_its_cap_is_exhausted_with_no_dollars_left() {
        let w = only_window(spend(Some(112.0), Some(560.0), Some(500.0)));
        assert_coherent_usd(&w, 560.0, 500.0);
        assert_eq!(w.state, WindowState::Exhausted);
        assert_eq!(w.remaining, Some(0.0));
        assert_eq!(w.remaining_percent, Some(0.0));
    }

    #[test]
    fn spend_without_dollars_or_without_a_cap_is_a_plain_percent_window() {
        for w in [
            spend(Some(40.0), None, None),
            spend(Some(40.0), Some(200.0), None),
            spend(Some(40.0), Some(200.0), Some(0.0)),
            spend(Some(40.0), None, Some(500.0)),
        ] {
            let w = only_window(w);
            assert_eq!(w.unit.as_deref(), Some("percent"));
            assert_eq!(w.used_percent, Some(40.0));
            assert_eq!((w.used_usd, w.limit_usd), (None, None));
            assert_eq!((w.remaining, w.limit), (None, None));
            let reading = w.reading.unwrap();
            assert_eq!(reading.unit.as_deref(), Some("percent"));
            assert_eq!((reading.used_usd, reading.limit_usd), (None, None));
        }
    }

    #[test]
    fn spend_dollars_without_a_cap_or_percent_record_spend_and_no_headroom() {
        let w = only_window(spend(None, Some(12.5), None));
        assert_eq!(w.unit.as_deref(), Some("usd"));
        assert_eq!(w.used_usd, Some(12.5));
        assert_eq!((w.limit_usd, w.limit, w.remaining), (None, None, None));
        assert_eq!(w.remaining_percent, None);
    }

    #[test]
    fn dollars_on_a_non_spend_window_are_ignored() {
        let mut w = five_hour(Some(34.0));
        w.used_usd = Some(9.0);
        w.limit_usd = Some(10.0);
        let w = only_window(w);
        assert_eq!(w.unit.as_deref(), Some("percent"));
        assert_eq!(w.used_percent, Some(34.0));
        assert_eq!((w.used_usd, w.limit_usd), (None, None));
    }

    #[test]
    fn a_push_cannot_claim_a_reset_in_the_pre_epoch() {
        let mut w = five_hour(Some(5.0));
        w.reset_at = Some(-5);
        let snap = snapshot_from_push(&params(vec![w]), 1_800_000_000).unwrap();
        assert_eq!(snap.windows[0].reset_at, None);
    }
}
