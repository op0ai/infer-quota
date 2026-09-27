//! Offline socket hotpath: framing, ACL, concurrent clients, fixture ingest,
//! and "no credentials on the wire". Spawns the `quotad` binary. No live HTTPS.

#![cfg(unix)]

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use quota_core::framing::{decode_len, encode_frame, read_frame, write_frame, MAX_FRAME_BYTES};
use quota_core::protocol::{
    CanStartResult, Request, Response, StatusResult, METHOD_ACCOUNTS_ADD, METHOD_ACCOUNTS_LIST,
    METHOD_CAN_START, METHOD_PACE, METHOD_PING, METHOD_STATUS, METHOD_WATCH,
};
use quota_core::types::{Availability, CanStartBasis, ProviderId};

struct Daemon {
    child: Child,
    dir: PathBuf,
    socket: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/codexbar")
}

fn spawn_daemon(enable_codexbar: bool) -> Daemon {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("quota-hotpath-{stamp}"));
    fs::create_dir_all(&dir).unwrap();
    let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    let socket = dir.join("quota.sock");
    let home = dir.join("home");
    fs::create_dir_all(&home).unwrap();
    let cfg_path = dir.join("config.json");
    let mut cfg = serde_json::json!({
        "history": false,
        "ring_capacity": 32,
        "refresh_min_secs": 3600,
        "refresh_max_secs": 3600,
        "http_timeout_secs": 1,
        "enable_codex": true,
        "enable_claude": true,
        "enable_codexbar_files": enable_codexbar,
        "accounts_path": dir.join("accounts.json").to_string_lossy(),
    });
    if enable_codexbar {
        cfg["codexbar_dir"] = serde_json::json!(fixtures_dir().to_string_lossy());
    } else {
        cfg["codexbar_dir"] = serde_json::json!(dir.join("empty-codexbar").to_string_lossy());
    }
    fs::write(&cfg_path, serde_json::to_vec_pretty(&cfg).unwrap()).unwrap();

    let bin = env!("CARGO_BIN_EXE_quotad");
    let child = Command::new(bin)
        .arg("--config")
        .arg(&cfg_path)
        .arg("--socket")
        .arg(&socket)
        .arg("run")
        .env("HOME", &home)
        .env("CODEX_HOME", home.join("no-codex"))
        .env("CLAUDE_CONFIG_DIR", home.join("no-claude"))
        .env("XDG_CONFIG_HOME", home.join("xdg-config"))
        .env("XDG_STATE_HOME", home.join("xdg-state"))
        .env("XDG_RUNTIME_DIR", &dir)
        .env_remove("QUOTA_SOCKET")
        .env("QUOTA_WATCH_IDLE_SECS", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn quotad");

    let daemon = Daemon { child, dir, socket };
    wait_ping(&daemon.socket, Duration::from_secs(8)).expect("quotad did not become ready");
    daemon
}

fn wait_ping(socket: &Path, timeout: Duration) -> Option<()> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Ok(mut s) = UnixStream::connect(socket) {
            let req = Request::new(1, METHOD_PING);
            if let Ok(bytes) = serde_json::to_vec(&req) {
                if write_frame(&mut s, &bytes).is_ok() {
                    if let Ok(payload) = read_frame(&mut s) {
                        if serde_json::from_slice::<Response>(&payload)
                            .map(|r| r.ok)
                            .unwrap_or(false)
                        {
                            return Some(());
                        }
                    }
                }
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
    None
}

fn rpc(socket: &Path, id: u64, method: &str, params: serde_json::Value) -> Response {
    let mut stream = UnixStream::connect(socket).expect("connect");
    let req = Request::with_params(id, method, params);
    let bytes = serde_json::to_vec(&req).unwrap();
    write_frame(&mut stream, &bytes).unwrap();
    let payload = read_frame(&mut stream).unwrap();
    serde_json::from_slice(&payload).unwrap()
}

fn assert_no_secrets(blob: &str) {
    for needle in [
        "WorkosCursorSessionToken",
        "access_token",
        "refresh_token",
        "sk-ant-",
        "sk-proj-",
        "eyJhbGci",
        "Bearer ",
        "password",
        "authFingerprint",
    ] {
        assert!(
            !blob.contains(needle),
            "socket JSON leaked {needle}: {blob}"
        );
    }
}

#[test]
fn socket_is_owner_only() {
    let d = spawn_daemon(false);
    let meta = fs::metadata(&d.socket).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    let parent = d.socket.parent().unwrap();
    let pmeta = fs::metadata(parent).unwrap();
    assert_eq!(pmeta.permissions().mode() & 0o777, 0o700);
}

#[test]
fn oversized_frame_is_rejected() {
    let d = spawn_daemon(false);
    let mut s = UnixStream::connect(&d.socket).unwrap();
    let header = ((MAX_FRAME_BYTES as u32) + 1).to_le_bytes();
    s.write_all(&header).unwrap();
    s.write_all(&[0u8; 16]).unwrap();
    let _ = s.flush();
    let mut buf = [0u8; 8];
    let n = s.read(&mut buf).unwrap_or(0);
    assert_eq!(n, 0, "daemon must close an oversized frame, not reply");
}

#[test]
fn truncated_length_prefix_closes() {
    let d = spawn_daemon(false);
    let mut s = UnixStream::connect(&d.socket).unwrap();
    s.write_all(&[1u8, 0]).unwrap();
    s.shutdown(std::net::Shutdown::Write).unwrap();
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf);
    assert!(buf.is_empty());
}

