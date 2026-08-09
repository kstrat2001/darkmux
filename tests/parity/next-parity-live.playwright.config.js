const { defineConfig, devices } = require("@playwright/test");
const fs = require("fs");
const path = require("path");
const { stageNextBundle } = require('./lib/stage-next-bundle.js');

// The live/SSE lens's OWN acceptance-gate harness (Packet 5). Same structure
// as its siblings — `next-playwright.config.js` (Packet 3, the runs lens,
// port 47920), `next-parity.playwright.config.js` (Packet 2, the machine
// lens, port 47921), `next-parity-catalog.playwright.config.js` (Packet 4,
// the catalog lens, port 47922), `next-parity-console.playwright.config.js`
// (Packet 6, the console lens, port 47923) — each lens/concern gets its own
// config + its own package.json script (`next-parity-live` here) rather than
// sharing one, so a script name never lies about which suite runs.
//
// Port 47924: the next free number after the console suite's 47923 (checked
// against every sibling config's own PORT const at the time this file was
// written — none of them claim 47924).
//
// This suite is a REAL-BROWSER requirement, not a stylistic choice: it
// exercises a real `EventSource` against `/flow/:date/stream`, which jsdom
// (this repo's vitest environment) doesn't implement at all — see
// `useLiveTail.test.ts`'s own doc for why THAT suite has to inject a mock
// instead. Playwright's Chromium has a real `EventSource`, driven by real
// (fast, loopback) HTTP responses this config's own `page.route` handlers
// fulfill — see `next-parity-live.spec.ts`'s module doc for how a
// `route.fulfill`-delivered SSE body behaves (it's a COMPLETE response, not
// a true open-ended stream, which shapes several of that file's assertions).
const REPO_ROOT = path.join(__dirname, "..", "..");
const NEXT_HTML = path.join(REPO_ROOT, "crates", "darkmux-serve", "assets", "next.html");
const SERVED = path.join(__dirname, ".served-next-live");
const PORT = 47924;

// (#1737) Staging goes through the shared helper, which REFUSES a stale
// bundle instead of silently serving one. See lib/stage-next-bundle.js.
stageNextBundle(SERVED, 'next-parity-live.playwright.config');

module.exports = defineConfig({
  testDir: __dirname,
  testMatch: ["next-parity-live.spec.ts"],
  forbidOnly: !!process.env.CI,
  retries: 0,
  // One worker, same as every sibling — the live tail's own timers/state
  // (the frozen `page.clock`, the `#modebadge` transitions) are per-page but
  // running several of these pages in parallel against the same static
  // server has no upside here and the sibling configs all made the same
  // call.
  fullyParallel: false,
  reporter: process.env.CI ? "github" : "list",
  use: {
    baseURL: `http://127.0.0.1:${PORT}`,
    trace: "retain-on-failure",
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
