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
| Tools | Python 3 `time.perf_counter` (hyperfine and GNU `time` **not installed**). This session: `sudo apt-get install musl-tools` succeeded (`musl-gcc` present). `quota-bench` for socket / pace / serialize / start. |

Default release profile (`Cargo.toml`): `lto = "thin"`, `codegen-units = 1`,
`strip = "debuginfo"`, `panic = "abort"`, `opt-level = "s"`.

Command that produced the product binaries: `cargo build --workspace --release`.

Optional size profile (not default): `[profile.dist]` inherits release with
`lto = "fat"` and `strip = "symbols"`. Tradeoff: smaller on-disk binaries,
longer compile, **no `.symtab`** (harder field debug). Default `release` is
unchanged.

`opt-level = "z"` was measured on this host and **rejected**: every product
binary grew vs `"s"` (quota +39 768, quotad +193 368, quota-ctl +306 432).

---

## 2026-09-27 14:12 UTC Pareto pass (VERIFIED)

Same VM class as the 13:16 UTC row (`uname` / `rustc` 1.83.0 / 4× Xeon @
2400 MHz / 15 GiB, 0 swap; `free -h` at 14:12 UTC: ~8.9 GiB used, ~6.7 GiB
available). Tree is `63a66a3` plus this pass. Nothing here is an estimate.

`x86_64-unknown-linux-musl` **is installed** on this host (`rustup target
add` + `musl-gcc` from `musl-tools`). Musl sizes are VERIFIED below.
`/proc/sys/vm/drop_caches` is still Permission denied. No live
`~/.codex/auth.json` / `~/.claude/.credentials.json`.

### Kept changes (measured)

| change | axis | before → after | trade? |
|--------|------|----------------|--------|
| `kill -0` via `Command` → `rustix::process::test_kill_process`; first probe on the accept-loop thread; Tokio blocking pool `max_blocking_threads=1`, `keep_alive=1ms`; Linux `SO_PEERCRED`; watch write 15s + idle 600s | **quotad** release size | 2 416 032 → **2 377 696** (−38 336, −1.6%) | net smaller. Peercred + watch timers did not offset the `Command` drop. |
| same | **quotad** dist | 1 792 592 → **1 759 832** (−32 760) | same |
| same | **quotad** musl release | 2 560 544 → **2 513 728** (−46 816) | same. Baseline musl taken on `63a66a3` this session, then rebuilt. |
| first probe sync + 1ms keep-alive | idle **Threads** | 2 → **1** (immediately after first `quota ping`) | **kept:** `max_blocking_threads=1` serializes scheduled vs client `refresh` HTTPS. Do not bump the pool without re-measuring Threads/VSZ. |
| same | idle **VmRSS** | 3308 → **3252** kB (−56). RssAnon 252 → 236. VSZ 72392 → **4752** (no parked blocking-thread stack). | none |
| unused `serde`/`thiserror` on `quota` / `quota-ctl` | quota release | 1 061 216 → 1 061 216 (0) | lock hygiene only |
| Linux `SO_PEERCRED` + watch timeouts | socket RTT | see §3. Three repeats: `status` mean **21.2** µs. This-host baseline 22.6 µs (one noisier run). #4 docs 21.5 µs. **Not claimed as a latency win.** | cost in the noise |

**Rejected (measured, not kept):** `opt-level = "z"` (all three bins larger).
Fat-LTO / `strip = "symbols"` as default `release` still trades compile time
and `.symtab`. Feature-gating rustls out of `quotad` drops HTTPS probes.
Default-off keychain on `quota-ctl` drops the OS backend.

`quota-ctl` release 4 096 608 → 4 098 288 (+1 680) and dist 2 779 904 →
2 775 816 (−4 088): **not claimed**. No `quota-ctl` code change except
unused-dep rows; treat as LTO noise.

### 1. Release binaries (VERIFIED)

`ls` / `stat` after `cargo build --locked --workspace --release`
(2026-09-27 14:12 UTC):

| path | bytes | vs 13:16 UTC | `ls -lh` |
|------|------:|-------------:|----------|
| `target/release/quota` | 1 061 216 | 0 | 1.1M |
| `target/release/quotad` | 2 377 696 | −38 336 | 2.3M |
| `target/release/quota-ctl` | 4 098 288 | +1 680 | 4.0M |

