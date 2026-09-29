# Socket protocol (v1)

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
| `status`            | `{ "provider": "all"\|"codex"\|"claude" }`          | `{ "snapshot": Snapshot }`                  |
| `pace`              | `{ "provider": ... }`                               | `{ "reports": [PaceReport] }`               |
| `can_start`         | `{ "tokens": u64, "deadline"?: i64, "provider" }`   | `{ "ok": bool, "answers": [CanStartAnswer] }` |
| `watch`             | `{ "provider": ... }`                               | stream of `status` results, same `id`       |
| `refresh`           | `{ "provider": ... }`                               | `{ "snapshot": Snapshot }` after a probe    |
| `accounts.list`     | `{}`                                                | `{ "version", "active_id", "accounts" }`    |
| `accounts.add`      | see below                                           | `{ "account", "active_id" }`                |
| `accounts.remove`   | `{ "id" }`                                          | same as `accounts.list`                     |
| `accounts.select`   | `{ "id": string\|null }`                            | same as `accounts.list`                     |

Existing `status` / `pace` / `can_start` / `ping` / `version` / `watch` are
unchanged. New methods are additive; `protocol` stays `1`.

`provider` values today are `all`, `codex`, and `claude` because `ProviderId`
has those variants. A new collector adds a variant. The snapshot math does
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

`protocol` in `version` is `1`. Additive optional fields do not bump it.
Removing or renaming a field does.

## Snapshot schema

See `quota_core::types`. Stable field names:

- `fetched_at` / `fetched_at_rfc3339`
- `providers[]`: `provider`, `status` (`ok` \| `unavailable`), `source`
  (`oauth` \| `cli` \| `cookie` \| `file`), `windows[]`, `credits?`, `plan?`,
  `error?`, `credential_path?`
- each window: `kind` (`session` \| `five_hour` \| `weekly` \| `monthly` \|
  `extra`), `label`, optional `used_percent`, `remaining_percent`, `remaining`,
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
