//! CodexBar *local snapshot* parser — not a collector.
//!
//! Maps the redacted CodexBar on-disk shape (secondary window with
//! `usedPercent` / `windowMinutes` / `resetsAt`) onto `quota-core` types.
//! HTTP probing stays in [`crate::codex`]. This module is for fixtures,
//! tests, and any later importer that already has a CodexBar JSON file.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use quota_core::timeutil::{parse_apple_reset_at, parse_reset_at_str};
use quota_core::types::{
    AdapterError, Availability, Credits, ProviderId, ProviderObservation, ProviderPermission,
    ProviderSnapshot, Snapshot, Source, UsageWindow, WindowKind, DEFAULT_READING_MAX_AGE_SECS,
};
use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CodexBarError {
    #[error("io: {0}")]
    Io(String),
    #[error("parse: {0}")]
    Parse(String),
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryRow {
    pub account_key: String,
    pub provider: String,
    pub resets_at: String,
    pub sampled_at: String,
    pub source: String,
    pub used_percent: f64,
    #[serde(default)]
    #[allow(dead_code)]
    pub v: Option<u32>,
    pub window_kind: String,
    pub window_minutes: i64,
}

impl HistoryRow {
    pub fn is_live(&self) -> bool {
        self.source == "live"
    }

    pub fn is_backfill(&self) -> bool {
        self.source == "backfill"
    }

    pub fn fetched_at(&self) -> Option<i64> {
        parse_reset_at_str(&self.sampled_at)
    }

    pub fn reset_at(&self) -> Option<i64> {
        parse_reset_at_str(&self.resets_at)
    }

    pub fn quota_window_kind(&self) -> WindowKind {
        classify_lane(&self.window_kind, self.window_minutes).0
    }

    pub fn to_snapshot(&self) -> Snapshot {
        let observed_at = self.fetched_at();
        let fetched = observed_at.unwrap_or(0);
        let (kind, label) = classify_lane(&self.window_kind, self.window_minutes);
        let window = UsageWindow::from_percent_at(
            kind,
            label,
            self.used_percent,
            self.reset_at(),
            Some(self.window_minutes.saturating_mul(60)),
            observed_at,
            DEFAULT_READING_MAX_AGE_SECS,
        );
        let provider = ProviderSnapshot::observed(ProviderObservation {
            provider: ProviderId::parse(&self.provider).unwrap_or(ProviderId::Codex),
            source: Some(Source::File),
            windows: vec![window],
            credits: None,
            plan: None,
            credential_path: None,
            observed_at,
            max_age_secs: DEFAULT_READING_MAX_AGE_SECS,
            permission: ProviderPermission::Unknown,
        });
        Snapshot::new(fetched, vec![provider])
    }
}

/// CodexBar lane → quota-core window kind. Duration wins over slot name.
pub fn map_codexbar_window_kind(kind: &str) -> WindowKind {
    classify_lane(kind, 0).0
}

fn classify_lane(kind: &str, minutes: i64) -> (WindowKind, String) {
    let mins = if minutes > 0 { Some(minutes) } else { None };
    quota_core::classify_codex_window(Some(kind), None, mins)
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Lane {
    #[serde(default)]
    #[allow(dead_code)]
    reset_description: Option<String>,
    #[serde(default)]
    resets_at: Option<serde_json::Value>,
    #[serde(default)]
    used_percent: Option<f64>,
    #[serde(default)]
    window_minutes: Option<i64>,
    #[serde(default)]
    updated_at: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProviderCost {
    #[serde(default)]
    currency_code: Option<String>,
    #[serde(default)]
    limit: Option<f64>,
    #[serde(default)]
    used: Option<f64>,
    #[serde(default)]
    #[allow(dead_code)]
    period: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreditsBlock {
    #[serde(default)]
    credits_available: Option<bool>,
    #[serde(default)]
    remaining: Option<f64>,
    #[serde(default)]
    #[allow(dead_code)]
    balance_read_succeeded: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotBody {
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    login_method: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    data_confidence: Option<String>,
    #[serde(default)]
    primary: Option<Lane>,
    #[serde(default)]
    secondary: Option<Lane>,
    #[serde(default)]
    tertiary: Option<Lane>,
    #[serde(default)]
    provider_cost: Option<ProviderCost>,
    #[serde(default)]
    credits_remaining: Option<f64>,
    #[serde(default)]
    updated_at: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotRecord {
    #[serde(default)]
    snapshot: Option<SnapshotBody>,
    #[serde(default)]
    credits: Option<CreditsBlock>,
    #[serde(default)]
    source_label: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct SnapshotFile {
    #[serde(default)]
    records: Vec<SnapshotRecord>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManagedAccount {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    workspace_label: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    workspace_account_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ManagedFile {
    #[serde(default)]
    accounts: Vec<ManagedAccount>,
}

fn lane_to_window(
    kind: &str,
    lane: Option<&Lane>,
    provider_observed_at: Option<i64>,
) -> Option<UsageWindow> {
    let lane = lane?;
    let minutes = lane.window_minutes.unwrap_or(0);
    let reset = lane.resets_at.as_ref().and_then(parse_apple_reset_at);
    let observed_at = lane
        .updated_at
        .as_ref()
        .and_then(parse_apple_reset_at)
        .or(provider_observed_at);
    let (wk, label) = classify_lane(kind, minutes);
    let duration = (minutes > 0).then(|| minutes.saturating_mul(60));
    Some(match lane.used_percent {
        Some(used) if used.is_finite() && used >= 0.0 => UsageWindow::from_percent_at(
            wk,
            label,
            used,
            reset,
            duration,
            observed_at,
            DEFAULT_READING_MAX_AGE_SECS,
        ),
        _ => UsageWindow::unreadable(
            wk,
            label,
            reset,
            duration,
            observed_at,
            DEFAULT_READING_MAX_AGE_SECS,
        ),
    })
}

fn credits_from_blocks(
    block: Option<&CreditsBlock>,
    cost: Option<&ProviderCost>,
    remaining_top: Option<f64>,
) -> Option<Credits> {
    let available = block.and_then(|c| c.credits_available);
    let remaining = block
        .and_then(|c| c.remaining)
        .or(remaining_top)
        .or_else(
            || match (cost.and_then(|c| c.limit), cost.and_then(|c| c.used)) {
                (Some(lim), Some(used)) => Some((lim - used).max(0.0)),
                _ => None,
            },
        );
    if available.is_none() && remaining.is_none() && cost.is_none() {
        return None;
    }
    Some(Credits {
        balance: remaining,
        unlimited: Some(false),
        has_credits: available,
        unit: cost
            .and_then(|c| c.currency_code.clone())
            .or_else(|| Some("credits".into())),
    })
}

fn source_from_label(label: Option<&str>) -> Source {
    match label {
        Some("oauth") => Source::Oauth,
        Some("cli") => Source::Cli,
        Some("cookie") => Source::Cookie,
        _ => Source::File,
    }
}

/// Parse the condensed expect / live-shape document (`CURRENT-SNAPSHOT.expect.json`).
pub fn parse_expect_snapshot(bytes: &[u8]) -> Result<ProviderSnapshot, CodexBarError> {
    let body: SnapshotBody =
        serde_json::from_slice(bytes).map_err(|e| CodexBarError::Parse(e.to_string()))?;
    Ok(body_to_provider(&body, None, None))
}

fn body_to_provider(
    body: &SnapshotBody,
    credits: Option<&CreditsBlock>,
    source_label: Option<&str>,
) -> ProviderSnapshot {
    let document_observed_at = body.updated_at.as_ref().and_then(parse_apple_reset_at);
    let mut windows = Vec::new();
    if let Some(w) = lane_to_window("primary", body.primary.as_ref(), document_observed_at) {
        windows.push(w);
    }
    if let Some(w) = lane_to_window("secondary", body.secondary.as_ref(), document_observed_at) {
        windows.push(w);
    }
    if let Some(w) = lane_to_window("tertiary", body.tertiary.as_ref(), document_observed_at) {
        windows.push(w);
    }

    // Without a document time the provider is only as fresh as its stalest lane.
    let observed_at = document_observed_at.or_else(|| {
        windows
            .iter()
            .map(|window| window.observed_at)
            .collect::<Option<Vec<_>>>()
            .and_then(|times| times.into_iter().min())
    });

    let credits = credits_from_blocks(credits, body.provider_cost.as_ref(), body.credits_remaining);
    let provider = body
        .provider
        .as_deref()
        .and_then(ProviderId::parse)
        .unwrap_or(ProviderId::Codex);

    if windows.is_empty() && credits.is_none() {
        let mut snap = ProviderSnapshot::unavailable(
            provider,
            AdapterError::new("empty", "CodexBar snapshot had no windows or credits"),
        );
        snap.source = Some(source_from_label(source_label));
        snap.plan = body.login_method.clone();
        return snap;
    }

    ProviderSnapshot::observed(ProviderObservation {
        provider,
        source: Some(source_from_label(source_label)),
        windows,
        credits,
        plan: body.login_method.clone(),
        credential_path: None,
        observed_at,
        max_age_secs: DEFAULT_READING_MAX_AGE_SECS,
        permission: ProviderPermission::Unknown,
    })
}

/// Parse `codex-account-snapshots.redacted.json` (versioned `records` envelope).
pub fn parse_account_snapshots(bytes: &[u8]) -> Result<Vec<ProviderSnapshot>, CodexBarError> {
    let file: SnapshotFile =
        serde_json::from_slice(bytes).map_err(|e| CodexBarError::Parse(e.to_string()))?;
    let mut out = Vec::new();
    for rec in &file.records {
        let Some(body) = rec.snapshot.as_ref() else {
            continue;
        };
        out.push(body_to_provider(
            body,
            rec.credits.as_ref(),
            rec.source_label.as_deref(),
        ));
    }
    Ok(out)
}

/// One CodexBar history row is a small JSON object. A longer line is skipped
/// so a truncated or hostile file cannot force an unbounded allocation.
const MAX_HISTORY_LINE: usize = 64 * 1024;

pub fn load_history_jsonl(path: &Path) -> Result<Vec<HistoryRow>, CodexBarError> {
    let mut rows = Vec::new();
    for_each_history_line(path, |line| {
        if let Ok(row) = serde_json::from_str::<HistoryRow>(line) {
            rows.push(row);
        }
    })?;
    Ok(rows)
}

/// Keep the last `cap` **parsed** rows. Malformed, partial, and over-long
/// lines do not occupy a slot.
pub fn load_history_jsonl_tail(path: &Path, cap: usize) -> Result<Vec<HistoryRow>, CodexBarError> {
    let cap = cap.max(1);
    let mut ring: VecDeque<HistoryRow> = VecDeque::with_capacity(cap);
    for_each_history_line(path, |line| {
        let Ok(row) = serde_json::from_str::<HistoryRow>(line) else {
            return;
        };
        if ring.len() == cap {
            ring.pop_front();
        }
        ring.push_back(row);
    })?;
    Ok(ring.into_iter().collect())
}

fn for_each_history_line(path: &Path, mut on_line: impl FnMut(&str)) -> Result<(), CodexBarError> {
    let file = quota_core::open_regular_file(path).map_err(|e| CodexBarError::Io(e.to_string()))?;
    let mut reader = BufReader::new(file);
    while let Some(line) = read_bounded_line(&mut reader)? {
        if line.trim().is_empty() {
            continue;
        }
        on_line(&line);
    }
    Ok(())
}

/// Next line that fits in [`MAX_HISTORY_LINE`], or `None` at EOF.
/// Over-long lines are discarded without being returned.
fn read_bounded_line<R: BufRead>(reader: &mut R) -> Result<Option<String>, CodexBarError> {
    loop {
        let mut buf: Vec<u8> = Vec::new();
        let mut oversized = false;
        let saw_newline = loop {
            let data = reader
                .fill_buf()
                .map_err(|e| CodexBarError::Io(e.to_string()))?;
            if data.is_empty() {
                break false;
            }
            if let Some(i) = data.iter().position(|&b| b == b'\n') {
                if !oversized && buf.len().saturating_add(i) <= MAX_HISTORY_LINE {
                    buf.extend_from_slice(&data[..i]);
                } else {
                    oversized = true;
                }
                reader.consume(i + 1);
                break true;
            }
            if oversized || buf.len().saturating_add(data.len()) > MAX_HISTORY_LINE {
                oversized = true;
                buf.clear();
                let n = data.len();
                reader.consume(n);
            } else {
                let n = data.len();
                buf.extend_from_slice(data);
                reader.consume(n);
            }
        };
        if !saw_newline && buf.is_empty() {
            return Ok(None);
        }
        if oversized {
            if saw_newline {
                continue;
            }
            return Ok(None);
        }
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
        match String::from_utf8(buf) {
            Ok(line) => return Ok(Some(line)),
            Err(_) => {
                if saw_newline {
                    continue;
                }
                return Ok(None);
            }
        }
    }
}

pub fn history_to_snapshots(rows: &[HistoryRow]) -> Vec<Snapshot> {
    rows.iter().map(HistoryRow::to_snapshot).collect()
}

/// Metadata-only import from `managed-codex-accounts.redacted.json`.
pub fn parse_managed_account_ids(
    bytes: &[u8],
) -> Result<Vec<(String, Option<String>)>, CodexBarError> {
    let file: ManagedFile =
        serde_json::from_slice(bytes).map_err(|e| CodexBarError::Parse(e.to_string()))?;
    Ok(file
        .accounts
        .into_iter()
        .filter_map(|a| a.id.map(|id| (id, a.email)))
        .collect())
}

pub fn workspace_fixtures_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/codexbar")
}

const MAX_SNAPSHOT_BYTES: u64 = 256 * 1024;
const MAX_HISTORY_ROWS: usize = 4096;

/// First readable snapshot in a CodexBar support dir. Skips `cursor-session.json`.
pub fn load_snapshot_from_dir(dir: &Path) -> Option<(ProviderSnapshot, PathBuf)> {
    for path in quota_core::codexbar_snapshot_candidates(dir) {
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if name == "cursor-session.json" {
            continue;
        }
        let Ok(bytes) = quota_core::read_file_capped(&path, MAX_SNAPSHOT_BYTES as usize) else {
            continue;
        };
        if let Ok(snaps) = parse_account_snapshots(&bytes) {
            if let Some(snap) = snaps.into_iter().next() {
                return Some((snap, path));
            }
        }
        if let Ok(snap) = parse_expect_snapshot(&bytes) {
            if snap.status != Availability::Unavailable {
                return Some((snap, path));
            }
        }
    }
    None
}

/// Newest `cap` history rows as snapshots (oldest first). Missing file → empty.
pub fn load_history_snapshots_from_dir(dir: &Path, cap: usize) -> Vec<Snapshot> {
    let cap = cap.clamp(1, MAX_HISTORY_ROWS);
    for path in quota_core::codexbar_history_candidates(dir) {
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if name == "cursor-session.json" {
            continue;
        }
        let Ok(rows) = load_history_jsonl_tail(&path, cap.min(MAX_HISTORY_ROWS)) else {
            continue;
        };
        if rows.is_empty() {
            continue;
        }
        return history_to_snapshots(&rows);
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use quota_core::math::{can_start, pace_for};
    use quota_core::types::CanStartBasis;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn fixtures() -> std::path::PathBuf {
        workspace_fixtures_dir()
    }

    fn unique_test_dir(prefix: &str) -> std::path::PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        for _ in 0..1_000 {
            let serial = NEXT.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("{prefix}-{}-{serial}", std::process::id()));
            match fs::create_dir(&dir) {
                Ok(()) => return dir,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create test directory {}: {error}", dir.display()),
            }
        }
        panic!("could not allocate unique test directory for {prefix}");
    }

    #[test]
    fn expect_snapshot_secondary_only() {
        let bytes = fs::read(fixtures().join("CURRENT-SNAPSHOT.expect.json")).unwrap();
        let snap = parse_expect_snapshot(&bytes).unwrap();
        assert_eq!(snap.provider, ProviderId::Codex);
        assert_eq!(snap.status, Availability::Stale);
        assert_eq!(snap.freshness, quota_core::types::Freshness::Stale);
        assert_eq!(snap.observed_at, Some(1_790_457_979));
        assert_eq!(snap.plan.as_deref(), Some("pro"));
        assert_eq!(snap.windows.len(), 1);
        let w = &snap.windows[0];
        assert_eq!(w.kind, WindowKind::Weekly);
        assert_eq!(w.used_percent, Some(59.0));
        assert_eq!(w.remaining_percent, Some(41.0));
        assert_eq!(w.limit_window_seconds, Some(10_080 * 60));
        assert_eq!(w.reset_at, Some(1_791_046_689));
        assert_eq!(w.observed_at, snap.observed_at);
        assert!(snap.windows.iter().all(|x| x.kind != WindowKind::Session));
        assert!(w.remaining.is_none());
        assert_eq!(w.unit.as_deref(), Some("percent"));
        let credits = snap.credits.as_ref().expect("credits");
        assert_eq!(credits.balance, Some(0.0));
        assert_eq!(credits.has_credits, None);
    }

    #[test]
    fn envelope_skips_null_primary_tertiary_and_marks_credits_unavailable() {
        let bytes = fs::read(fixtures().join("codex-account-snapshots.redacted.json")).unwrap();
        let snaps = parse_account_snapshots(&bytes).unwrap();
        assert_eq!(snaps.len(), 1);
        let snap = &snaps[0];
        assert_eq!(snap.windows.len(), 1);
        assert_eq!(snap.windows[0].kind, WindowKind::Weekly);
        assert_eq!(snap.source, Some(Source::Oauth));
        assert_eq!(snap.status, Availability::Stale);
        assert_eq!(snap.observed_at, Some(1_790_457_979));
        let credits = snap.credits.as_ref().unwrap();
        assert_eq!(credits.has_credits, Some(false));
        assert_eq!(credits.balance, Some(0.0));
        assert!(!bytes.windows(b"eyJ".len()).any(|w| w == b"eyJ"));
    }

    #[test]
    fn missing_all_lanes_is_unavailable() {
        let json = br#"{
            "provider": "codex",
            "primary": null,
            "secondary": null,
            "tertiary": null
        }"#;
        let snap = parse_expect_snapshot(json).unwrap();
        assert_eq!(snap.status, Availability::Unavailable);
        assert_eq!(snap.error.as_ref().unwrap().code, "empty");
    }

    #[test]
    fn history_counts_and_sources() {
        let rows = load_history_jsonl(&fixtures().join("usage-history.redacted.jsonl")).unwrap();
        assert_eq!(rows.len(), 1912);
        assert!(rows.iter().all(|r| r.window_kind == "secondary"));
        assert!(rows.iter().all(|r| r.provider == "codex"));
        assert!(rows.iter().all(|r| r.window_minutes == 10_080));
        let live = rows.iter().filter(|r| r.is_live()).count();
        let backfill = rows.iter().filter(|r| r.is_backfill()).count();
        assert_eq!(live, 1397);
        assert_eq!(backfill, 515);
        assert_eq!(live + backfill, 1912);
        let last = rows.last().unwrap();
        assert!(last.is_live());
        assert!((last.used_percent - 59.0).abs() < 1e-9);
        assert_eq!(last.resets_at, "2026-10-03T17:00:00Z");
    }

    #[test]
    fn stale_history_is_not_presented_as_current_capacity() {
        let rows = load_history_jsonl(&fixtures().join("usage-history.redacted.jsonl")).unwrap();
        let history = history_to_snapshots(&rows);
        let latest_prov = history.last().unwrap().by_id(ProviderId::Codex).unwrap();
        assert_eq!(latest_prov.status, Availability::Stale);
        let report = pace_for(&history, latest_prov);
        assert!(report.window_kind.is_none());
        assert!(report.burn_percent_per_hour.is_none());
        assert!(report.explanation.contains("stale"));

        let now = latest_prov
            .windows
            .first()
            .and_then(|w| w.reset_at)
            .unwrap_or(0)
            - 86_400;
        let answer = can_start(latest_prov, &history, 50_000, None, now);
        assert!(!answer.ok);
        assert_eq!(answer.basis, CanStartBasis::Unavailable);
        assert!(answer.explanation.contains("stale"));

        let exhausted_row = rows.iter().rev().find(|r| r.used_percent >= 100.0).unwrap();
        let exhausted = exhausted_row.to_snapshot();
        let p = exhausted.by_id(ProviderId::Codex).unwrap();
        assert_eq!(p.status, Availability::Stale);
        assert_eq!(p.exhausted_windows, ["weekly"]);
        let no = can_start(p, &[], 1, None, 1);
        assert!(!no.ok);
        assert_eq!(no.basis, CanStartBasis::Unavailable);
    }

    #[test]
    fn live_versus_backfill_pace() {
        let rows = load_history_jsonl(&fixtures().join("usage-history.redacted.jsonl")).unwrap();
        let live: Vec<_> = rows.iter().filter(|r| r.is_live()).cloned().collect();
        let backfill: Vec<_> = rows.iter().filter(|r| r.is_backfill()).cloned().collect();
        let live_hist = history_to_snapshots(&live);
        let back_hist = history_to_snapshots(&backfill);
        let live_p = live_hist.last().unwrap().by_id(ProviderId::Codex).unwrap();
        let back_p = back_hist.last().unwrap().by_id(ProviderId::Codex).unwrap();
        let live_rep = pace_for(&live_hist, live_p);
        let back_rep = pace_for(&back_hist, back_p);
        assert_eq!(live_rep.samples, 0);
        assert_eq!(back_rep.samples, 0);
        assert!(live_rep.burn_percent_per_hour.is_none());
        assert_eq!(live_p.status, Availability::Stale);
    }

    #[test]
    fn fresh_clock_live_and_backfill_keep_differentiated_burn() {
        let rows = load_history_jsonl(&fixtures().join("usage-history.redacted.jsonl")).unwrap();
        let mut live: Vec<_> = rows.iter().filter(|r| r.is_live()).cloned().collect();
        let mut backfill: Vec<_> = rows.iter().filter(|r| r.is_backfill()).cloned().collect();
        rebase_sample_times(&mut live);
        rebase_sample_times(&mut backfill);

        let live_hist = history_to_snapshots(&live);
        let back_hist = history_to_snapshots(&backfill);
        let live_p = live_hist.last().unwrap().by_id(ProviderId::Codex).unwrap();
        let back_p = back_hist.last().unwrap().by_id(ProviderId::Codex).unwrap();
        let live_rep = pace_for(&live_hist, live_p);
        let back_rep = pace_for(&back_hist, back_p);

        assert_eq!(live_p.status, Availability::Ok);
        assert_eq!(back_p.status, Availability::Ok);
        assert_eq!(live_rep.samples, live.len() as u32);
        assert_eq!(back_rep.samples, backfill.len() as u32);
        let live_burn = live_rep.burn_percent_per_hour.unwrap();
        let backfill_burn = back_rep.burn_percent_per_hour.unwrap();
        assert!(live_burn > 0.0);
        assert!(backfill_burn > 0.0);
        assert_ne!(live_burn, backfill_burn);
    }

    fn rebase_sample_times(rows: &mut [HistoryRow]) {
        let latest = rows.last().and_then(HistoryRow::fetched_at).unwrap();
        let now = quota_core::timeutil::now_unix();
        for row in rows {
            let original = row.fetched_at().unwrap();
            let age = latest.saturating_sub(original);
            row.sampled_at = quota_core::timeutil::format_rfc3339(now.saturating_sub(age));
        }
    }

    #[test]
    fn managed_accounts_have_ids_not_secrets() {
        let bytes = fs::read(fixtures().join("managed-codex-accounts.redacted.json")).unwrap();
        let ids = parse_managed_account_ids(&bytes).unwrap();
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0].0, "3D71AACB-624B-4702-ADB5-496FAD823580");
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("<redacted>"));
        assert!(!text.contains("eyJ"));
    }

    #[test]
    fn missing_lane_is_skipped_and_unreadable_lane_is_preserved() {
        assert!(lane_to_window("primary", None, None).is_none());
        let empty = Lane {
            reset_description: None,
            resets_at: None,
            used_percent: None,
            window_minutes: Some(100),
            updated_at: None,
        };
        let window = lane_to_window("primary", Some(&empty), None).unwrap();
        assert_eq!(window.state, quota_core::types::WindowState::Unknown);
        assert!(window.reading.is_none());
    }

    #[test]
    fn lane_time_makes_the_provider_current_when_the_document_has_none() {
        let now = quota_core::timeutil::now_unix();
        let doc = format!(
            r#"{{"provider":"codex","primary":{{"usedPercent":10,"windowMinutes":10080,"updatedAt":"{}"}}}}"#,
            quota_core::timeutil::format_rfc3339(now - 5)
        );
        let snap = parse_expect_snapshot(doc.as_bytes()).unwrap();
        assert_eq!(snap.observed_at, Some(now - 5));
        assert_eq!(snap.freshness, quota_core::types::Freshness::Current);
        assert_eq!(snap.status, Availability::Ok);
    }

    #[test]
    fn a_lane_without_any_time_keeps_the_provider_stale() {
        let now = quota_core::timeutil::now_unix();
        let doc = format!(
            r#"{{"provider":"codex","primary":{{"usedPercent":10,"windowMinutes":10080,"updatedAt":"{}"}},"secondary":{{"usedPercent":5,"windowMinutes":300}}}}"#,
            quota_core::timeutil::format_rfc3339(now - 5)
        );
        let snap = parse_expect_snapshot(doc.as_bytes()).unwrap();
        assert_eq!(snap.observed_at, None);
        assert_ne!(snap.status, Availability::Ok);
    }

    #[test]
    fn numeric_string_history_reset_is_unix_seconds() {
        let row: HistoryRow = serde_json::from_str(
            r#"{"accountKey":"a","provider":"codex","resetsAt":"1790000000","sampledAt":"1789990000","source":"live","usedPercent":10.0,"windowKind":"secondary","windowMinutes":10080}"#,
        )
        .unwrap();
        assert_eq!(row.reset_at(), Some(1_790_000_000));
        assert_eq!(row.fetched_at(), Some(1_789_990_000));
    }

    #[test]
    fn primary_lane_with_weekly_minutes_is_weekly() {
        let lane = Lane {
            reset_description: None,
            resets_at: None,
            used_percent: Some(59.0),
            window_minutes: Some(10_080),
            updated_at: None,
        };
        let w = lane_to_window("primary", Some(&lane), None).unwrap();
        assert_eq!(w.kind, WindowKind::Weekly);
        assert_eq!(w.label, "weekly");
    }

    #[test]
    fn load_snapshot_from_fixture_dir() {
        let (snap, path) = load_snapshot_from_dir(&fixtures()).expect("fixture snapshot");
        assert!(path.ends_with("codex-account-snapshots.redacted.json"));
        assert_eq!(snap.windows[0].kind, WindowKind::Weekly);
        assert_eq!(snap.windows[0].used_percent, Some(59.0));
        let hist = load_history_snapshots_from_dir(&fixtures(), 16);
        assert_eq!(hist.len(), 16);
        let tail =
            load_history_jsonl_tail(&fixtures().join("usage-history.redacted.jsonl"), 16).unwrap();
        assert_eq!(tail.len(), 16);
        assert!((tail.last().unwrap().used_percent - 59.0).abs() < 1e-9);
        assert_eq!(
            hist.last()
                .unwrap()
                .by_id(ProviderId::Codex)
                .unwrap()
                .windows[0]
                .kind,
            WindowKind::Weekly
        );
    }

    #[test]
    fn ignores_cursor_session_json() {
        let dir = unique_test_dir("quota-codexbar-skip");
        fs::write(
            dir.join("cursor-session.json"),
            r#"{"WorkosCursorSessionToken":"eyJhbGciOi.not-a-real-token"}"#,
        )
        .unwrap();
        assert!(load_snapshot_from_dir(&dir).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_or_malformed_history_falls_through() {
        let dir = unique_test_dir("quota-jsonl-fallback");
        fs::write(dir.join("usage-history.jsonl"), "{not json}\n").unwrap();
        let good = r#"{"accountKey":"a","provider":"codex","resetsAt":"2026-10-03T17:00:00Z","sampledAt":"2026-09-27T12:00:00Z","source":"live","usedPercent":10.0,"windowKind":"secondary","windowMinutes":10080}"#;
        fs::write(
            dir.join("usage-history.redacted.jsonl"),
            format!("{good}\n"),
        )
        .unwrap();
        let hist = load_history_snapshots_from_dir(&dir, 8);
        assert_eq!(hist.len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn truncated_last_jsonl_line_is_skipped() {
        let dir = unique_test_dir("quota-jsonl-trunc");
        let path = dir.join("history.jsonl");
        let good = r#"{"accountKey":"a","provider":"codex","resetsAt":"2026-10-03T17:00:00Z","sampledAt":"2026-09-27T12:00:00Z","source":"live","usedPercent":10.0,"windowKind":"secondary","windowMinutes":10080}"#;
        fs::write(&path, format!("{good}\n{{\"accountKey\":\"partial")).unwrap();
        let rows = load_history_jsonl(&path).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].used_percent, 10.0);
        let tail = load_history_jsonl_tail(&path, 8).unwrap();
        assert_eq!(tail.len(), 1);
        let tail_one = load_history_jsonl_tail(&path, 1).unwrap();
        assert_eq!(tail_one.len(), 1);
        assert_eq!(tail_one[0].used_percent, 10.0);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn partial_tail_does_not_evict_valid_row() {
        let dir = unique_test_dir("quota-jsonl-cap");
        let path = dir.join("history.jsonl");
        let row = |pct: f64| {
            format!(
                r#"{{"accountKey":"a","provider":"codex","resetsAt":"2026-10-03T17:00:00Z","sampledAt":"2026-09-27T12:00:00Z","source":"live","usedPercent":{pct},"windowKind":"secondary","windowMinutes":10080}}"#
            )
        };
        let huge = "x".repeat(MAX_HISTORY_LINE + 8);
        fs::write(
            &path,
            format!("{}\n{}\n{huge}\n{}", row(1.0), row(2.0), row(3.0)),
        )
        .unwrap();
        let tail = load_history_jsonl_tail(&path, 2).unwrap();
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0].used_percent, 2.0);
        assert_eq!(tail[1].used_percent, 3.0);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn snapshot_does_not_put_confidence_in_credential_path() {
        let bytes = fs::read(fixtures().join("CURRENT-SNAPSHOT.expect.json")).unwrap();
        let snap = parse_expect_snapshot(&bytes).unwrap();
        assert!(snap.credential_path.is_none());
        let encoded = serde_json::to_string(&snap).unwrap();
        assert!(!encoded.contains("credential_path"));
        assert!(encoded.contains("\"used_percent\":59"));
    }

    fn assert_sensitive_values_redacted(v: &serde_json::Value, name: &str) {
        match v {
            serde_json::Value::Object(map) => {
                for (k, child) in map {
                    if matches!(k.as_str(), "authFingerprint" | "managedHomePath") {
                        let s = child.as_str().unwrap_or("");
                        assert!(
                            s.contains("<redacted") || s.to_ascii_lowercase().starts_with("sha256"),
                            "{name} {k}={s:?} must be a redaction/hash, not a live value"
                        );
                    }
                    assert_sensitive_values_redacted(child, name);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    assert_sensitive_values_redacted(item, name);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn fixture_pack_has_no_live_credentials() {
        // Field *names* such as authFingerprint may appear in redacted
        // fixtures and notes. Only live-looking *values* fail the test.
        for name in [
            "CURRENT-SNAPSHOT.expect.json",
            "codex-account-snapshots.redacted.json",
            "managed-codex-accounts.redacted.json",
            "usage-history.redacted.jsonl",
        ] {
            let text = fs::read_to_string(fixtures().join(name)).unwrap();
            for needle in [
                "WorkosCursorSessionToken",
                "access_token",
                "refresh_token",
                "sk-ant-",
                "sk-proj-",
                "eyJhbGci",
            ] {
                assert!(!text.contains(needle), "{name} must not contain {needle}");
            }
            if name.ends_with(".jsonl") {
                for (i, line) in text.lines().enumerate() {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let v: serde_json::Value =
                        serde_json::from_str(line).unwrap_or_else(|e| panic!("{name}:{i}: {e}"));
                    assert_sensitive_values_redacted(&v, name);
                }
            } else {
                let v: serde_json::Value = serde_json::from_str(&text).unwrap();
                assert_sensitive_values_redacted(&v, name);
            }
        }
    }
}