`file`: ELF 64-bit LSB pie, x86-64, dynamically linked, **not stripped**.
`readelf -S`: no `.debug*`; all three have `.symtab`.

`sha256sum`:

```
1187dfb3c82475fd391298a2c374a7ff8d6c9e8e47722866119da00b42e91dd9  target/release/quota
a6fc28d4d1077c3329cc74c1e9799d853c32a0344a2c8526c191d50fd547672d  target/release/quotad
8b192aeeaf39975ab905273d807a7422fb47de920d2b27d7da858aaecfaab191  target/release/quota-ctl
```

`ldd` (quotad): `linux-vdso`, `libgcc_s.so.1`, `libc.so.6`,
`ld-linux-x86-64.so.2`. No OpenBao/keychain.

Harness `target/release/quota-bench`: 742 968 bytes (unchanged).

### 1b. Optional `dist` size pass (VERIFIED)

`cargo build --locked --workspace --profile dist --bins`. `file`: stripped.

| path | bytes | vs 13:16 UTC dist |
|------|------:|------------------:|
| `target/dist/quota` | 780 712 | 0 |
| `target/dist/quotad` | 1 759 832 | −32 760 |
| `target/dist/quota-ctl` | 2 775 816 | −4 088 (not claimed) |

### 1c. musl (VERIFIED)

`rustup target add x86_64-unknown-linux-musl` + `CC=musl-gcc`
`CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc`.
`file`: ELF 64-bit LSB pie, **static-pie**.

| path | release bytes | dist bytes |
|------|-------------:|-----------:|
| `quota` | 1 170 216 | 870 624 |
| `quotad` | 2 513 728 | 1 874 304 |
| `quota-ctl` | 4 240 384 | 2 898 496 |

Musl release vs glibc release: quota +109 000, quotad +136 032,
quota-ctl +142 096 (static libc). Dist musl vs dist glibc: quota +89 912,
quotad +114 472, quota-ctl +122 680. Expected for static-pie; not a
regression of the glibc lean path.

`63a66a3` musl release (this session, before the pass): quota 1 170 216,
quotad 2 560 544, quota-ctl 4 240 400.

### 2. Fixture pace / serialize / JSONL (VERIFIED)

Same commands as 13:16 UTC. Offline.

| name | n | mean | std | min | max | p50 | p95 | p99 |
|------|--:|------|-----|-----|-----|-----|-----|-----|
| `pace` (1912-row fixture) | 200 | 6.5 µs | 1.2 µs | 6.2 µs | 23.6 µs | 6.3 µs | 6.8 µs | 6.8 µs |
| `status_serialize` | 200 | 0.9 µs | 1.2 µs | 0.7 µs | 15.5 µs | 0.7 µs | 0.8 µs | 2.4 µs |
| `ring_replay_1912` | 200 | 210.9 µs | 11.2 µs | 204.1 µs | 300.7 µs | 206.2 µs | 227.7 µs | 247.7 µs |
| `jsonl_full_1912` | 50 | 934.2 µs | 28.9 µs | 916.0 µs | 1052.8 µs | 923.7 µs | 1020.8 µs | 1052.8 µs |
| `jsonl_tail_128` | 50 | 917.2 µs | 25.5 µs | 902.2 µs | 1019.4 µs | 907.6 µs | 988.6 µs | 1019.4 µs |

Same band as 13:16 UTC. Not claimed as a math win. Burn 12.9865 %/h.

### 3. Socket RTT / many clients / start (VERIFIED)

Isolated `$HOME`. First probe is local FS misses only.

This-host **before** (63a66a3 binaries, 13:59 UTC): ping 12.1 µs / status
22.6 µs (n=1000; status max 432 µs). After the pass, three repeats on the
new `quotad`:

| run | ping mean | status mean |
|----:|-----------|-------------|
| 1 | 11.3 µs | 21.2 µs |
| 2 | 10.6 µs | 21.2 µs |
| 3 | 11.7 µs | 21.2 µs |

