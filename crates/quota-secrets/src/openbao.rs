//! OpenBao KV v2 client. Compiled only with the `openbao` feature.
//!
//! `https://` uses rustls 0.21 (webpki roots, plus optional PEM CAs from
//! `QUOTA_OPENBAO_CA_FILE`). `http://` is loopback-only unless
//! `QUOTA_OPENBAO_ALLOW_PLAINTEXT=1`. `docker-compose.dev.yml` is the HTTP
//! path. Tests dial an in-process listener; they do not start OpenBao.

use std::io::{Read, Write};
use std::net::{IpAddr, TcpStream};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rustls::{Certificate, ClientConfig, ClientConnection, RootCertStore, ServerName, StreamOwned};

use crate::types::{SecretRecord, SecretsBackend, SecretsError};
use crate::{
    ENV_OPENBAO_ADDR, ENV_OPENBAO_ALLOW_PLAINTEXT, ENV_OPENBAO_CA_FILE, ENV_OPENBAO_MOUNT,
    ENV_OPENBAO_PREFIX, ENV_OPENBAO_TOKEN,
};

const MAX_BAO_BODY: usize = 64 * 1024;
const MAX_PUT_BYTES: usize = 32 * 1024;

/// KV v2 client. `Debug` redacts the token.
#[derive(Clone)]
pub struct OpenBaoBackend {
    addr: String,
    token: String,
    mount: String,
    prefix: String,
    timeout: Duration,
    allow_plain: bool,
    tls: Arc<ClientConfig>,
}

impl std::fmt::Debug for OpenBaoBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenBaoBackend")
            .field("addr", &self.addr)
            .field("token", &"<redacted>")
            .field("mount", &self.mount)
            .field("prefix", &self.prefix)
            .field("timeout", &self.timeout)
            .field("allow_plain", &self.allow_plain)
            .finish()
    }
}

impl OpenBaoBackend {
    pub fn from_env() -> Result<Option<Self>, SecretsError> {
        let addr = match std::env::var(ENV_OPENBAO_ADDR) {
            Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
            _ => return Ok(None),
        };
        let token = std::env::var(ENV_OPENBAO_TOKEN).map_err(|_| {
            SecretsError::Config(format!("{ENV_OPENBAO_TOKEN} is required when addr is set"))
        })?;
        if token.trim().is_empty() {
            return Err(SecretsError::Config(format!(
                "{ENV_OPENBAO_TOKEN} is empty"
            )));
        }
        let mount = std::env::var(ENV_OPENBAO_MOUNT).unwrap_or_else(|_| "secret".into());
        let prefix = std::env::var(ENV_OPENBAO_PREFIX).unwrap_or_else(|_| "quota".into());
        let allow_plain = std::env::var(ENV_OPENBAO_ALLOW_PLAINTEXT)
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let extra_ca = match std::env::var(ENV_OPENBAO_CA_FILE) {
            Ok(path) if !path.trim().is_empty() => {
                Some(std::fs::read(path.trim()).map_err(|e| {
                    SecretsError::Config(format!("{ENV_OPENBAO_CA_FILE} unreadable: {e}"))
                })?)
            }
            _ => None,
        };
        Self::from_parts(
            addr,
            token,
            mount,
            prefix,
            allow_plain,
            extra_ca.as_deref(),
            Duration::from_secs(5),
        )
        .map(Some)
    }

