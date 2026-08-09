const { defineConfig, devices } = require('@playwright/test');
const fs = require('fs');
const path = require('path');
const { stageNextBundle } = require('./lib/stage-next-bundle.js');

// The NEW-UI ACCEPTANCE-GATE harness (Packet 2 — the machine lens). Mirrors
// playwright.config.js's structure exactly (same reasoning: CJS because
// Playwright's own loader requires it, port pinned distinctly from both the
// legacy-parity harness (47919) and tests/e2e (47823) so all three suites
// can run concurrently) but serves a DIFFERENT artifact under test: the
// COMMITTED BUILT React port (`crates/darkmux-serve/assets/next.html`, `ui/`'s
// `bun run build` output), not the legacy `viewer.html`.
//
// This is the "did the PORT actually reproduce the SPEC" half of the parity
// story — `playwright.config.js` + extract.spec.ts recorded the spec (the
// goldens); `next-parity.spec.ts` (this config's test file) checks the port
// against it, with THE SAME route-interception corpus fixtures and frozen
// clock, so both sides of the comparison see identical inputs.
const REPO_ROOT = path.join(__dirname, '..', '..');
const NEXT_HTML = path.join(REPO_ROOT, 'crates', 'darkmux-serve', 'assets', 'next.html');
const SERVED = path.join(__dirname, '.served-next');
const PORT = 47921;

// (#1737) Staging goes through the shared helper, which REFUSES a stale
// bundle instead of silently serving one. See lib/stage-next-bundle.js.
stageNextBundle(SERVED, 'next-parity.playwright.config');

module.exports = defineConfig({
  testDir: __dirname,
  testMatch: ['next-parity.spec.ts'],
  forbidOnly: !!process.env.CI,
  retries: 0,
  fullyParallel: false,
  reporter: process.env.CI ? 'github' : 'list',
  use: {
    baseURL: `http://127.0.0.1:${PORT}`,
    trace: 'retain-on-failure',
    // Same determinism pin as playwright.config.js — see that file's
    // comment for why (relative ages / locale-formatted numbers read the
    // ambient TZ/locale unless pinned at the browser-context level).
    timezoneId: 'UTC',
    locale: 'en-US',
  },
  webServer: {
    command: `python3 -m http.server ${PORT} --directory ${SERVED}`,
    url: `http://127.0.0.1:${PORT}/index.html`,
    reuseExistingServer: !process.env.CI,
    timeout: 30_000,
  },
  projects: [{ name: 'chromium', use: { ...devices['Desktop Chrome'] } }],
});
