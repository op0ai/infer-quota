# quota

Tiny, MIT-licensed inference-quota **daemon + CLI + control plane** for
machines that already have Codex and/or Claude Code logged in.

`quotad` is the source of truth (polling + snapshots). `quota` is a thin
read client. `quota-ctl` is the mutating companion (accounts / refresh /
local secrets pointers) — a CodexBar-shaped foil that does **not** collect
usage itself.

A later macOS menu bar, Herdr statusline, tmux segment, or MCP server
should speak the same Unix socket — not scrape providers again.

| | URL |
|--|-----|
| Origin (working forge) | https://origin.cursor.com/op0/infer-quota |
| GitHub (public mirror) | https://github.com/op0ai/infer-quota |

Share page: [docs/site/index.html](docs/site/index.html). GitHub Pages from
`main` and the `/docs` folder publishes
<https://op0ai.github.io/infer-quota/site/>. Place the social image at
`docs/site/assets/og.png`.

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
                                           ── quota-ctl (accounts / refresh)
                                           ── future menu bar / tmux / MCP
```

## What v0 does

- Reuses **existing** OAuth / CLI session files. Never stores passwords. Never
  writes credential files.
- Optional OpenBao KV for *new* material the companion adds. OS keychain is a
  documented stub. See [docs/SECRETS.md](docs/SECRETS.md).
- Probes best-effort usage endpoints (see [docs/SOURCES.md](docs/SOURCES.md);
  several are **undocumented hypotheses**). On failure: `status: unavailable`
  plus a reason — **no fake remaining tokens**.
- Math: burn rate from a bounded snapshot ring, ETA to empty, `can_start`.
- Providers: **Codex** and **Claude** only.
- Offline CodexBar fixture parser + 1912-row history replay in tests.
- Optional read of CodexBar's macOS snapshot/history files when the live
  API is down (never `cursor-session.json`).
- Window kind follows published length: 604800s / 10080 min is **weekly**,
  even if the API stuffed it in `primary_window`.

## What v0 does not do

- Menu bar / WidgetKit / Qt
- MCP server
- Cookie-DB scraping (we refuse to hold a browser cookie database in memory)
- The rest of CodexBar's provider zoo
- Claiming calibrated accuracy beyond what the source published
- Converting `--tokens N` into a percent-only window

## Build / run

Requires Rust 1.83+ (stable).

```bash
cargo build --release
# binaries: target/release/quotad  target/release/quota  target/release/quota-ctl

quotad run                          # foreground; optional --socket PATH
quota status
quota status --json --provider codex
quota pace --provider claude
quota can-start --tokens 50000
quota watch
quota ping
quota version

quota-ctl ping
quota-ctl accounts add --provider codex --email you@example.com --select
quota-ctl accounts list
quota-ctl refresh
quota-ctl accounts remove --id acct_you_example_com
```

`cargo test --workspace` is offline: adapters use in-memory HTTP mocks;
CodexBar tests read `fixtures/codexbar/` only. OpenBao is not required.

### Optional local OpenBao

```bash
docker compose -f docker-compose.dev.yml up -d
export QUOTA_OPENBAO_ADDR=http://127.0.0.1:8200
export QUOTA_OPENBAO_TOKEN=dev-only-not-for-prod
quota-ctl secret backends
```

Dev-only. Not a production vault. Details: [docs/SECRETS.md](docs/SECRETS.md).

### Socket path

1. `--socket` / `QUOTA_SOCKET`
2. `~/.config/quota/config.json` → `socket`
3. `$XDG_RUNTIME_DIR/quota/quota.sock`
4. `~/.local/share/quota/quota.sock`

Protocol: **4-byte little-endian length + compact JSON**. Methods: `status`,
`pace`, `can_start`, `ping`, `version`, `watch`, plus additive `refresh` and
`accounts.*`. Full schema: [docs/protocol.md](docs/protocol.md).

## CLI

| Command | Purpose |
|---------|---------|
| `quotad run` | Foreground daemon |
| `quota status [--json] [--provider codex\|claude\|all]` | Latest snapshot |
| `quota pace [--provider …]` | Burn rate + ETA from the ring |
| `quota can-start --tokens N [--deadline UNIX] [--provider …]` | Fit a job before reset |
| `quota watch` | Stream snapshots after each refresh |
| `quota-ctl accounts list\|add\|remove\|select` | Metadata only (no tokens) |
| `quota-ctl refresh` | Immediate probe |
| `quota-ctl secret backends\|get\|put` | Local secrets chain |

Exit codes: `0` ok; `1` transport/RPC error; `2` `can-start` overall `no`
(or secret not found).

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
- Nothing in those files is uploaded, mirrored, or written to the socket.
- Optional JSONL history (`"history": true` in config) records snapshots
  (percents, reset times) — not secrets. Off by default.
- Account book (`accounts.json`) is metadata + optional `secret_ref` paths.
- Socket mode `0600`, directory `0700`.
- Fixtures in-repo are redacted (`accountKey` hashed; fingerprints stripped).

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

Measured binary size, socket RTT, RSS, fixture pace, and start times (one
machine, no invented figures): [docs/BENCHMARKS.md](docs/BENCHMARKS.md).

## Crates

| Crate | Role |
|-------|------|
| `quota-core` | Types, snapshot schema, math, framing, config, paths, Unix RPC |
| `quota-adapters` | Codex + Claude + CodexBar file parser (`Provider` trait, mocked HTTP) |
| `quota-secrets` | OpenBao / keychain stub / file OAuth chain |
| `quotad` | Daemon |
| `quota` | Read CLI (sync socket client; no HTTP) |
| `quota-ctl` | Control-plane CLI + `quota_ctl` library |
| `quota-bench` | Local harness (not shipped) |

## vs CodexBar

| | quota / quotad / quota-ctl | CodexBar |
|--|----------------------------|----------|
| Job | Core: session reuse, snapshot, math, socket | Product: menu bar, widgets, many providers |
| UI | None in v0 | Native macOS UI |
| Providers (v0) | Codex, Claude | Large set + cookies + cost scanners |
| Credential writes | Never (files). Optional OpenBao for *new* keys | Refresh/cookie import in some paths |
| Accuracy | Honest `percent_only` when no token budget | Richer UI; still often percent windows |
| Clients | Any process that can speak the socket | App-centric |

A menu bar can sit on this daemon the same way `quota watch` does.

## License

MIT — see [LICENSE](LICENSE).
