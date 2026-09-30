//! Burn-rate, ETA, and `can_start` helpers.
//!
//! Inputs are whatever the adapters actually observed. If a source only
//! publishes `used_percent`, we refuse to invent a token budget.

use std::collections::HashSet;

use crate::types::{
    Availability, CanStartAnswer, CanStartBasis, Freshness, PaceReport, PercentAdmission,
    ProviderId, ProviderSnapshot, Snapshot, UsageWindow, WindowKind, WindowState,
};

/// Percent that must stay untouched after an admitted job unless the caller says otherwise.
pub const DEFAULT_RESERVE_PERCENT: f64 = 2.0;

/// Drop samples that look like a window reset (used_percent fell by more than
/// `RESET_DROP`) and keep the last contiguous run.
const RESET_DROP: f64 = 1.0;

/// `(unix_secs, used_percent)` samples, oldest first.
pub fn select_samples(samples: &[(i64, f64)]) -> &[(i64, f64)] {
    if samples.len() < 2 {
        return samples;
    }
    let mut start = 0;
    for i in 1..samples.len() {
        if samples[i].1 + RESET_DROP < samples[i - 1].1 {
            start = i;
        }
    }
    &samples[start..]
}

/// Percent consumed per second from a contiguous sample run. `None` if we
/// cannot honestly compute a rate (too few points, or no time elapsed).
pub fn burn_percent_per_sec(samples: &[(i64, f64)]) -> Option<f64> {
    let run = select_samples(samples);
    if run.len() < 2 {
        return None;
    }
    let first = run.first()?;
    let last = run.last()?;
    let dt = last.0 - first.0;
    if dt <= 0 {
        return None;
    }
    Some((last.1 - first.1) / dt as f64)
}

pub fn eta_empty_secs(remaining_percent: f64, burn_per_sec: f64) -> Option<f64> {
    if remaining_percent <= 0.0 {
        return Some(0.0);
    }
    if burn_per_sec <= 0.0 {
        return None;
    }
    Some(remaining_percent / burn_per_sec)
}

fn kind_rank(kind: &WindowKind) -> u8 {
    match kind {
        WindowKind::Session => 0,
        WindowKind::FiveHour => 1,
        WindowKind::Weekly => 2,
        WindowKind::Monthly => 3,
        WindowKind::Extra => 4,
        WindowKind::Spend => 5,
    }
}

fn remaining_share(window: &UsageWindow) -> Option<f64> {
    window
        .remaining_percent
        .or_else(|| match (window.remaining, window.limit) {
            (Some(remaining), Some(limit)) if limit > 0.0 => Some(remaining / limit * 100.0),
            _ => None,
        })
}

/// The readable window with the least capacity left. Kind order only breaks ties.
fn binding_window(snap: &ProviderSnapshot) -> Option<&UsageWindow> {
    let share = |w: &UsageWindow| remaining_share(w).unwrap_or(f64::INFINITY);
    snap.windows
        .iter()
        .filter(|w| w.state != WindowState::Unknown)
        .min_by(|a, b| {
            share(a)
                .total_cmp(&share(b))
                .then_with(|| kind_rank(&a.kind).cmp(&kind_rank(&b.kind)))
        })
}

fn availability_message(snapshot: &ProviderSnapshot) -> String {
    if snapshot.status == Availability::Stale {
        return "provider observation is stale".to_string();
    }
    snapshot
        .error
        .as_ref()
        .map(|e| e.message.clone())
        .unwrap_or_else(|| "provider unavailable".to_string())
}

fn unavailable_answer(provider: ProviderId, explanation: String) -> CanStartAnswer {
    CanStartAnswer {
        provider,
        ok: false,
        basis: CanStartBasis::Unavailable,
        explanation,
        window_kind: None,
        remaining_percent: None,
        remaining_tokens: None,
        reset_at: None,
        eta_empty_secs: None,
        burn_percent_per_hour: None,
        admission: None,
    }
}

/// Shared refusal precedence for token and percent admission. In particular,
/// a reported unreadable window stays a veto even when the provider marks the
/// whole response unavailable because every measurement was unreadable.
fn eligibility_refusal(latest: &ProviderSnapshot) -> Option<CanStartAnswer> {
    if latest.is_for_another_account() {
        return Some(CanStartAnswer {
            provider: latest.provider,
            ok: false,
            basis: CanStartBasis::AccountChanged,
            explanation: format!(
                "{} account changed since reading; its quota is unknown until a refresh",
                latest.provider
            ),
            window_kind: None,
            remaining_percent: None,
            remaining_tokens: None,
            reset_at: None,
            eta_empty_secs: None,
            burn_percent_per_hour: None,
            admission: None,
        });
    }
    if latest.status == Availability::Stale || latest.freshness == Freshness::Stale {
        return Some(unavailable_answer(
            latest.provider,
            availability_message(latest),
        ));
    }
    if latest.permission == crate::types::ProviderPermission::LimitReached {
        return Some(unavailable_answer(
            latest.provider,
            "provider permission is limit_reached".to_string(),
        ));
    }
    let unknown: Vec<&UsageWindow> = latest
        .windows
        .iter()
        .filter(|window| window.state == WindowState::Unknown)
        .collect();
    if let Some(first) = unknown.first() {
        let labels: Vec<&str> = unknown.iter().map(|window| window.label.as_str()).collect();
        return Some(CanStartAnswer {
            provider: latest.provider,
            ok: false,
            basis: CanStartBasis::UnknownWindow,
            explanation: format!(
                "{} {} has no readable measurement and may be exhausted",
                latest.provider,
                labels.join(", ")
            ),
            window_kind: Some(first.kind.clone()),
            remaining_percent: None,
            remaining_tokens: None,
            reset_at: first.reset_at,
            eta_empty_secs: None,
            burn_percent_per_hour: None,
            admission: None,
        });
    }
    if latest.status != Availability::Ok {
        return Some(unavailable_answer(
            latest.provider,
            availability_message(latest),
        ));
    }
    None
}

