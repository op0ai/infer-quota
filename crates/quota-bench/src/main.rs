//! Harnesses for socket RTT, fixture pace math, ring/status serialize,
//! watch-client load, and daemon start. Not a shipped product binary.
//!
//! ```text
//! quota-bench --socket PATH [--warmup N] [--iters N]          # default: rtt
//! quota-bench pace [--history PATH] [--iters N]
//! quota-bench serialize [--history PATH] [--iters N] [--ring N]
//! quota-bench watch --socket PATH [--clients N] [--iters N]
//! quota-bench start --quotad PATH --socket PATH [--runs N]
//! quota-bench statusline --socket PATH --quota PATH [--input FILE] [--iters N] [--warmup N]
//! ```

use std::env;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use quota_adapters::codexbar::{
    history_to_snapshots, load_history_jsonl, load_history_jsonl_tail, workspace_fixtures_dir,
};
use quota_core::framing::{encode_frame, read_frame, write_frame};
use quota_core::math::pace_for;
use quota_core::protocol::{
    ProviderFilter, Request, Response, WatchParams, METHOD_OBSERVE, METHOD_PING, METHOD_STATUS,
    METHOD_WATCH,
};
use quota_core::types::{ProviderId, Snapshot};
use quota_core::MAX_FRAME_BYTES;

/// Every failure here comes from outside the harness (a path, a socket, a
/// child process, a flag), so it is reported and the run exits non-zero; the
/// harness never panics on it.
type Res<T> = Result<T, String>;

fn default_history() -> PathBuf {
    workspace_fixtures_dir().join("usage-history.redacted.jsonl")
}

fn number(raw: Option<String>, flag: &str) -> Res<usize> {
    let raw = raw.ok_or_else(|| format!("{flag} needs N"))?;
    raw.parse()
        .map_err(|_| format!("{flag} not an integer: {raw:?}"))
}

fn parse_usize(
    args: &mut std::iter::Peekable<env::Args>,
    flag: &str,
    default: usize,
) -> Res<usize> {
    if args.peek().map(|s| s.as_str()) == Some(flag) {
        args.next();
        number(args.next(), flag)
    } else {
        Ok(default)
    }
}

fn parse_path(args: &mut std::iter::Peekable<env::Args>, flag: &str) -> Res<Option<PathBuf>> {
    if args.peek().map(|s| s.as_str()) == Some(flag) {
        args.next();
        let path = args.next().ok_or_else(|| format!("{flag} needs a path"))?;
        Ok(Some(PathBuf::from(path)))
    } else {
        Ok(None)
    }
}

fn required(path: Option<PathBuf>, flag: &str) -> Res<PathBuf> {
    path.ok_or_else(|| format!("missing {flag} PATH"))
}

fn connect(socket: &Path) -> Res<UnixStream> {
    UnixStream::connect(socket).map_err(|e| format!("connect {}: {e}", socket.display()))
}

fn status_params() -> serde_json::Value {
    serde_json::json!({ "provider": "all" })
}