    fn from_parts(
        addr: String,
        token: String,
        mount: String,
        prefix: String,
        allow_plain: bool,
        extra_ca_pem: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<Self, SecretsError> {
        let addr = addr.trim().trim_end_matches('/').to_string();
        if addr.contains('@') {
            return Err(SecretsError::Config(
                "OpenBao URL must not embed credentials".into(),
            ));
        }
        fence_plain_http(&addr, allow_plain)?;
        let mount = validate_logical_path(mount.trim().trim_matches('/'))?;
        let prefix = validate_logical_path(prefix.trim().trim_matches('/'))?;
        Ok(Self {
            addr,
            token: token.trim().to_string(),
            mount,
            prefix,
            timeout,
            allow_plain,
            tls: tls_config(extra_ca_pem)?,
        })
    }

    fn kv_path(&self, logical: &str) -> Result<String, SecretsError> {
        let logical = validate_logical_path(logical)?;
        if self.prefix.is_empty() {
            Ok(logical)
        } else if logical.is_empty() {
            Ok(self.prefix.clone())
        } else {
            Ok(format!("{}/{}", self.prefix, logical))
        }
    }

    fn kv_url(&self, logical: &str) -> Result<String, SecretsError> {
        Ok(format!(
            "{}/v1/{}/data/{}",
            self.addr,
            self.mount,
            self.kv_path(logical)?
        ))
    }
}

fn tls_config(extra_ca_pem: Option<&[u8]>) -> Result<Arc<ClientConfig>, SecretsError> {
    match extra_ca_pem {
        Some(pem) => build_tls_config(Some(pem)),
        None => {
            static CFG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
            if let Some(cfg) = CFG.get() {
                return Ok(Arc::clone(cfg));
            }
            let cfg = build_tls_config(None)?;
            Ok(Arc::clone(CFG.get_or_init(|| cfg)))
        }
    }
}

fn build_tls_config(extra_ca_pem: Option<&[u8]>) -> Result<Arc<ClientConfig>, SecretsError> {
    let mut roots = RootCertStore::empty();
    roots.add_trust_anchors(webpki_roots::TLS_SERVER_ROOTS.iter().map(|ta| {
        rustls::OwnedTrustAnchor::from_subject_spki_name_constraints(
            ta.subject,
            ta.spki,
            ta.name_constraints,
        )
    }));
    if let Some(pem) = extra_ca_pem {
        add_pem_certs(&mut roots, pem)?;
    }
    Ok(Arc::new(
        ClientConfig::builder()
            .with_safe_defaults()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ))
}

fn add_pem_certs(roots: &mut RootCertStore, pem: &[u8]) -> Result<(), SecretsError> {
    let mut reader = std::io::Cursor::new(pem);
    let ders = rustls_pemfile::certs(&mut reader)
        .map_err(|e| SecretsError::Config(format!("OpenBao CA PEM: {e}")))?;
    if ders.is_empty() {
        return Err(SecretsError::Config(
            "OpenBao CA file contained no certificates".into(),
        ));
    }
    for der in ders {
        roots
            .add(&Certificate(der))
            .map_err(|_| SecretsError::Config("OpenBao CA certificate rejected".into()))?;
    }
    Ok(())
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

/// Authority host without port. `[::1]:8200` → `::1`; `127.0.0.1:8200` → `127.0.0.1`.
fn host_from_authority(authority: &str) -> &str {
    if let Some(rest) = authority.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(authority);
    }
    authority.split(':').next().unwrap_or(authority)
}

fn fence_plain_http(addr: &str, allow_plain: bool) -> Result<(), SecretsError> {
    let rest = match addr.strip_prefix("http://") {
        Some(r) => r,
        None if addr.starts_with("https://") => return Ok(()),
        None => {
            return Err(SecretsError::Config(
                "OpenBao addr must be http:// or https://".into(),
            ))
        }
    };
    let authority = rest.split('/').next().unwrap_or(rest);
    let host = host_from_authority(authority);
    if is_loopback_host(host) || allow_plain {
        return Ok(());
    }
    Err(SecretsError::Config(format!(
        "plain HTTP OpenBao to {host} is refused (loopback only; set {ENV_OPENBAO_ALLOW_PLAINTEXT}=1 for local-dev)"
    )))
}

fn valid_segment(seg: &str) -> bool {
    !seg.is_empty()
        && seg != "."
        && seg != ".."
        && !seg.contains('\\')
        && seg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
}

fn validate_logical_path(path: &str) -> Result<String, SecretsError> {
    let p = path.trim().trim_start_matches('/');
    if p.is_empty() {
        return Ok(String::new());
    }
    let mut parts = Vec::new();
    for seg in p.split('/') {
        if !valid_segment(seg) {
            return Err(SecretsError::Config(format!(
                "invalid OpenBao path segment {seg:?}"
            )));
        }
        parts.push(seg);
    }
    Ok(parts.join("/"))
}

impl SecretsBackend for OpenBaoBackend {
    fn name(&self) -> &'static str {
        "openbao"
    }

