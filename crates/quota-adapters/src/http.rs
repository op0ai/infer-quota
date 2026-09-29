//! Tiny HTTPS GET. Production uses rustls 0.21 + webpki-roots (no `url`/`icu`
//! stack — those crates currently exceed our 1.83 MSRV). Tests inject a mock.
//!
//! Response bodies are capped so a noisy endpoint cannot grow RSS.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use thiserror::Error;

pub const MAX_BODY_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
    pub retry_after_secs: Option<u64>,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum TransportError {
    #[error("http transport: {0}")]
    Message(String),
}

pub trait Transport: Send + Sync {
    fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<HttpResponse, TransportError>;
}

/// Blocking rustls client. Call from `spawn_blocking` in the daemon so the
/// current-thread runtime can keep accepting sockets.
pub struct TlsTransport {
    timeout: Duration,
}

impl TlsTransport {
    pub fn new(timeout: Duration) -> Self {
        Self { timeout }
    }
}

impl Transport for TlsTransport {
    fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<HttpResponse, TransportError> {
        https_get(url, headers, self.timeout)
    }
}

fn client_config() -> Arc<ClientConfig> {
    static CFG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    CFG.get_or_init(|| {
        let mut roots = RootCertStore::empty();
        roots.add_trust_anchors(webpki_roots::TLS_SERVER_ROOTS.iter().map(|ta| {
            rustls::OwnedTrustAnchor::from_subject_spki_name_constraints(
                ta.subject,
                ta.spki,
                ta.name_constraints,
            )
        }));
        Arc::new(
            ClientConfig::builder()
                .with_safe_defaults()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        )
    })
    .clone()
}

struct HttpsUrl {
    host: String,
    port: u16,
    path: String,
}

fn parse_https_url(url: &str) -> Result<HttpsUrl, TransportError> {
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| TransportError::Message("only https:// URLs are supported".into()))?;
    let (hostport, path) = match rest.split_once('/') {
        Some((h, p)) => (h, format!("/{p}")),
        None => (rest, "/".to_string()),
    };
    let (host, port) = if let Some((h, p)) = hostport.split_once(':') {
        let port = p
            .parse::<u16>()
            .map_err(|_| TransportError::Message("invalid port".into()))?;
        (h.to_string(), port)
    } else {
        (hostport.to_string(), 443)
    };
    if host.is_empty() {
        return Err(TransportError::Message("empty host".into()));
    }
    Ok(HttpsUrl { host, port, path })
}

fn https_get(
    url: &str,
    headers: &[(&str, &str)],
    timeout: Duration,
) -> Result<HttpResponse, TransportError> {
    let parsed = parse_https_url(url)?;
    let tcp = TcpStream::connect((parsed.host.as_str(), parsed.port))
        .map_err(|e| TransportError::Message(format!("connect: {e}")))?;
    tcp.set_read_timeout(Some(timeout))
        .and_then(|_| tcp.set_write_timeout(Some(timeout)))
        .map_err(|e| TransportError::Message(e.to_string()))?;

    let server_name = parsed
        .host
        .as_str()
        .try_into()
        .map_err(|e| TransportError::Message(format!("sni: {e}")))?;
    let conn = ClientConnection::new(client_config(), server_name)
        .map_err(|e| TransportError::Message(format!("tls: {e}")))?;
    let mut tls = StreamOwned::new(conn, tcp);

    let mut req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
        parsed.path, parsed.host
    );
    for (k, v) in headers {
        if k.eq_ignore_ascii_case("host") || k.eq_ignore_ascii_case("connection") {
            continue;
        }
        req.push_str(k);
        req.push_str(": ");
        req.push_str(v);
        req.push_str("\r\n");
    }
    req.push_str("\r\n");
    tls.write_all(req.as_bytes())
        .map_err(|e| TransportError::Message(format!("write: {e}")))?;

    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match tls.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let room = (MAX_BODY_BYTES + 8192).saturating_sub(raw.len());
                if room == 0 {
                    break;
                }
                raw.extend_from_slice(&buf[..n.min(room)]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => break,
            Err(e) => return Err(TransportError::Message(format!("read: {e}"))),
        }
    }
    parse_http_response(&raw)
}