| name | n | mean | std | min | max | p50 | p95 | p99 |
|------|--:|------|-----|-----|-----|-----|-----|-----|
| socket `ping` (run 2) | 1000 | 10.6 µs | 2.5 µs | 7.8 µs | 44.6 µs | 9.5 µs | 14.5 µs | 17.2 µs |
| socket `status` (run 2) | 1000 | 21.2 µs | 1.8 µs | 19.4 µs | 43.7 µs | 20.8 µs | 23.0 µs | 30.2 µs |
| `status_clients_8` | 1600 | 62.7 µs | 171.2 µs | 11.2 µs | 1555.7 µs | 35.3 µs | 152.1 µs | 1149.9 µs |
| `daemon_start_to_ping` | 8 | 5379 µs | 75 µs | 5283 µs | 5481 µs | 5403 µs | 5481 µs | 5481 µs |
| `daemon_warm_ping` | 8 | 103 µs | 119 µs | 30 µs | 332 µs | 36 µs | 332 µs | 332 µs |

`watch` first frame 661 bytes, `ok`. 8-client row is noisy (p99 1.1 ms on
a shared VM) — **not** claimed. Start-to-ping same band as 13:16 UTC
(5460 µs).

### 4. RSS (VERIFIED)

Idle `quotad` after first `quota ping` (both providers `unavailable`).
First probe ran on the runtime thread; blocking pool not created.

| metric | this-host 63a66a3 | after | source |
|--------|------------------:|------:|--------|
| VmRSS | 3308 kB | **3252** kB | `/proc/<pid>/status` |
| RssAnon | 252 kB | **236** kB | same |
| RssFile | 3056 kB | **3016** kB | same |
| Threads | 2 | **1** | same |
| VSZ | 72392 kB | **4752** kB | `ps -o vsz` |
| PSS | 1548 kB | 1557 kB | `smaps_rollup` |
| USS | 1484 kB | 1492 kB | Private_Clean + Private_Dirty |

`ps -o rss` agreed: **3252**. After the RTT/watch/start harness: VmRSS
3276 kB, Threads still 1. #4 docs idle was 3428 kB / 2 threads on a
busier RSS sample of the same tree class.

`max_blocking_threads=1` is the VSZ/Threads win: scheduled refresh and a
client `refresh` share one `spawn_blocking` slot (HTTPS serializes).
Leave the pool at 1 unless Threads/VSZ are re-measured.

### 5. CLI spawn (VERIFIED)

Python `time.perf_counter` / `subprocess.run`, warmup 5, warm page cache.
`quota` bytes unchanged — treat deltas as host noise.

| command | n | this-host 63a66a3 | after |
|---------|--:|-------------------|-------|
| `quota --socket … version` | 80 | 574.2 ± 110.0 µs | 512.9 ± 73.9 µs |
| `quota --socket … ping` | 80 | 538.4 ± 94.2 µs | 487.3 ± 44.0 µs |
| `quota-ctl --socket … ping` | 40 | 642.3 ± 115.9 µs | 583.9 ± 63.3 µs |

**Not claimed.** Same `quota` inode size; overlap with the 13:16 UTC
table (665 / 635 / 705 µs).

**Not measured:** live HTTPS usage-fetch, cold-page-fault distribution
(`drop_caches` Permission denied).

---

## 2026-09-27 13:16 UTC re-measure (VERIFIED, historical)

Same VM class (`uname` / `rustc` 1.83.0 / 4× Xeon @ 2400 MHz / 15 GiB, 0
swap; `free -h` at 13:14 UTC: ~8.9 GiB used, ~6.8 GiB available). Tree is
PR #1 tip `8b4f132` plus this PR’s tests, `install.sh`, and `docs/AGENT.md`.
Nothing here is an estimate. **Not a size-optimization pass.** Replaced
for day-to-day numbers by the 14:12 UTC Pareto section above.

`x86_64-unknown-linux-musl` was **not installed** in that session. Musl
is VERIFIED in the 14:12 UTC section. `/proc/sys/vm/drop_caches` is
Permission denied.

### 1. Release binaries (VERIFIED)

`ls -lh --full-time` / `stat` after `cargo build --locked --workspace --release`
(2026-09-27 13:13 UTC):

| path | bytes | `ls -lh` | mtime (UTC) |
|------|------:|----------|-------------|
| `target/release/quota` | 1 061 216 | 1.1M | 2026-09-27 13:13:44 |
| `target/release/quotad` | 2 416 032 | 2.4M | 2026-09-27 13:13:43 |
| `target/release/quota-ctl` | 4 096 608 | 4.0M | 2026-09-27 13:13:48 |

