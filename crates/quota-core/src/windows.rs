//! Classify Codex / CodexBar usage windows by published length.
//!
//! Slot names (`primary_window`, CodexBar `windowKind=primary`) are hints
//! only. Live `/wham/usage` has been observed putting a 7-day (604800s)
//! window in `primary_window`; labeling that `session` / `5h` is wrong.

use crate::types::WindowKind;

pub const FIVE_HOUR_SECS: i64 = 18_000;
pub const WEEK_SECS: i64 = 604_800;
pub const FIVE_HOUR_MINUTES: i64 = 300;
pub const WEEK_MINUTES: i64 = 10_080;

fn near(actual: i64, target: i64) -> bool {
    if target <= 0 {
        return false;
    }
    (actual - target).abs() <= target / 20
}

fn seconds_from(limit_window_seconds: Option<i64>, window_minutes: Option<i64>) -> Option<i64> {
    limit_window_seconds.filter(|s| *s > 0).or_else(|| {
        window_minutes
            .filter(|m| *m > 0)
            .map(|m| m.saturating_mul(60))
    })
}

/// Duration wins over slot name. `slot` is `"primary"` / `"secondary"` /
/// `"tertiary"` (or CodexBar `windowKind`).
pub fn classify_codex_window(
    slot: Option<&str>,
    limit_window_seconds: Option<i64>,
    window_minutes: Option<i64>,
) -> (WindowKind, String) {
    if let Some(secs) = seconds_from(limit_window_seconds, window_minutes) {
        if near(secs, WEEK_SECS) || (6 * 86_400..=8 * 86_400).contains(&secs) {
            return (WindowKind::Weekly, "weekly".into());
        }
        if near(secs, FIVE_HOUR_SECS) || (4 * 3600..=6 * 3600).contains(&secs) {
            return (WindowKind::Session, "5h".into());
        }
        if (25 * 86_400..=35 * 86_400).contains(&secs) {
            return (WindowKind::Monthly, "monthly".into());
        }
    }
    match slot {
        Some("primary") | Some("session") | Some("five_hour") => (WindowKind::Session, "5h".into()),
        Some("secondary") | Some("weekly") => (WindowKind::Weekly, "weekly".into()),
        Some("tertiary") | Some("extra") => (WindowKind::Extra, "extra".into()),
        Some("monthly") => (WindowKind::Monthly, "monthly".into()),
        _ => (WindowKind::Weekly, "weekly".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weekly_in_primary_slot_is_not_session() {
        let (kind, label) = classify_codex_window(Some("primary"), Some(WEEK_SECS), None);
        assert_eq!(kind, WindowKind::Weekly);
        assert_eq!(label, "weekly");
        let (kind, label) = classify_codex_window(Some("primary"), None, Some(WEEK_MINUTES));
        assert_eq!(kind, WindowKind::Weekly);
        assert_eq!(label, "weekly");
    }

    #[test]
    fn five_hour_primary_stays_session() {
        let (kind, label) = classify_codex_window(Some("primary"), Some(FIVE_HOUR_SECS), None);
        assert_eq!(kind, WindowKind::Session);
        assert_eq!(label, "5h");
    }

    #[test]
    fn secondary_10080_minutes_is_weekly() {
        let (kind, label) = classify_codex_window(Some("secondary"), None, Some(WEEK_MINUTES));
        assert_eq!(kind, WindowKind::Weekly);
        assert_eq!(label, "weekly");
    }

    #[test]
    fn slot_only_when_duration_missing() {
        let (kind, label) = classify_codex_window(Some("primary"), None, None);
        assert_eq!(kind, WindowKind::Session);
        assert_eq!(label, "5h");
    }
}
