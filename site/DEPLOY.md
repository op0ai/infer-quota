# Deploy fetchquota docs

Static output from Blume. No Workers, Alchemy, or app runtime. The custom domain is not attached yet (`fetchquota` or similar). Do not invent a production origin in `blume.config.ts` until that hostname exists.

## Build

From `site/` (Node.js 22.12+, [Bun](https://bun.sh) 1.4):

```bash
bun install --frozen-lockfile
bun run build
```

Output directory: `site/dist` (or `dist` if the host's root directory is `site`).

`bun run check` type-checks the Blume project. `bun run lint` and `bun run fmt:check` cover the small TypeScript and config surface (`oxlint`, `oxfmt`).

From the repository root the same scripts are `bun run dev`, `bun run build`, and `bun run check` after `bun run docs:install`.

## Vercel

- Root directory: `site`
- Framework preset: Other (or Astro if it does not rewrite the output dir)
- Install: `bun install --frozen-lockfile`
- Build: `bun run build`
- Output: `dist`
- Node.js: 22.x

Vercel detects the production origin at build time, so canonical URLs can stay correct before `deployment.site` is set. When the custom domain is the production domain, set `deployment.site` to that `https://` origin anyway so local and non-Vercel builds match.

A Git-connected project serves files from `dist/`. Redirect and header rules that Blume writes to `dist/vercel.json` are not read from the output folder on a Git deploy. This site has no redirects. Agents on the static deploy should use the `.md` mirrors (`/quotad.md`), not `Accept: text/markdown` negotiation (that header needs a server adapter).

## Cloudflare Pages

Same commands. Pages project:

- Root directory: `site` (or repository root with build `cd site && bun install --frozen-lockfile && bun run build` and output `site/dist`)
- Build command: `bun run build`
- Build output directory: `dist`
- Environment: `NODE_VERSION=22`

Cloudflare Pages exposes `CF_PAGES_URL`, which changes every deploy. After the custom domain is live, set `deployment.site` in `site/blume.config.ts` to that stable `https://` origin and redeploy. Until then, links inside `llms.txt` are root-relative.

No `wrangler` deploy. Do not switch `deployment` to `cloudflare()` unless you intentionally leave static hosting to run the docs MCP server.

## Custom domain

When the hostname is chosen:

1. Attach it in the Vercel or Cloudflare project.
2. Set `deployment.site` to `https://` plus that host.
3. Rebuild so `llms.txt`, the sitemap, and `agent-readability.json` use absolute URLs.

## Local preview

```bash
cd site
bun run build
bunx blume preview
```
