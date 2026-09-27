const { defineConfig, devices } = require("@playwright/test");
const path = require("path");
const crypto = require("crypto");
const { stageNextBundle } = require("./lib/stage-next-bundle.js");

// The layout suites' harness: every `next-parity-layout-*.spec.ts` pins a box
// the operator sized to fit (the fleet hero, a fleet card, the run page's
// MODEL section) to ONE size across every state it can show. Fixture and
// measurement live in `lib/layout-fixture.js`.
//
// Served by `lib/layout-server.js`, not `python3 -m http.server`: a live
// page needs an SSE stream that stays open to read as connected (see that
// file's own doc).
//
// The port. CI keeps 47926, the next free number after every sibling
// config's own PORT (47920-47925 at the time of writing); CI runs one
// checkout, so a fixed port cannot meet a stranger there. Locally, several
// checkouts (worktrees, sibling agents) run this suite at once, and with
// `reuseExistingServer` a fixed port meant one checkout silently tested
// ANOTHER checkout's server and bundle. So a local run gets a port of its
// own, derived from this checkout's path (stable across runs and across
// Playwright's workers, which each re-evaluate this file), in 48000-48999,
// clear of the siblings' fixed range. `DARKMUX_LAYOUT_PORT` overrides both.
const SERVED = path.join(__dirname, ".served-next-layout");
function checkoutPort() {
  const hash = crypto.createHash("sha256").update(path.resolve(__dirname)).digest();
  return 48000 + (hash.readUInt16BE(0) % 1000);
}
const PORT = Number(process.env.DARKMUX_LAYOUT_PORT) || (process.env.CI ? 47926 : checkoutPort());

// (#1737) Staging goes through the shared helper, which REFUSES a stale
// bundle instead of silently serving one. See lib/stage-next-bundle.js.
stageNextBundle(SERVED, "next-parity-layout.playwright.config");

module.exports = defineConfig({
  testDir: __dirname,
  testMatch: [/next-parity-layout-.*\.spec\.ts$/],
  forbidOnly: !!process.env.CI,
  retries: 0,
  fullyParallel: false,
  reporter: process.env.CI ? "github" : "list",
  timeout: 240_000,
  use: {
    baseURL: `http://127.0.0.1:${PORT}`,
    trace: "retain-on-failure",
    timezoneId: "UTC",
    locale: "en-US",
  },
  webServer: {
    command: `node ${path.join(__dirname, "lib", "layout-server.js")} ${SERVED} ${PORT}`,
    url: `http://127.0.0.1:${PORT}/index.html`,
    reuseExistingServer: !process.env.CI,
    timeout: 30_000,
  },
  projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"] } }],
});
