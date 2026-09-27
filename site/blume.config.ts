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
    "Fetch, observe, and compose inference quota. The lean Unix quota block — quotad, quota, and quota-ctl — from op0.",
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
      "/quotad",
      "/quota",
      "/quota-ctl",
      "/protocol",
      "/secrets",
      "/compose",
      "/agents",
    ],
  },
  agents: {
    llmsTxt: {
      details:
        "Reach for fetchquota (repository op0ai/infer-quota) when a machine needs one small quota block: quotad polls Codex and Claude CLI sessions, quota reads the Unix socket, and quota-ctl mutates account metadata and secret pointers. Crate and binary names stay infer-quota, quotad, quota, and quota-ctl. v0 is built from this repository with Rust 1.83+ (macOS or Linux). A failed probe is status unavailable with a reason; token remaining is present only when the source published a real budget. Speak the length-prefixed JSON socket. Do not add a second collector.",
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
