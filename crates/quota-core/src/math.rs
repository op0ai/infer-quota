//! Burn-rate, ETA, and `can_start` helpers.
//!
//! Inputs are whatever the adapters actually observed. If a source only
//! publishes `used_percent`, we refuse to invent a token budget.

use crate::types::{
    Availability, CanStartAnswer, CanStartBasis, PaceReport, ProviderId, ProviderSnapshot,
    Snapshot, UsageWindow, WindowKind,
};

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

fn binding_window(snap: &ProviderSnapshot) -> Option<&UsageWindow> {
    // Tightest remaining percent among known windows; prefer session/5h over weekly.
    let preferred = [
        WindowKind::Session,
        WindowKind::FiveHour,
        WindowKind::Weekly,
        WindowKind::Monthly,
        WindowKind::Extra,
    ];
    for kind in preferred {
        if let Some(w) = snap.windows.iter().find(|w| w.kind == kind) {
            return Some(w);
        }
    }
    snap.windows.first()
}

fn samples_for(history: &[Snapshot], provider: ProviderId, kind: &WindowKind) -> Vec<(i64, f64)> {
    let mut out = Vec::with_capacity(history.len());
    for snap in history {
        if let Some(p) = snap.by_id(provider) {
            if let Some(w) = p.window(kind) {
                if let Some(used) = w.used_percent {
                    out.push((snap.fetched_at, used));
                }
            }
        }
    }
    out
}

pub fn pace_for(history: &[Snapshot], latest: &ProviderSnapshot) -> PaceReport {
    if latest.status != Availability::Ok {
        let msg = latest
            .error
            .as_ref()
            .map(|e| e.message.clone())
            .unwrap_or_else(|| "provider unavailable".to_string());
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
            explanation: "no usage windows in last snapshot".to_string(),
        };
    };
    let samples = samples_for(history, latest.provider, &window.kind);
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
pub fn can_start(
    latest: &ProviderSnapshot,
    history: &[Snapshot],
    tokens: u64,
    deadline: Option<i64>,
    now: i64,
) -> CanStartAnswer {
    if latest.status != Availability::Ok {
        let msg = latest
            .error
            .as_ref()
            .map(|e| e.message.clone())
            .unwrap_or_else(|| "provider unavailable".to_string());
        return CanStartAnswer {
            provider: latest.provider,
            ok: false,
            basis: CanStartBasis::Unavailable,
            explanation: msg,
            window_kind: None,
            remaining_percent: None,
            remaining_tokens: None,
            reset_at: None,
            eta_empty_secs: None,
            burn_percent_per_hour: None,
        };
    }
    let Some(window) = binding_window(latest) else {
        return CanStartAnswer {
            provider: latest.provider,
            ok: false,
            basis: CanStartBasis::Unavailable,
            explanation: "no usage windows published".to_string(),
            window_kind: None,
            remaining_percent: None,
            remaining_tokens: None,
            reset_at: None,
            eta_empty_secs: None,
            burn_percent_per_hour: None,
        };
    };

    let samples = samples_for(history, latest.provider, &window.kind);
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AdapterError, Credits};

    fn pct_window(used: f64, reset: Option<i64>) -> UsageWindow {
        UsageWindow::from_percent(WindowKind::Session, "5h", used, reset, Some(18_000))
    }

    fn ok_provider(used: f64) -> ProviderSnapshot {
        ProviderSnapshot {
            provider: ProviderId::Codex,
            status: Availability::Ok,
            source: Some(crate::types::Source::Oauth),
            windows: vec![pct_window(used, Some(2_000))],
            credits: None,
            plan: Some("plus".into()),
            error: None,
            credential_path: None,
        }
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
    fn pace_needs_samples() {
        let p = ok_provider(25.0);
        let snap = Snapshot::new(1_000, vec![p.clone()]);
        let report = pace_for(&[snap], &p);
        assert_eq!(report.samples, 1);
        assert!(report.burn_percent_per_hour.is_none());
    }

    #[test]
    fn pace_computes_burn() {
        let p1 = ok_provider(10.0);
        let p2 = ok_provider(20.0);
        let h = vec![
            Snapshot::new(1_000, vec![p1]),
            Snapshot::new(1_000 + 3600, vec![p2.clone()]),
        ];
        let report = pace_for(&h, &p2);
        assert_eq!(report.samples, 2);
        let burn = report.burn_percent_per_hour.unwrap();
        assert!((burn - 10.0).abs() < 1e-6);
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
}
