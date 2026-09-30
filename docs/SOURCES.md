# Credential locations and usage APIs

The first shipped collectors are **Codex** and **Claude**, from early dogfood
against CodexBar-shaped data and Claude usage endpoints. **Claude statusline**
(push) and **Cursor** were added after them; each is its own crate. This note is those
probes. A later adapter gets its own section when it lands. Everything below
that is not an official, versioned public API is labeled **hypothesis**. When
a probe fails we return `status: unavailable` with a reason. We never invent
remaining tokens.

We **read** existing session files. We **never write** them and never store
passwords.

## Codex

### Credentials (observed / documented by Codex CLI + CodexBar)

| Path | When |
|------|------|
| `$CODEX_HOME/auth.json` | If `CODEX_HOME` is set — isolated; we do not fall through |
| `~/.codex/auth.json` | Default Codex CLI home |
| `~/.config/codex/auth.json` | Legacy fallback when `CODEX_HOME` is unset (CodexBar) |

Parsed fields only: `tokens.access_token`, `tokens.account_id` /
`tokens.chatgpt_account_id`, or top-level `access_token` /
`chatgpt_account_id`. Refresh tokens are not retained after parse.

### Usage probe (hypothesis)

`GET https://chatgpt.com/backend-api/wham/usage`

Headers: `Authorization: Bearer <access_token>`, optional
`ChatGPT-Account-Id`, `Accept: application/json`.

This is the route CodexBar and several community tools use. It is **not** a
documented public OpenAI REST API and can change without notice.

If `~/.codex/config.toml` sets `chatgpt_base_url`:

- ChatGPT `.../backend-api` → `{base}/wham/usage`
- anything else → `{base}/api/codex/usage` (**hypothesis**, OpenUsage)

### Response fields we accept (hypothesis)

`rate_limit.primary_window` / `secondary_window`:

- `used_percent` (0–100 consumed)
- `limit_window_seconds` (18000 ≈ 5h session, 604800 ≈ weekly)
- `reset_at` (unix seconds)

`credits.balance` / `has_credits` / `unlimited` when present.

**Duration wins over slot name.** `limit_window_seconds` (or CodexBar
`windowMinutes`) classifies the window:

- ~18000 s / 300 min → `kind: session`, label `5h`
- ~604800 s / 10080 min → `kind: weekly`, label `weekly` (CodexBar `secondary`)

`primary_window` is only a hint. Live dogfood on 2026-09-27 (Arth Mac, file
OAuth) published a **7-day** window (`used_percent` 59, `limit_window_seconds`
604800, plan `pro`, reset ~2026-10-03T16:58:09Z) inside `primary_window`.
Labeling that `session` / `5h` is a bug; we map it to weekly/secondary.

`secondary_window` with a real 5h duration would similarly be remapped.

### CodexBar local snapshot files (fixture parser, not a probe)

CodexBar on macOS stores a different JSON shape under
`~/Library/Application Support/CodexBar/` (do not copy raw). Redacted
fixtures live in `fixtures/codexbar/`.

Lanes `primary` / `secondary` / `tertiary` use `usedPercent`,
`windowMinutes`, `resetsAt`. Null lanes are skipped. This capture is
**secondary-only** (`windowMinutes=10080` → weekly). `creditsAvailable:
false` maps to `credits.has_credits=false` — we do not invent a balance.

Usage-history JSONL rows carry `source=live|backfill`. That is sample
provenance, not a credential `Source`.

### CodexBar file ingest (optional, supplemental)

When `enable_codexbar_files` is true (default), `quotad` may read — never
write — these names under `$QUOTA_CODEXBAR_DIR` or
`~/Library/Application Support/CodexBar/`:

| file | use |
|------|-----|
| `codex-account-snapshots.json` | Fallback snapshot if `/wham/usage` is down or creds missing |
| `usage-history.jsonl` | Seed the in-memory pace ring (newest `ring_capacity` rows) |

Redacted copies of the same shapes are in `fixtures/codexbar/` (`.redacted`
suffix). We **never** open `cursor-session.json` (WorkOS JWT).

**Hypothesis:** CodexBar's in-app snapshot is derived from the same
undocumented `/wham/usage` family. We do not claim the on-disk shape is a
stable API.

