//! `quota can-start --percent` against daemons of different protocol versions.
//! A protocol-1 daemon ignores the unknown `percent` field and would answer a
//! `tokens: 0` question, so the CLI must refuse before asking it.

#![cfg(unix)]

use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use quota_core::framing::{read_frame, write_frame};
use quota_core::protocol::{CanStartResult, Request, Response, VersionInfo};

/// Answers `version` with `protocol` and `can_start` with an unconditional
/// yes, the way a daemon that misread the question would.
fn fake_daemon(socket: PathBuf, protocol: u32) -> mpsc::Receiver<String> {
    let listener = UnixListener::bind(socket).unwrap();
    let (send, receive) = mpsc::channel();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let Ok(payload) = read_frame(&mut stream) else {
                continue;
            };
            let request: Request = serde_json::from_slice(&payload).unwrap();
            let _ = send.send(request.method.clone());
            let response = match request.method.as_str() {
                "version" => Response::result(
                    request.id,
                    VersionInfo {
                        protocol,
                        ..VersionInfo::current()
                    },
                ),
                _ => Response::result(
                    request.id,
                    CanStartResult {
                        ok: true,
                        answers: Vec::new(),
                    },
                ),
            };
            let _ = write_frame(&mut stream, &serde_json::to_vec(&response).unwrap());
        }
    });
    receive
}

fn can_start(socket: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_quota"))
        .arg("--socket")
        .arg(socket)
        .arg("can-start")
        .args(args)
        .output()
        .unwrap()
}

fn methods(requests: &mpsc::Receiver<String>) -> Vec<String> {
    let mut seen = Vec::new();
    while let Ok(method) = requests.recv_timeout(Duration::from_millis(200)) {
        seen.push(method);
    }
    seen
}

#[test]
fn a_protocol_1_daemon_is_never_asked_a_percent_question() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("quota.sock");
    let requests = fake_daemon(socket.clone(), 1);
    let out = can_start(&socket, &["--provider", "codex", "--percent", "5"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("needs quotad protocol 2"), "{stderr}");
    assert_eq!(methods(&requests), ["version"]);
}

#[test]
fn a_protocol_2_daemon_gets_the_percent_question() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("quota.sock");
    let requests = fake_daemon(socket.clone(), 2);
    let out = can_start(&socket, &["--provider", "codex", "--percent", "5"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(methods(&requests), ["version", "can_start"]);
}

#[test]
fn a_tokens_question_is_unchanged_and_skips_the_version_check() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("quota.sock");
    let requests = fake_daemon(socket.clone(), 1);
    let out = can_start(&socket, &["--provider", "codex", "--tokens", "5"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(methods(&requests), ["can_start"]);
}