/// Samples of one window from readings of the same provider account as `latest`.
/// Another account's readings never feed this account's pace; a reading whose
/// account is unknown matches only a latest reading whose account is unknown.
/// `history` must hold only readings of the active quota account: an unknown
/// provider account cannot tell two quota accounts apart.
/// A window is identified by kind *and* label so Cursor included/on-demand
/// and Claude Opus/Sonnet weekly limits cannot borrow each other's burn.
fn samples_for<'a, I>(
    history: I,
    latest: &ProviderSnapshot,
    window: &UsageWindow,
) -> Vec<(i64, f64)>
where
    I: IntoIterator<Item = &'a Snapshot>,
{
    // A Snapshot can be appended when a different provider refreshes. Pace
    // samples belong to the source observation, so use its evidence timestamp
    // and count an unchanged timestamp/value pair only once. Different values
    // observed in the same second remain visible, though they cannot imply a
    // burn rate until time elapses.
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for snap in history {
        if let Some(p) = snap
            .by_id(latest.provider)
            .filter(|p| p.account_digest == latest.account_digest)
        {
            if let Some(w) = p
                .windows
                .iter()
                .find(|w| w.kind == window.kind && w.label == window.label)
            {
                if let (Some(observed_at), Some(used)) =
                    (w.observed_at.or(p.observed_at), w.used_percent)
                {
                    if seen.insert((observed_at, used.to_bits())) {
                        out.push((observed_at, used));
                    }
                }
            }
        }
    }
    out.sort_by_key(|sample| sample.0);
    out
}

pub fn pace_for<'a, I>(history: I, latest: &ProviderSnapshot) -> PaceReport
where
    I: IntoIterator<Item = &'a Snapshot>,
{
    if latest.status != Availability::Ok {
        let msg = availability_message(latest);
        return PaceReport {
            provider: latest.provider,
            window_kind: None,
            used_percent: None,
            remaining_percent: None,
            burn_percent_per_hour: None,
            eta_empty_secs: None,
            reset_at: None,
            samples: 0,
            explanation: msg,
        };
    }
    let Some(window) = binding_window(latest) else {
        return PaceReport {
            provider: latest.provider,
            window_kind: None,
            used_percent: None,
            remaining_percent: None,
            burn_percent_per_hour: None,
            eta_empty_secs: None,
            reset_at: None,
            samples: 0,
            explanation: "no readable usage windows in last snapshot".to_string(),
        };
    };
    let samples = samples_for(history, latest, window);
    let burn = burn_percent_per_sec(&samples);
    let burn_hour = burn.map(|b| b * 3600.0);
    let rem = window.remaining_percent.unwrap_or(0.0);
    let eta = burn.and_then(|b| eta_empty_secs(rem, b));
    let explanation = match (burn_hour, eta, window.reset_at) {
        (None, _, _) => format!(
            "{} {}: {}% used, need more samples for a burn rate",
            latest.provider,
            window.label,
            window.used_percent.unwrap_or(0.0)
        ),
        (Some(h), Some(eta), Some(reset)) => format!(
            "{} {}: burning {:.2}%/h, empty in {:.0}s unless reset at {}",
            latest.provider,
            window.label,
            h,
            eta,
            crate::timeutil::format_rfc3339(reset)
        ),
        (Some(h), Some(eta), None) => {
            format!(
                "{} {}: burning {:.2}%/h, empty in {:.0}s (no reset published)",
                latest.provider, window.label, h, eta
            )
        }
        (Some(h), None, _) => format!(
            "{} {}: {:.2}%/h (not exhausting at current rate)",
            latest.provider, window.label, h
        ),
    };
    PaceReport {
        provider: latest.provider,
        window_kind: Some(window.kind.clone()),
        used_percent: window.used_percent,
        remaining_percent: window.remaining_percent,
        burn_percent_per_hour: burn_hour,
        eta_empty_secs: eta,
        reset_at: window.reset_at,
        samples: samples.len() as u32,
        explanation,
    }
}

/// Decide whether a job of `tokens` can finish before `deadline` (default:
/// window reset). We never convert percent→tokens.
pub fn can_start<'a, I>(
    latest: &ProviderSnapshot,
    history: I,
    tokens: u64,
    deadline: Option<i64>,
    now: i64,
) -> CanStartAnswer
where
    I: IntoIterator<Item = &'a Snapshot>,
{
    if let Some(refusal) = eligibility_refusal(latest) {
        return refusal;
    }
    let Some(window) = binding_window(latest) else {
        return CanStartAnswer {
            provider: latest.provider,
            ok: false,
            basis: CanStartBasis::Unavailable,
            explanation: "no readable usage windows published".to_string(),
            window_kind: None,
            remaining_percent: None,
            remaining_tokens: None,
            reset_at: None,
            eta_empty_secs: None,
            burn_percent_per_hour: None,
            admission: None,
        };
    };

    let samples = samples_for(history, latest, window);
    let burn = burn_percent_per_sec(&samples);
    let burn_hour = burn.map(|b| b * 3600.0);
    let rem_pct = window.remaining_percent;
    let eta = match (rem_pct, burn) {
        (Some(r), Some(b)) => eta_empty_secs(r, b),
        (Some(r), None) if r <= 0.0 => Some(0.0),
        _ => None,
    };

    let bind = match (deadline, window.reset_at) {
        (Some(d), Some(r)) => Some(d.min(r)),
        (Some(d), None) => Some(d),
        (None, Some(r)) => Some(r),
        (None, None) => None,
    };

    let remaining_tokens = match (window.remaining, window.unit.as_deref()) {
        (Some(n), Some("tokens")) => Some(n),
        (Some(n), Some("credits")) => Some(n),
        (Some(n), _) if window.used_percent.is_none() => Some(n),
        _ => None,
    };

    if let Some(left) = remaining_tokens {
        let projected = match (burn, bind) {
            (Some(_), Some(t)) if t > now && window.limit.or(window.remaining).is_some() => {
                // Token burn is unknown when the source only samples percent.
                // Project nothing extra; compare remaining vs tokens only.
                0.0
            }
            _ => 0.0,
        };
        let leftover = left - projected;
        let ok = leftover + f64::EPSILON >= tokens as f64;
        let explanation = if ok {
            format!(
                "{} {} has {:.0} {} left; {} tokens requested fits before {}",
                latest.provider,
                window.label,
                leftover,
                window.unit.as_deref().unwrap_or("units"),
                tokens,
                bind.map(crate::timeutil::format_rfc3339)
                    .unwrap_or_else(|| "no deadline".to_string())
            )
        } else {
            format!(
                "{} {} has {:.0} {} left; {} tokens requested does not fit",
                latest.provider,
                window.label,
                leftover,
                window.unit.as_deref().unwrap_or("units"),
                tokens
            )
        };
        return CanStartAnswer {
            provider: latest.provider,
            ok,
            basis: CanStartBasis::TokenBudget,
            explanation,
            window_kind: Some(window.kind.clone()),
            remaining_percent: rem_pct,
            remaining_tokens: Some(left),
            reset_at: window.reset_at,
            eta_empty_secs: eta,
            burn_percent_per_hour: burn_hour,
            admission: None,
        };
    }

    // Percent-only path: do not invent a token mapping.
    let rem = rem_pct.unwrap_or(0.0);
    if rem <= 0.0 {
        return CanStartAnswer {
            provider: latest.provider,
            ok: false,
            basis: CanStartBasis::PercentOnly,
            explanation: format!(
                "{} {} is exhausted (0% remaining). Reset {}",
                latest.provider,
                window.label,
                window
                    .reset_at
                    .map(crate::timeutil::format_rfc3339)
                    .unwrap_or_else(|| "unpublished".to_string())
            ),
            window_kind: Some(window.kind.clone()),
            remaining_percent: rem_pct,
            remaining_tokens: None,
            reset_at: window.reset_at,
            eta_empty_secs: eta,
            burn_percent_per_hour: burn_hour,
            admission: None,
        };
    }

    if tokens == 0 {
        return CanStartAnswer {
            provider: latest.provider,
            ok: true,
            basis: CanStartBasis::PercentOnly,
            explanation: format!(
                "zero-token job; {} {} has {:.1}% remaining",
                latest.provider, window.label, rem
            ),
            window_kind: Some(window.kind.clone()),
            remaining_percent: rem_pct,
            remaining_tokens: None,
            reset_at: window.reset_at,
            eta_empty_secs: eta,
            burn_percent_per_hour: burn_hour,
            admission: None,
        };
    }

    let pace_note = match (eta, bind) {
        (Some(eta), Some(t)) if t > now && eta < (t - now) as f64 => {
            format!(
                " Current burn would empty the window in {:.0}s, before the binding deadline.",
                eta
            )
        }
        _ => String::new(),
    };

    CanStartAnswer {
        provider: latest.provider,
        ok: false,
        basis: CanStartBasis::PercentOnly,
        explanation: format!(
            "{} {} reports {:.1}% remaining (percent only). Cannot map {} tokens onto an unpublished token limit.{}",
            latest.provider, window.label, rem, tokens, pace_note
        ),
        window_kind: Some(window.kind.clone()),
        remaining_percent: rem_pct,
        remaining_tokens: None,
        reset_at: window.reset_at,
        eta_empty_secs: eta,
        burn_percent_per_hour: burn_hour,
        admission: None,
    }
}