fn parse_http_response(raw: &[u8]) -> Result<HttpResponse, TransportError> {
    let sep = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| TransportError::Message("truncated HTTP response".into()))?;
    let header_bytes = &raw[..sep];
    let body = &raw[sep + 4..];
    let headers = std::str::from_utf8(header_bytes)
        .map_err(|_| TransportError::Message("non-utf8 HTTP headers".into()))?;
    let mut lines = headers.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| TransportError::Message("empty status line".into()))?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| TransportError::Message(format!("bad status line: {status_line}")))?;

    let mut retry_after_secs = None;
    let mut content_length = None;
    for line in lines {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        if k.eq_ignore_ascii_case("retry-after") {
            retry_after_secs = parse_retry_after(v.trim(), quota_core::timeutil::now_unix());
        } else if k.eq_ignore_ascii_case("content-length") {
            content_length = v.trim().parse::<usize>().ok();
        } else if k.eq_ignore_ascii_case("transfer-encoding")
            && v.to_ascii_lowercase().contains("chunked")
        {
            return decode_chunked(status, retry_after_secs, body);
        }
    }
    let mut body = body.to_vec();
    if let Some(n) = content_length {
        body.truncate(n.min(MAX_BODY_BYTES));
    } else if body.len() > MAX_BODY_BYTES {
        body.truncate(MAX_BODY_BYTES);
    }
    Ok(HttpResponse {
        status,
        body,
        retry_after_secs,
    })
}

fn parse_retry_after(value: &str, now: i64) -> Option<u64> {
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(seconds);
    }
    let target = parse_http_date(value, now)?;
    Some(target.saturating_sub(now).max(0) as u64)
}

/// RFC 9110 HTTP-date: IMF-fixdate, obsolete RFC 850, and ANSI C `asctime()`.
fn parse_http_date(value: &str, now: i64) -> Option<i64> {
    let normalized = value.replace([',', '-'], " ");
    let mut tokens = normalized.split_whitespace();
    let weekday = tokens.next()?;
    if !weekday.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let rest: Vec<&str> = tokens.collect();
    let (day, month, year, time) = match rest.as_slice() {
        [day, month, year, time, "GMT"] => (*day, *month, *year, *time),
        [month, day, time, year] => (*day, *month, *year, *time),
        _ => return None,
    };
    let day = day.parse::<u32>().ok()?;
    let month = month_number(month)?;
    let year = expand_year(year, now)?;
    quota_core::timeutil::parse_reset_at_str(&format!("{year:04}-{month:02}-{day:02}T{time}Z"))
}

fn month_number(name: &str) -> Option<u32> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    MONTHS
        .iter()
        .position(|month| *month == name)
        .map(|index| index as u32 + 1)
}

/// Two-digit RFC 850 years resolve to the century that is not more than 50
/// years ahead of `now` (RFC 9110 §5.6.7).
fn expand_year(raw: &str, now: i64) -> Option<i32> {
    let year = raw.parse::<i32>().ok()?;
    if raw.len() != 2 {
        return Some(year);
    }
    let current_year = quota_core::timeutil::format_rfc3339(now)
        .get(..4)?
        .parse::<i32>()
        .ok()?;
    let candidate = current_year - current_year % 100 + year;
    Some(if candidate > current_year + 50 {
        candidate - 100
    } else {
        candidate
    })
}

