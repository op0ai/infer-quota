//! `quota statusline`: the command Claude Code runs to draw its status line.
//!
//! It reads the JSON Claude Code sends on stdin, pushes the rate limits to
//! `quotad`, and prints one segment. It must never make the status line late
//! or empty: the push has a 50 ms budget, the segment is computed from stdin
//! alone, and every failure still exits 0.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use quota_core::protocol::METHOD_OBSERVE;
use quota_core::rpc::rpc_within;
use quota_core::timeutil::now_unix;
use quota_source_claude_statusline::{parse, MAX_INPUT_BYTES};

/// Budget for connect + write + ack. A status line redraws often; this is the
/// most a dead or wedged daemon can add to one.
pub const PUSH_BUDGET: Duration = Duration::from_millis(50);
/// A chained (previous) statusline command gets this long before it is dropped.
pub const CHAIN_BUDGET: Duration = Duration::from_millis(1000);
const CHAIN_OUTPUT_CAP: u64 = 64 * 1024;
const JOIN: &str = " │ ";

pub fn run(sock: &Path, chain: Option<&str>) {
    let input = read_input();
    let chained = chain.and_then(|command| spawn_chain(command, &input));
    let statusline = parse(&input).ok();
    if let Some(params) = statusline.as_ref().and_then(|s| s.observe_params()) {
        let _ = rpc_within(sock, 1, METHOD_OBSERVE, params, PUSH_BUDGET);
    }
    let segment = statusline
        .map(|s| s.segment(now_unix()))
        .unwrap_or_default();
    let previous = chained.map(collect_chain).unwrap_or_default();
    println!("{}", join_outputs(&previous, &segment));
}

fn read_input() -> Vec<u8> {
    let mut input = Vec::new();
    let _ = std::io::stdin()
        .lock()
        .take(MAX_INPUT_BYTES as u64 + 1)
        .read_to_end(&mut input);
    input
}

fn join_outputs(previous: &str, segment: &str) -> String {
    let previous = previous.trim_end_matches(['\n', '\r']);
    match (previous.is_empty(), segment.is_empty()) {
        (true, _) => segment.to_string(),
        (false, true) => previous.to_string(),
        (false, false) => format!("{previous}{JOIN}{segment}"),
    }
}

struct Chained {
    child: std::process::Child,
    output: mpsc::Receiver<String>,
}

/// Runs the user's previous statusline command with the same stdin. Both pipes
/// are serviced on their own threads so a chatty or stalled child cannot
/// deadlock us. The command leads its own process group, so a timeout can stop
/// everything it started, not only the shell.
fn spawn_chain(command: &str, input: &[u8]) -> Option<Chained> {
    let mut child = Command::new("sh")
        .args(["-c", command])
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    let stdout = child.stdout.take()?;
    let input = input.to_vec();
    std::thread::spawn(move || {
        let _ = std::io::Write::write_all(&mut stdin, &input);
    });
    let (send, output) = mpsc::channel();
    std::thread::spawn(move || {
        let mut text = Vec::new();
        let _ = stdout.take(CHAIN_OUTPUT_CAP).read_to_end(&mut text);
        let _ = send.send(String::from_utf8_lossy(&text).into_owned());
    });
    Some(Chained { child, output })
}

fn collect_chain(mut chained: Chained) -> String {
    let deadline = Instant::now() + CHAIN_BUDGET;
    let text = chained
        .output
        .recv_timeout(deadline.saturating_duration_since(Instant::now()));
    if text.is_err() {
        let group = rustix::process::Pid::from_child(&chained.child);
        let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
    }
    let _ = chained.child.kill();
    let _ = chained.child.wait();
    text.unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_prefers_whichever_side_has_content() {
        assert_eq!(join_outputs("", "5h 34%"), "5h 34%");
        assert_eq!(join_outputs("main ✓\n", ""), "main ✓");
        assert_eq!(join_outputs("main ✓\n", "5h 34%"), "main ✓ │ 5h 34%");
        assert_eq!(join_outputs("", ""), "");
    }

    #[test]
    fn chain_output_is_collected_and_a_stalled_chain_is_dropped() {
        let quick = spawn_chain("cat >/dev/null; printf 'prev'", b"{}").unwrap();
        assert_eq!(collect_chain(quick), "prev");

        let started = Instant::now();
        let slow = spawn_chain("sleep 30", b"{}").unwrap();
        assert_eq!(collect_chain(slow), "");
        assert!(started.elapsed() < CHAIN_BUDGET + Duration::from_millis(800));
    }

    #[test]
    fn a_timed_out_chain_takes_its_whole_process_group_down() {
        let dir = std::env::temp_dir().join(format!("quota-chain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        let marker = dir.join("grandchild-ran");
        let command = format!("(sleep 2; touch '{}') & sleep 30", marker.display());
        let chained = spawn_chain(&command, b"{}").unwrap();
        assert_eq!(collect_chain(chained), "");
        std::thread::sleep(Duration::from_millis(2_500));
        let survived = marker.exists();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            !survived,
            "a grandchild of the timed-out chain kept running"
        );
    }

    #[test]
    fn chain_receives_the_same_stdin() {
        let echo = spawn_chain("cat", br#"{"rate_limits":{}}"#).unwrap();
        assert_eq!(collect_chain(echo), r#"{"rate_limits":{}}"#);
    }
}