    fn get(&self, path: &str) -> Result<Option<SecretRecord>, SecretsError> {
        let url = self.kv_url(path)?;
        let (status, body) = http_json(
            "GET",
            &url,
            &self.token,
            None,
            self.timeout,
            self.allow_plain,
            &self.tls,
        )?;
        if status == 404 {
            return Ok(None);
        }
        if !(200..300).contains(&status) {
            return Err(SecretsError::Unavailable(format!(
                "OpenBao GET HTTP {status}"
            )));
        }
        let v: serde_json::Value =
            serde_json::from_slice(&body).map_err(|e| SecretsError::Parse(e.to_string()))?;
        let data = v
            .pointer("/data/data")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let value = data
            .get("value")
            .or_else(|| data.get("material"))
            .and_then(|x| x.as_str())
            .map(|s| s.to_string());
        Ok(value.map(|value| SecretRecord {
            backend: "openbao",
            path: path.to_string(),
            value,
        }))
    }

    fn put(&self, path: &str, value: &str) -> Result<(), SecretsError> {
        if value.is_empty() {
            return Err(SecretsError::Config(
                "refusing to store an empty secret".into(),
            ));
        }
        if value.len() > MAX_PUT_BYTES {
            return Err(SecretsError::Config(
                "secret value exceeds 32 KiB (must fit the OpenBao response cap)".into(),
            ));
        }
        let url = self.kv_url(path)?;
        let payload = serde_json::json!({ "data": { "value": value } });
        let bytes = serde_json::to_vec(&payload).map_err(|e| SecretsError::Parse(e.to_string()))?;
        let (status, _) = http_json(
            "POST",
            &url,
            &self.token,
            Some(&bytes),
            self.timeout,
            self.allow_plain,
            &self.tls,
        )?;
        if !(200..300).contains(&status) {
            return Err(SecretsError::Unavailable(format!(
                "OpenBao POST HTTP {status}"
            )));
        }
        Ok(())
    }

    fn delete(&self, path: &str) -> Result<(), SecretsError> {
        let url = format!(
            "{}/v1/{}/metadata/{}",
            self.addr,
            self.mount,
            self.kv_path(path)?
        );
        let (status, _) = http_json(
            "DELETE",
            &url,
            &self.token,
            None,
            self.timeout,
            self.allow_plain,
            &self.tls,
        )?;
        if status == 404 || (200..300).contains(&status) {
            return Ok(());
        }
        Err(SecretsError::Unavailable(format!(
            "OpenBao DELETE HTTP {status}"
        )))
    }
}

#[derive(Debug, Clone)]
struct HttpUrl {
    tls: bool,
    host: String,
    port: u16,
    path: String,
}

fn parse_url(url: &str) -> Result<HttpUrl, SecretsError> {
    let (tls, rest) = if let Some(rest) = url.strip_prefix("https://") {
        (true, rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (false, rest)
    } else {
        return Err(SecretsError::Config(
            "OpenBao addr must be http:// or https://".into(),
        ));
    };
    if rest.contains('@') {
        return Err(SecretsError::Config(
            "OpenBao URL must not embed credentials".into(),
        ));
    }
    let (hostport, path) = match rest.split_once('/') {
        Some((h, p)) => (h, format!("/{p}")),
        None => (rest, "/".into()),
    };
    let (host, port) = split_host_port(hostport, if tls { 443 } else { 80 })?;
    if host.is_empty() {
        return Err(SecretsError::Config("empty OpenBao host".into()));
    }
    Ok(HttpUrl {
        tls,
        host,
        port,
        path,
    })
}

fn split_host_port(hostport: &str, default_port: u16) -> Result<(String, u16), SecretsError> {
    if let Some(rest) = hostport.strip_prefix('[') {
        let (host, after) = rest
            .split_once(']')
            .ok_or_else(|| SecretsError::Config("invalid OpenBao IPv6 host".into()))?;
        let port = if after.is_empty() {
            default_port
        } else {
            after
                .strip_prefix(':')
                .ok_or_else(|| SecretsError::Config("invalid OpenBao IPv6 port".into()))?
                .parse::<u16>()
                .map_err(|_| SecretsError::Config("invalid OpenBao port".into()))?
        };
        return Ok((host.to_string(), port));
    }
    if let Some((h, p)) = hostport.rsplit_once(':') {
        if !h.is_empty() && !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) {
            let port = p
                .parse::<u16>()
                .map_err(|_| SecretsError::Config("invalid OpenBao port".into()))?;
            return Ok((h.to_string(), port));
        }
    }
    Ok((hostport.to_string(), default_port))
}

