# Socket protocol (v2)

`quotad` listens on a Unix domain socket (macOS + Linux).

## Path

`--socket` is accepted by `quotad`, `quota`, and `quota-ctl`. `--config` is
accepted by `quotad` only. Clients do not take `--config`.

`quotad` loads `--config PATH` when set, otherwise the default config file,
then applies `--socket`:

1. `--socket`
2. `socket` in that config file
3. `QUOTA_SOCKET` when set and non-empty
4. `$XDG_RUNTIME_DIR/quota/quota.sock` when `XDG_RUNTIME_DIR` is set and non-empty
5. `~/.local/share/quota/quota.sock`

The default config file is `$XDG_CONFIG_HOME/quota/config.json`, or
`~/.config/quota/config.json`. `quota` and `quota-ctl` read that default
file only (`Config::load_default()`). A socket that exists only in a
non-default config is not visible to them; pass `--socket` or put `socket`
in the default file.

The directory is created with mode `0700`; the socket is `0600`.
`quotad` refuses to bind if the socket path is a symlink, if a live
instance is already listening, or if a non-socket file occupies the path.
Stale sockets (connect fails) are unlinked. Concurrent clients are capped
(64). The socket is a **same-UID** trust boundary — there is no extra
peer-credential handshake.

The daemon starts accepting socket requests before the initial provider
refresh finishes. Claude's first macOS Keychain read may wait for the OS
approval prompt; that wait stays in the background refresh task and does not
hold up socket startup or reads from other providers.

## Framing

Length-prefixed JSON, little-endian:

```
[u32 LE payload_len][payload_len bytes of compact UTF-8 JSON]
```

Maximum payload: **262144 bytes**. Larger frames are rejected so a buggy peer
cannot grow RSS without bound.

Pretty-printed JSON is not used on the wire (newlines inside a value are still
legal JSON; the length prefix is the boundary).

## Request

```json
{"id": 1, "method": "status", "params": {"provider": "all"}}
```

| method              | params                                              | result                                      |
|---------------------|-----------------------------------------------------|---------------------------------------------|
| `ping`              | ignored                                             | `{"pong": true}`                            |
| `version`           | ignored                                             | `{"name","version","protocol"}`             |
| `status`            | `{ "provider": "all"\|"codex"\|"claude"\|"cursor" }` | `{ "snapshot": Snapshot }`                  |
| `pace`              | `{ "provider": ... }`                               | `{ "reports": [PaceReport] }`               |
| `can_start`         | `{ "tokens": u64 }` or `{ "percent": f64, "reserve"?: f64 }`, plus `"deadline"?: i64, "provider"` | `{ "ok": bool, "answers": [CanStartAnswer] }` |
| `observe`           | see below                                           | `{ "accepted", "observed_at" }`             |
| `watch`             | `{ "provider": ... }`                               | stream of `status` results, same `id`       |
| `refresh`           | `{ "provider": ... }`                               | `{ "snapshot": Snapshot }` after a probe    |
| `accounts.list`     | `{}`                                                | `{ "version", "active_id", "accounts" }`    |
| `accounts.add`      | see below                                           | `{ "account", "active_id" }`                |
| `accounts.remove`   | `{ "id" }`                                          | same as `accounts.list`                     |
| `accounts.select`   | `{ "id": string\|null }`                            | same as `accounts.list`                     |

### Version 2

`protocol` is `2`. Version 2 adds values to closed enums that a version-1
client cannot decode, so it is a breaking change for readers of `status`,
`pace`, `can_start` and `watch`:

| field                  | new value(s)         |
|------------------------|----------------------|
| `provider`             | `cursor`             |
| `source`               | `statusline`         |
| window `kind`          | `spend`              |
| `can_start` `basis`    | `percent_budget`     |

It also adds `can_start.percent` / `reserve` and the `observe` method. A
version-1 daemon ignores the unknown `percent` field and answers a
`tokens: 0` question instead, so a client must call `version` and require
`protocol >= 2` before sending `percent` (`quota can-start --percent` does,
and refuses against an older daemon). The `tokens` form of `can_start` and the
other request shapes are unchanged.

`provider` values today are `all`, `codex`, `claude`, and `cursor` because
`ProviderId` has those variants. A new collector adds a variant. The snapshot math does
not special-case the first two adapters.

### Account metadata (no secrets)

`accounts.add` params:

```json
{
  "id": "optional-stable-id",
  "provider": "codex",
  "email": "openai@ctx.op0.dev",
  "workspace_label": "Personal",
  "login_method": "pro",
  "workspace_account_id": "optional",
  "secret_ref": { "backend": "openbao", "path": "codex/work" },
  "home_path": "/optional/isolated/CODEX_HOME",
  "select": true
}
```