### Not in v0

- `codex app-server` JSON-RPC (`account/rateLimits/read`)
- Browser cookie stores (we will not load a cookie DB into memory)
- Writing refreshed tokens back to `auth.json` (Codex CLI owns that file)

## Claude

### Credentials

| Location | Notes |
|----------|-------|
| `$CLAUDE_CONFIG_DIR/.credentials.json` | Comma-separated dirs, Claude Code convention |
| `~/.claude/.credentials.json` | Linux + file fallback on macOS. Mode `0600` |
| macOS Keychain service `Claude Code-credentials` | **Read first on macOS** for the default account, through `quota-secrets` (`ClaudeCodeKeychain`, read-only). The file there is stale. macOS asks you to approve the first read. Skipped when `CLAUDE_CONFIG_DIR` or an account `home_path` isolates the account, or `QUOTA_NO_KEYCHAIN=1` |

OAuth block: `claudeAiOauth.accessToken`. `expiresAt` may be epoch **ms**
(≥ 1e11) or seconds — we detect the unit. We do not write the file.

If the OAuth block is missing (API-key / `ANTHROPIC_API_KEY` mode), the
subscription usage endpoint will not work. We report `no_oauth_token` rather
than calling it.

### Usage probe (hypothesis)

`GET https://api.anthropic.com/api/oauth/usage`

Headers: `Authorization: Bearer <accessToken>`,
`anthropic-beta: oauth-2025-04-20`.

Undocumented; same family of data Claude Code `/usage` shows. Often
rate-limited (HTTP 429). The daemon backs off to `refresh_max_secs`.

Live dogfood 2026-09-27: HTTP **401** (expired OAuth) → `status:
unavailable`, `code: unauthorized`. We do **not** invent Claude windows.

### Response fields we accept (hypothesis)

Buckets may be `null` (skip):

- `five_hour.utilization` + `resets_at` (RFC3339)
- `seven_day.*`
- `seven_day_opus` / `seven_day_sonnet` (extra weekly)
- `extra_usage` credits when present

`utilization` is percent **used** (0–100), not remaining.

### Statusline push (preferred, zero credentials)

Claude Code runs the configured `statusLine` command and writes a JSON object
to its stdin. `quota statusline` reads it, pushes an `observe` to `quotad`, and
prints a segment (`5h 34% ↺2h13m · wk 12% ↺3d4h`).

```json
{ "statusLine": { "type": "command", "command": "quota statusline" } }
```

To keep an existing statusline: `quota statusline --chain "<your old command>"`.
The chained command gets the same stdin; if it takes over 1 s it is dropped.
The push has a 50 ms budget and the segment is computed from stdin alone, so
the status line still prints when `quotad` is down. Exit code is always 0.

Fields pinned in [`fixtures/claude-statusline/`](../fixtures/claude-statusline/)
(each fixture names its sources). Changelog links are at the immutable commit
`anthropics/claude-code@8364969e`; the docs page is mutable, so the text we
read is pinned by date and hash (2026-09-29, sha256
`6ddde6b55a53ae3a10c1c105729cb2c112ed19b30ff343c37f4a5241bce20080`).

