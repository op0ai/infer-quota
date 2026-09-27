# Secrets

`quota-secrets` is the unified lookup crate. `quota-ctl` is the human CLI.
`quotad` never stores passwords, API keys, or JWTs. The chain is shared
across adapters. The CLI-file paths below are what the first shipped
collectors (Codex and Claude) read.

## Order (first hit wins)

1. **OpenBao** (optional) — KV v2. Used for material the companion *adds*
   (managed account keys). Not required for `cargo test` or a default
   `quotad` that only reuses existing CLI sessions.
2. **OS keychain** — macOS Keychain / Linux secret-service. **Scaffold** in
   this tree: the trait is there; Security.framework is not linked. On
   non-macOS the backend returns `unimplemented`.
3. **CLI files** (read-only, last resort) — the same paths `quota-adapters`
   already consult:
   - `$CODEX_HOME/auth.json` or `~/.codex/auth.json`
   - `$CLAUDE_CONFIG_DIR/.credentials.json` or `~/.claude/.credentials.json`

File writes are refused. Official CLIs own those files (`codex login`,
Claude Code `/login`).

## OpenBao (local, not production)

```bash
docker compose -f docker-compose.dev.yml up -d
export QUOTA_OPENBAO_ADDR=http://127.0.0.1:8200
export QUOTA_OPENBAO_TOKEN=dev-only-not-for-prod
export QUOTA_OPENBAO_MOUNT=secret
export QUOTA_OPENBAO_PREFIX=quota
quota-ctl secret backends          # openbao keychain file
# material from env — never argv, never committed
export QUOTA_PUT=sk-example-not-real
quota-ctl secret put codex/work --from-env QUOTA_PUT
quota-ctl secret get codex/work    # prints backend= and present=true only
```

The compose file starts **dev** OpenBao with a well-known root token. That
token is not a secret; do not reuse it anywhere else. The HTTP client in
`quota-secrets` is **plain HTTP** (no rustls). For TLS, put a proxy in front
or extend the scaffold.

Default `cargo test --workspace` does not start OpenBao and does not open
sockets to `:8200`.

## What never goes in git

- Passwords, refresh tokens, JWTs, `authFingerprint`, `managedHomePath`
- `~/.codex/auth.json`, `~/.claude/.credentials.json`, `cursor-session.json`
- Live OpenBao tokens (the compose token is a dummy labelled as such)

Account metadata on the Unix socket (`accounts.add`) may include a
`secret_ref` `{ backend, path }` — a pointer, not the bytes.

## Feature flag

Crate feature `openbao` is reserved and currently empty: the HTTP client
always compiles so tests can cover URL parsing without a live server.
Enable it in dependents if you want to advertise the optional dep.