`quota-ctl` is large because PR #1 links the real OS keychain
(`secret-service` / zbus) and the rustls OpenBao client. Earlier 1.16 MiB
tables in this file are **pre-keychain**. `quotad` / `quota` still do not
link `quota-secrets`. `--minimal` install skips `quota-ctl`.

`file`: ELF 64-bit LSB pie, x86-64, dynamically linked, **not stripped**.
`readelf -S`: no `.debug*`; all three have `.symtab`. Matches
`strip = "debuginfo"`.

`sha256sum`:

```
93bdd844c9fe2e83864fae421acafdba81748254df37a4a7b23306ccbcc205fd  target/release/quota
382c8938eab59121c55f1ad018181266f2a09fbec4a6ecaafccdf5e03a8d98fd  target/release/quotad
92b8d847bd2ff313066c40f71889639d5b4d88b69773d36b8c37b7b6349d01f9  target/release/quota-ctl
```

`ldd` (quotad): `linux-vdso`, `libgcc_s.so.1`, `libc.so.6`,
`ld-linux-x86-64.so.2`.

Harness `target/release/quota-bench`: 742 968 bytes (not a product binary).

### 1b. Optional `dist` size pass (VERIFIED)

`cargo build --locked --workspace --profile dist --bins` (41.8 s compile).
`file` says **stripped** (no `.symtab`).

| path | bytes | vs default release |
|------|------:|--------------------|
| `target/dist/quota` | 780 712 | −280 504 (−26%) |
| `target/dist/quotad` | 1 792 592 | −623 440 (−26%) |
| `target/dist/quota-ctl` | 2 779 904 | −1 316 704 (−32%) |

### 2. Fixture pace / serialize / JSONL (VERIFIED)

```
./target/release/quota-bench pace --iters 200
./target/release/quota-bench serialize --iters 200 --ring 128
./target/release/quota-bench jsonl --iters 50 --ring 128
```

| name | n | mean | std | min | max | p50 | p95 | p99 |
|------|--:|------|-----|-----|-----|-----|-----|-----|
| `pace` (1912-row fixture) | 200 | 6.5 µs | 1.0 µs | 6.2 µs | 19.8 µs | 6.4 µs | 6.8 µs | 6.9 µs |
| `status_serialize` | 200 | 0.9 µs | 0.6 µs | 0.9 µs | 9.3 µs | 0.9 µs | 1.0 µs | 1.8 µs |
| `ring_replay_1912` | 200 | 209.9 µs | 13.8 µs | 201.0 µs | 289.6 µs | 205.1 µs | 244.5 µs | 260.9 µs |
| `jsonl_full_1912` | 50 | 960.7 µs | 43.1 µs | 934.3 µs | 1174.9 µs | 944.8 µs | 1039.5 µs | 1174.9 µs |
| `jsonl_tail_128` | 50 | 943.9 µs | 40.3 µs | 914.6 µs | 1073.5 µs | 928.8 µs | 1059.3 µs | 1073.5 µs |

`pace` burn 12.9865 %/h; 153 134 / s; first call 21 166 ns. Percent-only
window. `can_start --tokens 50000` stays `basis: percent_only`.

**JSONL tail is not a parse-only win on this tree.** After
`0af8adf` the tail keeps the last 128 **parsed** rows so a partial or
over-long line cannot evict a valid sample. Both paths still read the
whole file through `O_NOFOLLOW` + a 64 KiB line cap. Mean delta
**−16.8 µs (−1.7%)**. The 12:21 UTC −74% table below measured the older
“keep last 128 strings, then parse those” implementation. Do not quote
that −74% for this commit.

### 3. Socket RTT / many clients / start (VERIFIED)

`quotad run --socket /tmp/quota-bench-rebase.sock`. Isolated `$HOME` (no
CLI creds). First probe is local FS misses only.

```
./target/release/quota-bench --socket … --warmup 100 --iters 1000
./target/release/quota-bench watch --socket … --clients 8 --iters 200
./target/release/quota-bench start --quotad ./target/release/quotad --socket /tmp/quota-start-rebase.sock --runs 8
```