| Field | Type / unit | Since | Source |
|-------|-------------|-------|--------|
| `rate_limits.five_hour.used_percentage` | number, percent used 0-100 | 2.1.80 | [CHANGELOG L5334-L5336](https://github.com/anthropics/claude-code/blob/8364969e9f5234ef3d9743cf7c790e9aab0ac3b1/CHANGELOG.md#L5334-L5336); [statusline docs](https://code.claude.com/docs/en/statusline) field table line 193 |
| `rate_limits.five_hour.resets_at` | Unix epoch seconds | 2.1.80 | same; docs line 194 |
| `rate_limits.seven_day.used_percentage` / `.resets_at` | same | 2.1.80 | same; docs lines 193-194 |
| `rate_limits.spend_limit.used_percentage` / `.resets_at` | percent (above 100 once over the limit), epoch seconds | 2.1.251 | [CHANGELOG L1686-L1690](https://github.com/anthropics/claude-code/blob/8364969e9f5234ef3d9743cf7c790e9aab0ac3b1/CHANGELOG.md#L1686-L1690); docs line 195 |
| `rate_limits.spend_limit.used_usd` / `.limit_usd` / `.period` | dollars, dollars, string | 2.1.284 | [CHANGELOG L3-L7](https://github.com/anthropics/claude-code/blob/8364969e9f5234ef3d9743cf7c790e9aab0ac3b1/CHANGELOG.md#L3-L7): "the status line's `rate_limits.spend_limit` also gains `used_usd`, `limit_usd` and `period`". Not yet in the docs field table |

`rate_limits` appears only for claude.ai Pro/Max subscribers or behind a
Claude apps gateway with a spend limit, and only after the first API response
of a session (docs line 339). Each window may be absent on its own. Absence is
an empty segment and no push, never a guessed number. A bucket that is present
but unreadable (not an object, or a missing/negative/non-numeric
`used_percentage` with no dollars to stand in) stays in the push as an
unreadable window and shows as `?`; it is never dropped, and it makes percent
admission refuse.

`spend_limit` becomes one `spend` window built from a single source of truth:

| Pushed | Window |
|--------|--------|
| `used_usd` and a positive `limit_usd` | `unit: usd` with `used_usd`, `limit_usd`, `limit`, `remaining` = limit − used (floor 0); percent derived from the dollars, so it cannot disagree with them. Over the cap it is `exhausted` |
| `used_percentage` only, or with dollars but no positive `limit_usd` (2.1.251-2.1.283) | `unit: percent`; no dollar fields |
| `used_usd` alone | `unit: usd`, spend recorded, no remaining percent (uncapped) |

Evidence is stamped with the daemon's clock at receipt and is current for 300 s
(the same rule as any reading). A current push outranks the OAuth poll below.

### Not in v0

- `claude.ai` cookie / org usage endpoints
- Local JSONL cost scans as a substitute for remaining quota
- Token refresh against `console.anthropic.com` (would mutate the shared file)

## Cursor

Off by default: `"enable_cursor": true` in the config. Crate
`quota-source-cursor`.

### Credential

The dashboard session cookie `WorkosCursorSessionToken`: either the bare value
(no `=`, `;`, `,`, quote, backslash or space) or a whole `Cookie:` header value
(`name=value` pairs joined by `;`, one of them a non-empty
`WorkosCursorSessionToken`). Anything else is `bad_credential` before any
request. We do **not** read a browser cookie store: you copy the
value once. It is looked up through `quota-secrets` at `cursor_secret_path`
(default `cursor/session`), first hit wins
(`quota_secrets::keychain_first_from_env_for_path`):

1. OS keychain: `QUOTA_CURSOR_SESSION=<value> quota-ctl secret put cursor/session --from-env QUOTA_CURSOR_SESSION`
2. OpenBao (quotad built with `--features openbao`, `QUOTA_OPENBAO_*` set)
3. A read-only file: `$QUOTA_CURSOR_COOKIE_FILE` or `~/.config/quota/cursor-session`,
   refused unless mode is `0600`

This order is Cursor's own. The default chain (`quota_secrets::from_env`,
OpenBao first; see [SECRETS.md](SECRETS.md)) is unchanged, and it is what
`quota-ctl secret` uses: `put` writes to its first writable backend, so with
OpenBao configured the cookie lands in OpenBao, and a keychain copy of
the configured path, if one exists, is read before it. `QUOTA_NO_KEYCHAIN=1`
skips that lookup, and a malformed OpenBao configuration does not prevent the
local keychain or cookie file from satisfying the read. The cookie file also
works when `cursor_secret_path` is custom.

The value goes into one `Cookie` header. It is never logged and never appears
in an error or a snapshot. A value with control or non-ASCII characters is
refused before any request (header injection).

### Usage probe (hypothesis)

`GET https://cursor.com/api/usage-summary` with `Cookie`. This is the route
cursor.com's own usage page and CodexBar call; it is not a versioned public
API. Followed from CodexBar at the immutable commit `steipete/CodexBar@25bba9b7`:

| What | CodexBar file and lines |
|------|-------------------------|
| request: `GET /api/usage-summary`, `Cookie` header, 401/403 = not logged in, JSON decode | [`CursorStatusProbe.swift` L1404-L1438](https://github.com/steipete/CodexBar/blob/25bba9b7fd9ce83c33053958f7366e23b2dc8a82/Sources/CodexBarCore/Providers/Cursor/CursorStatusProbe.swift#L1404-L1438) (header L1412, 401/403 L1420) |
| session cookie name `WorkosCursorSessionToken` | [`CursorStatusProbe.swift` L21-L22](https://github.com/steipete/CodexBar/blob/25bba9b7fd9ce83c33053958f7366e23b2dc8a82/Sources/CodexBarCore/Providers/Cursor/CursorStatusProbe.swift#L21-L22) |
| `CursorUsageSummary` (`billingCycleStart/End`, `membershipType`, `isUnlimited`, `individualUsage`) | [`CursorStatusProbe.swift` L201-L211](https://github.com/steipete/CodexBar/blob/25bba9b7fd9ce83c33053958f7366e23b2dc8a82/Sources/CodexBarCore/Providers/Cursor/CursorStatusProbe.swift#L201-L211) |
| `CursorPlanUsage` (`enabled/used/limit/remaining` in cents, `totalPercentUsed`) | [`CursorStatusProbe.swift` L250-L262](https://github.com/steipete/CodexBar/blob/25bba9b7fd9ce83c33053958f7366e23b2dc8a82/Sources/CodexBarCore/Providers/Cursor/CursorStatusProbe.swift#L250-L262) |
| `CursorOnDemandUsage` (cents; `limit` nil when uncapped) | [`CursorStatusProbe.swift` L270-L278](https://github.com/steipete/CodexBar/blob/25bba9b7fd9ce83c33053958f7366e23b2dc8a82/Sources/CodexBarCore/Providers/Cursor/CursorStatusProbe.swift#L270-L278) |
| billing cycle dates, plan percent precedence, on-demand cents to USD | [`CursorStatusProbe+UsageSummary.swift` L15-L16, L43-L61, L85-L86](https://github.com/steipete/CodexBar/blob/25bba9b7fd9ce83c33053958f7366e23b2dc8a82/Sources/CodexBarCore/Providers/Cursor/CursorStatusProbe+UsageSummary.swift#L43-L61) |

One deliberate difference: CodexBar's headline percent prefers
`totalPercentUsed` (L45-L46) and derives from cents only as a fallback. We
derive the `included` percent from cents and use `totalPercentUsed` only when
cents are missing, so the percent and the dollars we report cannot disagree.
Check this against the dashboard on the first live run.

Fields (all optional): `billingCycleStart`, `billingCycleEnd` (ISO-8601),
`membershipType`, `isUnlimited`, `individualUsage.plan.{enabled,used,limit,remaining,totalPercentUsed}`,
`individualUsage.onDemand.{enabled,used,limit,remaining}`. `used`/`limit`/`remaining`
are **cents**.

Mapping (`kind: spend`, `unit: usd`, evidence current 15 min, polled no more
often than every 5 min):

| Window label | From | Notes |
|--------------|------|-------|
| `included` | `plan.used` / `plan.limit` | dollars; percent derived from them. `totalPercentUsed` is used only when cents are missing |
| `on-demand` | `onDemand.used` / `onDemand.limit` | skipped only when `enabled: false`. With no limit it records spend and publishes no percent, so admission never treats it as headroom |

Every bucket that is not `enabled: false` becomes a window. One with no
readable measurement (`used` missing, null, negative or not finite, and for
`included` no `totalPercentUsed` either) is an `unknown` window: it is never
dropped, and percent admission refuses while it is unknown.

`billingCycleEnd` is `reset_at`; the cycle length is `limit_window_seconds`.
`isUnlimited` with no usage becomes `credits.unlimited`. 401/403 is
`unauthorized`, 429 is `rate_limited`; no readable plan or on-demand usage at
all (and not unlimited) is `empty`. Fixtures: [`fixtures/cursor/`](../fixtures/cursor/) (synthetic values).

## Accuracy

Percent windows cannot be mapped to `--tokens N` without a published token
limit. `can_start` then returns `basis: percent_only`, `ok: false`, and an
explanation — unless remaining is 0% (definitely no) or tokens is 0.
