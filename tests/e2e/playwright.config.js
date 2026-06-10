const { defineConfig, devices } = require('@playwright/test');
const fs = require('fs');
const path = require('path');

const SERVED = path.join(__dirname, '.served');
const PORT = 47823;

// Build the served harness at config-load time — BEFORE the webServer starts —
// so its readiness check (GET /index.html) finds the file. (Playwright starts
// the webServer before globalSetup, so the build can't live there.)
//
// The harness is the canonical viewer with the two static-playback metas
// injected after <head>, the same way scripts/build-demo.sh builds the public
// demo — pointed at the XSS regression fixture instead of the demo data. Same
// render path as the demo and a local `/play/<date>`; no viewer fork.
(function buildHarness() {
  const repo = path.join(__dirname, '..', '..');
  const viewer = fs.readFileSync(path.join(repo, 'crates', 'darkmux-serve', 'assets', 'viewer.html'), 'utf8');
  const fixture = fs.readFileSync(path.join(repo, 'tests', 'fixtures', 'xss-flow.jsonl'), 'utf8');
  const injected = viewer.replace(
    '<head>',
    '<head>\n<meta name="darkmux-mode" content="play">\n<meta name="darkmux-flow-src" content="./xss-flow.jsonl">'
  );
  if (injected === viewer) throw new Error('playwright.config: <head> anchor not found in viewer.html');
  fs.mkdirSync(SERVED, { recursive: true });
  fs.writeFileSync(path.join(SERVED, 'index.html'), injected);
  fs.writeFileSync(path.join(SERVED, 'xss-flow.jsonl'), fixture);
})();

// Serve over HTTP (not file://) so the viewer's boot() fetch('./xss-flow.jsonl')
// resolves — the same reason the public demo is served over HTTPS, not from disk.
module.exports = defineConfig({
  testDir: '.',
  forbidOnly: !!process.env.CI,
  retries: 0,
  reporter: process.env.CI ? 'github' : 'list',
  use: {
    baseURL: `http://127.0.0.1:${PORT}`,
    trace: 'retain-on-failure',
  },
  webServer: {
    command: `python3 -m http.server ${PORT} --directory ${SERVED}`,
    url: `http://127.0.0.1:${PORT}/index.html`,
    reuseExistingServer: !process.env.CI,
    timeout: 30_000,
  },
  projects: [{ name: 'chromium', use: { ...devices['Desktop Chrome'] } }],
});