/// One window's verdict for a percent request; `slack` orders windows by how
/// close they are to refusing.
struct PercentVerdict<'a> {
    window: &'a UsageWindow,
    admission: PercentAdmission,
    burn_hour: Option<f64>,
    eta: Option<f64>,
    slack: f64,
}

fn judge_window<'w, 'h, I>(
    window: &'w UsageWindow,
    latest: &ProviderSnapshot,
    history: I,
    percent: f64,
    reserve: f64,
    deadline: Option<i64>,
    now: i64,
) -> Option<PercentVerdict<'w>>
where
    I: IntoIterator<Item = &'h Snapshot>,
{
    let remaining = window.remaining_percent?;
    let remaining_after = remaining - percent;
    let headroom_ok = remaining_after >= reserve;
    let burn = burn_percent_per_sec(&samples_for(history, latest, window));
    let eta = burn.and_then(|b| eta_empty_secs(remaining_after, b));
    let burning = burn.is_some_and(|b| b > 0.0);
    let survives = |until: i64| eta.is_none_or(|eta| eta >= (until - now) as f64);
    let (pace_checked, pace_ok) = match window.reset_at.filter(|reset| *reset > now) {
        Some(reset) if burning => (true, survives(reset)),
        _ => (false, true),
    };
    let deadline_ok = deadline.is_none_or(|d| d > now && (!burning || survives(d)));
    Some(PercentVerdict {
        window,
        admission: PercentAdmission {
            requested_percent: percent,
            reserve_percent: reserve,
            remaining_after_percent: remaining_after,
            headroom_ok,
            pace_ok,
            pace_checked,
            deadline_ok,
        },
        burn_hour: burn.map(|b| b * 3600.0),
        eta,
        slack: remaining_after - reserve,
    })
}

