# Benchmarks

Numbers below were measured on one machine on **2026-09-26**. They are not
SLOs and are not portable. Nothing here is an estimate.

**Not measured:** live Codex/Claude HTTPS usage-fetch latency. This host had
no `~/.codex/auth.json` and no `~/.claude/.credentials.json`. Adapter probes
returned `unavailable` / `no_credentials` after local `stat`/`open` failures.
Do not read any millisecond figure in this file as “time to talk to OpenAI or
Anthropic.”

## Machine

| | |
|--|--|
| `uname -a` | `Linux cursor 6.12.94+ #1 SMP PREEMPT_DYNAMIC Thu Sep 24 16:04:37 UTC 2026 x86_64 x86_64 x86_64 GNU/Linux` |
| `rustc -vV` | `rustc 1.83.0 (90b35a623 2024-11-26)`, host `x86_64-unknown-linux-gnu`, LLVM 19.1.1 |
| `cargo -V` | `cargo 1.83.0 (5ffbef321 2024-10-29)` |
| CPU | 4× `Intel(R) Xeon(R) Processor` @ 2400 MHz (`siblings=4`, `cpu cores=4`) |
| RAM | 15 GiB, **0** swap (`free -h` at measure time: ~9.0 GiB used, ~6.7 GiB available) |
| Tools | `hyperfine 1.18.0`, GNU `time 1.9` |

Release profile used (`Cargo.toml`): `lto = "thin"`, `codegen-units = 1`,
`strip = "debuginfo"`, `panic = "abort"`, `opt-level = "s"`.

Command that produced the binaries: `cargo build --workspace --release`.

## 1. Release binaries

`ls -lh --full-time` and `stat` after that build:

| path | bytes | `ls -lh` | mtime (UTC) |
|------|------:|----------|-------------|
| `target/release/quota` | 1 051 768 | 1.1M | 2026-09-26 17:20:26 |
| `target/release/quotad` | 2 195 312 | 2.1M | 2026-09-26 17:20:48 |

`file`:

```
target/release/quota:  ELF 64-bit LSB pie executable, x86-64, version 1 (SYSV), dynamically linked, interpreter /lib64/ld-linux-x86-64.so.2, BuildID[sha1]=5719bb99959518301990bd459f28d8390045a478, for GNU/Linux 3.2.0, not stripped
target/release/quotad: ELF 64-bit LSB pie executable, x86-64, version 1 (SYSV), dynamically linked, interpreter /lib64/ld-linux-x86-64.so.2, BuildID[sha1]=79d476263fbba6dbbbe492302160ee6c5a741a09, for GNU/Linux 3.2.0, not stripped
```

Strip status (matches `strip = "debuginfo"`):

- `readelf -S`: **no** `.debug*` sections on either binary
- `readelf -S`: **both have `.symtab`** — `file` therefore says `not stripped`
- this is **not** a full `strip` (`strip = true` / `strip -s`)

`sha256sum`:

```
061470e92c531d355c248b703328090ca34381df01c0ae8e2de5c6ccb4899273  target/release/quota
a49420c32c8d5b31d8e1a6c25e72405c89e1028c544bda44a0cf35209e19912c  target/release/quotad
```

`ldd` (both): `linux-vdso`, `libgcc_s.so.1`, `libc.so.6`, `ld-linux-x86-64.so.2`.
No extra TLS/HTTP shared libs — rustls and ring are statically linked.

The RTT harness `target/release/quota-bench` is **not** a product binary
(546 816 bytes, also `not stripped` / no `.debug*`). It lives in
`crates/quota-bench`.

## 2. Startup (`quota` process spawn + one RPC)

`quotad` was already running on `/tmp/quota-bench.sock` (see §4). Each sample
is a new `quota` process: exec, connect, one framed RPC, print, exit.

`hyperfine` warned that sub-5 ms commands are noisy if the shell is included.
Primary numbers use **`hyperfine -N` (`--shell=none`)**, warmup 5, **80** runs.

| command | n | mean ± std | min | max | p50 | p95 | p99 |
|---------|--:|------------|-----|-----|-----|-----|-----|
| `quota --socket … version` | 80 | 613.5 ± 123.8 µs | 490.9 µs | 931.2 µs | 545.2 µs | 835.8 µs | 905.1 µs |
| `quota --socket … ping` | 80 | 635.3 ± 132.6 µs | 483.4 µs | 954.0 µs | 584.6 µs | 878.2 µs | 948.5 µs |

Percentiles are nearest-rank on hyperfine’s `times` array.

Page cache was **warm** for those 80-run series (`Major page faults: 0` on a
follow-up `/usr/bin/time -v` of `quota version`).

### One page-cache-drop sample (not a distribution)

`sync` + `echo 3 > /proc/sys/vm/drop_caches`, then one `quota version`:

