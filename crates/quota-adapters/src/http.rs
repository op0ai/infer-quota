//! Tiny HTTPS GET. Production uses rustls 0.21 + webpki-roots (no `url`/`icu`
//! stack — those crates currently exceed our 1.83 MSRV). Tests inject a mock.
//!
//! Response bodies are capped so a noisy endpoint cannot grow RSS.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use quota_core::types::MAX_RETRY_AFTER_SECS;
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

/// Lookups a stalled resolver may hold at once, across every host. Beyond
/// this a probe fails at once instead of starting another thread.
const MAX_OUTSTANDING_LOOKUPS: usize = 4;

/// Resolve and connect within one `timeout` budget. The read and write
/// timeouts bound only an established stream; without this a stalled DNS
/// lookup or TCP handshake would outlast the configured HTTP timeout.
fn connect_within(host: &str, port: u16, timeout: Duration) -> Result<TcpStream, TransportError> {
    let deadline = Instant::now() + timeout;
    let addrs = system_resolver().resolve(host, port, timeout, |host, port| {
        (host, port).to_socket_addrs().map(Iterator::collect)
    })?;
    connect_any(&addrs, deadline, |addr, left| {
        TcpStream::connect_timeout(addr, left)
    })
}

/// Try each address with an equal share of the time left, so one that
/// never answers cannot spend the budget of those after it.
fn connect_any<T>(
    addrs: &[SocketAddr],
    deadline: Instant,
    mut connect: impl FnMut(&SocketAddr, Duration) -> std::io::Result<T>,
) -> Result<T, TransportError> {
    let mut last = None;
    for (index, addr) in addrs.iter().enumerate() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let untried = u32::try_from(addrs.len() - index).unwrap_or(u32::MAX);
        match connect(addr, left / untried) {
            Ok(stream) => return Ok(stream),
            Err(e) => last = Some(e.to_string()),
        }
    }
    Err(TransportError::Message(format!(
        "connect: {}",
        last.unwrap_or_else(|| "timed out".into())
    )))
}

fn system_resolver() -> Arc<Resolver> {
    static RESOLVER: OnceLock<Arc<Resolver>> = OnceLock::new();
    RESOLVER
        .get_or_init(|| Arc::new(Resolver::new(MAX_OUTSTANDING_LOOKUPS)))
        .clone()
}

type LookupResult = Result<Vec<SocketAddr>, String>;

/// Blocking lookups run on worker threads so a caller can stop waiting at
/// its deadline. A worker that outlives its caller stays registered until it
/// returns: later callers for the same host wait on it rather than start
/// another, and at most `limit` workers exist however many callers time out.
struct Resolver {
    limit: usize,
    running: Mutex<Vec<Running>>,
}

struct Running {
    host: String,
    port: u16,
    lookup: Arc<Lookup>,
}

#[derive(Default)]
struct Lookup {
    result: Mutex<Option<LookupResult>>,
    done: Condvar,
}

