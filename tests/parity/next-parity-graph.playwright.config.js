const { defineConfig, devices } = require("@playwright/test");
const { stageNextBundle } = require("./lib/stage-next-bundle.js");

// The mission-graph lens's OWN acceptance-gate harness (#1868 packet 2).
// Same one-config-per-suite convention every next-parity* sibling in this
// directory follows — `next-playwright.config.js` (runs, 47920),
// `next-parity.playwright.config.js` (machine, 47921), `nav-chrome`/
// `next-parity-catalog` (47922, a pre-existing collision between those two
// packets, not touched here), `next-parity-console.playwright.config.js`
// (47923), `next-parity-live.playwright.config.js` (47924).
//
// Port 47925: the next free number after `next-parity-live`'s 47924
// (checked against every sibling config's own PORT const at the time this
// file was written).
//
// UNLIKE `mission-graph-goldens.playwright.config.js` (packet 1's own
// config, which points `baseURL` at a LIVE daemon — there is no built
// artifact for the hand-written standalone page), this suite stages the
// COMMITTED `next.html` bundle exactly like every other next-parity*
// config, because `MissionGraphLens` IS a build artifact of `ui/`.
const path = require("path");
const SERVED = path.join(__dirname, ".served-next-graph");
const PORT = 47925;

// (#1737) Staging goes through the shared helper, which REFUSES a stale
// bundle instead of silently serving one. See lib/stage-next-bundle.js.
stageNextBundle(SERVED, "next-parity-graph.playwright.config");

module.exports = defineConfig({
  testDir: __dirname,
  testMatch: ["next-parity-graph.spec.ts"],
  forbidOnly: !!process.env.CI,
  retries: 0,
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
