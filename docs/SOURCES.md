# Credential locations and usage APIs

v0 only talks to **Codex** and **Claude**. Everything below that is not an
official, versioned public API is labeled **hypothesis**. When a probe fails we
return `status: unavailable` with a reason. We never invent remaining tokens.

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

`primary_window` is mapped to `kind: session` (CodexBar's "session" lane).
`secondary_window` → `weekly`.

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
| macOS Keychain service `Claude Code-credentials` | **Documented only in v0.** We do not call Security.framework |

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

### Response fields we accept (hypothesis)

Buckets may be `null` (skip):

- `five_hour.utilization` + `resets_at` (RFC3339)
- `seven_day.*`
- `seven_day_opus` / `seven_day_sonnet` (extra weekly)
- `extra_usage` credits when present

`utilization` is percent **used** (0–100), not remaining.

### Not in v0

- Keychain reads
- `claude.ai` cookie / org usage endpoints
- Local JSONL cost scans as a substitute for remaining quota
- Token refresh against `console.anthropic.com` (would mutate the shared file)

## Accuracy

Percent windows cannot be mapped to `--tokens N` without a published token
limit. `can_start` then returns `basis: percent_only`, `ok: false`, and an
explanation — unless remaining is 0% (definitely no) or tokens is 0.