impl Resolver {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            running: Mutex::new(Vec::new()),
        }
    }

    fn resolve<F>(
        self: &Arc<Self>,
        host: &str,
        port: u16,
        timeout: Duration,
        lookup: F,
    ) -> Result<Vec<SocketAddr>, TransportError>
    where
        F: FnOnce(&str, u16) -> std::io::Result<Vec<SocketAddr>> + Send + 'static,
    {
        let pending = self.join_or_start(host, port, lookup)?;
        let result = lock(&pending.result);
        let (result, _) = pending
            .done
            .wait_timeout_while(result, timeout, |result| result.is_none())
            .unwrap_or_else(PoisonError::into_inner);
        match result.as_ref() {
            Some(Ok(addrs)) => Ok(addrs.clone()),
            Some(Err(e)) => Err(TransportError::Message(format!("resolve: {e}"))),
            None => Err(TransportError::Message("resolve: timed out".into())),
        }
    }

    fn join_or_start<F>(
        self: &Arc<Self>,
        host: &str,
        port: u16,
        lookup: F,
    ) -> Result<Arc<Lookup>, TransportError>
    where
        F: FnOnce(&str, u16) -> std::io::Result<Vec<SocketAddr>> + Send + 'static,
    {
        let pending = {
            let mut running = lock(&self.running);
            if let Some(same) = running.iter().find(|r| r.host == host && r.port == port) {
                return Ok(same.lookup.clone());
            }
            if running.len() >= self.limit {
                return Err(TransportError::Message(format!(
                    "resolve: {} earlier lookups have not returned",
                    running.len()
                )));
            }
            let pending = Arc::new(Lookup::default());
            running.push(Running {
                host: host.to_owned(),
                port,
                lookup: pending.clone(),
            });
            pending
        };
        let resolver = self.clone();
        let worker = pending.clone();
        let owned_host = host.to_owned();
        std::thread::Builder::new()
            .name("quota-resolve".into())
            .spawn(move || {
                let result = lookup(&owned_host, port).map_err(|e| e.to_string());
                resolver.finish(&worker, result);
            })
            .map_err(|e| {
                self.finish(&pending, Err(e.to_string()));
                TransportError::Message(format!("resolve: {e}"))
            })?;
        Ok(pending)
    }

    fn finish(&self, pending: &Arc<Lookup>, result: LookupResult) {
        *lock(&pending.result) = Some(result);
        pending.done.notify_all();
        lock(&self.running).retain(|r| !Arc::ptr_eq(&r.lookup, pending));
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn https_get(
    url: &str,
    headers: &[(&str, &str)],
    timeout: Duration,
) -> Result<HttpResponse, TransportError> {
    let parsed = parse_https_url(url)?;
    let tcp = connect_within(&parsed.host, parsed.port, timeout)?;
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

/// Seconds to wait, clamped to [`MAX_RETRY_AFTER_SECS`] for both forms. A
/// numeric value too large for `u64` is still a (very long) wait.
fn parse_retry_after(value: &str, now: i64) -> Option<u64> {
    let seconds = if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
        value.parse::<u64>().unwrap_or(u64::MAX)
    } else {
        let target = parse_http_date(value, now)?;
        target.saturating_sub(now).max(0) as u64
    };
    Some(seconds.min(MAX_RETRY_AFTER_SECS))
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

    fn resolve_within<F>(timeout: Duration, lookup: F) -> Result<Vec<SocketAddr>, TransportError>
    where
        F: FnOnce() -> std::io::Result<Vec<SocketAddr>> + Send + 'static,
    {
        Arc::new(Resolver::new(MAX_OUTSTANDING_LOOKUPS)).resolve(
            "resolver.test",
            443,
            timeout,
            move |_, _| lookup(),
        )
    }

    #[test]
    fn greptile_7_a_stalled_lookup_ends_at_the_timeout_not_when_the_resolver_returns() {
        let started = Instant::now();
        let result = resolve_within(Duration::from_millis(100), || {
            std::thread::sleep(Duration::from_secs(5));
            Ok(Vec::new())
        });
        assert_eq!(
            result,
            Err(TransportError::Message("resolve: timed out".into()))
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn greptile_7_a_prompt_lookup_returns_its_addresses_and_its_error() {
        let addr: SocketAddr = "127.0.0.1:443".parse().unwrap();
        assert_eq!(
            resolve_within(Duration::from_secs(5), move || Ok(vec![addr])),
            Ok(vec![addr])
        );
        let failed = resolve_within(Duration::from_secs(5), || {
            Err(std::io::Error::other("no such host"))
        });
        assert_eq!(
            failed,
            Err(TransportError::Message("resolve: no such host".into()))
        );
    }

    /// A resolver that never answers until the test lets it, counting the
    /// lookups it was asked for and how many were blocked at once.
    #[derive(Clone, Default)]
    struct StalledDns {
        released: Arc<(Mutex<bool>, Condvar)>,
        started: Arc<std::sync::atomic::AtomicUsize>,
        live: Arc<std::sync::atomic::AtomicUsize>,
        most_live: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl StalledDns {
        fn lookup(&self) -> impl FnOnce(&str, u16) -> std::io::Result<Vec<SocketAddr>> + Send {
            use std::sync::atomic::Ordering::SeqCst;
            let dns = self.clone();
            move |_, port| {
                dns.started.fetch_add(1, SeqCst);
                let live = dns.live.fetch_add(1, SeqCst) + 1;
                dns.most_live.fetch_max(live, SeqCst);
                let (released, wake) = &*dns.released;
                drop(
                    wake.wait_while(lock(released), |released| !*released)
                        .unwrap_or_else(PoisonError::into_inner),
                );
                dns.live.fetch_sub(1, SeqCst);
                Ok(vec![SocketAddr::from(([127, 0, 0, 1], port))])
            }
        }

        fn release(&self) {
            *lock(&self.released.0) = true;
            self.released.1.notify_all();
        }

        fn started(&self) -> usize {
            self.started.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    fn wait_until_idle(resolver: &Resolver) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !lock(&resolver.running).is_empty() {
            assert!(Instant::now() < deadline, "workers did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn review_r2_repeated_resolver_timeouts_cannot_accumulate_worker_threads() {
        let limit = 2;
        let resolver = Arc::new(Resolver::new(limit));
        let dns = StalledDns::default();
        let short = Duration::from_millis(5);

        for _ in 0..50 {
            assert_eq!(
                resolver.resolve("stalled.test", 443, short, dns.lookup()),
                Err(TransportError::Message("resolve: timed out".into()))
            );
        }
        assert_eq!(dns.started(), 1, "one stalled host holds one worker");

        let mut refused = 0;
        for n in 0..50 {
            let started = Instant::now();
            let host = format!("host-{n}.test");
            match resolver.resolve(&host, 443, short, dns.lookup()) {
                Err(TransportError::Message(m)) if m.contains("have not returned") => {
                    assert!(started.elapsed() < Duration::from_secs(1));
                    refused += 1;
                }
                other => assert_eq!(
                    other,
                    Err(TransportError::Message("resolve: timed out".into()))
                ),
            }
        }
        assert_eq!(dns.started(), limit);
        assert_eq!(refused, 50 - (limit - 1));
        assert_eq!(lock(&resolver.running).len(), limit);

        dns.release();
        wait_until_idle(&resolver);
        assert!(dns.most_live.load(std::sync::atomic::Ordering::SeqCst) <= limit);
        assert_eq!(
            resolver.resolve("stalled.test", 8443, Duration::from_secs(5), dns.lookup()),
            Ok(vec![SocketAddr::from(([127, 0, 0, 1], 8443))])
        );
        assert_eq!(dns.started(), limit + 1, "a freed slot takes a new lookup");
    }

    #[test]
    fn review_r2_a_later_caller_waits_on_the_stalled_lookup_and_gets_its_answer() {
        let resolver = Arc::new(Resolver::new(MAX_OUTSTANDING_LOOKUPS));
        let dns = StalledDns::default();
        assert_eq!(
            resolver.resolve("slow.test", 443, Duration::from_millis(5), dns.lookup()),
            Err(TransportError::Message("resolve: timed out".into()))
        );

        let releaser = {
            let dns = dns.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                dns.release();
            })
        };
        let answer = resolver.resolve("slow.test", 443, Duration::from_secs(5), dns.lookup());
        releaser.join().unwrap();

        assert_eq!(answer, Ok(vec![SocketAddr::from(([127, 0, 0, 1], 443))]));
        assert_eq!(dns.started(), 1);
        wait_until_idle(&resolver);
    }

    #[test]
    fn review_r2_a_black_holed_first_address_leaves_time_for_the_next() {
        let silent: SocketAddr = "[2001:db8::1]:443".parse().unwrap();
        let reachable: SocketAddr = "192.0.2.1:443".parse().unwrap();
        let budget = Duration::from_millis(400);
        let mut tried = Vec::new();

        let connected = connect_any(
            &[silent, reachable],
            Instant::now() + budget,
            |addr, share| {
                tried.push((*addr, share));
                if *addr == silent {
                    std::thread::sleep(share);
                    return Err(std::io::Error::from(std::io::ErrorKind::TimedOut));
                }
                Ok(*addr)
            },
        );

        assert_eq!(connected, Ok(reachable));
        assert_eq!(tried.len(), 2);
        assert!(tried[0].1 <= budget / 2, "{tried:?}");
        assert!(tried[1].1 > Duration::ZERO, "{tried:?}");
    }

    #[test]
    fn greptile_7_connect_reaches_a_listening_local_port_within_the_budget() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(connect_within("127.0.0.1", port, Duration::from_secs(5)).is_ok());
    }

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
    fn coderabbit_retry_after_numeric_and_date_share_one_ceiling() {
        let now = 1_445_412_300;
        assert_eq!(parse_retry_after("0", now), Some(0));
        assert_eq!(parse_retry_after("86400", now), Some(MAX_RETRY_AFTER_SECS));
        assert_eq!(parse_retry_after("86401", now), Some(MAX_RETRY_AFTER_SECS));
        assert_eq!(
            parse_retry_after("99999999999999999999999", now),
            Some(MAX_RETRY_AFTER_SECS)
        );
        assert_eq!(
            parse_retry_after("Fri, 21 Oct 2095 07:28:00 GMT", now),
            Some(MAX_RETRY_AFTER_SECS)
        );
        assert_eq!(parse_retry_after("-5", now), None);
        let raw = b"HTTP/1.1 429 Too Many\r\nRetry-After: 31536000\r\nContent-Length: 0\r\n\r\n";
        assert_eq!(
            parse_http_response(raw).unwrap().retry_after_secs,
            Some(MAX_RETRY_AFTER_SECS)
        );
    }

    #[test]
    fn two_digit_years_pick_the_century_within_fifty_years() {
        let now = 1_445_412_300;
        assert_eq!(expand_year("15", now), Some(2015));
        assert_eq!(expand_year("99", now), Some(1999));
        assert_eq!(expand_year("2015", now), Some(2015));
    }
}
