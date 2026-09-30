//! Offline socket hotpath: framing, ACL, concurrent clients, fixture ingest,
//! and "no credentials on the wire". Spawns the `quotad` binary. No live HTTPS.

#![cfg(unix)]

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use quota_core::framing::{decode_len, encode_frame, read_frame, write_frame, MAX_FRAME_BYTES};
use quota_core::protocol::{
    CanStartResult, Request, Response, StatusResult, METHOD_ACCOUNTS_ADD, METHOD_ACCOUNTS_LIST,
    METHOD_CAN_START, METHOD_OBSERVE, METHOD_PACE, METHOD_PING, METHOD_REFRESH, METHOD_STATUS,
    METHOD_WATCH,
};
use quota_core::types::{Availability, CanStartBasis, Freshness, ProviderId, Source};

struct Daemon {
    child: Child,
    _temp_dir: tempfile::TempDir,
    socket: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/codexbar")
}

fn spawn_daemon(enable_codexbar: bool) -> Daemon {
    spawn_daemon_with_claude_config_dir(enable_codexbar, None)
}

/// Spawn with an optional fixture-only Claude config override. The default
/// hotpath child never inherits the caller's `CLAUDE_CONFIG_DIR`.
fn spawn_daemon_with_claude_config_dir(
    enable_codexbar: bool,
    claude_config_dir: Option<&Path>,
) -> Daemon {
    let temp_dir = tempfile::tempdir().expect("unique hotpath test directory");
    let dir = temp_dir.path();
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
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
    let mut command = Command::new(bin);
    command
        .arg("--config")
        .arg(&cfg_path)
        .arg("--socket")
        .arg(&socket)
        .arg("run")
        .env("HOME", &home)
        .env("CODEX_HOME", home.join("no-codex"))
        .env_remove("CLAUDE_CONFIG_DIR")
        .env("XDG_CONFIG_HOME", home.join("xdg-config"))
        .env("XDG_STATE_HOME", home.join("xdg-state"))
        .env("XDG_RUNTIME_DIR", dir)
        .env_remove("QUOTA_SOCKET")
        .env("QUOTA_NO_KEYCHAIN", "1")
        .env("QUOTA_WATCH_IDLE_SECS", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(claude_config_dir) = claude_config_dir {
        command.env("CLAUDE_CONFIG_DIR", claude_config_dir);
    }
    let child = command.spawn().expect("spawn quotad");

    let daemon = Daemon {
        child,
        _temp_dir: temp_dir,
        socket,
    };
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
    let deadline = Instant::now() + Duration::from_secs(8);
    let (status, result) = loop {
        let status = rpc(&d.socket, 1, METHOD_STATUS, serde_json::json!({}));
        let result: StatusResult =
            serde_json::from_value(status.result.clone().expect("status result")).unwrap();
        if result.snapshot.by_id(ProviderId::Claude).is_some() {
            break (status, result);
        }
        assert!(
            Instant::now() < deadline,
            "initial refresh did not publish Claude's unavailable result"
        );
        thread::sleep(Duration::from_millis(10));
    };
    let codex = result
        .snapshot
        .by_id(ProviderId::Codex)
        .expect("codex present");
    assert_eq!(codex.status, Availability::Stale);
    assert_eq!(codex.freshness, Freshness::Stale);
    assert!(codex.observed_at.is_some());
    assert_eq!(codex.windows[0].used_percent, Some(59.0));
    assert_eq!(codex.windows[0].freshness, Freshness::Stale);
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
    assert_eq!(answers.answers[0].basis, CanStartBasis::Unavailable);
    assert!(answers.answers[0].explanation.contains("stale"));

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

fn observe_params(used: f64) -> serde_json::Value {
    serde_json::json!({
        "schema": 1,
        "provider": "claude",
        "source": "statusline",
        "windows": [
            {"kind": "five_hour", "label": "5h", "used_percent": used,
             "reset_at": quota_core::timeutil::now_unix() + 7_200, "limit_window_seconds": 18_000},
            {"kind": "weekly", "label": "weekly", "used_percent": 12.0,
             "reset_at": quota_core::timeutil::now_unix() + 200_000, "limit_window_seconds": 604_800}
        ]
    })
}

fn claude_of(socket: &Path) -> quota_core::types::ProviderSnapshot {
    let status = rpc(
        socket,
        90,
        METHOD_STATUS,
        serde_json::json!({"provider": "claude"}),
    );
    let result: StatusResult = serde_json::from_value(status.result.unwrap()).unwrap();
    result
        .snapshot
        .by_id(ProviderId::Claude)
        .cloned()
        .expect("claude present")
}

#[test]
fn statusline_push_becomes_current_claude_evidence_and_survives_a_refresh() {
    let d = spawn_daemon(false);
    assert_eq!(claude_of(&d.socket).status, Availability::Unavailable);

    let pushed = rpc(&d.socket, 1, METHOD_OBSERVE, observe_params(34.0));
    assert!(pushed.ok, "{pushed:?}");
    let claude = claude_of(&d.socket);
    assert_eq!(claude.status, Availability::Ok);
    assert_eq!(claude.source, Some(Source::Statusline));
    assert_eq!(claude.freshness, Freshness::Current);
    assert_eq!(claude.windows[0].used_percent, Some(34.0));
    assert!(claude.observed_at.is_some());

    // The daemon's own OAuth poll has no credentials here. It must not be
    // allowed to replace a current push with `no_credentials`.
    let refreshed = rpc(
        &d.socket,
        2,
        METHOD_REFRESH,
        serde_json::json!({"provider": "claude"}),
    );
    assert!(refreshed.ok);
    let after = claude_of(&d.socket);
    assert_eq!(after.status, Availability::Ok);
    assert_eq!(after.source, Some(Source::Statusline));
    assert_no_secrets(&serde_json::to_string(&pushed).unwrap());
}

#[test]
fn fixture_claude_config_override_keeps_statusline_push_out_of_isolated_account() {
    let claude_config = tempfile::tempdir().expect("fixture-only Claude config directory");
    let d = spawn_daemon_with_claude_config_dir(false, Some(claude_config.path()));
    let response = rpc(&d.socket, 1, METHOD_OBSERVE, observe_params(34.0));
    assert!(!response.ok);
    assert_eq!(response.error.unwrap().code, "account_scope");
    assert_eq!(
        claude_of(&d.socket).status,
        Availability::Unavailable,
        "the isolated fixture account must not receive an unscoped push"
    );
}

#[test]
fn percent_admission_answers_from_pushed_evidence_and_explains_itself() {
    let d = spawn_daemon(false);
    assert!(rpc(&d.socket, 1, METHOD_OBSERVE, observe_params(34.0)).ok);
    let can = rpc(
        &d.socket,
        2,
        METHOD_CAN_START,
        serde_json::json!({"percent": 10.0, "provider": "claude"}),
    );
    let result: CanStartResult = serde_json::from_value(can.result.unwrap()).unwrap();
    assert!(result.ok, "{result:?}");
    let answer = &result.answers[0];
    assert_eq!(answer.basis, CanStartBasis::PercentBudget);
    let admission = answer.admission.as_ref().unwrap();
    assert!(admission.headroom_ok && admission.pace_ok);
    assert_eq!(admission.reserve_percent, 2.0);

    let refused = rpc(
        &d.socket,
        3,
        METHOD_CAN_START,
        serde_json::json!({"percent": 70.0, "provider": "claude"}),
    );
    let result: CanStartResult = serde_json::from_value(refused.result.unwrap()).unwrap();
    assert!(!result.ok);
    assert!(!result.answers[0].admission.as_ref().unwrap().headroom_ok);

    let both = rpc(
        &d.socket,
        4,
        METHOD_CAN_START,
        serde_json::json!({"percent": 5.0, "tokens": 10, "provider": "claude"}),
    );
    assert_eq!(both.error.unwrap().code, "bad_params");
}

#[test]
fn a_push_with_an_unknown_schema_is_refused_and_stores_nothing() {
    let d = spawn_daemon(false);
    let mut params = observe_params(50.0);
    params["schema"] = serde_json::json!(2);
    let refused = rpc(&d.socket, 1, METHOD_OBSERVE, params);
    assert_eq!(refused.error.unwrap().code, "unsupported_schema");
    assert_eq!(claude_of(&d.socket).status, Availability::Unavailable);
}

#[test]
fn sigterm_removes_socket_and_lock_gracefully() {
    let mut daemon = spawn_daemon(false);
    let lock = daemon.socket.with_extension("lock");
    assert!(daemon.socket.exists());
    assert!(lock.exists());
    let status = Command::new("/bin/kill")
        .args(["-TERM", &daemon.child.id().to_string()])
        .status()
        .expect("send SIGTERM");
    assert!(status.success());
    let deadline = Instant::now() + Duration::from_secs(5);
    let exit = loop {
        if let Some(exit) = daemon.child.try_wait().expect("wait for quotad") {
            break exit;
        }
        assert!(Instant::now() < deadline, "quotad did not stop on SIGTERM");
        thread::sleep(Duration::from_millis(10));
    };
    assert!(exit.success());
    assert!(!daemon.socket.exists(), "SIGTERM must unlink the socket");
    assert!(!lock.exists(), "SIGTERM must remove the instance lock");
}

#[test]
fn sigterm_during_startup_probe_removes_socket_and_lock() {
    let temp_dir = tempfile::tempdir().expect("unique startup test directory");
    let dir = temp_dir.path();
    let socket = dir.join("startup.sock");
    let home = dir.join("codex-home");
    fs::create_dir_all(&home).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").expect("local TLS stall listener");
    let port = listener.local_addr().unwrap().port();
    fs::write(
        home.join("config.toml"),
        format!("chatgpt_base_url = \"https://localhost:{port}\"\n"),
    )
    .unwrap();
    fs::write(
        home.join("auth.json"),
        br#"{"access_token":"startup-test-token"}"#,
    )
    .unwrap();
    let cfg_path = dir.join("config.json");
    let cfg = serde_json::json!({
        "history": false,
        "ring_capacity": 16,
        "refresh_min_secs": 30,
        "refresh_max_secs": 300,
        "http_timeout_secs": 5,
        "enable_codex": true,
        "enable_claude": false,
        "enable_codexbar_files": false,
        "accounts_path": dir.join("accounts.json").to_string_lossy(),
    });
    fs::write(&cfg_path, serde_json::to_vec_pretty(&cfg).unwrap()).unwrap();

    let (connected_tx, connected_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(8)))
                        .unwrap();
                    let mut client_hello = [0_u8; 1];
                    if !matches!(stream.read(&mut client_hello), Ok(1)) {
                        return;
                    }
                    let _ = connected_tx.send(());
                    thread::sleep(Duration::from_secs(2));
                    return;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => return,
            }
        }
    });

    let child = Command::new(env!("CARGO_BIN_EXE_quotad"))
        .arg("--config")
        .arg(&cfg_path)
        .arg("--socket")
        .arg(&socket)
        .arg("run")
        .env("HOME", &home)
        .env("CODEX_HOME", &home)
        .env("CLAUDE_CONFIG_DIR", dir.join("no-claude"))
        .env("XDG_CONFIG_HOME", dir.join("xdg-config"))
        .env("XDG_STATE_HOME", dir.join("xdg-state"))
        .env("XDG_RUNTIME_DIR", dir)
        .env_remove("QUOTA_SOCKET")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn quotad with a stalled initial HTTPS probe");
    let mut daemon = Daemon {
        child,
        _temp_dir: temp_dir,
        socket: socket.clone(),
    };
    wait_ping(&socket, Duration::from_secs(8))
        .expect("quotad did not accept requests during the initial provider probe");
    connected_rx
        .recv_timeout(Duration::from_secs(8))
        .expect("initial provider probe did not connect to local stall server");
    assert!(daemon.child.try_wait().unwrap().is_none());
    let lock = socket.with_extension("lock");
    assert!(socket.exists());
    assert!(lock.exists());

    let status = Command::new("/bin/kill")
        .args(["-TERM", &daemon.child.id().to_string()])
        .status()
        .expect("send SIGTERM during startup");
    assert!(status.success());
    let deadline = Instant::now() + Duration::from_secs(8);
    let exit = loop {
        if let Some(exit) = daemon.child.try_wait().expect("wait for quotad") {
            break exit;
        }
        assert!(
            Instant::now() < deadline,
            "quotad did not stop during startup"
        );
        thread::sleep(Duration::from_millis(10));
    };
    assert!(exit.success(), "startup SIGTERM exit was {exit}");
    assert!(!socket.exists(), "startup SIGTERM must unlink the socket");
    assert!(
        !lock.exists(),
        "startup SIGTERM must remove the instance lock"
    );
    server.join().unwrap();
}