| name | n | mean | std | min | max | p50 | p95 | p99 |
|------|--:|------|-----|-----|-----|-----|-----|-----|
| socket `ping` | 1000 | 10.2 µs | 1.9 µs | 8.1 µs | 36.3 µs | 9.6 µs | 12.8 µs | 15.1 µs |
| socket `status` | 1000 | 21.5 µs | 2.2 µs | 20.2 µs | 51.8 µs | 20.9 µs | 24.2 µs | 32.0 µs |
| `status_clients_8` | 1600 | 70.3 µs | 15.2 µs | 21.1 µs | 514.2 µs | 68.9 µs | 80.7 µs | 94.6 µs |
| `daemon_start_to_ping` | 8 | 5460 µs | 141 µs | 5316 µs | 5777 µs | 5460 µs | 5777 µs | 5777 µs |
| `daemon_start_to_ping` + `QUOTA_CODEXBAR_DIR=fixtures/codexbar` | 8 | 5322 µs | 133 µs | 5160 µs | 5526 µs | 5373 µs | 5526 µs | 5526 µs |
| `daemon_warm_ping` | 8 | 184 µs | 152 µs | 34 µs | 407 µs | 219 µs | 407 µs | 407 µs |

`watch` first frame 645 bytes, `ok`. Deltas vs the 12:21 UTC row are
noisy µs on a shared VM — **not** claimed as wins or regressions.

### 4. RSS (VERIFIED)

Idle `quotad` after first `quota ping` (both providers `unavailable`):

| metric | value | source |
|--------|------:|--------|
| VmRSS | 3428 kB | `/proc/<pid>/status` |
| RssAnon | 272 kB | same |
| RssFile | 3156 kB | same |
| Threads | 2 | same |
| PSS | 1676 kB | `/proc/<pid>/smaps_rollup` |
| USS (`Private_Clean` + `Private_Dirty`) | 1604 kB | 1332 + 272 |

`ps -o rss` agreed: **3428**.

CLI one-shot VmHWM, tight `/proc` poll, samples with HWM below 1000 kB
dropped (process exited before a useful read):

| command | observed VmHWM (kB) |
|---------|---------------------|
| `quota version` | 1292–2564 (first five: 2008, 2348, 2364, 2132, 1292) |
| `quota status` | 1892–2556 (n=17) |
| `quota-ctl ping` | 1340–3048 (first five: 2972, 1528, 1340, 2252, 2188) |

### 5. CLI spawn (VERIFIED)

Python `time.perf_counter` around `subprocess.run` (no shell), warmup 5,
page cache warm. `quotad` already up.

| command | n | mean ± std | min | max | p50 | p95 | p99 |
|---------|--:|------------|-----|-----|-----|-----|-----|
| `quota --socket … version` | 80 | 665.0 ± 148.7 µs | 539.6 µs | 1268.7 µs | 607.4 µs | 1060.6 µs | 1156.5 µs |
| `quota --socket … ping` | 80 | 635.0 ± 109.4 µs | 529.7 µs | 938.5 µs | 590.0 µs | 906.0 µs | 929.6 µs |
| `quota-ctl --socket … ping` | 40 | 705.0 ± 114.3 µs | 590.9 µs | 970.1 µs | 672.9 µs | 922.6 µs | 970.1 µs |

**Not measured:** live HTTPS usage-fetch, musl, cold-page-fault
distribution.

---

## 2026-09-27 hardening pass (VERIFIED, historical)

Same VM class as the tables below (`uname` / `rustc` / 4× Xeon @ 2400 MHz /
15 GiB). Measured after the socket/creds/JSONL hardening commit, **before**
the parsed-row tail change and the real keychain. Kept so the original
JSONL −74% write-up is not deleted. For this PR’s tree use the 13:16 UTC
section above.

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

# musl (needs rust-std + musl-gcc)
rustup target add x86_64-unknown-linux-musl
CC=musl-gcc CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc \
  cargo build --locked --workspace --release --target x86_64-unknown-linux-musl --bins

# RSS
PID=$(pidof quotad)
grep -E '^(VmRSS|RssAnon|RssFile|Threads)' /proc/$PID/status
cat /proc/$PID/smaps_rollup
```

`quota-bench` percentiles use nearest-rank: `round(p * (n-1))` on a sorted
sample vector.
