# Benchmarks

Numbers below were measured on this VM. They are not SLOs and are not
portable. Nothing here is an estimate. Rows marked **VERIFIED** were
stopwatch-timed or read from `/proc` / `stat` in this session.

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
| RAM | 15 GiB, **0** swap (`free -h` at 2026-09-27 12:00 UTC: ~8.9 GiB used, ~6.8 GiB available) |
| Tools | Python 3 `time.perf_counter` (hyperfine and GNU `time` **not installed**; `apt-get` lock not writable). `quota-bench` for socket / pace / serialize / start. |

Default release profile (`Cargo.toml`): `lto = "thin"`, `codegen-units = 1`,
`strip = "debuginfo"`, `panic = "abort"`, `opt-level = "s"`.

Command that produced the product binaries: `cargo build --workspace --release`.

Optional size profile (not default): `[profile.dist]` inherits release with
`lto = "fat"` and `strip = "symbols"`. Tradeoff: smaller on-disk binaries,
longer compile, **no `.symtab`** (harder field debug). Default `release` is
unchanged.

---

## 2026-09-27 hardening pass (VERIFIED)

Same VM class as the earlier 2026-09-27 tables below (`uname` / `rustc` /
4× Xeon @ 2400 MHz / 15 GiB). Measured after the socket/creds/JSONL
hardening commit on this branch. Nothing here is an estimate.

**Claimed win (only this one):** CodexBar JSONL seed now streams a 128-line
tail and parses those rows only.

```
./target/release/quota-bench jsonl --iters 50 --ring 128
```

| name | n | mean | std | min | max | p50 | p95 | p99 |
|------|--:|------|-----|-----|-----|-----|-----|-----|
| `jsonl_full_1912` (parse every line) | 50 | 839.5 µs | 51.8 µs | 813.8 µs | 1123.1 µs | 822.2 µs | 933.5 µs | 1123.1 µs |
| `jsonl_tail_128` (keep last 128 lines, parse those) | 50 | 218.2 µs | 10.8 µs | 210.1 µs | 265.4 µs | 215.3 µs | 251.8 µs | 265.4 µs |

Delta: **−621.3 µs mean (−74%)**. Same fixture, same binary, alternating
full vs tail in one process. This is the daemon seed path
(`load_history_snapshots_from_dir`).

**Not a size win.** Default `release` binaries are slightly larger than
the pre-hardening 2026-09-27 build (more socket/path checks). Do not
read this as a regression of `[profile.dist]`.

`ls -lh --full-time` / `stat` after `cargo build --workspace --release`
(2026-09-27 12:21 UTC):

| path | bytes | vs pre-hardening release |
|------|------:|--------------------------|
| `target/release/quota` | 1 060 528 | +8 416 |
| `target/release/quotad` | 2 348 360 | +89 912 |
| `target/release/quota-ctl` | 1 163 184 | +4 480 |

`file`: ELF 64-bit LSB pie, x86-64, dynamically linked, **not stripped**.
`ldd` (quotad): `linux-vdso`, `libgcc_s.so.1`, `libc.so.6`,
`ld-linux-x86-64.so.2`.

`sha256sum`:

```
fd18cc1c793f73394e65091816a981d63ef9279cd5b20bdad4a52931b8563b61  target/release/quota
8c41cf1535353bdf383f37d90cd05f51a56398087b9098bbf80670e377f23114  target/release/quotad
f1b496a94e255d36fda4d3a9033cad64bc069834eee88a538a694c1ad5760660  target/release/quota-ctl
```

### Same-band re-measure (VERIFIED, not claimed as wins)

`pace` / serialize / socket RTT / 8-client status / start-to-ping stay in
the same band as the earlier 2026-09-27 session. `pace` mean 6.5 µs vs
5.9 µs; `status` socket 24.2 µs vs 20.9 µs; start-to-ping 5361 µs vs
5346 µs. Those deltas are **not** treated as regressions or improvements
(noisy µs on a shared VM). `history_ref` removes clones on the daemon
`pace` RPC path; this offline `pace_for` bench still owns a `Vec` of
1912 snapshots and does not isolate that change.

| name | n | mean | notes |
|------|--:|------|-------|
| `pace` (1912-row fixture) | 200 | 6.5 µs | burn 12.9865 %/h; 153 071 / s |
| `status_serialize` | 200 | 0.8 µs | same as prior |
| `ring_replay_1912` | 200 | 204.0 µs | bench `Vec::remove(0)`, not `Store` |
| socket `ping` | 1000 | 9.5 µs | prior 9.6 µs |
| socket `status` | 1000 | 24.2 µs | prior 20.9 µs |
| `status_clients_8` | 1600 | 60.6 µs | prior 59.3 µs |
| `daemon_start_to_ping` | 8 | 5361 µs | no CodexBar dir |
| `daemon_start_to_ping` + `QUOTA_CODEXBAR_DIR=fixtures/codexbar` | 8 | 5422 µs | seeds 128 history snaps |
| idle `quotad` VmRSS | — | 3276 kB | prior 3224 kB; USS 1496 kB; 2 threads |