fn stats(ns: &mut [u64]) -> (f64, f64, f64, f64, f64, f64, f64) {
    ns.sort_unstable();
    let n = ns.len();
    if n == 0 {
        return (0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
    }
    let mean = ns.iter().sum::<u64>() as f64 / n as f64;
    let var = ns
        .iter()
        .map(|&x| {
            let d = x as f64 - mean;
            d * d
        })
        .sum::<f64>()
        / n as f64;
    let pct = |p: f64| -> f64 {
        let idx = ((p * (n as f64 - 1.0)).round() as usize).min(n - 1);
        ns[idx] as f64
    };
    (
        mean,
        var.sqrt(),
        ns[0] as f64,
        ns[n - 1] as f64,
        pct(0.50),
        pct(0.95),
        pct(0.99),
    )
}

fn print_row(name: &str, mut samples: Vec<u64>) {
    let n = samples.len();
    let (mean, std, min, max, p50, p95, p99) = stats(&mut samples);
    println!(
        "{name}: n={n} mean={:.1}us std={:.1}us min={:.1}us max={:.1}us p50={:.1}us p95={:.1}us p99={:.1}us",
        mean / 1000.0,
        std / 1000.0,
        min / 1000.0,
        max / 1000.0,
        p50 / 1000.0,
        p95 / 1000.0,
        p99 / 1000.0
    );
}

fn rpc(stream: &mut UnixStream, id: u64, method: &str, params: serde_json::Value) -> Res<()> {
    let req = Request::with_params(id, method, params);
    let bytes = serde_json::to_vec(&req).map_err(|e| format!("encode {method}: {e}"))?;
    write_frame(stream, &bytes).map_err(|e| format!("write {method}: {e}"))?;
    let payload = read_frame(stream).map_err(|e| format!("read {method}: {e}"))?;
    let resp: Response =
        serde_json::from_slice(&payload).map_err(|e| format!("decode {method}: {e}"))?;
    if resp.ok {
        Ok(())
    } else {
        Err(format!("rpc {method} failed: {:?}", resp.error))
    }
}

fn bench_rtt(socket: &Path, warmup: usize, iters: usize) -> Res<()> {
    let mut stream = connect(socket)?;
    let mut ping = Vec::with_capacity(iters);
    for i in 0..warmup {
        rpc(&mut stream, i as u64, METHOD_PING, serde_json::Value::Null)?;
    }
    for i in 0..iters {
        let t0 = Instant::now();
        rpc(
            &mut stream,
            (warmup + i) as u64,
            METHOD_PING,
            serde_json::Value::Null,
        )?;
        ping.push(t0.elapsed().as_nanos() as u64);
    }
    print_row("ping", ping);

    let mut status = Vec::with_capacity(iters);
    for i in 0..warmup {
        rpc(&mut stream, i as u64, METHOD_STATUS, status_params())?;
    }
    for i in 0..iters {
        let t0 = Instant::now();
        rpc(
            &mut stream,
            (warmup + i) as u64,
            METHOD_STATUS,
            status_params(),
        )?;
        status.push(t0.elapsed().as_nanos() as u64);
    }
    print_row("status", status);
    Ok(())
}

fn load_history(history: &Path) -> Res<Vec<quota_adapters::codexbar::HistoryRow>> {
    load_history_jsonl(history).map_err(|e| format!("load {}: {e}", history.display()))
}

fn bench_pace(history: &Path, iters: usize) -> Res<()> {
    let rows = load_history(history)?;
    let snaps = history_to_snapshots(&rows);
    if rows.is_empty() {
        return Err(format!("{} has no usable rows", history.display()));
    }
    let latest = snaps
        .last()
        .and_then(|s| s.by_id(ProviderId::Codex).cloned())
        .ok_or_else(|| format!("{} has no codex snapshot", history.display()))?;

    let t0 = Instant::now();
    let report = pace_for(&snaps, &latest);
    let first = t0.elapsed();
    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t = Instant::now();
        let _ = pace_for(&snaps, &latest);
        samples.push(t.elapsed().as_nanos() as u64);
    }
    let elapsed: u128 = samples.iter().map(|&x| x as u128).sum();
    let per_sec = if elapsed == 0 {
        0.0
    } else {
        (iters as f64) * 1_000_000_000.0 / elapsed as f64
    };
    println!(
        "pace_fixture: rows={} first_ns={} burn_per_h={:?} samples={} iters={} throughput={:.1}/s",
        rows.len(),
        first.as_nanos(),
        report.burn_percent_per_hour,
        report.samples,
        iters,
        per_sec
    );
    print_row("pace", samples);
    println!("pace_throughput_per_sec: {per_sec:.1}");
    Ok(())
}