/// Percent admission: may a job that spends about `percent` of a window start?
///
/// Every readable window that publishes a remaining percent must pass, so a
/// 5-hour window at 90% refuses a 15% job even when the weekly window is
/// empty-handed. The tightest window is the one reported. A window passes when
/// all three hold: `remaining - percent >= reserve`; at the current burn the
/// remainder after the job outlasts the window's reset (the pace veto); and, if
/// a `deadline` was given, it is still ahead and the remainder outlasts it. A
/// deadline only ever adds a refusal; it never waives the pace veto.
pub fn can_start_percent<'h, I>(
    latest: &ProviderSnapshot,
    history: I,
    percent: f64,
    reserve: f64,
    deadline: Option<i64>,
    now: i64,
) -> CanStartAnswer
where
    I: IntoIterator<Item = &'h Snapshot> + Clone,
{
    if let Some(refusal) = eligibility_refusal(latest) {
        return refusal;
    }
    let mut verdicts: Vec<PercentVerdict<'_>> = latest
        .windows
        .iter()
        .filter_map(|w| judge_window(w, latest, history.clone(), percent, reserve, deadline, now))
        .collect();
    if verdicts.is_empty() {
        return unavailable_answer(
            latest.provider,
            "no readable window publishes a remaining percent".to_string(),
        );
    }
    verdicts.sort_by(|a, b| a.slack.total_cmp(&b.slack));
    let failing = verdicts
        .iter()
        .position(|v| !(v.admission.headroom_ok && v.admission.pace_ok && v.admission.deadline_ok));
    let chosen = &verdicts[failing.unwrap_or(0)];
    let ok = failing.is_none();
    let window = chosen.window;
    let admission = chosen.admission.clone();
    let reset = window
        .reset_at
        .map(crate::timeutil::format_rfc3339)
        .unwrap_or_else(|| "unpublished".to_string());
    let pace_note = match (admission.pace_checked, chosen.eta) {
        (true, Some(eta)) => format!("pace: empties in {eta:.0}s after the job"),
        (true, None) => "pace: not exhausting at the current burn".to_string(),
        (false, _) => "pace: no burn rate or no future reset yet, not a veto".to_string(),
    };
    let verdict = match (
        admission.headroom_ok,
        admission.pace_ok,
        admission.deadline_ok,
    ) {
        (true, true, true) => "admitted",
        (false, _, _) => "refused: not enough headroom above the reserve",
        (true, false, _) => "refused: the pace projection empties the window before it resets",
        (true, true, false) => "refused: the job does not fit before the deadline",
    };
    let explanation = format!(
        "{} {} {verdict}. {:.1}% remaining - {:.1}% requested = {:.1}% left, reserve {:.1}%; {pace_note}; reset {reset}",
        latest.provider,
        window.label,
        window.remaining_percent.unwrap_or(0.0),
        admission.requested_percent,
        admission.remaining_after_percent,
        admission.reserve_percent,
    );
    CanStartAnswer {
        provider: latest.provider,
        ok,
        basis: CanStartBasis::PercentBudget,
        explanation,
        window_kind: Some(window.kind.clone()),
        remaining_percent: window.remaining_percent,
        remaining_tokens: None,
        reset_at: window.reset_at,
        eta_empty_secs: chosen.eta,
        burn_percent_per_hour: chosen.burn_hour,
        admission: Some(admission),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AdapterError, Credits, ProviderId, ProviderObservation};

    fn provider_at(provider: ProviderId, used: f64, observed_at: i64) -> ProviderSnapshot {
        ProviderSnapshot::observed(ProviderObservation {
            provider,
            source: Some(crate::types::Source::Oauth),
            windows: vec![UsageWindow::from_percent_at(
                WindowKind::Session,
                "5h",
                used,
                Some(observed_at.saturating_add(100_000)),
                Some(18_000),
                Some(observed_at),
                crate::types::DEFAULT_READING_MAX_AGE_SECS,
            )],
            credits: None,
            plan: Some("plus".into()),
            credential_path: None,
            observed_at: Some(observed_at),
            max_age_secs: crate::types::DEFAULT_READING_MAX_AGE_SECS,
            permission: crate::types::ProviderPermission::Unknown,
        })
    }

    fn ok_provider(used: f64) -> ProviderSnapshot {
        provider_at(ProviderId::Codex, used, crate::timeutil::now_unix())
    }

    #[test]
    fn burn_rate_simple() {
        let samples = [(100, 10.0), (200, 20.0)];
        let rate = burn_percent_per_sec(&samples).unwrap();
        assert!((rate - 0.1).abs() < 1e-9);
    }

    #[test]
    fn burn_rate_ignores_pre_reset() {
        let samples = [(0, 90.0), (50, 95.0), (100, 5.0), (200, 15.0)];
        let rate = burn_percent_per_sec(&samples).unwrap();
        assert!((rate - 0.1).abs() < 1e-9);
    }

    #[test]
    fn eta_empty() {
        assert_eq!(eta_empty_secs(50.0, 0.5), Some(100.0));
        assert_eq!(eta_empty_secs(0.0, 0.5), Some(0.0));
        assert_eq!(eta_empty_secs(10.0, 0.0), None);
    }

    #[test]
    fn can_start_exhausted() {
        let p = ok_provider(100.0);
        let a = can_start(&p, &[], 1, None, 1_000);
        assert!(!a.ok);
        assert_eq!(a.basis, CanStartBasis::PercentOnly);
        assert!(a.explanation.contains("exhausted"));
    }

    #[test]
    fn can_start_percent_refuses_token_mapping() {
        let p = ok_provider(20.0);
        let a = can_start(&p, &[], 10_000, None, 1_000);
        assert!(!a.ok);
        assert_eq!(a.basis, CanStartBasis::PercentOnly);
        assert!(a.explanation.contains("Cannot map"));
    }

    #[test]
    fn can_start_zero_tokens_ok_when_remaining() {
        let p = ok_provider(20.0);
        let a = can_start(&p, &[], 0, None, 1_000);
        assert!(a.ok);
    }

    #[test]
    fn can_start_token_budget() {
        let mut p = ok_provider(20.0);
        p.windows[0].remaining = Some(80_000.0);
        p.windows[0].limit = Some(100_000.0);
        p.windows[0].unit = Some("tokens".into());
        p.windows[0].used_percent = None;
        p.windows[0].remaining_percent = None;
        let a = can_start(&p, &[], 50_000, None, 1_000);
        assert!(a.ok);
        assert_eq!(a.basis, CanStartBasis::TokenBudget);
        let b = can_start(&p, &[], 90_000, None, 1_000);
        assert!(!b.ok);
    }

    #[test]
    fn can_start_unavailable() {
        let p = ProviderSnapshot::unavailable(
            ProviderId::Claude,
            AdapterError::new("no_creds", "missing"),
        );
        let a = can_start(&p, &[], 1, None, 1);
        assert!(!a.ok);
        assert_eq!(a.basis, CanStartBasis::Unavailable);
    }

    #[test]
    fn provider_refusal_is_not_cleared_by_low_usage_percent() {
        let mut p = ok_provider(1.0);
        p.permission = crate::types::ProviderPermission::LimitReached;
        let answer = can_start(&p, &[], 0, None, crate::timeutil::now_unix());
        assert!(!answer.ok);
        assert_eq!(answer.basis, CanStartBasis::Unavailable);
        assert!(answer.explanation.contains("limit_reached"));
    }

    #[test]
    fn unavailable_empty_response_preserves_provider_refusal_for_can_start() {
        let mut p = ProviderSnapshot::unavailable(
            ProviderId::Codex,
            AdapterError::new("empty", "usage response had no windows"),
        );
        p.permission = crate::types::ProviderPermission::LimitReached;

        let answer = can_start(&p, &[], 0, None, crate::timeutil::now_unix());

        assert!(!answer.ok);
        assert_eq!(answer.basis, CanStartBasis::Unavailable);
        assert_eq!(answer.explanation, "provider permission is limit_reached");
    }

    #[test]
    fn unknown_windows_remain_null_and_every_exhausted_window_is_named() {
        let now = crate::timeutil::now_unix();
        let windows = vec![
            UsageWindow::from_percent_at(
                WindowKind::Session,
                "5h",
                100.0,
                None,
                Some(18_000),
                Some(now),
                300,
            ),
            UsageWindow::from_percent_at(
                WindowKind::Weekly,
                "weekly",
                100.0,
                None,
                Some(604_800),
                Some(now),
                300,
            ),
            UsageWindow::unreadable(WindowKind::Monthly, "monthly", None, None, Some(now), 300),
        ];
        let snapshot = ProviderSnapshot::observed(ProviderObservation {
            provider: ProviderId::Codex,
            source: Some(crate::types::Source::Oauth),
            windows,
            credits: None,
            plan: None,
            credential_path: None,
            observed_at: Some(now),
            max_age_secs: 300,
            permission: crate::types::ProviderPermission::Unknown,
        });
        assert_eq!(snapshot.exhausted_windows, ["5h", "weekly"]);
        assert_eq!(
            snapshot.windows[2].state,
            crate::types::WindowState::Unknown
        );
        assert!(snapshot.windows[2].reading.is_none());
        let json = serde_json::to_value(&snapshot).unwrap();
        assert!(json["windows"][2]["reading"].is_null());
        assert!(json["windows"][0]["observed_at"].is_number());
        assert_eq!(json["windows"][0]["max_age_secs"], 300);
        assert_eq!(
            json["exhausted_windows"],
            serde_json::json!(["5h", "weekly"])
        );
    }

    #[test]
    fn pace_needs_samples() {
        let p = ok_provider(25.0);
        let snap = Snapshot::new(1_000, vec![p.clone()]);
        let report = pace_for(&[snap], &p);
        assert_eq!(report.samples, 1);
        assert!(report.burn_percent_per_hour.is_none());
    }

    #[test]
    fn pace_computes_burn() {
        let now = crate::timeutil::now_unix();
        let p1 = provider_at(ProviderId::Codex, 10.0, now - 3_600);
        let p2 = provider_at(ProviderId::Codex, 20.0, now);
        let h = vec![
            Snapshot::new(now - 3_600, vec![p1]),
            Snapshot::new(now, vec![p2.clone()]),
        ];
        let report = pace_for(&h, &p2);
        assert_eq!(report.samples, 2);
        let burn = report.burn_percent_per_hour.unwrap();
        assert!((burn - 10.0).abs() < 1e-6);
    }

    #[test]
    fn other_provider_refresh_does_not_duplicate_pace_sample() {
        let now = crate::timeutil::now_unix();
        let codex_first = provider_at(ProviderId::Codex, 10.0, now - 100);
        let claude_first = provider_at(ProviderId::Claude, 20.0, now - 100);
        let codex_second = provider_at(ProviderId::Codex, 15.0, now - 50);
        let claude_second = provider_at(ProviderId::Claude, 30.0, now);
        let history = vec![
            Snapshot::new(now - 100, vec![codex_first, claude_first.clone()]),
            Snapshot::new(now - 50, vec![codex_second.clone(), claude_first]),
            // Claude refreshes and causes a new daemon Snapshot. Codex's
            // unchanged provider evidence must not become another sample.
            Snapshot::new(now, vec![codex_second.clone(), claude_second]),
        ];

        let report = pace_for(&history, &codex_second);

        assert_eq!(report.samples, 2);
        assert!((report.burn_percent_per_hour.unwrap() - 360.0).abs() < 1e-6);
    }

    #[test]
    fn unused_credits_type_keeps_schema() {
        // Sanity: Credits remains constructible for adapters.
        let _ = Credits {
            balance: Some(0.0),
            unlimited: Some(false),
            has_credits: Some(false),
            unit: Some("credits".into()),
        };
    }

    #[test]
    fn can_start_percent_only_stays_false_with_plenty_left() {
        let p = ok_provider(1.0); // 99% remaining, still no token budget
        let a = can_start(&p, &[], 1, None, 1_000);
        assert!(!a.ok);
        assert_eq!(a.basis, CanStartBasis::PercentOnly);
        assert!(a.remaining_tokens.is_none());
        assert!(a.explanation.contains("Cannot map"));
        assert!(!a.explanation.contains("fits before"));
    }

    #[test]
    fn can_start_percent_only_notes_burn_before_deadline() {
        let now = crate::timeutil::now_unix();
        let p1 = provider_at(ProviderId::Codex, 10.0, now - 100);
        let p2 = provider_at(ProviderId::Codex, 90.0, now);
        let history = vec![
            Snapshot::new(now - 100, vec![p1]),
            Snapshot::new(now, vec![p2.clone()]),
        ];
        let a = can_start(&p2, &history, 50_000, Some(now + 8_900), now);
        assert!(!a.ok);
        assert_eq!(a.basis, CanStartBasis::PercentOnly);
        assert!(a.explanation.contains("Current burn would empty"));
    }

    #[test]
    fn can_start_no_windows_is_unavailable() {
        let mut p = ok_provider(20.0);
        p.windows.clear();
        let a = can_start(&p, &[], 1, None, 1);
        assert!(!a.ok);
        assert_eq!(a.basis, CanStartBasis::Unavailable);
        assert!(a.explanation.contains("no readable usage windows"));
    }

    #[test]
    fn binding_window_prefers_session_over_weekly() {
        let mut p = ok_provider(20.0);
        p.windows.push(UsageWindow::from_percent(
            WindowKind::Weekly,
            "weekly",
            5.0,
            Some(9_000),
            Some(604_800),
        ));
        let report = pace_for(&[Snapshot::new(1_000, vec![p.clone()])], &p);
        assert_eq!(report.window_kind, Some(WindowKind::Session));
    }

    #[test]
    fn greptile_2_unreadable_short_window_blocks_zero_token_start() {
        let now = crate::timeutil::now_unix();
        let mut p = provider_at(ProviderId::Codex, 10.0, now);
        p.windows[0] = UsageWindow::unreadable(
            WindowKind::Session,
            "5h",
            Some(now + 3_600),
            Some(18_000),
            Some(now),
            crate::types::DEFAULT_READING_MAX_AGE_SECS,
        );
        p.windows.push(UsageWindow::from_percent_at(
            WindowKind::Weekly,
            "weekly",
            10.0,
            None,
            Some(604_800),
            Some(now),
            crate::types::DEFAULT_READING_MAX_AGE_SECS,
        ));
        p.refresh_freshness(now);
        assert_eq!(p.status, Availability::Ok);

        let a = can_start(&p, &[], 0, None, now);

        assert!(!a.ok);
        assert_eq!(a.basis, CanStartBasis::UnknownWindow);
        assert_eq!(a.window_kind, Some(WindowKind::Session));
        assert!(a.explanation.contains("5h"), "{}", a.explanation);
    }

    #[test]
    fn greptile_2_tightest_readable_window_binds_not_kind_order() {
        let now = crate::timeutil::now_unix();
        let mut p = provider_at(ProviderId::Codex, 20.0, now);
        p.windows.push(UsageWindow::from_percent_at(
            WindowKind::Weekly,
            "weekly",
            100.0,
            None,
            Some(604_800),
            Some(now),
            crate::types::DEFAULT_READING_MAX_AGE_SECS,
        ));
        p.refresh_freshness(now);

        let a = can_start(&p, &[], 0, None, now);

        assert!(!a.ok);
        assert_eq!(a.window_kind, Some(WindowKind::Weekly));
        assert!(a.explanation.contains("exhausted"), "{}", a.explanation);
        let pace = pace_for(&[Snapshot::new(now, vec![p.clone()])], &p);
        assert_eq!(pace.window_kind, Some(WindowKind::Weekly));
    }

    #[test]
    fn can_start_credits_unit_is_token_budget() {
        let mut p = ok_provider(20.0);
        p.windows[0].remaining = Some(12.0);
        p.windows[0].limit = Some(20.0);
        p.windows[0].unit = Some("credits".into());
        p.windows[0].used_percent = None;
        p.windows[0].remaining_percent = None;
        let a = can_start(&p, &[], 10, None, 1_000);
        assert!(a.ok);
        assert_eq!(a.basis, CanStartBasis::TokenBudget);
        let b = can_start(&p, &[], 13, None, 1_000);
        assert!(!b.ok);
    }

    fn history_burning(used_then: f64, used_now: f64, now: i64) -> Vec<Snapshot> {
        vec![
            Snapshot::new(
                now - 3_600,
                vec![provider_at(ProviderId::Codex, used_then, now - 3_600)],
            ),
            Snapshot::new(now, vec![provider_at(ProviderId::Codex, used_now, now)]),
        ]
    }

    #[test]
    fn percent_admission_admits_with_headroom_and_names_its_tests() {
        let now = crate::timeutil::now_unix();
        let p = provider_at(ProviderId::Codex, 50.0, now);
        let a = can_start_percent(&p, &[], 10.0, DEFAULT_RESERVE_PERCENT, None, now);
        assert!(a.ok, "{}", a.explanation);
        assert_eq!(a.basis, CanStartBasis::PercentBudget);
        let admission = a.admission.unwrap();
        assert_eq!(admission.remaining_after_percent, 40.0);
        assert!(admission.headroom_ok && admission.pace_ok);
        assert!(!admission.pace_checked, "one sample cannot veto");
    }

    #[test]
    fn percent_admission_reserve_boundary_is_inclusive() {
        let now = crate::timeutil::now_unix();
        let p = provider_at(ProviderId::Codex, 90.0, now);
        assert!(can_start_percent(&p, &[], 8.0, 2.0, None, now).ok);
        let refused = can_start_percent(&p, &[], 8.1, 2.0, None, now);
        assert!(!refused.ok);
        assert!(!refused.admission.unwrap().headroom_ok);
    }

    #[test]
    fn percent_admission_refuses_the_live_codex_case() {
        let now = crate::timeutil::now_unix();
        let p = provider_at(ProviderId::Codex, 99.0, now);
        let a = can_start_percent(&p, &[], 1.0, DEFAULT_RESERVE_PERCENT, None, now);
        assert!(!a.ok);
        assert_eq!(a.basis, CanStartBasis::PercentBudget);
        assert!(a.explanation.contains("not enough headroom"));
    }

    #[test]
    fn percent_admission_pace_can_veto_when_headroom_passes() {
        let now = crate::timeutil::now_unix();
        let history = history_burning(40.0, 50.0, now);
        let p = provider_at(ProviderId::Codex, 50.0, now);
        let a = can_start_percent(&p, &history, 10.0, 2.0, None, now);
        assert!(!a.ok, "{}", a.explanation);
        let admission = a.admission.unwrap();
        assert!(admission.headroom_ok);
        assert!(admission.pace_checked && !admission.pace_ok);
        assert_eq!(a.eta_empty_secs.map(|eta| eta.round()), Some(14_400.0));
        assert!(a.explanation.contains("pace projection"));
    }

    #[test]
    fn percent_admission_an_earlier_deadline_does_not_waive_the_pace_veto() {
        let now = crate::timeutil::now_unix();
        let history = history_burning(40.0, 50.0, now);
        let p = provider_at(ProviderId::Codex, 50.0, now);
        let without = can_start_percent(&p, &history, 10.0, 2.0, None, now);
        let with = can_start_percent(&p, &history, 10.0, 2.0, Some(now + 600), now);
        assert!(!without.ok && !with.ok, "{}", with.explanation);
        let admission = with.admission.unwrap();
        assert!(admission.headroom_ok && admission.deadline_ok);
        assert!(admission.pace_checked && !admission.pace_ok);
        assert!(with.explanation.contains("pace projection"));
    }

    #[test]
    fn percent_admission_a_deadline_only_adds_a_refusal() {
        let now = crate::timeutil::now_unix();
        let history = history_burning(40.0, 50.0, now);
        let mut p = provider_at(ProviderId::Codex, 50.0, now);
        p.windows[0].reset_at = Some(now + 10_000);
        let a = can_start_percent(&p, &history, 1.0, 2.0, None, now);
        assert!(a.ok, "{}", a.explanation);
        let past = can_start_percent(&p, &history, 1.0, 2.0, Some(now - 1), now);
        assert!(!past.ok);
        assert!(!past.admission.unwrap().deadline_ok);
        assert!(past.explanation.contains("before the deadline"));
    }

    #[test]
    fn percent_admission_a_deadline_the_burn_cannot_reach_refuses_without_a_reset() {
        let now = crate::timeutil::now_unix();
        let history = history_burning(40.0, 50.0, now);
        let mut p = provider_at(ProviderId::Codex, 50.0, now);
        p.windows[0].reset_at = None;
        let open = can_start_percent(&p, &history, 10.0, 2.0, None, now);
        assert!(open.ok && !open.admission.unwrap().pace_checked);
        let late = can_start_percent(&p, &history, 10.0, 2.0, Some(now + 20_000), now);
        assert!(!late.ok, "{}", late.explanation);
        assert!(!late.admission.unwrap().deadline_ok);
    }

    #[test]
    fn percent_admission_reports_the_tightest_window() {
        let now = crate::timeutil::now_unix();
        let mut p = provider_at(ProviderId::Codex, 90.0, now);
        p.windows.push(UsageWindow::from_percent_at(
            WindowKind::Weekly,
            "weekly",
            10.0,
            None,
            None,
            Some(now),
            300,
        ));
        let a = can_start_percent(&p, &[], 15.0, 2.0, None, now);
        assert!(!a.ok);
        assert_eq!(a.window_kind, Some(WindowKind::Session));
        let b = can_start_percent(&p, &[], 5.0, 2.0, None, now);
        assert!(b.ok);
        assert_eq!(b.window_kind, Some(WindowKind::Session));
    }

    #[test]
    fn percent_admission_never_treats_an_uncapped_spend_counter_as_headroom() {
        let now = crate::timeutil::now_unix();
        let mut p = provider_at(ProviderId::Cursor, 0.0, now);
        p.windows = vec![UsageWindow::from_spend_at(
            WindowKind::Spend,
            "on-demand",
            12.5,
            None,
            None,
            None,
            Some(now),
            300,
        )];
        let a = can_start_percent(&p, &[], 1.0, 2.0, None, now);
        assert!(!a.ok);
        assert_eq!(a.basis, CanStartBasis::Unavailable);
    }

    #[test]
    fn percent_admission_uses_dollar_windows_through_their_percent() {
        let now = crate::timeutil::now_unix();
        let mut p = provider_at(ProviderId::Cursor, 0.0, now);
        p.windows = vec![UsageWindow::from_spend_at(
            WindowKind::Spend,
            "on-demand",
            12.0,
            Some(60.0),
            None,
            None,
            Some(now),
            300,
        )];
        assert_eq!(p.windows[0].used_percent, Some(20.0));
        assert_eq!(p.windows[0].remaining, Some(48.0));
        assert!(can_start_percent(&p, &[], 70.0, 2.0, None, now).ok);
        assert!(!can_start_percent(&p, &[], 79.0, 2.0, None, now).ok);
    }

    fn cursor_at(included_usd: f64, on_demand_usd: f64, reset: i64, at: i64) -> ProviderSnapshot {
        let mut p = provider_at(ProviderId::Cursor, 0.0, at);
        p.windows = [("included", included_usd), ("on-demand", on_demand_usd)]
            .into_iter()
            .map(|(label, used)| {
                UsageWindow::from_spend_at(
                    WindowKind::Spend,
                    label,
                    used,
                    Some(100.0),
                    Some(reset),
                    None,
                    Some(at),
                    300,
                )
            })
            .collect();
        p
    }

    #[test]
    fn pace_history_is_keyed_by_window_identity_not_kind() {
        let now = crate::timeutil::now_unix();
        let reset = now + 36_000;
        let then = cursor_at(50.0, 10.0, reset, now - 3_600);
        let latest = cursor_at(50.0, 50.0, reset, now);
        let history = vec![
            Snapshot::new(now - 3_600, vec![then]),
            Snapshot::new(now, vec![latest.clone()]),
        ];
        let on_demand = &latest.windows[1];
        let samples = samples_for(&history, &latest, on_demand);
        assert_eq!(
            samples.iter().map(|s| s.1).collect::<Vec<_>>(),
            [10.0, 50.0]
        );

        let a = can_start_percent(&latest, &history, 5.0, 2.0, None, now);
        assert!(!a.ok, "{}", a.explanation);
        assert!(a.explanation.contains("on-demand"), "{}", a.explanation);
        assert!(!a.admission.unwrap().pace_ok);
    }

    #[test]
    fn percent_admission_refuses_when_a_reported_window_is_unreadable() {
        let now = crate::timeutil::now_unix();
        let mut p = provider_at(ProviderId::Claude, 10.0, now);
        p.windows.push(UsageWindow::unreadable(
            WindowKind::Weekly,
            "weekly",
            None,
            Some(604_800),
            Some(now),
            300,
        ));
        let a = can_start_percent(&p, &[], 1.0, 2.0, None, now);
        assert!(!a.ok, "{}", a.explanation);
        assert_eq!(a.basis, CanStartBasis::UnknownWindow);
        assert_eq!(a.window_kind, Some(WindowKind::Weekly));
        assert!(a.explanation.contains("weekly has no readable measurement"));
    }

    #[test]
    fn percent_admission_inspects_windows_before_all_unreadable_status() {
        let now = crate::timeutil::now_unix();
        let p = ProviderSnapshot::observed(crate::types::ProviderObservation {
            provider: ProviderId::Claude,
            source: Some(crate::types::Source::Oauth),
            windows: vec![UsageWindow::unreadable(
                WindowKind::Weekly,
                "sonnet weekly",
                None,
                Some(604_800),
                Some(now),
                300,
            )],
            credits: None,
            plan: None,
            credential_path: None,
            observed_at: Some(now),
            max_age_secs: 300,
            permission: crate::types::ProviderPermission::Unknown,
        });
        assert_eq!(p.status, Availability::Unavailable);

        let answer = can_start_percent(&p, &[], 1.0, 2.0, None, now);
        assert!(!answer.ok, "{}", answer.explanation);
        assert_eq!(answer.basis, CanStartBasis::UnknownWindow);
        assert_eq!(answer.window_kind, Some(WindowKind::Weekly));
        assert!(answer.explanation.contains("sonnet weekly"));
    }

    #[test]
    fn percent_admission_refuses_stale_and_unavailable_evidence() {
        let now = crate::timeutil::now_unix();
        let mut p = provider_at(ProviderId::Codex, 10.0, now - 10_000);
        p.refresh_freshness(now);
        assert_eq!(p.status, Availability::Stale);
        let a = can_start_percent(&p, &[], 1.0, 2.0, None, now);
        assert!(!a.ok);
        assert_eq!(a.basis, CanStartBasis::Unavailable);
    }

    mod deadline_property {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            /// A deadline is only ever an extra refusal: whenever the same
            /// question without one is refused, adding any deadline (past,
            /// near, beyond the reset) stays refused.
            #[test]
            fn adding_a_deadline_never_turns_a_refusal_into_an_admit(
                used_then in 0.0f64..100.0,
                used_now in 0.0f64..100.0,
                percent in 0.1f64..100.0,
                reserve in 0.0f64..50.0,
                reset_in in proptest::option::of(1i64..200_000),
                deadline_in in -10_000i64..400_000,
            ) {
                let now = crate::timeutil::now_unix();
                let history = history_burning(used_then, used_now, now);
                let mut p = provider_at(ProviderId::Codex, used_now, now);
                p.windows[0].reset_at = reset_in.map(|r| now + r);
                let without = can_start_percent(&p, &history, percent, reserve, None, now);
                let with = can_start_percent(&p, &history, percent, reserve, Some(now + deadline_in), now);
                if !without.ok {
                    prop_assert!(!with.ok, "{} / {}", without.explanation, with.explanation);
                }
                if let (Some(a), Some(b)) = (without.admission, with.admission) {
                    prop_assert_eq!((a.headroom_ok, a.pace_ok), (b.headroom_ok, b.pace_ok));
                }
            }
        }
    }
    #[test]
    fn review_r2_pace_history_spanning_two_account_digests_uses_only_the_active_account() {
        let now = crate::timeutil::now_unix();
        let reading = |digest: Option<&str>, used, observed_at| {
            let mut p = provider_at(ProviderId::Codex, used, observed_at);
            p.account_digest = digest.map(str::to_string);
            p
        };
        let (a, b) = ("a".repeat(64), "b".repeat(64));
        let b_first = reading(Some(&b), 65.0, now - 50);
        let b_latest = reading(Some(&b), 70.0, now);
        let unknown = reading(None, 20.0, now - 150);
        let history: Vec<Snapshot> = [
            reading(Some(&a), 10.0, now - 200),
            unknown.clone(),
            reading(Some(&a), 60.0, now - 100),
            b_first,
            b_latest.clone(),
        ]
        .into_iter()
        .map(|p| Snapshot::new(p.observed_at.unwrap(), vec![p]))
        .collect();

        let pace = pace_for(&history, &b_latest);
        assert_eq!(pace.samples, 2);
        assert!((pace.burn_percent_per_hour.unwrap() - 360.0).abs() < 1e-6);
        let answer = can_start(&b_latest, &history, 1, None, now);
        assert_eq!(answer.burn_percent_per_hour, pace.burn_percent_per_hour);

        assert_eq!(pace_for(&history, &unknown).samples, 1);
    }

    #[test]
    fn round4_a_reading_for_another_account_refuses_admission_and_pace() {
        let now = crate::timeutil::now_unix();
        let mut reading = provider_at(ProviderId::Codex, 10.0, now);
        reading.account_digest = Some("a".repeat(64));
        let history = [Snapshot::new(now, vec![reading.clone()])];
        assert!(can_start(&reading, &history, 0, None, now).ok);

        let elsewhere = reading.for_another_account();
        assert_eq!(elsewhere.account_digest, reading.account_digest);
        for tokens in [0, 1_000] {
            let answer = can_start(&elsewhere, &history, tokens, None, now);
            assert!(!answer.ok);
            assert_eq!(answer.basis, CanStartBasis::AccountChanged);
            assert!(answer.explanation.contains("account changed since reading"));
            assert_eq!(answer.remaining_percent, None);
        }
        let pace = pace_for(&history, &elsewhere);
        assert_eq!(pace.samples, 0);
        assert_eq!(pace.used_percent, None);
        assert!(pace.explanation.contains("account changed since reading"));
        let percent = can_start_percent(&elsewhere, &history, 1.0, 2.0, None, now);
        assert!(!percent.ok);
        assert_eq!(percent.basis, CanStartBasis::AccountChanged);
    }

    #[test]
    fn round6_an_all_unreadable_snapshot_answers_unknown_window_not_unavailable() {
        let now = crate::timeutil::now_unix();
        let unreadable = |kind, label| {
            UsageWindow::unreadable(
                kind,
                label,
                Some(now + 3_600),
                None,
                Some(now),
                crate::types::DEFAULT_READING_MAX_AGE_SECS,
            )
        };
        let p = ProviderSnapshot::observed(ProviderObservation {
            provider: ProviderId::Codex,
            source: Some(crate::types::Source::Oauth),
            windows: vec![
                unreadable(WindowKind::Session, "5h"),
                unreadable(WindowKind::Weekly, "weekly"),
            ],
            credits: None,
            plan: None,
            credential_path: None,
            observed_at: Some(now),
            max_age_secs: crate::types::DEFAULT_READING_MAX_AGE_SECS,
            permission: crate::types::ProviderPermission::Unknown,
        });
        assert_eq!(p.status, Availability::Unavailable);

        for tokens in [0, 1_000] {
            let a = can_start(&p, &[], tokens, None, now);
            assert!(!a.ok);
            assert_eq!(a.basis, CanStartBasis::UnknownWindow);
            assert_eq!(a.window_kind, Some(WindowKind::Session));
            assert!(a.explanation.contains("5h, weekly"), "{}", a.explanation);
        }
    }

    fn unreadable_window(now: i64, observed_at: i64) -> UsageWindow {
        UsageWindow::unreadable(
            WindowKind::Session,
            "5h",
            Some(now + 3_600),
            None,
            Some(observed_at),
            crate::types::DEFAULT_READING_MAX_AGE_SECS,
        )
    }

    fn observed_with(
        window: UsageWindow,
        observed_at: i64,
        permission: crate::types::ProviderPermission,
    ) -> ProviderSnapshot {
        ProviderSnapshot::observed(ProviderObservation {
            provider: ProviderId::Codex,
            source: Some(crate::types::Source::Oauth),
            windows: vec![window],
            credits: None,
            plan: None,
            credential_path: None,
            observed_at: Some(observed_at),
            max_age_secs: crate::types::DEFAULT_READING_MAX_AGE_SECS,
            permission,
        })
    }

    #[test]
    fn greptile_7_a_stale_reading_with_an_unreadable_window_is_unavailable_not_a_veto() {
        let now = crate::timeutil::now_unix();
        let long_ago = now - 10 * crate::types::DEFAULT_READING_MAX_AGE_SECS as i64;
        let mut mixed = provider_at(ProviderId::Codex, 10.0, long_ago);
        mixed.windows[0] = unreadable_window(now, long_ago);
        mixed.refresh_freshness(now);
        assert_eq!(mixed.status, Availability::Stale);

        let mut all_unreadable = observed_with(
            unreadable_window(now, long_ago),
            long_ago,
            crate::types::ProviderPermission::Unknown,
        );
        all_unreadable.refresh_freshness(now);
        assert_eq!(all_unreadable.status, Availability::Unavailable);
        assert_eq!(all_unreadable.freshness, Freshness::Stale);

        for stale in [&mixed, &all_unreadable] {
            for tokens in [0, 1_000] {
                let a = can_start(stale, &[], tokens, None, now);
                assert!(!a.ok);
                assert_eq!(a.basis, CanStartBasis::Unavailable, "{}", a.explanation);
            }
        }
    }

    #[test]
    fn greptile_7_a_current_unreadable_window_still_answers_unknown_window() {
        let now = crate::timeutil::now_unix();
        let mixed = observed_with(
            unreadable_window(now, now),
            now,
            crate::types::ProviderPermission::Allowed,
        );
        let a = can_start(&mixed, &[], 0, None, now);
        assert!(!a.ok);
        assert_eq!(a.basis, CanStartBasis::UnknownWindow);
    }

    #[test]
    fn greptile_7_an_explicit_refusal_is_named_even_beside_an_unreadable_window() {
        let now = crate::timeutil::now_unix();
        let refused = observed_with(
            unreadable_window(now, now),
            now,
            crate::types::ProviderPermission::LimitReached,
        );
        let a = can_start(&refused, &[], 0, None, now);
        assert!(!a.ok);
        assert_eq!(a.basis, CanStartBasis::Unavailable);
        assert!(a.explanation.contains("limit_reached"), "{}", a.explanation);

        let mut low_usage = provider_at(ProviderId::Codex, 1.0, now);
        low_usage.permission = crate::types::ProviderPermission::LimitReached;
        let a = can_start(&low_usage, &[], 0, None, now);
        assert!(!a.ok);
        assert_eq!(a.basis, CanStartBasis::Unavailable);
        assert!(a.explanation.contains("limit_reached"), "{}", a.explanation);
    }
}
