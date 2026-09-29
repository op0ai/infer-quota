use quota_core::protocol::CanStartResult;
use quota_core::types::{Availability, PaceReport, ProviderPermission, ProviderSnapshot, Snapshot};

fn permission_label(permission: ProviderPermission) -> &'static str {
    match permission {
        ProviderPermission::Allowed => "allowed",
        ProviderPermission::LimitReached => "limit_reached",
        ProviderPermission::Unknown => "unknown",
    }
}

fn unavailable_status_line(p: &ProviderSnapshot, src: &str) -> String {
    format!(
        "{:<8} unavailable  src={src} permission={}",
        p.provider.as_str(),
        permission_label(p.permission)
    )
}

/// Remaining wait, counted down to the daemon's deadline rather than echoing
/// the delay the provider originally sent.
fn retry_after_line(p: &ProviderSnapshot) -> Option<String> {
    let remaining = match p.retry_after_until {
        Some(until) => {
            u64::try_from(until.saturating_sub(quota_core::timeutil::now_unix())).ok()?
        }
        None => p.retry_after_secs?,
    };
    (remaining > 0).then(|| format!("retry-after {remaining}s"))
}

pub fn print_status(snap: &Snapshot) {
    println!("fetched {}", snap.fetched_at_rfc3339);
    if snap.providers.is_empty() {
        println!("(no providers in snapshot — is this a fresh daemon?)");
        return;
    }
    for p in &snap.providers {
        let src = p
            .source
            .map(|s| format!("{s:?}").to_lowercase())
            .unwrap_or_else(|| "-".into());
        match p.status {
            Availability::Ok | Availability::Stale => {
                let plan = p.plan.as_deref().unwrap_or("");
                let status = match p.status {
                    Availability::Ok => "ok",
                    Availability::Stale => "stale",
                    Availability::Unavailable => unreachable!(),
                };
                let permission = permission_label(p.permission);
                println!(
                    "{:<8} {:<10} src={src} permission={permission} {plan}",
                    p.provider.as_str(),
                    status
                );
                println!(
                    "         evidence observed_at={:?} max_age={}s freshness={:?}",
                    p.observed_at, p.max_age_secs, p.freshness
                );
                if p.windows.is_empty() {
                    println!("         (no windows)");
                }
                for w in &p.windows {
                    let used = w
                        .used_percent
                        .map(|n| format!("{n:.1}% used"))
                        .unwrap_or_else(|| "used n/a".into());
                    let rem = w
                        .remaining_percent
                        .map(|n| format!("{n:.1}% left"))
                        .or_else(|| w.remaining.map(|n| format!("{n:.0} left")))
                        .unwrap_or_else(|| "left n/a".into());
                    let reset = w.reset_at_rfc3339.clone().unwrap_or_else(|| "-".into());
                    println!(
                        "         {:<14} {:<9} {used:>14}  {rem:>14}  reset {reset}",
                        w.label,
                        format!("{:?}", w.state).to_lowercase(),
                    );
                }
                if !p.exhausted_windows.is_empty() {
                    println!("         exhausted: {}", p.exhausted_windows.join(", "));
                }
                if let Some(c) = &p.credits {
                    println!(
                        "         credits        balance={:?} has={:?} unlimited={:?}",
                        c.balance, c.has_credits, c.unlimited
                    );
                }
            }
            Availability::Unavailable => {
                let reason = p
                    .error
                    .as_ref()
                    .map(|e| format!("{}: {}", e.code, e.message))
                    .unwrap_or_else(|| "unavailable".into());
                println!("{}", unavailable_status_line(p, &src));
                println!("         {reason}");
                if let Some(line) = retry_after_line(p) {
                    println!("         {line}");
                }
                if let Some(path) = &p.credential_path {
                    println!("         consulted {path}");
                }
            }
        }
    }
}

pub fn print_pace(reports: &[PaceReport]) {
    if reports.is_empty() {
        println!("no pace reports");
        return;
    }
    for r in reports {
        println!("{}", r.explanation);
        if let Some(b) = r.burn_percent_per_hour {
            println!("  burn {:.3} %/h  samples {}", b, r.samples);
        } else {
            println!("  burn n/a  samples {}", r.samples);
        }
        if let Some(eta) = r.eta_empty_secs {
            println!("  eta-empty {:.0}s", eta);
        }
    }
}

pub fn print_can_start(result: &CanStartResult) {
    println!("overall {}", if result.ok { "yes" } else { "no" });
    for a in &result.answers {
        println!(
            "{:<8} {}  [{:?}] {}",
            a.provider.as_str(),
            if a.ok { "yes" } else { "no" },
            a.basis,
            a.explanation
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quota_core::types::{AdapterError, ProviderId};

    #[test]
    fn unavailable_status_includes_limit_reached_permission() {
        let mut provider = ProviderSnapshot::unavailable(
            ProviderId::Codex,
            AdapterError::new("empty", "usage response had no windows"),
        );
        provider.permission = ProviderPermission::LimitReached;
        provider.source = Some(quota_core::types::Source::Oauth);

        let line = unavailable_status_line(&provider, "oauth");

        assert!(line.contains("unavailable"));
        assert!(line.contains("permission=limit_reached"));
    }

    #[test]
    fn retry_after_counts_down_to_the_deadline() {
        let now = quota_core::timeutil::now_unix();
        let mut provider = ProviderSnapshot::unavailable(
            ProviderId::Claude,
            AdapterError::new("rate_limited", "HTTP 429"),
        );
        provider.retry_after_secs = Some(120);
        provider.retry_after_until = Some(now + 30);
        let line = retry_after_line(&provider).unwrap();
        assert!(["retry-after 30s", "retry-after 29s"].contains(&line.as_str()));

        provider.retry_after_until = Some(now - 5);
        assert_eq!(retry_after_line(&provider), None);
    }
}
