import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { viteSingleFile } from "vite-plugin-singlefile";

// Packet 1 (UI port) — build config only. This project builds to exactly ONE
// self-contained `dist/index.html` (inlined JS/CSS, no separate chunks), which
// the `copy-artifact` script commits to
// `crates/darkmux-serve/assets/next.html`. That file is `include_str!`'d by
// the daemon at `GET /next` (see `crates/darkmux-serve/src/lib.rs`) so the
// release binary stays self-contained and node-free — the same posture the
// legacy `viewer.html` already has.
//
// `base: "./"` matters even for a singlefile build: with the default `/`
// base, Vite would still emit an absolute-rooted asset reference in the HTML
// before `vite-plugin-singlefile` inlines it, and a relative `./` base is the
// documented-safe combination for that plugin.
export default defineConfig({
  base: "./",
  plugins: [react(), viteSingleFile()],
  build: {
    // A single logical build target keeps output to one HTML file — no
    // vendor-chunk splitting to fight the singlefile plugin over.
    cssCodeSplit: false,
    assetsInlineLimit: 100_000_000,
  },
});
