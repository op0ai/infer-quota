# quota

Rust-native inference quota. Tiny, MIT-licensed **quotad · quota · quota-ctl**:
daemon, read CLI, and control plane for quota and pool math.

Collectors are adapters. The first shipped ones read Codex and Claude
sessions — early dogfood against CodexBar-shaped files and Claude usage
endpoints, not an architectural limit. Any source that exposes quota belongs
in another adapter. See [site/docs/adapters.mdx](site/docs/adapters.mdx).

`quotad` polls enabled adapters, stores the snapshot, and owns the Unix
socket. `quota` is a thin read client. `quota-ctl` mutates accounts, refresh,
and local secret pointers. Usage collection stays in `quotad`.

A menu bar, Herdr statusline, tmux segment, or MCP server should speak that
socket. Provider HTTP stays in the adapters.

| | URL |
|--|-----|
| Origin (working forge) | https://origin.cursor.com/op0/infer-quota |
| GitHub (public mirror) | https://github.com/op0ai/infer-quota |

[CodexBar](https://github.com/steipete/CodexBar) is a separate menu-bar
product (many providers, cookies, widgets, UI). This repository is the small
Rust core: adapters, a snapshot, the math, a socket.

```
  quota source
       │
       ▼
  adapter          shipped today: Codex, Claude
       │
       ▼
    quotad         snapshot + pool math
       │
       │  length-prefixed JSON
       ▼
  Unix socket ── quota
              ── quota-ctl
              ── other surfaces
```

## What v0 does

- Reuses **existing** OAuth / CLI session files. Never stores passwords. Never
  writes credential files.
- Optional OpenBao KV (rustls for `https://`) for *new* material the
  companion adds. OS keychain is Linux secret-service and macOS
  Security.framework. See [docs/SECRETS.md](docs/SECRETS.md).
- Probes best-effort usage endpoints (see [docs/SOURCES.md](docs/SOURCES.md);
  several are **undocumented hypotheses**). On failure: `status: unavailable`
  plus a reason — **no fake remaining tokens**.
- Math: burn rate from a bounded snapshot ring, ETA to empty, `can_start`.
- First collectors: **Codex** and **Claude** adapters (`Provider` in
  `quota-adapters`). `enable_codex` / `enable_claude` default on. Another
  provider is a code change on that trait, `ProviderId`, and the probe list
  in `quotad`.
- Offline CodexBar fixture parser + 1912-row history replay in tests.
- Optional read of CodexBar's macOS snapshot/history files when the live
  API is down (never `cursor-session.json`).
- Window kind follows published length: 604800s / 10080 min is **weekly**,
  even if the API stuffed it in `primary_window`.

## What v0 does not do

- Menu bar / WidgetKit / Qt
- MCP server
- Cookie-DB scraping (we refuse to hold a browser cookie database in memory)
- Claiming calibrated accuracy beyond what the source published
- Converting `--tokens N` into a percent-only window

## Build / run

Requires Rust 1.83+ (stable).

```bash
cargo build --release
# fresh checkout, not on PATH:
#   ./target/release/quotad
#   ./target/release/quota
#   ./target/release/quota-ctl

./target/release/quotad run         # foreground; optional --socket PATH
./target/release/quota status
./target/release/quota status --json --provider codex
./target/release/quota pace --provider claude
./target/release/quota can-start --tokens 50000
./target/release/quota watch
./target/release/quota ping
./target/release/quota version

./target/release/quota-ctl ping
./target/release/quota-ctl accounts add --provider codex --email you@example.com --select
./target/release/quota-ctl accounts list
./target/release/quota-ctl refresh
./target/release/quota-ctl accounts remove --id acct_you_example_com
```

`cargo test --workspace` is offline: adapters use in-memory HTTP mocks;
CodexBar tests read `fixtures/codexbar/` only. OpenBao is not required.

### Optional local OpenBao

```bash
docker compose -f docker-compose.dev.yml up -d
export QUOTA_OPENBAO_ADDR=http://127.0.0.1:8200
export QUOTA_OPENBAO_TOKEN=dev-only-not-for-prod
./target/release/quota-ctl secret backends
```

Both variables are required. Address without a non-empty token is a config error, not a chain that omits OpenBao.

Dev-only. Not a production vault. Details: [docs/SECRETS.md](docs/SECRETS.md).

### Socket path

`--socket` is on `quotad`, `quota`, and `quota-ctl`. `--config` is on `quotad` only.

1. `--socket`
2. `socket` in the config file `quotad` loaded (`--config PATH`, else the default file). Clients read only the default file: `$XDG_CONFIG_HOME/quota/config.json`, else `~/.config/quota/config.json`
3. `QUOTA_SOCKET`
4. `$XDG_RUNTIME_DIR/quota/quota.sock`
5. `~/.local/share/quota/quota.sock`

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

Exit codes: `0` ok; `1` transport, RPC, or secrets-chain error (OpenBao
connect failure, or address set without a token); `2` `can-start` overall
`no`, or `secret get` when nothing is found. An OpenBao HTTP status other
than 404 or 2xx is treated as a miss when no later backend has the path.

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
- Socket mode `0600`, directory `0700`. Symlink socket paths are refused.
- Fixtures in-repo are redacted (`accountKey` hashed; fingerprints stripped).
- Threat model and residual gaps: [docs/SECURITY.md](docs/SECURITY.md).

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
| `quota-adapters` | `Provider` trait. First collectors: Codex and Claude, plus the CodexBar file fallback |
| `quota-secrets` | OpenBao (feature) / OS keychain / read-only file OAuth |
| `quotad` | Daemon |
| `quota` | Read CLI (sync socket client; no HTTP) |
| `quota-ctl` | Control-plane CLI + `quota_ctl` library |
| `quota-bench` | Local harness (not shipped) |

## vs CodexBar

| | quota / quotad / quota-ctl | CodexBar |
|--|----------------------------|----------|
| Job | Core: adapters, snapshot, math, socket | Product: menu bar, widgets, many providers |
| UI | None in v0 | Native macOS UI |
| Collectors | First adapters: Codex, Claude. Math and socket stay provider-agnostic | Large set + cookies + cost scanners |
| Credential writes | Never (files). Optional OpenBao for *new* keys | Refresh/cookie import in some paths |
| Accuracy | Honest `percent_only` when no token budget | Richer UI; still often percent windows |
| Clients | Any process that can speak the socket | App-centric |

A menu bar can sit on this daemon the same way `quota watch` does.

## Docs

Public pages live in [`site/`](site/) ([Blume](https://useblume.dev), static HTML). The site title is **fetchquota**: Rust-native inference quota, quota and pool math, quotad · quota · quota-ctl. Crate and binary names stay `infer-quota`, `quotad`, `quota`, and `quota-ctl`. Engineering notes stay in [`docs/`](docs/). Runnable composition examples will live in [`examples/`](examples/) and on the docs Examples page; none are shipped yet.

```bash
cd site
bun install
bun run dev     # http://localhost:4321
bun run build   # site/dist
bun run check   # blume check
```

Node.js 22.12+ and Bun 1.4. From the repo root, `bun run docs:install`, `bun run dev`, `bun run build`, and `bun run check` call the same scripts. Deploy on Vercel or Cloudflare Pages: [site/DEPLOY.md](site/DEPLOY.md). A custom domain is not attached yet.

Agent entry points after `bun run build`: `site/dist/llms.txt`, `site/dist/llms-full.txt`, per-page `.md` mirrors, and `site/dist/api/docs/pages.json`. The docs MCP server stays off so v1 remains static.

An earlier static share stub (`docs/site`, pull request #2) is superseded by this Blume site. Publish `site/`, not a second HTML page under `docs/`.

## License

MIT — see [LICENSE](LICENSE).
