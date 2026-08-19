const { defineConfig, devices } = require("@playwright/test");

// #1868 packet 1's own acceptance-gate harness, the same one-config-per-suite
// convention every next-parity* sibling in this directory follows (its own
// testMatch, its own package.json script, so a script name never lies about
// which suite runs).
//
// UNLIKE every next-parity* sibling, this config does NOT stage a copy of a
// built bundle and serve it over `python3 -m http.server`, because there is
// no BUILT bundle to stage. The page under test,
// `crates/darkmux-serve/assets/mission-graph.html`, is a hand-written,
// no-build-step page the daemon serves LIVE at `GET /mission/:id/graph`
// (see that file's own module doc). This is the STANDALONE page's own
// parity capture, taken BEFORE it gets folded into the React port (the
// later packet in #1868). So `baseURL` points straight at the operator's
// live daemon instead of a staged static server.
//
// `page.route` (installed by `mission-graph-goldens.spec.ts` via
// `lib/mock-routes.js`) intercepts every DATA endpoint the page fetches
// (`/mission/:id/graph.json`, `/flow-mission/:id`, `/flow/:date`,
// `/flow/:date/stream`), so nothing here depends on the daemon's live DATA.
// What it does still depend on is the daemon actually running, so the
// page's own static HTML/CSS/JS and the vendored `/vendor/reactflow-bundle.*`
// assets have somewhere to load from: `mock-routes.js`'s catch-all
// `route.continue()`s those straight through to the real daemon rather than
// 404ing them, exactly like every other unmatched static-asset path this
// harness already forwards.
//
// `DARKMUX_DAEMON_URL` overrides the default, matching `record.mjs`'s own
// env var, the daemon this suite's corpus was recorded from.
const DAEMON_URL = process.env.DARKMUX_DAEMON_URL || "http://127.0.0.1:8765";

module.exports = defineConfig({
  testDir: __dirname,
  testMatch: ["mission-graph-goldens.spec.ts"],
  forbidOnly: !!process.env.CI,
  retries: 0,
  // One worker: the capture tests WRITE goldens/*.txt that the red-prove
  // tests in the same file then read back; see the spec's own module doc.
  // Running out of order (or in parallel) would race that write/read.
  fullyParallel: false,
  workers: 1,
  reporter: process.env.CI ? "github" : "list",
  use: {
    baseURL: DAEMON_URL,
    trace: "retain-on-failure",
    timezoneId: "UTC",
    locale: "en-US",
    headless: true,
  },
  projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"] } }],
});
