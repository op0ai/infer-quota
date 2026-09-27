# Security notes

What this tree guarantees, what it does not, and residual gaps after the
2026-09-27 hardening pass. No live credentials, JWTs, or `auth.json`
contents belong in git or in this document.

## Trust model

`quotad` is a **same-UID** daemon. The Unix socket is created `0600` in a
`0700` directory. Any process running as the same user that can connect
may call `status`, `refresh`, and `accounts.*`. That is intentional: the
socket is the control plane for the user session, not a multi-tenant API.

`quota-ctl` never collects usage. It only RPCs the daemon and talks to
`quota-secrets` locally.

## Hardened in this tree

| Area | Behavior |
|------|----------|
| Socket bind | Refuse a symlink at the socket path. If a live listener is already bound, fail with “already running” instead of unlinking. Stale sockets are removed; non-socket files are not clobbered. Parent dir `0700`, socket `0600`. |
| Credential files | Size-capped reads (64 KiB) with `O_NOFOLLOW` / symlink refuse. No TOCTOU `metadata` then unbounded `fs::read`. |
| `$HOME` unset | Fail closed. No probes of `/.codex/auth.json`. |
| `home_path` | Must be absolute and must not contain `..`. `accounts.add` **rejects** an invalid path (does not silently fall back to the default CLI home). Persisted books are re-sanitized on load. Applied only to the **active account’s provider** (Codex vs Claude). |
| HTTP errors | Status code only in snapshots. Response bodies are not copied into `error.message` or history JSONL. |
| CodexBar ingest | Fixed filenames only. Never `cursor-session.json`. Malformed/partial JSONL lines are skipped (writer race). Empty/unparseable history candidates fall through. Snapshot reads are size-capped and symlink-safe. |
| OpenBao | Plain HTTP is **exact** loopback (`127.0.0.1`, `localhost`, `::1`) unless `QUOTA_OPENBAO_ALLOW_PLAINTEXT=1`. Nested prefixes (`quota/prod`) are allowed; `..` is not. `put` values capped at 32 KiB. Response bodies capped. `secret get` prints `present=true` only. `secret put` requires `--from-env` (never argv). File backend is read-only. |
| State files | Accounts / optional history dirs `0700`. Files are created `0600` (`O_CREAT` mode), not chmod-after-write. |
| Instance lock | Atomic `mkdir` on `{socket}.lock` (pid file; stale dir removed if `kill -0` fails) before unlinking a stale socket, so two startups cannot steal each other’s bind. |
| Client flood | 16 RPC + 48 watch slots. First-frame idle timeout 15s. Watchers cannot exhaust `status`/`refresh`. |
| Window kinds | Unknown slot + no duration → `extra`/`unknown`, not invented `weekly`. |

## Residual gaps (explicit)

1. **`secret_ref` is metadata only.** `quotad` does **not** fetch tokens from
   OpenBao. Collection still reads CLI session files (`auth.json` /
   `.credentials.json`). This is deliberate: secrets stay out of the
   always-on daemon. Operators who set `secret_ref` must not assume the
   daemon is vault-only. Wiring that up would pull `quota-secrets` into
   `quotad` — out of scope for this pass.

2. **No `SO_PEERCRED` check.** `0600` already limits the socket to the
   owner. A compromised same-user process is inside the trust boundary.

3. **OpenBao client is still plain HTTP.** Loopback-fenced and documented.
   TLS requires a local proxy or a future rustls client in `quota-ctl`
   (not in `quotad`).

4. **Keychain backend is a stub** (`unimplemented`). The chain falls
   through to the file backend.

5. **Custom `--socket` under `/tmp`.** Parent is created `0700`, symlink
   bind is refused, but a world-writable *ancestor* is still a local
   attack surface. Prefer `$XDG_RUNTIME_DIR`.

6. **`rustls-webpki` 0.101.7 advisories (accepted).** `quotad` stays on
   `rustls 0.21.12` for MSRV 1.83 without pulling `url`/`icu`. CI
   `cargo audit` **ignores** `RUSTSEC-2026-0098`, `RUSTSEC-2026-0104`,
   and `RUSTSEC-2026-0099`. Those are patched only in `rustls-webpki`
   0.103+ / `rustls` 0.23+. No `cargo deny` / license policy yet.

7. **Optional `history: true` JSONL** persists snapshots (including
   `credential_path` and error codes) on disk. Off by default.

8. **Feature-gating rustls out of `quotad`** (file-only CodexBar builds)
   is not implemented. TLS is always linked when Codex/Claude HTTPS
   probes are compiled in.

9. **Watch connections have no per-idle timeout after subscribe.** The
   first-frame timeout applies to handshake and RPC reads. A same-UID
   peer can hold a watch slot until disconnect.

## What must never be committed

- `auth.json`, `.credentials.json`, `cursor-session.json`
- JWTs (`eyJ…`), refresh tokens, `authFingerprint`, live OpenBao tokens
- Real `managedHomePath` values from a live CodexBar install

Fixtures under `fixtures/codexbar/` are redacted; CI runs
`scripts/check_fixtures.py` (JWT-like bytes **and** credential field
values, including `*.redacted.*`).
