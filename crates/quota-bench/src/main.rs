//! Harnesses for socket RTT, fixture pace math, ring/status serialize,
//! watch-client load, and daemon start. Not a shipped product binary.
//!
//! ```text
//! quota-bench --socket PATH [--warmup N] [--iters N]          # default: rtt
//! quota-bench pace [--history PATH] [--iters N]
//! quota-bench serialize [--history PATH] [--iters N] [--ring N]
//! quota-bench watch --socket PATH [--clients N] [--iters N]
//! quota-bench start --quotad PATH --socket PATH [--runs N]
//! ```

use std::env;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use quota_adapters::codexbar::{
    history_to_snapshots, load_history_jsonl, load_history_jsonl_tail, workspace_fixtures_dir,
};
use quota_core::framing::{encode_frame, read_frame, write_frame};
use quota_core::math::pace_for;
use quota_core::protocol::{
    ProviderFilter, Request, Response, StatusParams, WatchParams, METHOD_PING, METHOD_STATUS,
    METHOD_WATCH,
};
use quota_core::types::{ProviderId, Snapshot};
use quota_core::MAX_FRAME_BYTES;

fn default_history() -> PathBuf {
    workspace_fixtures_dir().join("usage-history.redacted.jsonl")
}

fn parse_usize(args: &mut std::iter::Peekable<env::Args>, flag: &str, default: usize) -> usize {
    if args.peek().map(|s| s.as_str()) == Some(flag) {
        args.next();
        args.next()
            .unwrap_or_else(|| panic!("{flag} needs N"))
            .parse()
            .unwrap_or_else(|_| panic!("{flag} not an integer"))
    } else {
        default
    }
}

