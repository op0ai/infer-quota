# Agent handoff (under two minutes)

Point an agent at this repo. Do **not** scrape provider credential files.
`quotad` is the only process that may read existing CLI sessions. `quota`
talks to a Unix socket.

## 1. Install once

From the repo root (Rust 1.83+):

```bash
./install.sh                 # ~/.local/bin/{quotad,quota,quota-ctl}
# or lean (no control plane / OpenBao client):
./install.sh --minimal       # also removes leftover quota-ctl from the prefix
export PATH="$HOME/.local/bin:$PATH"
```

`--from-release` fetches a GitHub release tarball when one exists for this
triple; otherwise it builds. There is no menu bar, MCP server, or UI in this
tree.

Targeted cargo (same as `--minimal`):

```bash
cargo build --release -p quotad -p quota
```

`quota-ctl` is optional. It mutates account *metadata* and can store *new*
material in OpenBao (crate feature `openbao`, rustls). The OS keychain is
real (Linux secret-service / macOS Security.framework) and returns
`Unavailable` when the session bus is down — it is not a stub. It is not a
collector. `quotad` does not link `quota-secrets`.

## 2. Start the daemon

```bash
SOCK="${QUOTA_SOCKET:-${XDG_RUNTIME_DIR:+$XDG_RUNTIME_DIR/quota/quota.sock}}"
SOCK="${SOCK:-$HOME/.local/share/quota/quota.sock}"
export QUOTA_SOCKET="$SOCK"
mkdir -p "$(dirname "$QUOTA_SOCKET")"
quotad run --socket "$QUOTA_SOCKET" &
quota --socket "$QUOTA_SOCKET" ping
```

`--socket` wins. Then `config.json` `"socket"`. Then `QUOTA_SOCKET`. Then
`$XDG_RUNTIME_DIR/quota/quota.sock` or `~/.local/share/quota/quota.sock`.
The commands above pass `--socket`. Directory mode `0700`, socket mode
`0600`. Prefer `$XDG_RUNTIME_DIR` over `/tmp`.

## 3. Read numbers

```bash
quota --socket "$QUOTA_SOCKET" status --json
quota --socket "$QUOTA_SOCKET" pace --json --provider codex
quota --socket "$QUOTA_SOCKET" can-start --tokens 50000 --json
```

`status: unavailable` plus a reason is a real answer. Do not invent remaining
tokens or extra providers. `can-start` on a percent-only window (Codex
`/wham/usage`, Claude `/api/oauth/usage`, CodexBar fixtures) returns
`basis: percent_only` and `ok: false` unless the window is exhausted (hard no)
or `--tokens 0`.

Protocol: 4-byte little-endian length + compact JSON, max 256 KiB. Schema:
[protocol.md](protocol.md).

## 4. Never do this

| path / action | why |
|---------------|-----|
| `~/.codex/auth.json` | access + refresh tokens. Daemon reads; you do not copy. |
| `~/.claude/.credentials.json` | OAuth block. Same rule. |
| `cursor-session.json` | WorkOS JWT. Never open. |
| Browser cookie DBs | Refused. We will not load a cookie store into RAM. |
| Invent Gemini / OpenRouter / … | v0 adapters are Codex + Claude only. |
| Commit fixtures with live keys | Use `fixtures/codexbar/*.redacted.*` only. |
| Put secrets on argv | `quota-ctl secret put` is `--from-env` only. |
| Print secret bytes | `quota-ctl secret get` prints `present=true` only. |

Offline tests and benches use `fixtures/codexbar/` and HTTP mocks. They do
not measure live provider latency.

## 5. Optional control plane

```bash
quota-ctl --socket "$QUOTA_SOCKET" accounts add --provider codex --email you@example.com --select
quota-ctl --socket "$QUOTA_SOCKET" accounts list --json
quota-ctl --socket "$QUOTA_SOCKET" refresh
```

Account records are metadata + optional `secret_ref` `{backend, path}`.
Tokens never go on the socket.
