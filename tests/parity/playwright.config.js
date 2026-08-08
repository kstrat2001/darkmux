const { defineConfig, devices } = require('@playwright/test');
const fs = require('fs');
const path = require('path');

// CJS throughout, mirroring tests/e2e/playwright.config.js's working pattern
// exactly — this file is required by Playwright's own loader as CommonJS, so
// it does not import lib/paths.mjs (an ES module; `require()` cannot load
// .mjs). The spec files and the bun-run scripts import that module as ESM
// instead; this config recomputes the same two paths inline.
const REPO_ROOT = path.join(__dirname, '..', '..');
const VIEWER_HTML = path.join(REPO_ROOT, 'crates', 'darkmux-serve', 'assets', 'viewer.html');
const SERVED = path.join(__dirname, '.served');
const PORT = 47919; // distinct from tests/e2e's 47823 so both suites can run concurrently

// Build the served harness at config-load time — same reason tests/e2e's
// config does this before the webServer starts (its readiness GET needs the
// file to already exist). One page:
//
//   index.html — `darkmux-mode=live`, no `darkmux-date`/`darkmux-flow-src`
//                 meta. This is the ONLY way to reach the viewer's real
//                 daemon-fetch code path (boot()'s `wantsPlayback` is false,
//                 `missionGraphReachable()` is true) — see injectedMeta()'s
//                 own doc comment in viewer.html. extract.spec.ts and
//                 redprove.spec.ts both serve THIS page and drive everything
//                 through page.route() interception; no daemon is ever
//                 contacted. Same render path as the real daemon; no viewer
//                 fork — mirrors the injection tests/e2e/playwright.config.js
//                 already does for its play-mode harness variants, just with
//                 mode=live instead of mode=play.
(function buildHarness() {
  const viewer = fs.readFileSync(VIEWER_HTML, 'utf8');
  const injected = viewer.replace('<head>', '<head>\n<meta name="darkmux-mode" content="live">');
  if (injected === viewer) throw new Error('playwright.config: <head> anchor not found in viewer.html');
  fs.mkdirSync(SERVED, { recursive: true });
  fs.writeFileSync(path.join(SERVED, 'index.html'), injected);
})();

module.exports = defineConfig({
  testDir: __dirname,
  testMatch: ['extract.spec.ts', 'redprove.spec.ts'],
  forbidOnly: !!process.env.CI,
  retries: 0,
  fullyParallel: false, // extract.spec.ts writes goldens/ to disk — keep it single-threaded and deterministic
  reporter: process.env.CI ? 'github' : 'list',
  use: {
    baseURL: `http://127.0.0.1:${PORT}`,
    trace: 'retain-on-failure',
    // Determinism (QA finding, post-0a review): every golden renders
    // relative ages, "today"/date math, and possibly locale-formatted
    // numbers — all of which read the AMBIENT timezone/locale unless pinned
    // here. Without this, 4 of 6 goldens (anything touching a timestamp)
    // differ between a run on the operator's machine (Asia/Kuala_Lumpur)
    // and a run on a CI runner (UTC). Pinned to UTC/en-US so the corpus's
    // own recorded dates (captured in UTC — see record.mjs's todayUTC()) and
    // the extraction's rendering agree regardless of the ambient TZ the
    // test process happens to run under.
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