fn parse_path(args: &mut std::iter::Peekable<env::Args>, flag: &str) -> Option<PathBuf> {
    if args.peek().map(|s| s.as_str()) == Some(flag) {
        args.next();
        Some(PathBuf::from(
            args.next().unwrap_or_else(|| panic!("{flag} needs a path")),
        ))
    } else {
        None
    }
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

fn rpc(stream: &mut UnixStream, id: u64, method: &str, params: serde_json::Value) {
    let req = Request::with_params(id, method, params);
    let bytes = serde_json::to_vec(&req).expect("serialize");
    write_frame(stream, &bytes).expect("write");
    let payload = read_frame(stream).expect("read");
    let resp: Response = serde_json::from_slice(&payload).expect("decode");
    assert!(resp.ok, "rpc {method} failed: {resp:?}");
}

fn bench_rtt(socket: &Path, warmup: usize, iters: usize) {
    let mut stream = UnixStream::connect(socket).unwrap_or_else(|e| {
        panic!("connect {}: {e}", socket.display());
    });
    let mut ping = Vec::with_capacity(iters);
    for i in 0..warmup {
        rpc(&mut stream, i as u64, METHOD_PING, serde_json::Value::Null);
    }
    for i in 0..iters {
        let t0 = Instant::now();
        rpc(
            &mut stream,
            (warmup + i) as u64,
            METHOD_PING,
            serde_json::Value::Null,
        );
        ping.push(t0.elapsed().as_nanos() as u64);
    }
    print_row("ping", ping);

    let mut status = Vec::with_capacity(iters);
    for i in 0..warmup {
        rpc(
            &mut stream,
            i as u64,
            METHOD_STATUS,
            serde_json::to_value(StatusParams {
                provider: ProviderFilter::All,
            })
            .unwrap(),
        );
    }
    for i in 0..iters {
        let t0 = Instant::now();
        rpc(
            &mut stream,
            (warmup + i) as u64,
            METHOD_STATUS,
            serde_json::to_value(StatusParams {
                provider: ProviderFilter::All,
            })
            .unwrap(),
        );
        status.push(t0.elapsed().as_nanos() as u64);
    }
    print_row("status", status);
}

fn bench_pace(history: &Path, iters: usize) {
    let rows = load_history_jsonl(history).expect("load history");
    let snaps = history_to_snapshots(&rows);
    assert!(!rows.is_empty(), "history has no usable rows");
    let latest = snaps
        .last()
        .and_then(|s| s.by_id(ProviderId::Codex).cloned())
        .expect("codex snapshot");

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
}

fn bench_serialize(history: &Path, iters: usize, ring: usize) {
    let rows = load_history_jsonl(history).expect("load history");
    let snaps = history_to_snapshots(&rows);
    let ring_snaps: Vec<Snapshot> = snaps.iter().rev().take(ring).cloned().collect();

    let mut ser = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t0 = Instant::now();
        let latest = ring_snaps.first().cloned().unwrap();
        let json = serde_json::to_vec(&latest).expect("json");
        assert!(json.len() < MAX_FRAME_BYTES);
        let frame = encode_frame(&json).expect("frame");
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
}

fn bench_watch(socket: &Path, clients: usize, iters: usize) {
    let mut handles = Vec::new();
    for c in 0..clients {
        let path = socket.to_path_buf();
        handles.push(thread::spawn(move || {
            let mut stream = UnixStream::connect(&path).expect("connect");
            let mut samples = Vec::with_capacity(iters);
            for i in 0..iters {
                let t0 = Instant::now();
                rpc(
                    &mut stream,
                    (c * 10_000 + i) as u64,
                    METHOD_STATUS,
                    serde_json::to_value(StatusParams {
                        provider: ProviderFilter::All,
                    })
                    .unwrap(),
                );
                samples.push(t0.elapsed().as_nanos() as u64);
            }
            samples
        }));
    }
    let mut all = Vec::new();
    for h in handles {
        all.extend(h.join().expect("thread"));
    }
    print_row(&format!("status_clients_{clients}"), all);

    // One watch subscribe + N status on sibling connections (watch holds the socket).
    let mut watch = UnixStream::connect(socket).expect("watch connect");
    let req = Request::with_params(
        99_999,
        METHOD_WATCH,
        WatchParams {
            provider: ProviderFilter::All,
        },
    );
    let bytes = serde_json::to_vec(&req).unwrap();
    write_frame(&mut watch, &bytes).unwrap();
    let first = read_frame(&mut watch).unwrap();
    let resp: Response = serde_json::from_slice(&first).unwrap();
    assert!(resp.ok);
    drop(watch);
    println!("watch_subscribe: first_frame_ok=true bytes={}", first.len());
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

fn spawn_quotad(quotad: &Path, socket: &Path) -> Child {
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
        .expect("spawn quotad")
}

fn stop_child(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn bench_jsonl(history: &Path, iters: usize, cap: usize) {
    let expected_full = load_history_jsonl(history).expect("full jsonl").len();
    let expected_tail = expected_full.min(cap.max(1));
    let mut full = Vec::with_capacity(iters);
    let mut tail = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t0 = Instant::now();
        let rows = load_history_jsonl(history).expect("full jsonl");
        assert_eq!(rows.len(), expected_full);
        full.push(t0.elapsed().as_nanos() as u64);
        let t1 = Instant::now();
        let rows = load_history_jsonl_tail(history, cap).expect("tail jsonl");
        assert_eq!(rows.len(), expected_tail);
        tail.push(t1.elapsed().as_nanos() as u64);
    }
    print_row(&format!("jsonl_full_{expected_full}"), full);
    print_row(&format!("jsonl_tail_{expected_tail}"), tail);
}

fn bench_start(quotad: &Path, socket: &Path, runs: usize) {
    let mut cold = Vec::new();
    let mut warm = Vec::new();
    for i in 0..runs {
        let child = spawn_quotad(quotad, socket);
        let ready = wait_ping(socket, Duration::from_secs(10)).expect("cold start timeout");
        cold.push(ready.as_nanos() as u64);
        if i == 0 {
            // Warm: daemon already up; measure a fresh connect+ping.
            let t0 = Instant::now();
            let mut s = UnixStream::connect(socket).unwrap();
            rpc(&mut s, 1, METHOD_PING, serde_json::Value::Null);
            warm.push(t0.elapsed().as_nanos() as u64);
        } else {
            let t0 = Instant::now();
            let mut s = UnixStream::connect(socket).unwrap();
            rpc(&mut s, 1, METHOD_PING, serde_json::Value::Null);
            warm.push(t0.elapsed().as_nanos() as u64);
        }
        stop_child(child);
        let _ = std::fs::remove_file(socket);
        // Subsequent runs are warm-binary (page cache) but cold-process.
    }
    print_row("daemon_start_to_ping", cold);
    print_row("daemon_warm_ping", warm);
}

fn usage() -> ! {
    eprintln!(
        "quota-bench --socket PATH [--warmup N] [--iters N]\n\
         quota-bench pace [--history PATH] [--iters N]\n\
         quota-bench serialize [--history PATH] [--iters N] [--ring N]\n\
         quota-bench watch --socket PATH [--clients N] [--iters N]\n\
         quota-bench start --quotad PATH --socket PATH [--runs N]\n\
         quota-bench jsonl [--history PATH] [--iters N] [--ring N]"
    );
    std::process::exit(2);
}

fn main() {
    let mut args = env::args().peekable();
    let _bin = args.next();
    let mode = args.peek().cloned().unwrap_or_else(|| "--socket".into());
    match mode.as_str() {
        "pace" => {
            args.next();
            let history = parse_path(&mut args, "--history").unwrap_or_else(default_history);
            let iters = parse_usize(&mut args, "--iters", 200);
            bench_pace(&history, iters);
        }
        "serialize" => {
            args.next();
            let history = parse_path(&mut args, "--history").unwrap_or_else(default_history);
            let iters = parse_usize(&mut args, "--iters", 200);
            let ring = parse_usize(&mut args, "--ring", 128);
            bench_serialize(&history, iters, ring);
        }
        "watch" => {
            args.next();
            let socket = parse_path(&mut args, "--socket").unwrap_or_else(|| usage());
            let clients = parse_usize(&mut args, "--clients", 8);
            let iters = parse_usize(&mut args, "--iters", 200);
            bench_watch(&socket, clients, iters);
        }
        "jsonl" => {
            args.next();
            let history = parse_path(&mut args, "--history").unwrap_or_else(default_history);
            let iters = parse_usize(&mut args, "--iters", 50);
            let cap = parse_usize(&mut args, "--ring", 128);
            bench_jsonl(&history, iters, cap);
        }
        "start" => {
            args.next();
            let quotad = parse_path(&mut args, "--quotad").unwrap_or_else(|| usage());
            let socket = parse_path(&mut args, "--socket").unwrap_or_else(|| usage());
            let runs = parse_usize(&mut args, "--runs", 8);
            bench_start(&quotad, &socket, runs);
        }
        "--help" | "-h" => usage(),
        _ => {
            let mut socket = None;
            let mut warmup = 100usize;
            let mut iters = 1000usize;
            while let Some(a) = args.next() {
                match a.as_str() {
                    "--socket" => {
                        socket = Some(PathBuf::from(args.next().expect("--socket needs a path")));
                    }
                    "--warmup" => {
                        warmup = args.next().expect("--warmup needs N").parse().expect("n");
                    }
                    "--iters" => {
                        iters = args.next().expect("--iters needs N").parse().expect("n");
                    }
                    other => panic!("unknown arg {other}"),
                }
            }
            let socket = socket.expect("quota-bench --socket PATH");
            bench_rtt(&socket, warmup, iters);
        }
    }
}
