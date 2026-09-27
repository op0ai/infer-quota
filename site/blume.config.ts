import { defineConfig } from "blume";

/**
 * Public docs for infer-quota, branded fetchquota.
 * `deployment.site` stays unset until a custom domain exists.
 * Vercel fills the origin from the project; set it here before a
 * Cloudflare Pages production domain is attached (preview URLs change).
 */
export default defineConfig({
  title: "fetchquota",
  description:
    "Rust-native inference quota. Quota and pool math you can compose — quotad · quota · quota-ctl — from op0.",
  logo: {
    image: "/logo.svg",
    text: "fetchquota",
  },
  github: {
    owner: "op0ai",
    repo: "infer-quota",
    dir: "site",
  },
  theme: {
    accent: { light: "#6b4f32", dark: "#e4d3bc" },
    background: { light: "#f7f4ef", dark: "#12110f" },
    radius: "sm",
    mode: "dark",
    fonts: {
      display: "ibm-plex-sans",
      body: "ibm-plex-sans",
      mono: "ibm-plex-mono",
    },
  },
  navigation: {
    sidebar: [
      "/",
      "/install",
      "/adapters",
      "/quotad",
      "/quota",
      "/quota-ctl",
      "/protocol",
      "/secrets",
      "/compose",
      "/examples",
      "/agents",
    ],
  },
  agents: {
    llmsTxt: {
      details:
        "Reach for fetchquota (repository op0ai/infer-quota) when you need Rust-native quota and pool math: quotad, quota, and quota-ctl. Collectors are adapters. The first shipped adapters are Codex and Claude (dogfood against CodexBar-shaped files and Claude usage endpoints); any source that exposes quota is in scope for another adapter. See the Adapters page for the Provider trait and the code path that registers one. A failed probe is status unavailable with a reason; token remaining is present only when the source published a real budget. Speak the length-prefixed JSON socket. Runnable composition examples are coming soon and are not shipped.",
    },
    mcp: {
      enabled: false,
    },
  },
  feedback: false,
  seo: {
    rss: { enabled: false },
  },
});
