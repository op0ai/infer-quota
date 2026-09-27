//! Optional OpenBao KV v2 client (HTTP).
//!
//! Configured only when `QUOTA_OPENBAO_ADDR` and `QUOTA_OPENBAO_TOKEN` are set.
//! Default tests never construct a live client. HTTPS is accepted as a URL
//! scheme but the scaffold speaks **plain HTTP** (docker-compose.dev.yml).
//! Talk TLS through a local proxy if you need it.
//!
//! Plain HTTP is fenced to loopback (`127.0.0.1`, `localhost`, `::1`) unless
//! `QUOTA_OPENBAO_ALLOW_PLAINTEXT=1` is set. Logical KV paths reject `..`.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use crate::types::{SecretRecord, SecretsBackend, SecretsError};
use crate::{
    ENV_OPENBAO_ADDR, ENV_OPENBAO_ALLOW_PLAINTEXT, ENV_OPENBAO_MOUNT, ENV_OPENBAO_PREFIX,
    ENV_OPENBAO_TOKEN,
};

const MAX_BAO_BODY: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct OpenBaoBackend {
    pub addr: String,
    pub token: String,
    pub mount: String,
    pub prefix: String,
    pub timeout: Duration,
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
        let addr = addr.trim_end_matches('/').to_string();
        fence_plain_http(&addr, allow_plain)?;
        let mount = validate_logical_path(mount.trim().trim_matches('/'))?;
        let prefix = validate_logical_path(prefix.trim().trim_matches('/'))?;
        Ok(Some(Self {
            addr,
            token: token.trim().to_string(),
            mount,
            prefix,
            timeout: Duration::from_secs(5),
        }))
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
        None => return Ok(()),
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
        let (status, body) = http_json("GET", &url, &self.token, None, self.timeout)?;
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
        if value.len() > 32 * 1024 {
            return Err(SecretsError::Config(
                "secret value exceeds 32 KiB (must fit the OpenBao response cap)".into(),
            ));
        }
        let url = self.kv_url(path)?;
        let payload = serde_json::json!({ "data": { "value": value } });
        let bytes = serde_json::to_vec(&payload).map_err(|e| SecretsError::Parse(e.to_string()))?;
        let (status, _) = http_json("POST", &url, &self.token, Some(&bytes), self.timeout)?;
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
        let (status, _) = http_json("DELETE", &url, &self.token, None, self.timeout)?;
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
    host: String,
    port: u16,
    path: String,
}

fn parse_http_url(url: &str) -> Result<HttpUrl, SecretsError> {
    if url.starts_with("https://") {
        return Err(SecretsError::Unimplemented(
            "OpenBao scaffold is HTTP-only (use docker-compose.dev.yml or a local TLS proxy)"
                .into(),
        ));
    }
    let rest = url.strip_prefix("http://").ok_or_else(|| {
        SecretsError::Config("OpenBao addr must be http://… for the scaffold".into())
    })?;
    let (hostport, path) = match rest.split_once('/') {
        Some((h, p)) => (h, format!("/{p}")),
        None => (rest, "/".into()),
    };
    let (host, port) = split_host_port(hostport)?;
    Ok(HttpUrl { host, port, path })
}

fn split_host_port(hostport: &str) -> Result<(String, u16), SecretsError> {
    if let Some(rest) = hostport.strip_prefix('[') {
        let (host, after) = rest
            .split_once(']')
            .ok_or_else(|| SecretsError::Config("invalid OpenBao IPv6 host".into()))?;
        let port = if after.is_empty() {
            80
        } else {
            after
                .strip_prefix(':')
                .ok_or_else(|| SecretsError::Config("invalid OpenBao IPv6 port".into()))?
                .parse::<u16>()
                .map_err(|_| SecretsError::Config("invalid OpenBao port".into()))?
        };
        return Ok((host.to_string(), port));
    }
    if let Some((h, p)) = hostport.split_once(':') {
        let port = p
            .parse::<u16>()
            .map_err(|_| SecretsError::Config("invalid OpenBao port".into()))?;
        Ok((h.to_string(), port))
    } else {
        Ok((hostport.to_string(), 80))
    }
}

fn http_json(
    method: &str,
    url: &str,
    token: &str,
    body: Option<&[u8]>,
    timeout: Duration,
) -> Result<(u16, Vec<u8>), SecretsError> {
    let parsed = parse_http_url(url)?;
    let mut stream = TcpStream::connect((parsed.host.as_str(), parsed.port))
        .map_err(|e| SecretsError::Io(format!("connect: {e}")))?;
    stream
        .set_read_timeout(Some(timeout))
        .and_then(|_| stream.set_write_timeout(Some(timeout)))
        .map_err(|e| SecretsError::Io(e.to_string()))?;

    let mut req = format!(
        "{method} {} HTTP/1.1\r\nHost: {}\r\nX-Vault-Token: {token}\r\nAccept: application/json\r\nConnection: close\r\n",
        parsed.path, parsed.host
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
    Ok((status, raw[sep + 4..].to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_env_absent_is_none() {
        // Parallel tests share process env; only assert the constructor is safe
        // when addr is empty. We do not mutate env here.
        let addr = std::env::var(ENV_OPENBAO_ADDR).unwrap_or_default();
        if addr.trim().is_empty() {
            assert!(OpenBaoBackend::from_env().unwrap().is_none());
        }
    }

    #[test]
    fn https_rejected() {
        let err = parse_http_url("https://bao.example/v1/secret/data/x").unwrap_err();
        assert!(matches!(err, SecretsError::Unimplemented(_)));
    }

    #[test]
    fn http_url_splits() {
        let u = parse_http_url("http://127.0.0.1:8200/v1/secret/data/quota/k").unwrap();
        assert_eq!(u.host, "127.0.0.1");
        assert_eq!(u.port, 8200);
        assert_eq!(u.path, "/v1/secret/data/quota/k");
        let v6 = parse_http_url("http://[::1]:8200/v1/secret/data/x").unwrap();
        assert_eq!(v6.host, "::1");
        assert_eq!(v6.port, 8200);
    }

    #[test]
    fn kv_path_prefixes() {
        let b = OpenBaoBackend {
            addr: "http://127.0.0.1:8200".into(),
            token: "dev".into(),
            mount: "secret".into(),
            prefix: "quota".into(),
            timeout: Duration::from_secs(1),
        };
        assert_eq!(b.kv_path("codex/work").unwrap(), "quota/codex/work");
        assert_eq!(
            b.kv_url("codex/work").unwrap(),
            "http://127.0.0.1:8200/v1/secret/data/quota/codex/work"
        );
    }

    #[test]
    fn rejects_parent_dir_path() {
        let b = OpenBaoBackend {
            addr: "http://127.0.0.1:8200".into(),
            token: "dev".into(),
            mount: "secret".into(),
            prefix: "quota".into(),
            timeout: Duration::from_secs(1),
        };
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
        assert!(fence_plain_http("http://127.0.0.1.evil.example:8200", false).is_err());
    }

    #[test]
    fn nested_prefix_is_allowed() {
        let p = validate_logical_path("quota/prod").unwrap();
        assert_eq!(p, "quota/prod");
        assert!(validate_logical_path("quota/../etc").is_err());
    }
}
