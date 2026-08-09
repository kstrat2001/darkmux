const { defineConfig, devices } = require("@playwright/test");
const fs = require("fs");
const path = require("path");
const { stageNextBundle } = require('./lib/stage-next-bundle.js');

// The console (CLI-panel) lens's OWN acceptance-gate harness (Packet 6).
// Same structure as its siblings — `next-playwright.config.js` (Packet 3,
// the runs lens, port 47920), `next-parity.playwright.config.js` (Packet 2,
// the machine lens, port 47921), `next-parity-catalog.playwright.config.js`
// (Packet 4, the catalog lens, port 47922) — each lens suite gets its own
// config + its own package.json script (`next-parity-console` here) rather
// than sharing one, so a script name never lies about which lens it runs
// and each is individually invokable. This file REPLACES this lens's prior
// arrangement (a shared `testMatch` array on `next-playwright.config.js`,
// alongside the runs suite) — a QA post-merge review (2026-08-09) found that
// shape doesn't survive concurrent lens packets: Packet 4's own back-merge
// resolution independently reverted `next-playwright.config.js`'s testMatch
// to runs-only for exactly the same reason this file's siblings state
// above, which would have silently dropped console coverage from `bun run
// next-parity-runs` had this file not been split out to match.
//
// Port 47923: distinct from every sibling above plus `tests/e2e`'s 47823 and
// `ui/verify`'s live throwaway-daemon range 8790+, so every suite can run
// concurrently without colliding. (Note for the morning port audit: Packet
// 1.5's `nav-chrome.playwright.config.js` and Packet 4's
// `next-parity-catalog.playwright.config.js` both currently claim 47922 —
// a genuine collision between those two packets, pre-existing before this
// file, not touched here since it belongs to packets this one doesn't own.)
//
// No meta-injection here (see `lib/extract-next-lens.js`'s module doc for
// why `viewer.html`'s `darkmux-mode=live` trick has no analog in `/next`) —
// the artifact is served byte-for-byte as committed.
const REPO_ROOT = path.join(__dirname, "..", "..");
const NEXT_HTML = path.join(REPO_ROOT, "crates", "darkmux-serve", "assets", "next.html");
const SERVED = path.join(__dirname, ".served-next-console");
const PORT = 47923;

// (#1737) Staging goes through the shared helper, which REFUSES a stale
// bundle instead of silently serving one. See lib/stage-next-bundle.js.
stageNextBundle(SERVED, 'next-parity-console.playwright.config');

module.exports = defineConfig({
  testDir: __dirname,
  testMatch: ["next-parity-console.spec.ts"],
  forbidOnly: !!process.env.CI,
  retries: 0,
  fullyParallel: false,
  reporter: process.env.CI ? "github" : "list",
  use: {
    baseURL: `http://127.0.0.1:${PORT}`,
    trace: "retain-on-failure",
    // Same determinism pin as every sibling config — relative-age
    // rendering reads the ambient TZ/locale unless pinned at the browser
    // context level.
    timezoneId: "UTC",
    locale: "en-US",
  },
  webServer: {
    command: `python3 -m http.server ${PORT} --directory ${SERVED}`,
    url: `http://127.0.0.1:${PORT}/index.html`,
    reuseExistingServer: !process.env.CI,
    timeout: 30_000,
  },
  projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"] } }],
});
