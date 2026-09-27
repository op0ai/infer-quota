//! CodexBar *local snapshot* parser — not a collector.
//!
//! Maps the redacted CodexBar on-disk shape (secondary window with
//! `usedPercent` / `windowMinutes` / `resetsAt`) onto `quota-core` types.
//! HTTP probing stays in [`crate::codex`]. This module is for fixtures,
//! tests, and any later importer that already has a CodexBar JSON file.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use quota_core::timeutil::{parse_reset_at, parse_reset_at_str};
use quota_core::types::{
    AdapterError, Availability, Credits, ProviderId, ProviderSnapshot, Snapshot, Source,
    UsageWindow, WindowKind,
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
        let fetched = self.fetched_at().unwrap_or(0);
        let (kind, label) = classify_lane(&self.window_kind, self.window_minutes);
        let window = UsageWindow::from_percent(
            kind,
            label,
            self.used_percent,
            self.reset_at(),
            Some(self.window_minutes.saturating_mul(60)),
        );
        let provider = ProviderSnapshot {
            provider: ProviderId::parse(&self.provider).unwrap_or(ProviderId::Codex),
            status: Availability::Ok,
            source: Some(Source::File),
            windows: vec![window],
            credits: None,
            plan: None,
            error: None,
            credential_path: None,
        };
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

fn lane_to_window(kind: &str, lane: Option<&Lane>) -> Option<UsageWindow> {
    let lane = lane?;
    let used = lane.used_percent?;
    let minutes = lane.window_minutes.unwrap_or(0);
    let reset = lane.resets_at.as_ref().and_then(parse_reset_at);
    let (wk, label) = classify_lane(kind, minutes);
    Some(UsageWindow::from_percent(
        wk,
        label,
        used,
        reset,
        if minutes > 0 {
            Some(minutes.saturating_mul(60))
        } else {
            None
        },
    ))
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
    let mut windows = Vec::new();
    if let Some(w) = lane_to_window("primary", body.primary.as_ref()) {
        windows.push(w);
    }
    if let Some(w) = lane_to_window("secondary", body.secondary.as_ref()) {
        windows.push(w);
    }
    if let Some(w) = lane_to_window("tertiary", body.tertiary.as_ref()) {
        windows.push(w);
    }

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

    ProviderSnapshot {
        provider,
        status: Availability::Ok,
        source: Some(source_from_label(source_label)),
        windows,
        credits,
        plan: body.login_method.clone(),
        error: None,
        credential_path: body.data_confidence.clone(),
    }
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

pub fn load_history_jsonl(path: &Path) -> Result<Vec<HistoryRow>, CodexBarError> {
    let file = fs::File::open(path).map_err(|e| CodexBarError::Io(e.to_string()))?;
    let reader = BufReader::new(file);
    let mut rows = Vec::new();
    for (i, line) in reader.lines().enumerate() {
        let line = line.map_err(|e| CodexBarError::Io(e.to_string()))?;
        if line.trim().is_empty() {
            continue;
        }
        let row: HistoryRow = serde_json::from_str(&line)
            .map_err(|e| CodexBarError::Parse(format!("line {}: {e}", i + 1)))?;
        rows.push(row);
    }
    Ok(rows)
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
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        if meta.len() > MAX_SNAPSHOT_BYTES {
            continue;
        }
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        if let Ok(snaps) = parse_account_snapshots(&bytes) {
            if let Some(snap) = snaps.into_iter().next() {
                return Some((snap, path));
            }
        }
        if let Ok(snap) = parse_expect_snapshot(&bytes) {
            if snap.status == Availability::Ok {
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
        let Ok(rows) = load_history_jsonl(&path) else {
            continue;
        };
        let n = rows.len();
        let start = n.saturating_sub(cap.min(MAX_HISTORY_ROWS));
        return history_to_snapshots(&rows[start..]);
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use quota_core::math::{can_start, pace_for};
    use quota_core::types::CanStartBasis;

    fn fixtures() -> std::path::PathBuf {
        workspace_fixtures_dir()
    }

    #[test]
    fn expect_snapshot_secondary_only() {
        let bytes = fs::read(fixtures().join("CURRENT-SNAPSHOT.expect.json")).unwrap();
        let snap = parse_expect_snapshot(&bytes).unwrap();
        assert_eq!(snap.provider, ProviderId::Codex);
        assert_eq!(snap.status, Availability::Ok);
        assert_eq!(snap.plan.as_deref(), Some("pro"));
        assert_eq!(snap.windows.len(), 1);
        let w = &snap.windows[0];
        assert_eq!(w.kind, WindowKind::Weekly);
        assert_eq!(w.used_percent, Some(59.0));
        assert_eq!(w.remaining_percent, Some(41.0));
        assert_eq!(w.limit_window_seconds, Some(10_080 * 60));
        assert_eq!(w.reset_at, Some(812_739_489));
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
    fn replay_pace_and_can_start_percent_only() {
        let rows = load_history_jsonl(&fixtures().join("usage-history.redacted.jsonl")).unwrap();
        let history = history_to_snapshots(&rows);
        let latest_prov = history.last().unwrap().by_id(ProviderId::Codex).unwrap();
        let report = pace_for(&history, latest_prov);
        assert_eq!(report.window_kind, Some(WindowKind::Weekly));
        assert_eq!(report.used_percent, Some(59.0));
        assert!(report.samples >= 2);
        assert!(report.burn_percent_per_hour.is_some());

        let now = latest_prov
            .windows
            .first()
            .and_then(|w| w.reset_at)
            .unwrap_or(0)
            - 86_400;
        let answer = can_start(latest_prov, &history, 50_000, None, now);
        assert!(!answer.ok);
        assert_eq!(answer.basis, CanStartBasis::PercentOnly);
        assert!(
            answer.explanation.contains("Cannot map") || answer.explanation.contains("percent")
        );

        let exhausted_row = rows.iter().rev().find(|r| r.used_percent >= 100.0).unwrap();
        let exhausted = exhausted_row.to_snapshot();
        let p = exhausted.by_id(ProviderId::Codex).unwrap();
        let no = can_start(p, &[], 1, None, 1);
        assert!(!no.ok);
        assert_eq!(no.basis, CanStartBasis::PercentOnly);
        assert!(no.explanation.contains("exhausted"));
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
        assert_eq!(live_rep.samples, live.len() as u32);
        assert_eq!(back_rep.samples, backfill.len() as u32);
        assert!(live_rep.burn_percent_per_hour.is_some());
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
    fn null_lane_object_is_skipped() {
        assert!(lane_to_window("primary", None).is_none());
        let empty = Lane {
            reset_description: None,
            resets_at: None,
            used_percent: None,
            window_minutes: Some(100),
        };
        assert!(lane_to_window("primary", Some(&empty)).is_none());
    }

    #[test]
    fn primary_lane_with_weekly_minutes_is_weekly() {
        let lane = Lane {
            reset_description: None,
            resets_at: None,
            used_percent: Some(59.0),
            window_minutes: Some(10_080),
        };
        let w = lane_to_window("primary", Some(&lane)).unwrap();
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
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("quota-codexbar-skip-{stamp}"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("cursor-session.json"),
            r#"{"WorkosCursorSessionToken":"eyJhbGciOi.not-a-real-token"}"#,
        )
        .unwrap();
        assert!(load_snapshot_from_dir(&dir).is_none());
        let _ = fs::remove_dir_all(&dir);
    }
}