**Not measured:** live HTTPS usage-fetch, musl, `profile.dist` on this
exact commit (prior dist −25% still applies to the *profile*, not these
byte counts).

---

## 2026-09-27 re-measure (VERIFIED, pre-hardening)

Same class of host as the 2026-09-26 session (see historical tables below).
Binaries include `quota-ctl` and the accounts/refresh protocol.

### 1. Release binaries (VERIFIED)

`ls -lh --full-time` / `stat` after `cargo build --workspace --release`
(2026-09-27 11:59 UTC):

| path | bytes | `ls -lh` | mtime (UTC) |
|------|------:|----------|-------------|
| `target/release/quota` | 1 052 112 | 1.1M | 2026-09-27 11:59:05 |
| `target/release/quotad` | 2 258 448 | 2.2M | 2026-09-27 11:59:07 |
| `target/release/quota-ctl` | 1 158 704 | 1.2M | 2026-09-27 11:59:06 |

`file`: ELF 64-bit LSB pie, x86-64, dynamically linked, **not stripped**
(`file` wording). `readelf -S`: **no** `.debug*` sections; **all three have
`.symtab`**. Matches `strip = "debuginfo"`.

`sha256sum`:

```
1321131a66fcea925e997083dbd0b235938f0f7bb2858df813d27b9391d0263a  target/release/quota
1bf54fd5b1a623ee2ecd6def1da2967baf2f63c7455d6278c8b27691e053eba5  target/release/quotad
459b3a1741ad8556585f9ebcfbc4834edef417a5cb31a0adfdaaff1b43d5f176  target/release/quota-ctl
```

`ldd` (quota and quotad): `linux-vdso`, `libgcc_s.so.1`, `libc.so.6`,
`ld-linux-x86-64.so.2`. rustls/ring statically linked.

Harness `target/release/quota-bench`: 737 432 bytes (not a product binary).

### 1b. Optional `dist` size pass (VERIFIED)

`cargo build --workspace --profile dist --bins` (19.17 s compile on this
host). `file` says **stripped** (no `.symtab`).

| path | bytes | vs default release |
|------|------:|--------------------|
| `target/dist/quota` | 776 616 | −275 496 (−26%) |
| `target/dist/quotad` | 1 686 096 | −572 352 (−25%) |
| `target/dist/quota-ctl` | 780 712 | −377 992 (−33%) |

**Not measured:** `x86_64-unknown-linux-musl` (target not installed). Do not
invent a musl size.

### 2. Startup (`quota` / `quota-ctl` process spawn + one RPC) (VERIFIED)

`quotad` already running on `/tmp/quota-bench.sock`. Each sample is a new
process: exec, connect, one framed RPC, print, exit.

No hyperfine. Python `time.perf_counter` around `subprocess.run` (no shell),
warmup 5, **80** runs (40 for `quota-ctl`). Page cache **warm**.

| command | n | mean ± std | min | max | p50 | p95 | p99 |
|---------|--:|------------|-----|-----|-----|-----|-----|
| `quota --socket … version` | 80 | 628.5 ± 137.3 µs | 488.8 µs | 1063.2 µs | 579.8 µs | 907.5 µs | 1038.1 µs |
| `quota --socket … ping` | 80 | 576.2 ± 114.6 µs | 476.1 µs | 821.9 µs | 513.8 µs | 762.7 µs | 777.1 µs |
| `quota-ctl --socket … ping` | 40 | 618.5 ± 113.5 µs | 499.3 µs | 807.7 µs | 553.7 µs | 788.2 µs | 807.7 µs |

Percentiles: nearest-rank on the sorted `perf_counter` samples.

`/proc/sys/vm/drop_caches` is **Permission denied** on this VM. No new
cold-page-fault distribution.

### 3. Socket RTT (persistent connection) (VERIFIED)

```
./target/release/quota-bench --socket /tmp/quota-bench.sock --warmup 100 --iters 1000
```

| method | n | mean | stddev | min | max | p50 | p95 | p99 |
|--------|--:|------|--------|-----|-----|-----|-----|-----|
| `ping` | 1000 | 9.6 µs | 1.9 µs | 7.5 µs | 36.4 µs | 9.1 µs | 11.8 µs | 14.7 µs |
| `status` | 1000 | 20.9 µs | 6.9 µs | 17.6 µs | 233.2 µs | 20.6 µs | 20.9 µs | 23.1 µs |

