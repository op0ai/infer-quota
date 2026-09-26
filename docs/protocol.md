# Socket protocol (v1)

`quotad` listens on a Unix domain socket (macOS + Linux).

## Path

Resolution order:

1. `--socket` on `quotad` / `quota`
2. `QUOTA_SOCKET`
3. `socket` in `~/.config/quota/config.json` (or `$XDG_CONFIG_HOME/quota/config.json`)
4. `$XDG_RUNTIME_DIR/quota/quota.sock` when `XDG_RUNTIME_DIR` is set and non-empty
5. `~/.local/share/quota/quota.sock`

The directory is created with mode `0700`; the socket is `0600`.

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

| method       | params                                              | result                                      |
|--------------|-----------------------------------------------------|---------------------------------------------|
| `ping`       | ignored                                             | `{"pong": true}`                            |
| `version`    | ignored                                             | `{"name","version","protocol"}`             |
| `status`     | `{ "provider": "all"\|"codex"\|"claude" }`          | `{ "snapshot": Snapshot }`                  |
| `pace`       | `{ "provider": ... }`                               | `{ "reports": [PaceReport] }`               |
| `can_start`  | `{ "tokens": u64, "deadline"?: i64, "provider" }`   | `{ "ok": bool, "answers": [CanStartAnswer] }` |
| `watch`      | `{ "provider": ... }`                               | stream of `status` results, same `id`       |

`deadline` is UTC unix seconds. When omitted, `can_start` binds to the window's
published `reset_at`.

`watch` keeps the connection open. After the first snapshot, each daemon
refresh sends another `Response` with the same `id`. A later `ping` on that
connection is answered; closing the socket unsubscribes.

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
    header = s.recv(4)
    n = struct.unpack("<I", header)[0]
    body = b""
    while len(body) < n:
        body += s.recv(n - len(body))
    return json.loads(body)
```

A macOS menu bar or Herdr statusline is a renderer: call `status` / `pace` /
`watch` and draw. An MCP server would expose the same methods as tools.
