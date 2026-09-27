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
| Socket bind | Refuse a symlink at the socket path. If a live listener is already bound, fail with “already running” instead of unlinking. Stale sockets are removed; non-socket files are not clobbered. A directory we create is `0700`; `/tmp` itself is not. The socket is created with umask `0177` (mode `0600`) and chmod `0600` is required before listen. |
| Credential files | Size-capped reads (64 KiB). Symlinks, directories, and FIFOs are refused (`O_NOFOLLOW` + `O_NONBLOCK`, then `fstat`). No TOCTOU `metadata` then unbounded `fs::read`. |
| `$HOME` unset | Fail closed. No probes of `/.codex/auth.json`. |
| `home_path` | Must be absolute and must not contain `..`. `accounts.add` **rejects** an invalid path (does not silently fall back to the default CLI home). Persisted books are re-sanitized on load. Applied only to the **active account’s provider** (Codex vs Claude). |
| HTTP errors | Status code only in snapshots. Response bodies are not copied into `error.message` or history JSONL. |
| CodexBar ingest | Fixed filenames only. Never `cursor-session.json`. History lines longer than 64 KiB are skipped. The tail ring keeps the last N **parsed** rows, so a partial line does not evict a valid one. Empty candidates fall through. Opens are regular-file only. |
| OpenBao | Feature `openbao` (enabled by `quota-ctl`, not by `quotad`). `https://` is rustls 0.21; optional `QUOTA_OPENBAO_CA_FILE` PEM. Plain HTTP is **exact** loopback (`127.0.0.1`, `localhost`, `::1`) unless `QUOTA_OPENBAO_ALLOW_PLAINTEXT=1`. Nested prefixes (`quota/prod`) are allowed; `..` is not. `put` values capped at 32 KiB. Response bodies capped. Token and secret bytes are redacted from `Debug`. `secret get` prints `present=true` only. `secret put` requires `--from-env` (never argv). File backend is read-only. |
| State files | Accounts / optional history dirs we create are `0700`. Files are opened `0600` with `O_NOFOLLOW` and refused when the inode is group- or other-readable, before any bytes are written. |
| Instance lock | `mkdir` mode `0700` on `{socket}.lock`. The pid is written and re-read before the lock is claimed. A missing pid is stolen only after it stays missing (a live starter is not unlinked mid-write). |
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

3. **OpenBao plain HTTP is dev-only.** `https://` is rustls in `quota-ctl`.
   `docker-compose.dev.yml` stays HTTP on loopback with the documented
   dummy token. A live OpenBao TLS deployment was not exercised here
   (an in-process handshake against a CA minted in the test is covered).

4. **Keychain runtime is platform-dependent.** Linux links secret-service
   and returns `Unavailable` when the session bus is down (the chain then
   skips to read-only files). macOS links Security.framework; that path
   is not executed in this Linux CI. Other operating systems get an
   explicit `Unavailable`. A live keychain roundtrip was not run here.

5. **Custom `--socket` under `/tmp`.** A *subdirectory we create*
   (for example `/tmp/quota-run/`) is `0700`. `/tmp` itself stays
   shared: `ensure_private_dir` does not chmod an existing parent when
   that chmod is denied, and a socket path of `/tmp/quota.sock` does
   not get a private parent. Symlink bind is still refused. umask is
   process-global for the duration of `bind` and is restored immediately
   after; another thread creating a file in that instant would see umask
   `0177`. Prefer `$XDG_RUNTIME_DIR`.

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
