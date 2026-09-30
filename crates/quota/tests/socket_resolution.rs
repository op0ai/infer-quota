//! `quota` resolves its socket without letting a broken default config hide
//! a daemon that `QUOTA_SOCKET` names. No network; a local fake daemon only.

use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::process::Command;

use tempfile::TempDir;

use quota_core::framing::{decode_len, encode_frame};
use quota_core::protocol::{Pong, Request, Response};

fn scratch_dir() -> TempDir {
    let dir = tempfile::Builder::new().prefix("qsr-").tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("cfg/quota")).unwrap();
    dir
}

#[test]
fn greptile_5_quota_ping_reaches_quota_socket_despite_malformed_config() {
    let scratch = scratch_dir();
    let dir = scratch.path();
    std::fs::write(dir.join("cfg/quota/config.json"), b"{ malformed").unwrap();
    let socket = dir.join("d.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let daemon = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut header = [0u8; 4];
        stream.read_exact(&mut header).unwrap();
        let mut body = vec![0u8; decode_len(header).unwrap()];
        stream.read_exact(&mut body).unwrap();
        let request: Request = serde_json::from_slice(&body).unwrap();
        let reply = Response::result(request.id, Pong { pong: true });
        let frame = encode_frame(&serde_json::to_vec(&reply).unwrap()).unwrap();
        stream.write_all(&frame).unwrap();
        request.method
    });

    let output = Command::new(env!("CARGO_BIN_EXE_quota"))
        .arg("ping")
        .env("HOME", dir)
        .env("XDG_CONFIG_HOME", dir.join("cfg"))
        .env_remove("XDG_RUNTIME_DIR")
        .env("QUOTA_SOCKET", &socket)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "pong");
    assert_eq!(daemon.join().unwrap(), "ping");

    let without_env = Command::new(env!("CARGO_BIN_EXE_quota"))
        .arg("ping")
        .env("HOME", dir)
        .env("XDG_CONFIG_HOME", dir.join("cfg"))
        .env_remove("XDG_RUNTIME_DIR")
        .env_remove("QUOTA_SOCKET")
        .output()
        .unwrap();
    assert!(!without_env.status.success());
    assert!(String::from_utf8_lossy(&without_env.stderr).contains("malformed config"));
}
