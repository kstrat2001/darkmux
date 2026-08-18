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
    // (finding, see lib/paths.js's SERVE_TOKEN comment) Docker's port
    // forwarding doesn't preserve loopback identity, so darkmux serve's
    // #881 auth gate treats every request here as remote and requires
    // this bearer token — attached context-wide so it rides on BOTH page
    // navigation and every fetch() the loaded viewer (next.html) issues
    // itself.
    extraHTTPHeaders: { Authorization: `Bearer ${SERVE_TOKEN}` },
  },
  // QA finding (M2): a top-level `use.viewport` is NOT the last word —
  // `devices['Desktop Chrome']` carries its OWN `viewport: {1280,720}`,
  // and a project's `use` object REPLACES the top-level one key-for-key
  // (it doesn't merge underneath it), so spreading the device here silently
  // overrode the phone-width viewport for every scenario: every
  // render-sanity "no horizontal scroll at phone width" claim was actually
  // measured at 1280px, and the gallery PNGs came out 1280 wide instead of
  // 390. tests/parity's playwright.config.js has this same
  // `projects: [{ use: { ...devices[...] } }]` shape and is NOT affected —
  // it makes no phone-width claim, so there's nothing for the device's
  // viewport to silently clobber there. The trap is specifically "a device
  // spread AFTER a viewport override" — the fix is naming the viewport
  // AFTER the spread, in the SAME object, so it's the last key to apply.
  projects: [
    {
      name: 'chromium',
      use: {
        ...devices['Desktop Chrome'],
        viewport: { width: 390, height: 844 }, // phone width — the chrome-order horizontal-scroll precedent
      },
    },
  ],
});