#[test]
fn scheduled_refresh_keeps_socket_and_sigterm_responsive() {
    let temp_dir = tempfile::tempdir().expect("unique scheduled-refresh test directory");
    let dir = temp_dir.path();
    let socket = dir.join("scheduled.sock");
    let home = dir.join("codex-home");
    fs::create_dir_all(&home).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").expect("local TLS stall listener");
    let port = listener.local_addr().unwrap().port();
    let (connected_tx, connected_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(15)))
                        .unwrap();
                    let mut client_hello = [0_u8; 1];
                    if matches!(stream.read(&mut client_hello), Ok(1)) {
                        let _ = connected_tx.send(());
                        let _ = release_rx.recv_timeout(Duration::from_secs(15));
                    }
                    return;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => return,
            }
        }
    });

    let cfg_path = dir.join("config.json");
    let cfg = serde_json::json!({
        "history": false,
        "ring_capacity": 16,
        "refresh_min_secs": 5,
        "refresh_max_secs": 5,
        "http_timeout_secs": 30,
        "enable_codex": true,
        "enable_claude": false,
        "enable_codexbar_files": false,
        "accounts_path": dir.join("accounts.json").to_string_lossy(),
    });
    fs::write(&cfg_path, serde_json::to_vec_pretty(&cfg).unwrap()).unwrap();

    let child = Command::new(env!("CARGO_BIN_EXE_quotad"))
        .arg("--config")
        .arg(&cfg_path)
        .arg("--socket")
        .arg(&socket)
        .arg("run")
        .env("HOME", &home)
        .env("CODEX_HOME", &home)
        .env("CLAUDE_CONFIG_DIR", dir.join("no-claude"))
        .env("XDG_CONFIG_HOME", dir.join("xdg-config"))
        .env("XDG_STATE_HOME", dir.join("xdg-state"))
        .env("XDG_RUNTIME_DIR", dir)
        .env_remove("QUOTA_SOCKET")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn quotad before scheduled refresh");
    let mut daemon = Daemon {
        child,
        _temp_dir: temp_dir,
        socket: socket.clone(),
    };
    wait_ping(&socket, Duration::from_secs(8)).expect("quotad did not become ready");

    fs::write(
        home.join("config.toml"),
        format!("chatgpt_base_url = \"https://localhost:{port}\"\n"),
    )
    .unwrap();
    fs::write(
        home.join("auth.json"),
        br#"{"access_token":"scheduled-refresh-test-token"}"#,
    )
    .unwrap();
    connected_rx
        .recv_timeout(Duration::from_secs(12))
        .expect("scheduled refresh did not connect to the stalled HTTPS server");

    let ping_responsive = (|| -> Result<bool, Box<dyn std::error::Error>> {
        let mut stream = UnixStream::connect(&socket)?;
        stream.set_read_timeout(Some(Duration::from_millis(500)))?;
        let request = Request::new(9, METHOD_PING);
        write_frame(&mut stream, &serde_json::to_vec(&request).unwrap())?;
        let payload = read_frame(&mut stream)?;
        let response: Response = serde_json::from_slice(&payload)?;
        Ok(response.ok)
    })()
    .unwrap_or(false);

    let status = Command::new("/bin/kill")
        .args(["-TERM", &daemon.child.id().to_string()])
        .status()
        .expect("send SIGTERM during scheduled refresh");
    assert!(status.success());
    let cleanup_deadline = Instant::now() + Duration::from_millis(750);
    let lock = socket.with_extension("lock");
    while (socket.exists() || lock.exists()) && Instant::now() < cleanup_deadline {
        thread::sleep(Duration::from_millis(5));
    }
    let socket_removed_before_probe_finished = !socket.exists();
    let lock_removed_before_probe_finished = !lock.exists();

    // Process exit must complete while the provider is still stalled. This
    // catches runtime shutdown waiting for a blocking probe worker.
    let deadline = Instant::now() + Duration::from_secs(3);
    let exit = loop {
        if let Some(exit) = daemon.child.try_wait().expect("wait for quotad") {
            break exit;
        }
        assert!(
            Instant::now() < deadline,
            "quotad did not exit while the scheduled provider probe was stalled"
        );
        thread::sleep(Duration::from_millis(10));
    };
    assert!(
        exit.success(),
        "scheduled-refresh SIGTERM exit while probe stalled was {exit}"
    );

    // Only release the test server after process exit has been observed.
    let _ = release_tx.send(());
    server.join().unwrap();

    assert!(
        ping_responsive,
        "socket ping must complete during scheduled refresh"
    );
    assert!(
        socket_removed_before_probe_finished && lock_removed_before_probe_finished,
        "SIGTERM must clean socket and lock before probe completion (socket_removed={}, lock_removed={})",
        socket_removed_before_probe_finished,
        lock_removed_before_probe_finished
    );
    assert!(exit.success(), "scheduled-refresh SIGTERM exit was {exit}");
    assert!(!socket.exists());
    assert!(!socket.with_extension("lock").exists());
}

#[test]
fn malformed_config_does_not_start_with_defaults() {
    let temp_dir = tempfile::tempdir().expect("unique malformed-config test directory");
    let dir = temp_dir.path();
    let cfg = dir.join("config.json");
    let socket = dir.join("quota.sock");
    fs::write(&cfg, b"{ malformed").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_quotad"))
        .arg("--config")
        .arg(&cfg)
        .arg("--socket")
        .arg(&socket)
        .arg("run")
        .output()
        .expect("run quotad with malformed config");
    assert!(!output.status.success());
    assert!(!socket.exists());
    assert!(String::from_utf8_lossy(&output.stderr).contains("malformed config JSON"));
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
