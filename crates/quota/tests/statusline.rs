//! `quota statusline` as Claude Code runs it: JSON on stdin, one line out.
//! A fake daemon stands in for `quotad`, so these tests pin the CLI contract
//! (what it pushes, what it prints, how long it may take) without a live
//! provider or network.

#![cfg(unix)]

use std::io::Write;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use quota_core::framing::{read_frame, write_frame};
use quota_core::protocol::{ObserveResult, Request, Response};

fn fixture(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/claude-statusline")
        .join(name);
    std::fs::read(path).unwrap()
}

struct Run {
    stdout: String,
    success: bool,
    elapsed: Duration,
}

fn run_quota(socket: &Path, chain: Option<&str>, stdin: &[u8]) -> Run {
    let mut command = Command::new(env!("CARGO_BIN_EXE_quota"));
    command.arg("--socket").arg(socket).arg("statusline");
    if let Some(chain) = chain {
        command.args(["--chain", chain]);
    }
    let started = Instant::now();
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    let out = child.wait_with_output().unwrap();
    Run {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        success: out.status.success(),
        elapsed: started.elapsed(),
    }
}

/// Accepts connections, records each request, and answers like `quotad`.
fn fake_daemon(socket: PathBuf, reply: bool) -> mpsc::Receiver<Request> {
    let listener = UnixListener::bind(socket).unwrap();
    let (send, receive) = mpsc::channel();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let send = send.clone();
            thread::spawn(move || {
                let Ok(payload) = read_frame(&mut stream) else {
                    return;
                };
                let request: Request = serde_json::from_slice(&payload).unwrap();
                let id = request.id;
                let _ = send.send(request);
                if reply {
                    let ok = Response::result(
                        id,
                        ObserveResult {
                            accepted: 1,
                            observed_at: 0,
                        },
                    );
                    let _ = write_frame(&mut stream, &serde_json::to_vec(&ok).unwrap());
                } else {
                    thread::sleep(Duration::from_secs(5));
                }
            });
        }
    });
    receive
}

fn socket_in(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("quota.sock")
}

#[test]
fn pushes_the_pinned_rate_limits_and_prints_the_segment() {
    let dir = tempfile::tempdir().unwrap();
    let requests = fake_daemon(socket_in(&dir), true);
    let run = run_quota(&socket_in(&dir), None, &fixture("full-2.1.80.json"));
    assert!(run.success);
    assert!(run.stdout.contains("5h 34%"), "{}", run.stdout);
    assert!(run.stdout.contains("wk 12%"), "{}", run.stdout);

    let request = requests.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(request.method, "observe");
    assert_eq!(request.params["schema"], 1);
    assert_eq!(request.params["provider"], "claude");
    assert_eq!(request.params["source"], "statusline");
    assert_eq!(request.params["windows"][0]["kind"], "five_hour");
    assert_eq!(request.params["windows"][0]["used_percent"], 34.0);
    assert_eq!(request.params["windows"][1]["kind"], "weekly");
    assert!(
        request.params.get("observed_at").is_none(),
        "the daemon stamps evidence time, not the client"
    );
}

#[test]
fn prints_the_segment_when_quotad_is_down() {
    let dir = tempfile::tempdir().unwrap();
    let run = run_quota(&socket_in(&dir), None, &fixture("full-2.1.80.json"));
    assert!(run.success);
    assert!(run.stdout.contains("5h 34%"), "{}", run.stdout);
    assert!(run.elapsed < Duration::from_secs(1), "{:?}", run.elapsed);
}

/// Push budget from `quota statusline` (50 ms), and the scheduling noise a
/// loaded test host may add on top of a process spawn.
const PUSH_BUDGET: Duration = Duration::from_millis(50);
const SPAWN_SLACK: Duration = Duration::from_millis(100);

#[test]
fn a_wedged_quotad_costs_the_status_line_the_push_budget_and_no_more() {
    let dir = tempfile::tempdir().unwrap();
    let down = tempfile::tempdir().unwrap();
    let input = fixture("full-2.1.80.json");
    let baseline = (0..3)
        .map(|_| run_quota(&socket_in(&down), None, &input).elapsed)
        .min()
        .unwrap();

    let requests = fake_daemon(socket_in(&dir), false);
    let run = run_quota(&socket_in(&dir), None, &input);
    assert!(run.success);
    assert!(run.stdout.contains("5h 34%"), "{}", run.stdout);
    let pushed = requests.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(
        pushed.method, "observe",
        "the push reached the wedged daemon"
    );
    assert!(
        run.elapsed >= PUSH_BUDGET,
        "{:?} is under the push budget, so the wait was never exercised",
        run.elapsed
    );
    assert!(
        run.elapsed < baseline + PUSH_BUDGET + SPAWN_SLACK,
        "a daemon that never replies held the status line for {:?} (daemon down: {baseline:?})",
        run.elapsed
    );
}

#[test]
fn chain_output_is_joined_with_the_segment_even_when_quotad_is_down() {
    let dir = tempfile::tempdir().unwrap();
    let run = run_quota(
        &socket_in(&dir),
        Some("cat >/dev/null; printf 'main'"),
        &fixture("full-2.1.80.json"),
    );
    assert!(run.success);
    assert_eq!(run.stdout.trim_end(), "main │ 5h 34% · wk 12%");
}

#[test]
fn the_chained_command_sees_the_same_stdin_json() {
    let dir = tempfile::tempdir().unwrap();
    let run = run_quota(
        &socket_in(&dir),
        Some("grep -o '\"session_id\": *\"[^\"]*\"' | head -1"),
        &fixture("full-2.1.80.json"),
    );
    assert!(
        run.stdout.contains("00000000-0000-4000-8000-000000000001"),
        "{}",
        run.stdout
    );
}

#[test]
fn a_session_without_rate_limits_pushes_nothing_and_prints_only_the_chain() {
    let dir = tempfile::tempdir().unwrap();
    let requests = fake_daemon(socket_in(&dir), true);
    let run = run_quota(
        &socket_in(&dir),
        Some("printf 'main'"),
        &fixture("fresh-session.json"),
    );
    assert!(run.success);
    assert_eq!(run.stdout.trim_end(), "main");
    assert!(requests.recv_timeout(Duration::from_millis(300)).is_err());
}

#[test]
fn garbage_stdin_still_exits_zero_and_keeps_the_chain() {
    let dir = tempfile::tempdir().unwrap();
    let run = run_quota(
        &socket_in(&dir),
        Some("printf 'main'"),
        b"\xff\xfe not json",
    );
    assert!(run.success);
    assert_eq!(run.stdout.trim_end(), "main");

    let run = run_quota(&socket_in(&dir), None, b"");
    assert!(run.success);
    assert_eq!(run.stdout.trim_end(), "");
}