fn bench_serialize(history: &Path, iters: usize, ring: usize) -> Res<()> {
    let rows = load_history(history)?;
    let snaps = history_to_snapshots(&rows);
    let ring_snaps: Vec<Snapshot> = snaps.iter().rev().take(ring).cloned().collect();
    let newest = ring_snaps
        .first()
        .ok_or_else(|| format!("{} has no snapshots", history.display()))?;

    let mut ser = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t0 = Instant::now();
        let latest = newest.clone();
        let json = serde_json::to_vec(&latest).map_err(|e| format!("encode snapshot: {e}"))?;
        if json.len() >= MAX_FRAME_BYTES {
            return Err(format!(
                "snapshot is {} bytes, over the frame cap",
                json.len()
            ));
        }
        let frame = encode_frame(&json).map_err(|e| format!("frame: {e}"))?;
        let _ = frame.len();
        ser.push(t0.elapsed().as_nanos() as u64);
    }
    print_row("status_serialize", ser);

    let mut ring_push = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t0 = Instant::now();
        let mut buf = Vec::with_capacity(ring);
        for s in &snaps {
            if buf.len() == ring {
                buf.remove(0);
            }
            buf.push(s.clone());
        }
        ring_push.push(t0.elapsed().as_nanos() as u64);
    }
    print_row(&format!("ring_replay_{}", snaps.len()), ring_push);
    Ok(())
}

fn bench_watch(socket: &Path, clients: usize, iters: usize) -> Res<()> {
    let mut handles = Vec::new();
    for c in 0..clients {
        let path = socket.to_path_buf();
        handles.push(thread::spawn(move || -> Res<Vec<u64>> {
            let mut stream = connect(&path)?;
            let mut samples = Vec::with_capacity(iters);
            for i in 0..iters {
                let t0 = Instant::now();
                rpc(
                    &mut stream,
                    (c * 10_000 + i) as u64,
                    METHOD_STATUS,
                    status_params(),
                )?;
                samples.push(t0.elapsed().as_nanos() as u64);
            }
            Ok(samples)
        }));
    }
    let mut all = Vec::new();
    for h in handles {
        all.extend(
            h.join()
                .map_err(|_| "a status client thread panicked".to_string())??,
        );
    }
    print_row(&format!("status_clients_{clients}"), all);

    // One watch subscribe + N status on sibling connections (watch holds the socket).
    let mut watch = connect(socket)?;
    let req = Request::with_params(
        99_999,
        METHOD_WATCH,
        WatchParams {
            provider: ProviderFilter::All,
        },
    );
    let bytes = serde_json::to_vec(&req).map_err(|e| format!("encode watch: {e}"))?;
    write_frame(&mut watch, &bytes).map_err(|e| format!("write watch: {e}"))?;
    let first = read_frame(&mut watch).map_err(|e| format!("read watch: {e}"))?;
    let resp: Response =
        serde_json::from_slice(&first).map_err(|e| format!("decode watch: {e}"))?;
    if !resp.ok {
        return Err(format!("watch failed: {:?}", resp.error));
    }
    drop(watch);
    println!("watch_subscribe: first_frame_ok=true bytes={}", first.len());
    Ok(())
}

fn wait_ping(socket: &Path, timeout: Duration) -> Option<Duration> {
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
                            return Some(start.elapsed());
                        }
                    }
                }
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
    None
}