| source | what |
|--------|------|
| `/usr/bin/time -f` | `elapsed_sec=0.00` (centisecond resolution), `major_faults=6`, `minor_faults=110`, `max_rss_kb=2500` |
| `time.perf_counter` around `subprocess.run` (a second drop) | `wall_sec=0.003913`, child `majflt=6` |

That is **one** cold-ish exec, not a mean.

## 3. Socket RTT (persistent connection)

Harness: `crates/quota-bench` — one `UnixStream`, length-prefixed JSON, same
decode path as the CLI (`write_frame` / `read_frame` / `serde_json`). Timer is
`std::time::Instant` around write + read + JSON parse of the response.
**100 warmup + 1000 measured** RPCs per method. Percentiles: nearest-rank on
sorted nanosecond samples, printed as microseconds.

```
./target/release/quota-bench --socket /tmp/quota-bench.sock --warmup 100 --iters 1000
```

| method | n | mean | stddev | p50 | p95 | p99 |
|--------|--:|------|--------|-----|-----|-----|
| `ping` | 1000 | 11.7 µs | 5.7 µs | 11.9 µs | 15.5 µs | 16.2 µs |
| `status` | 1000 | 20.9 µs | 1.1 µs | 20.6 µs | 24.1 µs | 25.0 µs |

`status` is larger because the payload includes the current snapshot (two
`unavailable` providers on this host). This is **not** CLI process-spawn time
(that is §2).

## 4. RSS

`quotad run --socket /tmp/quota-bench.sock`. First probe completed (both
providers `unavailable`). Process `S (sleeping)`.

**Idle, ~0.5 s after first successful `quota ping`:**

| metric | value | source |
|--------|------:|--------|
| VmRSS | 3156 kB | `/proc/<pid>/status` |
| RssAnon | 240 kB | same |
| RssFile | 2916 kB | same |
| Threads | 2 | same (Tokio current-thread + a leftover blocking-pool thread after the first `spawn_blocking` probe) |
| PSS | 1452 kB | `/proc/<pid>/smaps_rollup` |
| USS (`Private_Clean` + `Private_Dirty`) | 1384 kB | 1144 + 240 kB |

`ps -o rss` agreed: **3156**.

**After the RTT harness (still idle, no extra providers):**

| metric | value |
|--------|------:|
| VmRSS | 3360 kB |
| RssAnon | 252 kB |
| RssFile | 3108 kB |
| Threads | 1 |
| PSS | 1475 kB |
| USS | 1396 kB (1144 + 252) |

### `quota` one-shot (GNU `time -v`, warm cache)

| command | Maximum resident set size |
|---------|---------------------------|
| `quota version` | 2576 kB |
| `quota status` | 2660 kB |

`Elapsed (wall clock)` printed `0:00.00` — timer quantum is too coarse for
sub-10 ms; use §2 for client latency.

## 5. Adaptive refresh (from code, not timed)

Defaults in `quota-core` (`DEFAULT_REFRESH_MIN_SECS` / `DEFAULT_REFRESH_MAX_SECS`
and `Config::refresh_*` clamps):

| knob | default | clamp |
|------|--------:|-------|
| `refresh_min_secs` | 30 | 5 … 3600 |
| `refresh_max_secs` | 300 | ≥ min … 86400 |
| `http_timeout_secs` | 10 | (used as the HTTPS client timeout; **not exercised** here) |
| ring capacity | 128 | 8 … 4096 |

Daemon behavior (`quotad/src/daemon.rs`, `refresh`):

- starts at `refresh_min_secs()` (**30 s**)
- HTTP **429** (`error.code == "rate_limited"`) → jump to **max** (300 s)
- no provider `ok` → `interval * 2`, clamped to [min, max]
- usage **changed** → back to **min** (30 s)
- usage **stable** → `interval * 3/2`, clamped to [min, max]

On this host both adapters were unavailable, so after the first refresh the
code would set `30 * 2 = 60` seconds. That backoff was **not** stopwatch-timed.

Probe work here was local filesystem misses only. **No** live adapter RTT.

## 6. How to reproduce

```bash
cargo build --workspace --release
file target/release/quota target/release/quotad
ls -lh target/release/quota target/release/quotad

SOCK=/tmp/quota-bench.sock
./target/release/quotad run --socket "$SOCK" &
# wait until `quota --socket "$SOCK" ping` succeeds

hyperfine -N --warmup 5 --runs 80 -- \
  "./target/release/quota --socket $SOCK version"
hyperfine -N --warmup 5 --runs 80 -- \
  "./target/release/quota --socket $SOCK ping"

./target/release/quota-bench --socket "$SOCK" --warmup 100 --iters 1000

# RSS
PID=$(pidof quotad)   # or the background PID
grep -E '^(VmRSS|RssAnon|RssFile|Threads)' /proc/$PID/status
cat /proc/$PID/smaps_rollup
```

`quota-bench` percentiles use nearest-rank: `round(p * (n-1))` on a sorted
sample vector.
