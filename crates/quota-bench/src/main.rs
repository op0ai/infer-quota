//! Persistent-connection Unix-socket RTT harness.
//!
//! Measures framed JSON RPC on an already-open socket (not process spawn).
//! Usage: quota-bench --socket PATH [--warmup N] [--iters N]

use std::env;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Instant;

use quota_core::framing::{read_frame, write_frame};
use quota_core::protocol::{
    ProviderFilter, Request, Response, StatusParams, METHOD_PING, METHOD_STATUS,
};

fn parse_args() -> (PathBuf, usize, usize) {
    let mut socket = None;
    let mut warmup = 100usize;
    let mut iters = 1000usize;
    let mut args = env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--socket" => {
                socket = Some(PathBuf::from(args.next().expect("--socket needs a path")));
            }
            "--warmup" => {
                warmup = args
                    .next()
                    .expect("--warmup needs N")
                    .parse()
                    .expect("warmup");
            }
            "--iters" => {
                iters = args
                    .next()
                    .expect("--iters needs N")
                    .parse()
                    .expect("iters");
            }
            other => panic!("unknown arg {other}"),
        }
    }
    let socket = socket.unwrap_or_else(|| {
        panic!("quota-bench --socket PATH [--warmup 100] [--iters 1000]");
    });
    (socket, warmup, iters)
}

fn rpc(stream: &mut UnixStream, id: u64, method: &str, params: serde_json::Value) {
    let req = Request::with_params(id, method, params);
    let bytes = serde_json::to_vec(&req).expect("serialize");
    write_frame(stream, &bytes).expect("write");
    let payload = read_frame(stream).expect("read");
    let resp: Response = serde_json::from_slice(&payload).expect("decode");
    assert!(resp.ok, "rpc {method} failed: {resp:?}");
}

fn stats(ns: &mut [u64]) -> (f64, f64, f64, f64, f64) {
    ns.sort_unstable();
    let n = ns.len();
    let mean = ns.iter().sum::<u64>() as f64 / n as f64;
    let var = ns
        .iter()
        .map(|&x| {
            let d = x as f64 - mean;
            d * d
        })
        .sum::<f64>()
        / n as f64;
    let stddev = var.sqrt();
    let pct = |p: f64| -> f64 {
        if n == 0 {
            return 0.0;
        }
        let idx = ((p * (n as f64 - 1.0)).round() as usize).min(n - 1);
        ns[idx] as f64
    };
    (mean, stddev, pct(0.50), pct(0.95), pct(0.99))
}

fn bench(
    stream: &mut UnixStream,
    method: &str,
    warmup: usize,
    iters: usize,
    mut make_params: impl FnMut() -> serde_json::Value,
) -> Vec<u64> {
    for i in 0..warmup {
        rpc(stream, i as u64, method, make_params());
    }
    let mut samples = Vec::with_capacity(iters);
    for i in 0..iters {
        let t0 = Instant::now();
        rpc(stream, (warmup + i) as u64, method, make_params());
        samples.push(t0.elapsed().as_nanos() as u64);
    }
    samples
}

fn print_row(name: &str, mut samples: Vec<u64>) {
    let (mean, std, p50, p95, p99) = stats(&mut samples);
    println!(
        "{name}: n={} mean={:.1}us std={:.1}us p50={:.1}us p95={:.1}us p99={:.1}us",
        samples.len(),
        mean / 1000.0,
        std / 1000.0,
        p50 / 1000.0,
        p95 / 1000.0,
        p99 / 1000.0
    );
}

fn main() {
    let (socket, warmup, iters) = parse_args();
    let path = Path::new(&socket);
    let mut stream = UnixStream::connect(path).unwrap_or_else(|e| {
        panic!("connect {}: {e}", path.display());
    });
    let ping = bench(&mut stream, METHOD_PING, warmup, iters, || {
        serde_json::Value::Null
    });
    print_row("ping", ping);

    let status = bench(&mut stream, METHOD_STATUS, warmup, iters, || {
        serde_json::to_value(StatusParams {
            provider: ProviderFilter::All,
        })
        .expect("params")
    });
    print_row("status", status);
}