fn spawn_quotad(quotad: &Path, socket: &Path) -> Res<Child> {
    if socket.exists() {
        let _ = std::fs::remove_file(socket);
    }
    Command::new(quotad)
        .arg("run")
        .arg("--socket")
        .arg(socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn {}: {e}", quotad.display()))
}

fn stop_child(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn bench_jsonl(history: &Path, iters: usize, cap: usize) -> Res<()> {
    let expected_full = load_history(history)?.len();
    let expected_tail = expected_full.min(cap.max(1));
    let mut full = Vec::with_capacity(iters);
    let mut tail = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t0 = Instant::now();
        let rows = load_history(history)?;
        if rows.len() != expected_full {
            return Err(format!("{} changed while benchmarking", history.display()));
        }
        full.push(t0.elapsed().as_nanos() as u64);
        let t1 = Instant::now();
        let rows = load_history_jsonl_tail(history, cap)
            .map_err(|e| format!("tail {}: {e}", history.display()))?;
        if rows.len() != expected_tail {
            return Err(format!("{} changed while benchmarking", history.display()));
        }
        tail.push(t1.elapsed().as_nanos() as u64);
    }
    print_row(&format!("jsonl_full_{expected_full}"), full);
    print_row(&format!("jsonl_tail_{expected_tail}"), tail);
    Ok(())
}

/// One cold start: time to the first answered ping, then a fresh connect+ping
/// against the now-running daemon.
fn start_once(socket: &Path) -> Res<(u64, u64)> {
    let ready = wait_ping(socket, Duration::from_secs(10))
        .ok_or_else(|| format!("quotad did not answer on {} within 10 s", socket.display()))?;
    let t0 = Instant::now();
    let mut s = connect(socket)?;
    rpc(&mut s, 1, METHOD_PING, serde_json::Value::Null)?;
    Ok((ready.as_nanos() as u64, t0.elapsed().as_nanos() as u64))
}

fn bench_start(quotad: &Path, socket: &Path, runs: usize) -> Res<()> {
    let mut cold = Vec::new();
    let mut warm = Vec::new();
    for _ in 0..runs {
        let child = spawn_quotad(quotad, socket)?;
        let timed = start_once(socket);
        stop_child(child);
        let _ = std::fs::remove_file(socket);
        let (ready, ping) = timed?;
        cold.push(ready);
        warm.push(ping);
        // Subsequent runs are warm-binary (page cache) but cold-process.
    }
    print_row("daemon_start_to_ping", cold);
    print_row("daemon_warm_ping", warm);
    Ok(())
}

/// Push RTT, then `quota statusline` end to end (fork + exec + parse + push +
/// print) against a running quotad, then the same with the daemon absent.
fn bench_statusline(
    socket: &Path,
    quota: &Path,
    input: &Path,
    warmup: usize,
    iters: usize,
) -> Res<()> {
    let stdin_json = std::fs::read(input).map_err(|e| format!("read {}: {e}", input.display()))?;
    let statusline = quota_source_claude_statusline::parse(&stdin_json)
        .map_err(|e| format!("{}: {e}", input.display()))?;
    let params = statusline
        .observe_params()
        .ok_or_else(|| format!("{} carries no rate_limits to push", input.display()))?;

    let push = |socket: &Path| -> Res<u64> {
        let t0 = Instant::now();
        let resp = quota_core::rpc::rpc_within(
            socket,
            1,
            METHOD_OBSERVE,
            &params,
            Duration::from_millis(50),
        )
        .map_err(|e| format!("push to {}: {e}", socket.display()))?;
        if !resp.ok {
            return Err(format!("observe refused: {:?}", resp.error));
        }
        Ok(t0.elapsed().as_nanos() as u64)
    };
    for _ in 0..warmup {
        push(socket)?;
    }
    let pushes = (0..iters).map(|_| push(socket)).collect::<Res<_>>()?;
    print_row("push_rtt(new conn, ack)", pushes);

    let run = |socket: &Path| -> Res<u64> {
        let t0 = Instant::now();
        let mut child = Command::new(quota)
            .arg("--socket")
            .arg(socket)
            .arg("statusline")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("spawn {}: {e}", quota.display()))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "quota statusline has no stdin pipe".to_string())?;
        std::io::Write::write_all(&mut stdin, &stdin_json)
            .map_err(|e| format!("write quota statusline stdin: {e}"))?;
        drop(stdin);
        let out = child
            .wait_with_output()
            .map_err(|e| format!("wait for quota statusline: {e}"))?;
        if !out.status.success() || out.stdout.is_empty() {
            return Err(format!("quota statusline printed nothing ({})", out.status));
        }
        Ok(t0.elapsed().as_nanos() as u64)
    };
    for _ in 0..warmup {
        run(socket)?;
    }
    let runs = (0..iters).map(|_| run(socket)).collect::<Res<_>>()?;
    print_row("statusline_e2e", runs);

    let absent = socket.with_extension("absent");
    let runs = (0..iters).map(|_| run(&absent)).collect::<Res<_>>()?;
    print_row("statusline_daemon_down", runs);
    Ok(())
}

