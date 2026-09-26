use quota_core::protocol::CanStartResult;
use quota_core::types::{Availability, PaceReport, Snapshot};

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
            Availability::Ok => {
                let plan = p.plan.as_deref().unwrap_or("");
                println!("{:<8} ok       src={src} {plan}", p.provider.as_str());
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
                        "         {:<14} {used:>14}  {rem:>14}  reset {reset}",
                        w.label
                    );
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
                println!("{:<8} unavailable  src={src}", p.provider.as_str());
                println!("         {reason}");
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