`secret_ref` is a pointer. The socket must not carry passwords, JWTs, or
access tokens — put those through `quota-secrets` / `quota-ctl secret put`.
`quotad` still owns polling; `quota-ctl` is the mutating control surface.
When an account is selected and `home_path` is set, the next `refresh`
uses that path as `CODEX_HOME` (Codex) or the Claude config dir (Claude)
**only if the active account’s provider matches**. Relative paths and
`..` segments are dropped. `secret_ref` is stored as a pointer; `quotad`
does not fetch vault material (see [SECURITY.md](SECURITY.md)).

`deadline` is UTC unix seconds. When omitted, `can_start` binds to the window's
published `reset_at`.

### Percent admission

`can_start` takes **either** `tokens` (unchanged) **or** `percent` (0, 100],
never both (`bad_params`). With `percent`, every readable window that
publishes a remaining percent must pass, and the tightest one is reported:

1. `remaining - percent >= reserve` (`reserve` defaults to `2`, range `[0, 100)`)
2. the pace veto: at the current burn the window is not emptied before it
   resets, `(remaining - percent) / burn >= reset - now`. It binds to the
   window's own reset and never to `deadline`. With no burn rate yet or no
   published reset, pace cannot veto and the answer says so
   (`pace_checked: false`).
3. if `deadline` is given, it is still ahead and, at the current burn, the
   window outlasts it (`deadline_ok`).

A deadline can only add a refusal. An earlier deadline never waives the pace
veto: the same question without `deadline` refused stays refused with any
deadline.

The answer has `basis: "percent_budget"` and an `admission` object
(`requested_percent`, `reserve_percent`, `remaining_after_percent`,
`headroom_ok`, `pace_ok`, `pace_checked`, `deadline_ok`; the last is absent
from older daemons and reads as true). A window at 5h and one at weekly
are not comparable in tokens, so the same percent is applied to each window:
this is conservative, never optimistic.

Refusals with `basis: "unavailable"`: stale, unavailable or `limit_reached`
evidence; any window the source reported but that is unreadable (the readable
ones cannot vouch for it); and a provider with no window that publishes a
remaining percent. An uncapped `spend` window (dollars with no `limit_usd`)
has no remaining percent, so it takes no part in the decision: alone it
refuses, and beside a capped window only the capped window is judged.

### `observe` (push, schema 1)

A passive source pushes a reading instead of being polled. Today that is
`quota statusline` for Claude Code.

```json
{"id":1,"method":"observe","params":{
  "schema":1,"provider":"claude","source":"statusline",
  "windows":[{"kind":"five_hour","label":"5h","used_percent":34,
              "reset_at":1746540000,"limit_window_seconds":18000}]}}
```

- `schema` is the payload version. An unknown value is refused
  (`unsupported_schema`); the daemon never guesses at fields.
- Only `source: "statusline"` for `provider: "claude"` may push
  (`unsupported_source`, `unsupported_provider`). 1-8 windows.
- A statusline reading has no account identifier. It is accepted only when no
  Claude quota account is explicitly selected; otherwise the daemon returns
  `account_scope` and keeps the existing reading and poll schedule unchanged.
- When a current Claude OAuth reading contains windows the statusline does not
  report (including separate `opus weekly` and `sonnet weekly` limits), those
  windows are retained in the pushed snapshot. Percent admission still checks
  every retained readable window.
- There is **no client timestamp**. The daemon stamps `observed_at` with its
  own clock at receipt, so the normal evidence-age rules apply and a client
  cannot backdate or future-date a reading.
- A push whose windows are all unreadable is refused (`unreadable`) and never
  replaces existing evidence. A push with some unreadable windows is kept, and
  those windows stay `unknown`, which refuses percent admission.