fn usage() -> ! {
    eprintln!(
        "quota-bench --socket PATH [--warmup N] [--iters N]\n\
         quota-bench pace [--history PATH] [--iters N]\n\
         quota-bench serialize [--history PATH] [--iters N] [--ring N]\n\
         quota-bench watch --socket PATH [--clients N] [--iters N]\n\
         quota-bench start --quotad PATH --socket PATH [--runs N]\n\
         quota-bench jsonl [--history PATH] [--iters N] [--ring N]\n\
         quota-bench statusline --socket PATH --quota PATH [--input FILE] [--iters N] [--warmup N]"
    );
    std::process::exit(2);
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("quota-bench: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Res<()> {
    let mut args = env::args().peekable();
    let _bin = args.next();
    let mode = args.peek().cloned().unwrap_or_else(|| "--socket".into());
    match mode.as_str() {
        "pace" => {
            args.next();
            let history = parse_path(&mut args, "--history")?.unwrap_or_else(default_history);
            let iters = parse_usize(&mut args, "--iters", 200)?;
            bench_pace(&history, iters)
        }
        "serialize" => {
            args.next();
            let history = parse_path(&mut args, "--history")?.unwrap_or_else(default_history);
            let iters = parse_usize(&mut args, "--iters", 200)?;
            let ring = parse_usize(&mut args, "--ring", 128)?;
            bench_serialize(&history, iters, ring)
        }
        "watch" => {
            args.next();
            let socket = required(parse_path(&mut args, "--socket")?, "--socket")?;
            let clients = parse_usize(&mut args, "--clients", 8)?;
            let iters = parse_usize(&mut args, "--iters", 200)?;
            bench_watch(&socket, clients, iters)
        }
        "jsonl" => {
            args.next();
            let history = parse_path(&mut args, "--history")?.unwrap_or_else(default_history);
            let iters = parse_usize(&mut args, "--iters", 50)?;
            let cap = parse_usize(&mut args, "--ring", 128)?;
            bench_jsonl(&history, iters, cap)
        }
        "statusline" => {
            args.next();
            let socket = required(parse_path(&mut args, "--socket")?, "--socket")?;
            let quota = required(parse_path(&mut args, "--quota")?, "--quota")?;
            let input = parse_path(&mut args, "--input")?.unwrap_or_else(|| {
                workspace_fixtures_dir().join("../claude-statusline/full-2.1.80.json")
            });
            let iters = parse_usize(&mut args, "--iters", 300)?;
            let warmup = parse_usize(&mut args, "--warmup", 30)?;
            bench_statusline(&socket, &quota, &input, warmup, iters)
        }
        "start" => {
            args.next();
            let quotad = required(parse_path(&mut args, "--quotad")?, "--quotad")?;
            let socket = required(parse_path(&mut args, "--socket")?, "--socket")?;
            let runs = parse_usize(&mut args, "--runs", 8)?;
            bench_start(&quotad, &socket, runs)
        }
        "--help" | "-h" => usage(),
        _ => {
            let mut socket = None;
            let mut warmup = 100usize;
            let mut iters = 1000usize;
            while let Some(a) = args.next() {
                match a.as_str() {
                    "--socket" => {
                        let path = args.next().ok_or("--socket needs a path")?;
                        socket = Some(PathBuf::from(path));
                    }
                    "--warmup" => warmup = number(args.next(), "--warmup")?,
                    "--iters" => iters = number(args.next(), "--iters")?,
                    other => return Err(format!("unknown arg {other}")),
                }
            }
            bench_rtt(&required(socket, "--socket")?, warmup, iters)
        }
    }
}
