import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { viteSingleFile } from "vite-plugin-singlefile";

// Packet 1 (UI port) — build config only. This project builds to exactly ONE
// self-contained `dist/index.html` (inlined JS/CSS, no separate chunks), which
// the `copy-artifact` script commits to
// `crates/darkmux-serve/assets/next.html`. That file is `include_str!`'d by
// the daemon at `GET /next` (see `crates/darkmux-serve/src/lib.rs`) so the
// release binary stays self-contained and node-free — the same posture the
// legacy `viewer.html` had before its retirement (#1806).
//
// `base: "./"` matters even for a singlefile build: with the default `/`
// base, Vite would still emit an absolute-rooted asset reference in the HTML
// before `vite-plugin-singlefile` inlines it, and a relative `./` base is the
// documented-safe combination for that plugin.
// ── DEV SERVER (build output is unaffected by anything below) ────────────
//
// `bun run dev` gives hot-module reload against the REAL daemon's data. This
// exists because the production loop is brutal for looking at pixels: the
// bundle is `include_str!`'d into the binary, so a one-line CSS change needs
// a ~89s Rust rebuild plus a daemon restart before it can be seen — measured,
// and the same cost the legacy viewer had. `bun run build` itself is 0.8s;
// the other 99% is Rust relinking a file it only embeds.
//
// With this proxy, an API request from the dev server goes to the daemon on
// 8765 while the UI hot-reloads locally in well under a second. Nothing here
// ships: `vite build` ignores `server`.
//
// `host: true` binds beyond loopback so the dev server is reachable from a
// phone over the tailnet — reviewing mobile layout on an actual phone is the
// only way this project has reliably caught mobile bugs.
const DAEMON = "http://127.0.0.1:8765";
const API_PREFIXES = [
  "/flow", "/flow-days", "/flow-missions", "/flow-mission", "/flow-session",
  "/flow-status", "/runs", "/missions", "/phases", "/machine", "/fleet",
  "/lab", "/panel", "/worktree-summary", "/health", "/mission",
];

export default defineConfig({
  base: "./",
  server: {
    host: true,
    port: 5273,
    // Vite rejects unknown Host headers with a 403 (DNS-rebinding guard).
    // The tailnet name is exactly such a host, and reviewing on a real phone
    // is how this project actually finds mobile bugs — so allow the tailnet
    // domain rather than disabling the check outright.
    allowedHosts: [".ts.net", "localhost"],
    proxy: Object.fromEntries(
      API_PREFIXES.map((p) => [p, { target: DAEMON, changeOrigin: true }]),
    ),
  },
  plugins: [react(), viteSingleFile()],
  build: {
    // A single logical build target keeps output to one HTML file — no
    // vendor-chunk splitting to fight the singlefile plugin over.
    cssCodeSplit: false,
    assetsInlineLimit: 100_000_000,
  },
});