`status` payload: two `unavailable` providers. This is **not** CLI spawn
time (that is §2).

### 4. RSS (VERIFIED)

`quotad run --socket /tmp/quota-bench.sock`. First probe completed (both
providers `unavailable`). After a successful `quota ping`:

| metric | value | source |
|--------|------:|--------|
| VmRSS | 3224 kB | `/proc/<pid>/status` |
| RssAnon | 260 kB | same |
| RssFile | 2964 kB | same |
| Threads | 2 | same |
| PSS | 1530 kB | `/proc/<pid>/smaps_rollup` |
| USS (`Private_Clean` + `Private_Dirty`) | 1456 kB | 0 + 1456 kB |

`ps -o rss` agreed: **3224**. Unchanged after `quota-ctl ping` /
`accounts list` / `refresh` (still 3224 / 260 / 2964 / 2 threads).

#### CLI one-shot RSS (VERIFIED, `/proc` poll)

GNU `time -v` is not installed. Polled `/proc/<pid>/status` `VmHWM` while
the child ran (20 samples, warm cache):

| command | observed VmHWM (kB) |
|---------|---------------------|
| `quota version` | 2532–2664 (first five: 2664, 2656, 2592, 2656, 2564) |
| `quota status` | 2472–2620 (first five: 2620, 2492, 2544, 2596, 2472) |
| `quota-ctl ping` | 2516–2664 (first five: 2652, 2664, 2664, 2516, 2616) |

`os.wait4` `ru_maxrss` on the same children reported ~13824 kB — **not**
used here; it does not match `/proc` VmHWM / the 2026-09-26 GNU `time`
method.

### 5. Fixture-driven pace math (VERIFIED)

Offline. No HTTP. 1912-row `fixtures/codexbar/usage-history.redacted.jsonl`.

```
./target/release/quota-bench pace --iters 200
```

| | |
|--|--|
| rows | 1912 |
| first `pace_for` | 26 378 ns |
| last-row burn | 12.9865 %/h |
| history samples used | 1912 |
| 200-iter mean | 5.9 µs (std 1.1, min 5.8, max 20.9, p50 5.8, p95 5.9, p99 6.1) |
| throughput | 169 091 `pace_for` / s |

Honest result: percent-only window. `can_start --tokens 50000` stays
`basis: percent_only` (see adapter tests). This is **not** a token budget.

### 6. Ring-buffer / status serialization (VERIFIED)

```
./target/release/quota-bench serialize --iters 200 --ring 128
```

| name | n | mean | std | min | max | p50 | p95 | p99 |
|------|--:|------|-----|-----|-----|-----|-----|-----|
| `status_serialize` (JSON + length prefix of latest snapshot) | 200 | 0.8 µs | 0.7 µs | 0.7 µs | 10.1 µs | 0.7 µs | 0.8 µs | 1.3 µs |
| `ring_replay_1912` (push 1912 snaps through a 128-cap `Vec`) | 200 | 199.4 µs | 4.7 µs | 196.0 µs | 232.0 µs | 198.3 µs | 206.4 µs | 224.8 µs |

### 7. Many watch/status clients (VERIFIED)

Daemon up. Eight threads, each 200 `status` RPCs on its own `UnixStream`,
plus one `watch` subscribe (first frame 601 bytes, `ok`).

```
./target/release/quota-bench watch --socket /tmp/quota-bench.sock --clients 8 --iters 200
```

| name | n | mean | std | min | max | p50 | p95 | p99 |
|------|--:|------|-----|-----|-----|-----|-----|-----|
| `status_clients_8` | 1600 | 59.3 µs | 19.8 µs | 20.1 µs | 519.4 µs | 58.3 µs | 61.7 µs | 72.2 µs |

Tokio `current_thread` serializes accept/RPC; eight concurrent clients
raise per-RPC latency vs the single-connection §3 `status` (~21 µs).

### 8. Cold process start vs warm ping (VERIFIED)

```
./target/release/quota-bench start --quotad ./target/release/quotad --socket /tmp/quota-start.sock --runs 8
```

Each run: spawn `quotad`, time until `ping` succeeds, then one extra ping
on the live daemon, then kill. Binaries already in page cache after the
release build. First probe is local FS misses only.

