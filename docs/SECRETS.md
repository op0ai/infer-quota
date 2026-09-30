# Secrets

`quota-secrets` is the unified lookup crate. `quota-ctl` is the human CLI.
`quotad` never stores passwords, API keys, or JWTs. It links this crate only to
*read*: Claude Code's own Keychain item on macOS, and the Cursor session cookie
(`quota-source-cursor`). The CLI-file paths below are what the first shipped
collectors (Codex and Claude) read.

## Order (first hit wins)

This is the default chain, `quota_secrets::from_env`, used by `quota-ctl`.
The Cursor session cookie has its own order,
`quota_secrets::keychain_first_from_env`: **OS keychain**, then **OpenBao**
(when configured), then the read-only cookie file below. Claude Code's item is
not in that chain. `QUOTA_NO_KEYCHAIN=1` omits the OS keychain from both Cursor
and the default lookup chain. If OpenBao configuration is incomplete, the
Cursor chain tries local keychain and cookie-file fallbacks before returning
the configuration error. A configured `cursor_secret_path` is also recognized
by the file backend, so the fallback works with a custom logical path.

1. **OpenBao** (feature `openbao`) — KV v2. `quota-ctl` enables the feature.
   Building `quota-secrets` without it omits the HTTP client and rustls.
   Used for material the companion *adds*. Not required for `cargo test` or
   a default `quotad` that only reuses existing CLI sessions.
2. **OS keychain**
   - **Linux:** secret-service over the session bus (`secret-service` 4,
     zbus, RustCrypto, no libdbus). Item attributes:
     `application=infer-quota`, `path=<logical path>`. If there is no
     session bus, the backend returns `Unavailable` and the chain skips it.
   - **macOS:** Security.framework generic password, service `infer-quota`,
     account = logical path. This code is compiled only for
     `target_os = "macos"`.
   - **Other OS:** `Unavailable` with an explicit message. The chain skips
     it. This is not a silent success.
   Then, macOS only and read-only, **Claude Code's own item** (service
   `Claude Code-credentials`), served for the logical path `claude` alone.
   macOS may prompt on the first read from a new binary. `QUOTA_NO_KEYCHAIN=1`
   skips both keychain backends. `quotad` runs the first provider refresh in
   the background, so a Keychain approval wait does not delay socket startup.
3. **CLI files** (read-only, last resort) — the same paths `quota-adapters`
   already consult:
   - `$CODEX_HOME/auth.json` or `~/.codex/auth.json`
   - `$CLAUDE_CONFIG_DIR/.credentials.json` or `~/.claude/.credentials.json`
   - `$QUOTA_CURSOR_COOKIE_FILE` or `~/.config/quota/cursor-session` (Cursor
     cookie; refused unless mode `0600`)

File writes are refused (`ReadOnly`). Official CLIs own those files
(`codex login`, Claude Code `/login`). `secret put` never creates or
rewrites them.

`secret get` prints `backend=`, `path=`, and `present=true` only. The
secret bytes are not an argument of that formatter. `secret put` reads
material from `--from-env` (never argv) and refuses an empty value.
`Debug` on the OpenBao client and on `SecretRecord` redacts the token and
the value.

## OpenBao

`https://` uses rustls 0.21 with the webpki root set. Set
`QUOTA_OPENBAO_CA_FILE` to a PEM file of extra CA certificates for a
private endpoint. `http://` is exact loopback (`127.0.0.1`, `localhost`,
`::1`, including bracketed IPv6) unless `QUOTA_OPENBAO_ALLOW_PLAINTEXT=1`.
URLs must not embed userinfo. Logical KV paths may contain nested segments
(`quota/prod`) and must not contain `..`. `put` values are capped at 32 KiB.
Response bodies are capped at 64 KiB. Error strings carry HTTP status codes,
not response bodies or the token.

### Local HTTP (dev only)

`docker-compose.dev.yml` starts OpenBao in `-dev` mode. The token below is
a dummy label, not a production credential.

```bash
docker compose -f docker-compose.dev.yml up -d
export QUOTA_OPENBAO_ADDR=http://127.0.0.1:8200
export QUOTA_OPENBAO_TOKEN=dev-only-not-for-prod
export QUOTA_OPENBAO_MOUNT=secret
export QUOTA_OPENBAO_PREFIX=quota
quota-ctl secret backends
# material from env — never argv, never committed
export QUOTA_PUT=sk-example-not-real
quota-ctl secret put codex/work --from-env QUOTA_PUT
quota-ctl secret get codex/work    # backend= path= present=true
```

OpenBao joins the default chain only when `QUOTA_OPENBAO_ADDR` and
`QUOTA_OPENBAO_TOKEN` are both set and non-empty. Address without a token is a
config error on `secret backends`, `secret get`, and `secret put`. The Cursor
read-only chain defers that config error until its local fallbacks are checked.

### TLS

```bash
export QUOTA_OPENBAO_ADDR=https://bao.example:8200
export QUOTA_OPENBAO_TOKEN=...          # not from argv, not committed
export QUOTA_OPENBAO_CA_FILE=/path/to/ca.pem   # optional extra roots
```

Default port for `https://` with no port is 443.

## What `cargo test` does

`cargo test --workspace` does **not** start Docker and does **not** dial
`:8200`. OpenBao tests use an in-process TCP listener (plain HTTP) and an
in-process rustls listener with a throwaway CA and leaf embedded in the
test. Keychain unit tests use an in-memory test platform and never access the
user's OS keychain.

`quotad` does call this crate, read-only, through two collectors: the Claude
OAuth adapter reads Claude Code's own Keychain item (macOS) before the
credentials file, and `quota-source-cursor` reads the Cursor session cookie
through `keychain_first_from_env`. Codex still reads its CLI session file
directly. `secret_ref` on an account is metadata only: the daemon does not
resolve it and never writes to any backend.

## What never goes in git

- Passwords, refresh tokens, JWTs, `authFingerprint`, `managedHomePath`
- `~/.codex/auth.json`, `~/.claude/.credentials.json`, `cursor-session.json`
- Live OpenBao tokens (the compose token is a dummy labelled as such)

Account metadata on the Unix socket (`accounts.add`) may include a
`secret_ref` `{ backend, path }` — a pointer, not the bytes.

## VERIFIED / UNVERIFIED

**VERIFIED** (this tree, `cargo test`, no live OpenBao):

- Feature `openbao` compiles the client and rustls; without the feature the
  module is absent. `quota-ctl` enables it. `quotad` links `quota-secrets`
  without it unless built with `--features openbao`.
- Plain-HTTP loopback fence, nested prefix, 32 KiB put cap, empty-secret
  refusal, credential-in-URL refusal.
- In-process mock HTTP get/put/delete.
- In-process rustls handshake to `localhost`. The throwaway CA, leaf, and
  leaf key are string constants in the unit test (not a credential file,
  not an operator key). The CA private key is not in the tree.
- `Debug` redacts the OpenBao token and `SecretRecord.value`.
- `secret get` presence line does not contain the material.
- File backend `put` returns `ReadOnly`.
- Linux keychain calls secret-service. With no session bus the test returns
  `Unavailable` and does not panic.

**UNVERIFIED:**

- A live `docker compose` OpenBao (dev HTTP). The commands above match the
  compose file; they were not required for the default test run.
- A live Linux secret-service roundtrip (this environment has no session bus).
- macOS Security.framework (no macOS runner here). The calls match
  `security-framework` 2.11 generic-password APIs.
