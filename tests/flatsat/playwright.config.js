const { defineConfig, devices } = require('@playwright/test');
const { SERVE_TOKEN } = require('./lib/paths.js');

// CJS, matching tests/parity's playwright.config.js. Unlike parity, there
// is no `webServer` here — `up.sh` (via `check.sh`/`bun run up`) already
// brings the whole compose fleet up before scenarios run; this config just
// points at the hub's real mapped port. Never 8765 (the operator's live
// daemon).
const HUB_PORT = 18765;

module.exports = defineConfig({
  testDir: __dirname,
  testMatch: 'scenarios/*.spec.ts',
  forbidOnly: !!process.env.CI,
  retries: 0,
  fullyParallel: false, // scenarios mutate shared container state (pause/stop/kill) — must not interleave
  workers: 1,
  reporter: process.env.CI ? 'github' : 'list',
  use: {
    baseURL: `http://127.0.0.1:${HUB_PORT}`,
    trace: 'retain-on-failure',
    viewport: { width: 390, height: 844 }, // phone width — the chrome-order horizontal-scroll precedent
    // (finding, see lib/paths.js's SERVE_TOKEN comment) Docker's port
    // forwarding doesn't preserve loopback identity, so darkmux serve's
    // #881 auth gate treats every request here as remote and requires
    // this bearer token — attached context-wide so it rides on BOTH page
    // navigation and every fetch() the loaded viewer.html issues itself.
    extraHTTPHeaders: { Authorization: `Bearer ${SERVE_TOKEN}` },
  },
  projects: [{ name: 'chromium', use: { ...devices['Desktop Chrome'] } }],
});