fn host_header(host: &str, port: u16, tls: bool) -> String {
    let bracketed = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    let default_port = if tls { 443 } else { 80 };
    if port == default_port {
        bracketed
    } else {
        format!("{bracketed}:{port}")
    }
}

fn server_name(host: &str) -> Result<ServerName, SecretsError> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(ServerName::IpAddress(ip));
    }
    ServerName::try_from(host).map_err(|_| SecretsError::Config("invalid OpenBao TLS name".into()))
}

fn http_json(
    method: &str,
    url: &str,
    token: &str,
    body: Option<&[u8]>,
    timeout: Duration,
    allow_plain: bool,
    tls: &Arc<ClientConfig>,
) -> Result<(u16, Vec<u8>), SecretsError> {
    let parsed = parse_url(url)?;
    let tcp = TcpStream::connect((parsed.host.as_str(), parsed.port))
        .map_err(|e| SecretsError::Io(format!("connect: {e}")))?;
    tcp.set_read_timeout(Some(timeout))
        .and_then(|_| tcp.set_write_timeout(Some(timeout)))
        .map_err(|e| SecretsError::Io(e.to_string()))?;
    if parsed.tls {
        let name = server_name(&parsed.host)?;
        let conn = ClientConnection::new(Arc::clone(tls), name)
            .map_err(|_| SecretsError::Io("OpenBao TLS handshake setup failed".into()))?;
        let mut stream = StreamOwned::new(conn, tcp);
        exchange(&mut stream, method, &parsed, token, body)
    } else {
        let fence = format!("http://{}", host_header(&parsed.host, parsed.port, false));
        fence_plain_http(&fence, allow_plain)?;
        let mut stream = tcp;
        exchange(&mut stream, method, &parsed, token, body)
    }
}

fn exchange<S: Read + Write>(
    stream: &mut S,
    method: &str,
    parsed: &HttpUrl,
    token: &str,
    body: Option<&[u8]>,
) -> Result<(u16, Vec<u8>), SecretsError> {
    let host = host_header(&parsed.host, parsed.port, parsed.tls);
    let mut req = format!(
        "{method} {} HTTP/1.1\r\nHost: {host}\r\nX-Vault-Token: {token}\r\nAccept: application/json\r\nConnection: close\r\n",
        parsed.path
    );
    if let Some(b) = body {
        req.push_str("Content-Type: application/json\r\n");
        req.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    req.push_str("\r\n");
    stream
        .write_all(req.as_bytes())
        .map_err(|e| SecretsError::Io(e.to_string()))?;
    if let Some(b) = body {
        stream
            .write_all(b)
            .map_err(|e| SecretsError::Io(e.to_string()))?;
    }
    stream
        .flush()
        .map_err(|e| SecretsError::Io(e.to_string()))?;

    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if raw.len() + n > MAX_BAO_BODY + 8192 {
                    return Err(SecretsError::Io("OpenBao response too large".into()));
                }
                raw.extend_from_slice(&buf[..n]);
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof && !raw.is_empty() => break,
            Err(e) => return Err(SecretsError::Io(e.to_string())),
        }
    }
    parse_status_body(&raw)
}

fn parse_status_body(raw: &[u8]) -> Result<(u16, Vec<u8>), SecretsError> {
    let sep = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| SecretsError::Parse("truncated HTTP response".into()))?;
    let headers = std::str::from_utf8(&raw[..sep])
        .map_err(|_| SecretsError::Parse("non-utf8 HTTP headers".into()))?;
    let status = headers
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| SecretsError::Parse("bad status line".into()))?;
    let mut body = raw[sep + 4..].to_vec();
    if body.len() > MAX_BAO_BODY {
        return Err(SecretsError::Io("OpenBao response too large".into()));
    }
    if let Some(n) = content_length(headers) {
        body.truncate(n.min(MAX_BAO_BODY));
    }
    Ok((status, body))
}

