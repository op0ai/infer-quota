//! Tiny RFC3339 helpers without a datetime crate (keeps the MSRV and RSS small).
//! Snapshots store unix seconds plus a formatted string so clients do not need
//! a date library.

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn format_rfc3339(unix_secs: i64) -> String {
    let (y, m, d, hh, mm, ss) = civil_from_unix(unix_secs);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Accept RFC3339, unix seconds (number or numeric string), or millisecond
/// timestamps (`>= 10^11` treated as ms, matching Claude Code `expiresAt`).
pub fn parse_reset_at(value: &serde_json::Value) -> Option<i64> {
    match value {
        serde_json::Value::Null => None,
        serde_json::Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .map(normalize_epoch),
        serde_json::Value::String(s) => parse_reset_at_str(s),
        _ => None,
    }
}

pub fn parse_reset_at_str(s: &str) -> Option<i64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    if let Ok(n) = t.parse::<i64>() {
        return Some(normalize_epoch(n));
    }
    if let Ok(n) = t.parse::<f64>() {
        return Some(normalize_epoch(n as i64));
    }
    parse_rfc3339(t)
}

fn normalize_epoch(n: i64) -> i64 {
    if n.abs() >= 100_000_000_000 {
        n / 1000
    } else {
        n
    }
}

/// Minimal RFC3339: `YYYY-MM-DDTHH:MM:SS[.frac](Z|+HH:MM|-HH:MM)`.
fn parse_rfc3339(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date, rest) = s.split_once('T').or_else(|| s.split_once('t'))?;
    let mut date_parts = date.split('-');
    let year: i32 = date_parts.next()?.parse().ok()?;
    let month: u32 = date_parts.next()?.parse().ok()?;
    let day: u32 = date_parts.next()?.parse().ok()?;

    let rest = rest.trim();
    let (time, offset_secs) = split_offset(rest)?;
    let mut tparts = time.split(':');
    let hour: u32 = tparts.next()?.parse().ok()?;
    let minute: u32 = tparts.next()?.parse().ok()?;
    let sec_raw = tparts.next()?;
    let sec_whole = sec_raw.split(['.', ',']).next()?;
    let second: u32 = sec_whole.parse().ok()?;

    let unix = unix_from_civil(year, month, day, hour, minute, second)?;
    Some(unix - offset_secs)
}

fn split_offset(rest: &str) -> Option<(&str, i64)> {
    if let Some(time) = rest.strip_suffix('Z').or_else(|| rest.strip_suffix('z')) {
        return Some((time, 0));
    }
    let bytes = rest.as_bytes();
    let mut idx = None;
    for (i, b) in bytes.iter().enumerate().rev() {
        if *b == b'+' || (*b == b'-' && i >= 8) {
            idx = Some(i);
            break;
        }
    }
    let i = idx?;
    let (time, off) = rest.split_at(i);
    let sign = if off.as_bytes().first()? == &b'-' {
        -1i64
    } else {
        1
    };
    let off = &off[1..];
    let (oh, om) = if let Some((h, m)) = off.split_once(':') {
        (h.parse::<i64>().ok()?, m.parse::<i64>().ok()?)
    } else if off.len() == 4 {
        (off[..2].parse().ok()?, off[2..].parse().ok()?)
    } else if off.len() == 2 {
        (off.parse().ok()?, 0)
    } else {
        return None;
    };
    Some((time, sign * (oh * 3600 + om * 60)))
}

/// Howard Hinnant civil-from-days (UTC).
fn civil_from_unix(unix_secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    let z = unix_secs.div_euclid(86400) + 719_468;
    let secs = unix_secs.rem_euclid(86400) as u32;
    let hh = secs / 3600;
    let mm = (secs % 3600) / 60;
    let ss = secs % 60;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d, hh, mm, ss)
}

fn unix_from_civil(
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let y = if month <= 2 { year - 1 } else { year } as i64;
    let m = if month <= 2 { month + 9 } else { month - 3 } as i64;
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86400 + hour as i64 * 3600 + minute as i64 * 60 + second as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_roundtrip_seconds() {
        let s = format_rfc3339(1_700_000_000);
        let parsed = parse_reset_at_str(&s).expect("parse");
        assert_eq!(parsed, 1_700_000_000);
        assert_eq!(s, "2023-11-14T22:13:20Z");
    }

    #[test]
    fn millis_detected() {
        assert_eq!(normalize_epoch(1_759_700_000_000), 1_759_700_000);
        assert_eq!(normalize_epoch(1_759_700_000), 1_759_700_000);
    }

    #[test]
    fn offset_rfc3339() {
        let parsed = parse_reset_at_str("2026-04-11T07:00:00.528743+00:00").expect("parse");
        assert_eq!(parsed, parse_reset_at_str("2026-04-11T07:00:00Z").unwrap());
        let plus = parse_reset_at_str("2026-04-11T08:00:00+01:00").unwrap();
        let z = parse_reset_at_str("2026-04-11T07:00:00Z").unwrap();
        assert_eq!(plus, z);
    }

    #[test]
    fn known_epoch() {
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(parse_reset_at_str("1970-01-01T00:00:00Z"), Some(0));
    }
}
