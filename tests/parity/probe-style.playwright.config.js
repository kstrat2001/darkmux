const { defineConfig, devices } = require("@playwright/test");
const fs = require("fs");
const path = require("path");
const { stageNextBundle } = require('./lib/stage-next-bundle.js');

// The catalog+replay-by-query lens's OWN acceptance-gate harness (Packet 4
// — the catalog panel and #mission=/#session=/bare-#<date> replay
// surfaces). Same structure as its two siblings — `next-playwright.config.js`
// (Packet 3, the runs lens, port 47920) and `next-parity.playwright.config.js`
// (Packet 2, the machine lens, port 47921) — each lens suite gets its own
// config + its own package.json script (`next-parity-catalog` here) rather
// than sharing one, so a script name never lies about which lens it runs
// and each is individually invokable. Port 47933: distinct from all of the
// above plus `tests/e2e`'s 47823 and `ui/verify`'s live throwaway-daemon
// range 8790+, so every suite can run concurrently without colliding.
//
// No meta-injection here (see `lib/extract-next-lens.js`'s module doc for
// why `viewer.html`'s `darkmux-mode=live` trick has no analog in `/next`) —
// the artifact is served byte-for-byte as committed.
const REPO_ROOT = path.join(__dirname, "..", "..");
const NEXT_HTML = path.join(REPO_ROOT, "crates", "darkmux-serve", "assets", "next.html");
const SERVED = path.join(__dirname, ".served-next-probe");
const PORT = 47933;

// (#1737) Staging goes through the shared helper, which REFUSES a stale
// bundle instead of silently serving one. See lib/stage-next-bundle.js.
stageNextBundle(SERVED, 'probe-style.playwright.config');

module.exports = defineConfig({
  testDir: __dirname,
  testMatch: ["probe-style.spec.ts"],
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
