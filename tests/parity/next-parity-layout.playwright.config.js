const { defineConfig, devices } = require("@playwright/test");
const path = require("path");
const { stageNextBundle } = require("./lib/stage-next-bundle.js");

// The layout suites' harness: every `next-parity-layout-*.spec.ts` pins a box
// the operator sized to fit (the fleet hero, a fleet card, the run page's
// MODEL section) to ONE size across every state it can show. Fixture and
// measurement live in `lib/layout-fixture.js`.
//
// Port 47926: the next free number after every sibling config's own PORT
// (47920-47925 at the time of writing). Served by `lib/layout-server.js`,
// not `python3 -m http.server`: a live page needs an SSE stream that stays
// open to read as connected (see that file's own doc).
const SERVED = path.join(__dirname, ".served-next-layout");
const PORT = 47926;

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