- Precedence for Claude: while a push is current (age at most 300 s,
  inclusive; the same rule as every reading's freshness), a poll result for
  Claude is not committed. The check runs when the poll result is applied,
  under the write lock, so a poll that started before a push cannot overwrite
  it. The daemon also skips starting the OAuth poll while the push is current.
  When the push ages out, polling resumes. Pushes do not retune
  the poll interval, and repeated identical pushes refresh the newest ring
  entry instead of adding one per push (at most one per 60 s).
- A `spend` window with `used_usd` and a positive `limit_usd` is stored as a
  USD window: `unit: "usd"`, `limit`, `remaining` = limit − used (floor 0), and
  a percent derived from the dollars (a pushed `used_percent` is ignored).
  Without a positive cap the pushed percent makes a plain percent window with
  no dollar fields; dollars alone make an uncapped USD window. `used_usd` /
  `limit_usd` on any other kind are ignored.

Windows of kind `spend` (Cursor included usage and on-demand, Claude
`spend_limit`) appear in `status` with `unit: "usd"` and `used_usd` whenever
the source gave dollars, plus `limit_usd`, `limit` and `remaining` when it gave
a positive cap. The same fields are on the window's `reading`.

Each `CanStartAnswer` names its `basis`:

| `basis` | Meaning |
|---|---|
| `token_budget` | The window publishes a token budget, and the answer uses it. |
| `percent_only` | The window publishes only a percentage, so a token request cannot be mapped (`ok: false` unless exhausted). |
| `unavailable` | The provider has no current reading. |
| `unknown_window` | The provider reported a window whose measurement could not be read, so that limit may already be exhausted. Refuses. |
| `account_changed` | The reading was taken for a provider account other than the one the credentials name now. It is checked at every use, not only at publication. Refuses. |

When more than one basis applies, `can_start` answers with the first of `account_changed`, a stale reading (`unavailable`), the provider's own `limit_reached` refusal (`unavailable`, worded as a refusal), `unknown_window`, any other `unavailable`, then the headroom bases. A stale reading never answers `unknown_window`: it has no current reading, so an all-provider answer leaves it out like any other `unavailable` one. Every reading is attributed at use, an error reading included, unless it names no account and holds no quota evidence.

The credentials' account is `named`, `unnamed` (credentials load but name no account; every Claude credential) or `absent` (none load): unless the reading and the credentials name the same account, the reading answers only while exactly one account is known across it, the credentials and the provider's other local sources such as CodexBar, and some credential or local source is present, so a reading whose credentials were removed with no other source refuses with `account_changed`.

`watch` keeps the connection open. After the first snapshot, each daemon
refresh sends another `Response` with the same `id`. A later `ping` on that
connection is answered and is the only inbound keepalive that resets the
watch idle timer (junk frames do not). Closing the socket unsubscribes.
Idle is `max(600s, refresh_max_secs + 30s)` unless `QUOTA_WATCH_IDLE_SECS`
is set. The bundled client does not heartbeat; it relies on refresh
snapshots.

## Response

```json
{"id": 1, "ok": true, "result": { ... }}
```

or

```json
{"id": 1, "ok": false, "error": {"code": "unknown_method", "message": "..."}}
```

`protocol` in `version` is `2`. A new method or a new optional field does
not bump it. Removing or renaming a field, a new value in a closed enum, or a
request field that changes what a method answers does.

## Snapshot schema

See `quota_core::types`. Stable field names:

- `fetched_at` / `fetched_at_rfc3339`
- `providers[]`: `provider` (`codex` \| `claude` \| `cursor`), `status`
  (`ok` \| `unavailable`), `source` (`oauth` \| `cli` \| `cookie` \| `file` \|
  `statusline`), `windows[]`, `credits?`, `plan?`, `error?`, `credential_path?`
- each window: `kind` (`session` \| `five_hour` \| `weekly` \| `monthly` \|
  `extra` \| `spend`), `label`, optional `used_percent`, `remaining_percent`, `remaining`,
  `limit`, `unit`, `reset_at`, `reset_at_rfc3339`, `limit_window_seconds`

Clients **must ignore unknown fields**.

## Future clients (menu bar, Herdr, tmux, MCP)

Talk to this socket. Do not re-implement provider HTTP.

`quota-ctl` (library crate `quota_ctl`) is the CodexBar-shaped foil: add /
list / remove / select accounts and `refresh`. Menu-bar / tmux / MCP clients
should call the same methods — they must not become a second collector.

Python sketch:

```python
import json, os, socket, struct
from pathlib import Path

def socket_path():
    runtime = os.environ.get("XDG_RUNTIME_DIR")
    if runtime:
        return Path(runtime) / "quota" / "quota.sock"
    return Path.home() / ".local/share/quota/quota.sock"

def rpc(method, params=None, sock_path=None):
    payload = json.dumps({"id": 1, "method": method, "params": params or {}}).encode()
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect(str(sock_path or socket_path()))
    s.sendall(struct.pack("<I", len(payload)) + payload)
    header = b""
    while len(header) < 4:
        chunk = s.recv(4 - len(header))
        if not chunk:
            raise EOFError("socket closed while reading frame header")
        header += chunk
    n = struct.unpack("<I", header)[0]
    body = b""
    while len(body) < n:
        chunk = s.recv(n - len(body))
        if not chunk:
            raise EOFError("socket closed while reading frame body")
        body += chunk
    return json.loads(body)
```

A macOS menu bar or Herdr statusline is a renderer: call `status` / `pace` /
`watch` and draw. An MCP server would expose the same methods as tools.
