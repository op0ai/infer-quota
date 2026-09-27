# CodexBar fixtures (redacted)

Offline captures for adapter and math tests. **Redacted only** — never replace
these with live Mac files.

| file | what |
|------|------|
| `usage-history.redacted.jsonl` | 1912 rows. `accountKey` is SHA256-truncated. Fields: `provider`, `resetsAt`, `sampledAt`, `source` (`live` \| `backfill`), `usedPercent`, `windowKind`, `windowMinutes`. This capture is `windowKind=secondary` only. |
| `codex-account-snapshots.redacted.json` | CodexBar snapshot envelope. `primary` / `tertiary` are null; `secondary` has `usedPercent`, `windowMinutes`, `resetsAt`. Fingerprints redacted. |
| `managed-codex-accounts.redacted.json` | Account metadata. No auth fingerprints, home paths, or tokens. |
| `CURRENT-SNAPSHOT.expect.json` | Condensed expected live shape for parser tests. |

## Do not commit

- `cursor-session.json` (WorkOS / Cursor JWT)
- `~/.codex/auth.json`
- `~/.claude/.credentials.json`
- raw `authFingerprint`, managed home paths, or any bearer token

Live Mac probes stay on the laptop. CI uses these files only.

In-repo files are the source of truth. A redacted dump from a laptop must
match these shapes; do not replace them with live Mac files.