fn content_length(headers: &str) -> Option<usize> {
    headers.lines().find_map(|line| {
        let (k, v) = line.split_once(':')?;
        if k.eq_ignore_ascii_case("content-length") {
            v.trim().parse().ok()
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;
    use std::time::Duration;

    fn test_backend(addr: &str) -> OpenBaoBackend {
        OpenBaoBackend::from_parts(
            addr.into(),
            "dev-token-not-logged".into(),
            "secret".into(),
            "quota".into(),
            false,
            None,
            Duration::from_secs(2),
        )
        .unwrap()
    }

    fn read_request(sock: &mut TcpStream) -> Vec<u8> {
        sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut raw = Vec::new();
        let mut buf = [0u8; 2048];
        loop {
            match sock.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    raw.extend_from_slice(&buf[..n]);
                    if let Some(sep) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = std::str::from_utf8(&raw[..sep]).unwrap_or("");
                        let cl = content_length(headers).unwrap_or(0);
                        if raw.len() >= sep + 4 + cl {
                            break;
                        }
                    }
                }
                Err(_) => break,
            }
        }
        raw
    }

    fn serve_once(status: u16, body: &str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let body = body.to_string();
        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let req = read_request(&mut sock);
            assert!(
                req.windows(b"X-Vault-Token: dev-token-not-logged\r\n".len())
                    .any(|window| window == b"X-Vault-Token: dev-token-not-logged\r\n"),
                "token header missing"
            );
            let resp = format!(
                "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes());
        });
        format!("http://{addr}")
    }

    #[test]
    fn from_env_absent_is_none() {
        let addr = std::env::var(ENV_OPENBAO_ADDR).unwrap_or_default();
        if addr.trim().is_empty() {
            assert!(OpenBaoBackend::from_env().unwrap().is_none());
        }
    }

    #[test]
    fn https_url_defaults_to_443() {
        let u = parse_url("https://bao.example/v1/secret/data/x").unwrap();
        assert!(u.tls);
        assert_eq!(u.host, "bao.example");
        assert_eq!(u.port, 443);
        assert_eq!(u.path, "/v1/secret/data/x");
    }

    #[test]
    fn http_url_splits() {
        let u = parse_url("http://127.0.0.1:8200/v1/secret/data/quota/k").unwrap();
        assert!(!u.tls);
        assert_eq!(u.host, "127.0.0.1");
        assert_eq!(u.port, 8200);
        assert_eq!(u.path, "/v1/secret/data/quota/k");
        let v6 = parse_url("http://[::1]:8200/v1/secret/data/x").unwrap();
        assert_eq!(v6.host, "::1");
        assert_eq!(v6.port, 8200);
    }

    #[test]
    fn kv_path_prefixes() {
        let b = test_backend("http://127.0.0.1:8200");
        assert_eq!(b.kv_path("codex/work").unwrap(), "quota/codex/work");
        assert_eq!(
            b.kv_url("codex/work").unwrap(),
            "http://127.0.0.1:8200/v1/secret/data/quota/codex/work"
        );
    }

    #[test]
    fn rejects_parent_dir_path() {
        let b = test_backend("http://127.0.0.1:8200");
        assert!(b.kv_path("../other").is_err());
        assert!(b.kv_path("a/../../b").is_err());
        assert!(b.kv_path("ok/name").is_ok());
    }

    #[test]
    fn fence_non_loopback_http() {
        let err = fence_plain_http("http://evil.example:8200", false).unwrap_err();
        assert!(matches!(err, SecretsError::Config(_)));
        assert!(fence_plain_http("http://127.0.0.1:8200", false).is_ok());
        assert!(fence_plain_http("http://localhost:8200", false).is_ok());
        assert!(fence_plain_http("http://[::1]:8200", false).is_ok());
        assert!(fence_plain_http("http://evil.example:8200", true).is_ok());
        assert!(fence_plain_http("https://bao.example", false).is_ok());
        assert!(fence_plain_http("http://127.0.0.1.evil.example:8200", false).is_err());
        assert!(OpenBaoBackend::from_parts(
            "http://user:pass@127.0.0.1:8200".into(),
            "t".into(),
            "secret".into(),
            "quota".into(),
            true,
            None,
            Duration::from_secs(1),
        )
        .is_err());
    }

    #[test]
    fn nested_prefix_is_allowed() {
        let p = validate_logical_path("quota/prod").unwrap();
        assert_eq!(p, "quota/prod");
        assert!(validate_logical_path("quota/../etc").is_err());
    }

    #[test]
    fn debug_redacts_token() {
        let b = test_backend("http://127.0.0.1:8200");
        let rendered = format!("{b:?}");
        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains("dev-token-not-logged"));
    }

    #[test]
    fn mock_http_get_put_delete() {
        let material = "mock-secret-value";
        let get_addr = serve_once(
            200,
            &format!(r#"{{"data":{{"data":{{"value":"{material}"}}}}}}"#),
        );
        let backend = test_backend(&get_addr);
        let rec = backend.get("codex/work").unwrap().unwrap();
        assert_eq!(rec.backend, "openbao");
        assert_eq!(rec.value, material);
        assert!(!format!("{rec:?}").contains(material));

        let missing = serve_once(404, "");
        let backend = test_backend(&missing);
        assert!(backend.get("codex/missing").unwrap().is_none());

        let put_addr = serve_once(204, "");
        let backend = test_backend(&put_addr);
        backend.put("codex/work", material).unwrap();
        assert!(backend.put("codex/work", "").is_err());

        let del_addr = serve_once(204, "");
        let backend = test_backend(&del_addr);
        backend.delete("codex/work").unwrap();
    }

    /// Throwaway CA for the in-process listener. Not a public root.
    const TEST_CA_PEM: &str = "\
-----BEGIN CERTIFICATE-----
MIIBhjCCASugAwIBAgIULYfPHIAG4+/j2M7qG9FDnMMsOn4wCgYIKoZIzj0EAwIw
GDEWMBQGA1UEAwwNcXVvdGEtdGVzdC1jYTAeFw0yNjA5MjcxMzA2NDdaFw0zNjA5
MjQxMzA2NDdaMBgxFjAUBgNVBAMMDXF1b3RhLXRlc3QtY2EwWTATBgcqhkjOPQIB
BggqhkjOPQMBBwNCAARuZOZSihFZFqQWmWYPtjvAEgk6nnD70Ef0U3Hy9HmLx+oN
0bhUaC5LcLdZ4uYpIUAscTnTCM85FDXe+85dLIsyo1MwUTAdBgNVHQ4EFgQU/c8m
qN4F35+IraXAdymk7G6FTJgwHwYDVR0jBBgwFoAU/c8mqN4F35+IraXAdymk7G6F
TJgwDwYDVR0TAQH/BAUwAwEB/zAKBggqhkjOPQQDAgNJADBGAiEAlxTny5BuFwWY
jLQiLuaIvkpob3KjFaJpMyPRIaEBgvgCIQDf583jAZQaE2mVhjwO9tWZTKtEQQcB
kgrJwTu7OAMKqw==
-----END CERTIFICATE-----
";

    /// Leaf for `localhost`, signed by `TEST_CA_PEM`. Not CA:TRUE.
    const TEST_LEAF_PEM: &str = "\
-----BEGIN CERTIFICATE-----
MIIBhjCCASygAwIBAgIUa0vyCEDbAxD70hNHsX6sp7OU6r0wCgYIKoZIzj0EAwIw
GDEWMBQGA1UEAwwNcXVvdGEtdGVzdC1jYTAeFw0yNjA5MjcxMzA2NDdaFw0zNjA5
MjQxMzA2NDdaMBQxEjAQBgNVBAMMCWxvY2FsaG9zdDBZMBMGByqGSM49AgEGCCqG
SM49AwEHA0IABD3JJPGXYG199Sx5QjWbIFvzG9DPcprgSTrEQC4RyK+rAw7EwbVT
gEE78otHNzsK58JXq6zHzRRU9THC4GrJDyejWDBWMBQGA1UdEQQNMAuCCWxvY2Fs
aG9zdDAdBgNVHQ4EFgQUJw+o/qdGQNpRMY03cTLUupQHTuQwHwYDVR0jBBgwFoAU
/c8mqN4F35+IraXAdymk7G6FTJgwCgYIKoZIzj0EAwIDSAAwRQIhAKLUMwq/2gEP
scBCyursxcxEsmyJU4xmZaPaDWHJQgGMAiAkexZtPV1RLEMVbK9XdYy8Z7jbzePA
ytL+V/a58h+2jg==
-----END CERTIFICATE-----
";

    /// PKCS#8 P-256 key matching `TEST_LEAF_PEM`. In-process test server only.
    const TEST_LEAF_KEY_PEM: &str = "\
-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgxus3NZACwzx8ejNe
G+6g5ie+uy0wtVulCljjfzkOKpShRANCAAQ9ySTxl2BtffUseUI1myBb8xvQz3Ka
4Ek6xEAuEcivqwMOxMG1U4BBO/KLRzc7CufCV6usx80UVPUxwuBqyQ8n
-----END PRIVATE KEY-----
";

    #[test]
    fn rustls_localhost_roundtrip() {
        let mut roots = RootCertStore::empty();
        add_pem_certs(&mut roots, TEST_CA_PEM.as_bytes()).unwrap();
        let mut cert_reader = std::io::Cursor::new(TEST_LEAF_PEM.as_bytes());
        let cert_der = rustls_pemfile::certs(&mut cert_reader)
            .unwrap()
            .into_iter()
            .next()
            .expect("cert");
        let mut key_reader = std::io::Cursor::new(TEST_LEAF_KEY_PEM.as_bytes());
        let key_der = rustls_pemfile::pkcs8_private_keys(&mut key_reader)
            .unwrap()
            .into_iter()
            .next()
            .expect("key");
        let client_cfg = Arc::new(
            ClientConfig::builder()
                .with_safe_defaults()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        );
        let server_cfg = Arc::new(
            rustls::ServerConfig::builder()
                .with_safe_defaults()
                .with_no_client_auth()
                .with_single_cert(vec![Certificate(cert_der)], rustls::PrivateKey(key_der))
                .unwrap(),
        );

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = r#"{"data":{"data":{"value":"tls-material"}}}"#;
        let (err_tx, err_rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let send_err = |msg: String| {
                let _ = err_tx.send(msg);
            };
            let (tcp, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(err) => return send_err(format!("accept: {err}")),
            };
            if let Err(err) = tcp.set_read_timeout(Some(Duration::from_secs(2))) {
                return send_err(format!("timeout: {err}"));
            }
            if let Err(err) = tcp.set_write_timeout(Some(Duration::from_secs(2))) {
                return send_err(format!("timeout: {err}"));
            }
            let conn = match rustls::ServerConnection::new(server_cfg) {
                Ok(conn) => conn,
                Err(err) => return send_err(format!("server tls: {err}")),
            };
            let mut stream = StreamOwned::new(conn, tcp);
            let mut raw = Vec::new();
            let mut buf = [0u8; 2048];
            loop {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        raw.extend_from_slice(&buf[..n]);
                        if raw.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    Err(err) => return send_err(format!("server read: {err}")),
                }
            }
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            if let Err(err) = stream
                .write_all(resp.as_bytes())
                .and_then(|()| stream.flush())
            {
                return send_err(format!("server write: {err}"));
            }
            let _ = err_tx.send(String::new());
        });

        let backend = OpenBaoBackend {
            addr: format!("https://localhost:{port}"),
            token: "tls-token-not-logged".into(),
            mount: "secret".into(),
            prefix: "quota".into(),
            timeout: Duration::from_secs(2),
            allow_plain: false,
            tls: client_cfg,
        };
        let got = backend.get("codex/work");
        let server_msg = err_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or_else(|err| err.to_string());
        let rec = got
            .unwrap_or_else(|err| panic!("client {err}; server [{server_msg}]"))
            .unwrap();
        assert!(server_msg.is_empty(), "{server_msg}");
        assert_eq!(rec.value, "tls-material");
        assert!(!format!("{backend:?}").contains("tls-token-not-logged"));
    }
}
