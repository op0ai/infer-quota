# quota

Tiny, MIT-licensed inference-quota **daemon + CLI** for machines that already
have Codex and/or Claude Code logged in.

`quotad` is the source of truth. `quota` is a thin client. A later macOS menu
bar, Herdr statusline, tmux segment, or MCP server should speak the same Unix
socket — not scrape providers again.

This is **not** [CodexBar](https://github.com/steipete/CodexBar). CodexBar is a
full menu-bar product with many providers, cookies, widgets, and UI.
`quota` is the small rust-native core: reuse sessions, publish numbers, do the
math.

```
  Codex ~/.codex/auth.json ──┐
                             ├──► quotad (1 OS thread + blocking HTTP)
  Claude ~/.claude/          │         │
         .credentials.json ──┘         │  length-prefixed JSON
                                       ▼
                              Unix socket ── quota CLI
                                           ── future menu bar / tmux / MCP
```

## What v0 does

- Reuses **existing** OAuth / CLI session files. Never stores passwords. Never
  writes credential files.
- Probes best-effort usage endpoints (see [docs/SOURCES.md](docs/SOURCES.md);
  several are **undocumented hypotheses**). On failure: `status: unavailable`
  plus a reason — **no fake remaining tokens**.
- Math: burn rate from a bounded snapshot ring, ETA to empty, `can_start`.
- Providers: **Codex** and **Claude** only.

## What v0 does not do

- Menu bar / WidgetKit / Qt
- MCP server
- Cookie-DB scraping (we refuse to hold a browser cookie database in memory)
- The rest of CodexBar's provider zoo
- Claiming calibrated accuracy beyond what the source published

## Build / run

Requires Rust 1.83+ (stable).

```bash
cargo build --release
# binaries: target/release/quotad  target/release/quota

quotad run                          # foreground; optional --socket PATH
quota status
quota status --json --provider codex
quota pace --provider claude
quota can-start --tokens 50000
quota watch
quota ping
quota version
```

`cargo test --workspace` is offline: adapters use in-memory HTTP mocks.

### Socket path

1. `--socket` / `QUOTA_SOCKET`
2. `~/.config/quota/config.json` → `socket`
3. `$XDG_RUNTIME_DIR/quota/quota.sock`
4. `~/.local/share/quota/quota.sock`

Protocol: **4-byte little-endian length + compact JSON**. Methods: `status`,
`pace`, `can_start`, `ping`, `version`, `watch`. Full schema:
[docs/protocol.md](docs/protocol.md).

## CLI

| Command | Purpose |
|---------|---------|
| `quotad run` | Foreground daemon |
| `quota status [--json] [--provider codex\|claude\|all]` | Latest snapshot |
| `quota pace [--provider …]` | Burn rate + ETA from the ring |
| `quota can-start --tokens N [--deadline UNIX] [--provider …]` | Fit a job before reset |
| `quota watch` | Stream snapshots after each refresh |

Exit codes: `0` ok; `1` transport/RPC error; `2` `can-start` overall `no`.

`can-start` cannot honestly convert `--tokens` to a percent-only window
(Codex `/wham/usage` and Claude `/api/oauth/usage` typically publish
`used_percent` / `utilization`, not a token budget). In that case the answer
is `basis: percent_only` with an explanation. Exhausted (0% left) is a hard
no. A published token/credit remaining is compared directly.

## Privacy

- Read-only access to files the official CLIs already created.
- Access tokens live in RAM only for the duration of one HTTP GET. Refresh
  tokens are not kept after JSON parse.
- We do **not** refresh OAuth or rewrite `auth.json` / `.credentials.json`.
  If a token is stale, re-login with `codex login` or Claude Code `/login`.
- Optional JSONL history (`"history": true` in config) records snapshots
  (percents, reset times) — not secrets. Off by default.
- Socket mode `0600`, directory `0700`.

## Performance / memory

Documented so later contributors do not undo it:

- Tokio **`current_thread`** runtime. No GUI crates. A small rustls HTTPS GET
  (no `url`/`icu` stack) runs inside `spawn_blocking`.
- Adaptive refresh: `refresh_min_secs` (default 30) while numbers move;
  backoff toward `refresh_max_secs` (default 300) when stable, unavailable, or
  HTTP 429.
- In-memory ring (default 128 snapshots). Optional append-only JSONL, never
  replayed on startup.
- Credential files capped at 64 KiB; HTTP bodies capped at 64 KiB; frames at
  256 KiB.
- Release profile: `lto = thin`, `opt-level = s`, `panic = abort`, strip
  debuginfo.

Config example: [docs/config.example.json](docs/config.example.json).

Measured binary size, socket RTT, and RSS (one machine, no invented
figures): [docs/BENCHMARKS.md](docs/BENCHMARKS.md).

## Crates

| Crate | Role |
|-------|------|
| `quota-core` | Types, snapshot schema, math, framing, config, paths |
| `quota-adapters` | Codex + Claude modules (`Provider` trait, mocked HTTP in tests) |
| `quotad` | Daemon |
| `quota` | CLI (sync socket client; no HTTP) |

## vs CodexBar

| | quota / quotad | CodexBar |
|--|----------------|----------|
| Job | Core: session reuse, snapshot, math, socket | Product: menu bar, widgets, many providers |
| UI | None in v0 | Native macOS UI |
| Providers (v0) | Codex, Claude | Large set + cookies + cost scanners |
| Credential writes | Never | Refresh/cookie import in some paths |
| Clients | Any process that can speak the socket | App-centric |

A menu bar can sit on this daemon the same way `quota watch` does.

## License

MIT — see [LICENSE](LICENSE).