| name | n | mean | std | min | max | p50 | p95 | p99 |
|------|--:|------|-----|-----|-----|-----|-----|-----|
| `daemon_start_to_ping` (process + bind + first probe + ping) | 8 | 5346 µs | 110 µs | 5173 µs | 5518 µs | 5372 µs | 5518 µs | 5518 µs |
| `daemon_warm_ping` (daemon already up, new connect) | 8 | 131 µs | 118 µs | 34 µs | 285 µs | 44 µs | 285 µs | 285 µs |

No disk-cache drop (permission denied). These are **warm-binary, cold-process**
starts, not a machine-cold boot.

### 9. Adaptive refresh (from code, not timed)

Unchanged defaults (`quota-core`):

| knob | default | clamp |
|------|--------:|-------|
| `refresh_min_secs` | 30 | 5 … 3600 |
| `refresh_max_secs` | 300 | ≥ min … 86400 |
| `http_timeout_secs` | 10 | (HTTPS timeout; **not exercised** here) |
| ring capacity | 128 | 8 … 4096 |

On this host both adapters were unavailable, so after the first refresh the
code would set `30 * 2 = 60` seconds. That backoff was **not** stopwatch-timed.

---

## Historical: 2026-09-26 (kept)

The tables that shipped with the first BENCHMARKS pass. Same machine class.
Product binaries then: `quota` 1 051 768 B, `quotad` 2 195 312 B (no
`quota-ctl`). Replaced for day-to-day numbers by the 2026-09-27 section;
kept so the original CLI-spawn / RTT / RSS write-up is not deleted.

### Release binaries (2026-09-26)

| path | bytes | `ls -lh` | mtime (UTC) |
|------|------:|----------|-------------|
| `target/release/quota` | 1 051 768 | 1.1M | 2026-09-26 17:20:26 |
| `target/release/quotad` | 2 195 312 | 2.1M | 2026-09-26 17:20:48 |

`sha256sum` then:
`061470e92c531d355c248b703328090ca34381df01c0ae8e2de5c6ccb4899273` (`quota`),
`a49420c32c8d5b31d8e1a6c25e72405c89e1028c544bda44a0cf35209e19912c` (`quotad`).

### CLI spawn via hyperfine `-N` (2026-09-26)

| command | n | mean ± std | min | max | p50 | p95 | p99 |
|---------|--:|------------|-----|-----|-----|-----|-----|
| `quota --socket … version` | 80 | 613.5 ± 123.8 µs | 490.9 µs | 931.2 µs | 545.2 µs | 835.8 µs | 905.1 µs |
| `quota --socket … ping` | 80 | 635.3 ± 132.6 µs | 483.4 µs | 954.0 µs | 584.6 µs | 878.2 µs | 948.5 µs |

One `drop_caches` sample then: `major_faults=6`, Python wall 0.003913 s.
Not repeatable here (no drop_caches permission).

### Socket RTT (2026-09-26)

| method | n | mean | stddev | p50 | p95 | p99 |
|--------|--:|------|--------|-----|-----|-----|
| `ping` | 1000 | 11.7 µs | 5.7 µs | 11.9 µs | 15.5 µs | 16.2 µs |
| `status` | 1000 | 20.9 µs | 1.1 µs | 20.6 µs | 24.1 µs | 25.0 µs |

### RSS (2026-09-26)

Idle after first ping: VmRSS **3156** kB, RssAnon 240, RssFile 2916,
Threads 2, PSS 1452, USS 1384. After RTT harness: VmRSS 3360 kB.
GNU `time -v`: `quota version` 2576 kB, `quota status` 2660 kB.

---

## How to reproduce

```bash
cargo build --workspace --release
file target/release/quota target/release/quotad target/release/quota-ctl
ls -lh target/release/quota target/release/quotad target/release/quota-ctl

./target/release/quota-bench pace --iters 200
./target/release/quota-bench serialize --iters 200 --ring 128
./target/release/quota-bench jsonl --iters 50 --ring 128

SOCK=/tmp/quota-bench.sock
./target/release/quotad run --socket "$SOCK" &
# wait until `quota --socket "$SOCK" ping` succeeds

./target/release/quota-bench --socket "$SOCK" --warmup 100 --iters 1000
./target/release/quota-bench watch --socket "$SOCK" --clients 8 --iters 200
./target/release/quota-bench start --quotad ./target/release/quotad --socket /tmp/quota-start.sock --runs 8

# optional size pass (does not change default release)
cargo build --workspace --profile dist --bins

# RSS
PID=$(pidof quotad)
grep -E '^(VmRSS|RssAnon|RssFile|Threads)' /proc/$PID/status
cat /proc/$PID/smaps_rollup
```

`quota-bench` percentiles use nearest-rank: `round(p * (n-1))` on a sorted
sample vector.