fn decode_chunked(
    status: u16,
    retry_after_secs: Option<u64>,
    mut rest: &[u8],
) -> Result<HttpResponse, TransportError> {
    let mut body = Vec::new();
    loop {
        let nl = rest
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| TransportError::Message("truncated chunk".into()))?;
        let size_line = std::str::from_utf8(&rest[..nl])
            .map_err(|_| TransportError::Message("bad chunk size".into()))?
            .split(';')
            .next()
            .unwrap_or("")
            .trim();
        let size = usize::from_str_radix(size_line, 16)
            .map_err(|_| TransportError::Message("bad chunk size".into()))?;
        rest = &rest[nl + 2..];
        if size == 0 {
            break;
        }
        if rest.len() < size + 2 {
            return Err(TransportError::Message("truncated chunk body".into()));
        }
        let take = size.min(MAX_BODY_BYTES.saturating_sub(body.len()));
        body.extend_from_slice(&rest[..take]);
        rest = &rest[size + 2..];
        if body.len() >= MAX_BODY_BYTES {
            break;
        }
    }
    Ok(HttpResponse {
        status,
        body,
        retry_after_secs,
    })
}

/// In-memory GET mock for adapter tests. No network.
#[derive(Debug, Default)]
pub struct MockTransport {
    pub next: Option<Result<HttpResponse, TransportError>>,
    pub last_url: std::sync::Mutex<Option<String>>,
}

impl MockTransport {
    pub fn ok_json(status: u16, json: &str) -> Self {
        Self {
            next: Some(Ok(HttpResponse {
                status,
                body: json.as_bytes().to_vec(),
                retry_after_secs: None,
            })),
            last_url: std::sync::Mutex::new(None),
        }
    }
}

impl Transport for MockTransport {
    fn get(&self, url: &str, _headers: &[(&str, &str)]) -> Result<HttpResponse, TransportError> {
        if let Ok(mut g) = self.last_url.lock() {
            *g = Some(url.to_string());
        }
        match &self.next {
            Some(Ok(r)) => Ok(r.clone()),
            Some(Err(e)) => Err(e.clone()),
            None => Err(TransportError::Message("no mock response queued".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_https_url() {
        let u = parse_https_url("https://api.anthropic.com/api/oauth/usage").unwrap();
        assert_eq!(u.host, "api.anthropic.com");
        assert_eq!(u.port, 443);
        assert_eq!(u.path, "/api/oauth/usage");
    }

    #[test]
    fn parses_response_with_length() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}extra";
        let r = parse_http_response(raw).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"{}");
    }

    #[test]
    fn parses_chunked() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n\r\n";
        let r = parse_http_response(raw).unwrap();
        assert_eq!(r.body, b"{}");
    }

    #[test]
    fn parses_retry_after_delta_and_http_date() {
        assert_eq!(parse_retry_after("17", 1_000), Some(17));
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT", 1_445_412_300),
            Some(180)
        );
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:13:00 GMT", 1_445_412_300),
            Some(0)
        );
    }

    #[test]
    fn parses_every_rfc9110_http_date_form() {
        let now = 1_445_412_300;
        for date in [
            "Wed, 21 Oct 2015 07:28:00 GMT",
            "Wednesday, 21-Oct-15 07:28:00 GMT",
            "Wed Oct 21 07:28:00 2015",
        ] {
            assert_eq!(parse_retry_after(date, now), Some(180), "{date}");
        }
        assert_eq!(
            parse_retry_after("Wed Oct  1 07:28:00 2015", now),
            Some(0),
            "single-digit asctime day"
        );
    }

    #[test]
    fn rejects_malformed_http_dates() {
        let now = 1_445_412_300;
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:28:00 PST", now),
            None
        );
        assert_eq!(
            parse_retry_after("Wed, 21 Foo 2015 07:28:00 GMT", now),
            None
        );
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 25:28:00 GMT", now),
            None
        );
        assert_eq!(parse_retry_after("soon", now), None);
    }

    #[test]
    fn two_digit_years_pick_the_century_within_fifty_years() {
        let now = 1_445_412_300;
        assert_eq!(expand_year("15", now), Some(2015));
        assert_eq!(expand_year("99", now), Some(1999));
        assert_eq!(expand_year("2015", now), Some(2015));
    }
}