#[test]
fn concurrent_clients_ping_and_status() {
    let d = spawn_daemon(false);
    let socket = d.socket.clone();
    let mut handles = Vec::new();
    for c in 0..8 {
        let path = socket.clone();
        handles.push(thread::spawn(move || {
            let mut stream = UnixStream::connect(&path).unwrap();
            for i in 0..20 {
                let req = Request::new((c * 100 + i) as u64, METHOD_PING);
                write_frame(&mut stream, &serde_json::to_vec(&req).unwrap()).unwrap();
                let payload = read_frame(&mut stream).unwrap();
                let resp: Response = serde_json::from_slice(&payload).unwrap();
                assert!(resp.ok);
            }
        }));
    }
    for h in handles {
        h.join().expect("client thread");
    }
    let status = rpc(&d.socket, 9_001, METHOD_STATUS, serde_json::json!({}));
    assert!(status.ok);
    let result: StatusResult =
        serde_json::from_value(status.result.clone().expect("status result")).unwrap();
    assert_eq!(result.snapshot.providers.len(), 2);
    for p in &result.snapshot.providers {
        assert_eq!(p.status, Availability::Unavailable);
        assert!(p.windows.is_empty());
    }
    assert_no_secrets(&serde_json::to_string(&status).unwrap());
}

#[test]
fn fixture_ingest_and_percent_only_can_start() {
    let d = spawn_daemon(true);
    let status = rpc(&d.socket, 1, METHOD_STATUS, serde_json::json!({}));
    let result: StatusResult =
        serde_json::from_value(status.result.clone().expect("status result")).unwrap();
    let codex = result
        .snapshot
        .by_id(ProviderId::Codex)
        .expect("codex present");
    assert_eq!(codex.status, Availability::Ok);
    assert_eq!(codex.windows[0].used_percent, Some(59.0));
    let claude = result
        .snapshot
        .by_id(ProviderId::Claude)
        .expect("claude present");
    assert_eq!(claude.status, Availability::Unavailable);

    let can = rpc(
        &d.socket,
        2,
        METHOD_CAN_START,
        serde_json::json!({ "tokens": 50_000, "provider": "codex" }),
    );
    let answers: CanStartResult =
        serde_json::from_value(can.result.clone().expect("can_start result")).unwrap();
    assert!(!answers.ok);
    assert_eq!(answers.answers[0].basis, CanStartBasis::PercentOnly);
    assert!(answers.answers[0].explanation.contains("Cannot map"));

    let pace = rpc(
        &d.socket,
        3,
        METHOD_PACE,
        serde_json::json!({ "provider": "codex" }),
    );
    assert!(pace.ok);
    assert_no_secrets(&serde_json::to_string(&status).unwrap());
    assert_no_secrets(&serde_json::to_string(&can).unwrap());
    assert_no_secrets(&serde_json::to_string(&pace).unwrap());
}

#[test]
fn accounts_rpc_is_metadata_only() {
    let d = spawn_daemon(false);
    let stuffed = serde_json::json!({
        "id": "acct_hot",
        "provider": "codex",
        "email": "openai@ctx.op0.dev",
        "access_token": "sk-ant-secret-wire",
        "refresh_token": "rt-wire",
        "secret_ref": { "backend": "openbao", "path": "quota/codex/work" },
        "select": true
    });
    let add = rpc(&d.socket, 40, METHOD_ACCOUNTS_ADD, stuffed);
    assert!(add.ok, "{add:?}");
    let listed = rpc(&d.socket, 41, METHOD_ACCOUNTS_LIST, serde_json::json!({}));
    let blob = serde_json::to_string(&listed).unwrap();
    assert!(blob.contains("acct_hot"));
    assert!(blob.contains("quota/codex/work"));
    assert_no_secrets(&blob);
    assert!(!blob.contains("sk-ant-secret-wire"));
    assert!(!blob.contains("rt-wire"));
}

#[test]
fn decode_len_helper_matches_daemon_cap() {
    assert!(decode_len(((MAX_FRAME_BYTES as u32) + 1).to_le_bytes()).is_err());
    let frame = encode_frame(br#"{"id":1,"method":"ping"}"#).unwrap();
    assert_eq!(&frame[..4], &(frame.len() as u32 - 4).to_le_bytes());
}

#[test]
fn watch_subscribe_then_idle_timeout() {
    let d = spawn_daemon(false);
    let mut s = UnixStream::connect(&d.socket).unwrap();
    let req = Request::new(70, METHOD_WATCH);
    write_frame(&mut s, &serde_json::to_vec(&req).unwrap()).unwrap();
    let first = read_frame(&mut s).expect("watch first frame");
    let resp: Response = serde_json::from_slice(&first).unwrap();
    assert!(resp.ok);
    // Daemon refresh is 3600s; idle override is 1s. No further snapshots.
    thread::sleep(Duration::from_millis(1500));
    let mut buf = Vec::new();
    let n = s.read_to_end(&mut buf).unwrap_or(0);
    assert_eq!(n, 0, "watch slot must drop after idle timeout, got {buf:?}");
}
