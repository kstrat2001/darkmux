const { defineConfig, devices } = require("@playwright/test");
const fs = require("fs");
const path = require("path");

// The `/next` (React port) half of the parity harness. Mirrors
// `playwright.config.js` exactly (CJS, same webServer-serves-a-static-copy
// pattern) but points at the BUILT `next.html` artifact instead of
// `viewer.html`, and runs on its OWN port (47920 — distinct from this
// directory's own 47919, `tests/e2e`'s 47823, and `ui/verify`'s live
// throwaway-daemon port 8790+) so all of these can run concurrently without
// colliding.
//
// No meta-injection here (see `lib/extract-next-lens.js`'s module doc for
// why `viewer.html`'s `darkmux-mode=live` trick has no analog in `/next`) —
// the artifact is served byte-for-byte as committed.
const REPO_ROOT = path.join(__dirname, "..", "..");
const NEXT_HTML = path.join(REPO_ROOT, "crates", "darkmux-serve", "assets", "next.html");
const SERVED = path.join(__dirname, ".served-next");
const PORT = 47920;

(function stageArtifact() {
  if (!fs.existsSync(NEXT_HTML)) {
    throw new Error(
      `next-playwright.config: ${NEXT_HTML} does not exist — run \`cd ui && bun run build\` first (the artifact is committed, not built by this config).`,
    );
  }
  fs.mkdirSync(SERVED, { recursive: true });
  fs.copyFileSync(NEXT_HTML, path.join(SERVED, "index.html"));
})();

module.exports = defineConfig({
  testDir: __dirname,
  // ADDITIVE: each lens packet appends its own spec file here rather than
  // fighting over one shared file (a packet-2-era shared `next-parity.spec.ts`
  // may fold these together later — see next-parity-runs.spec.ts's own doc —
  // that's a rename, not a rewrite, when it happens).
  testMatch: ["next-parity-runs.spec.ts", "next-parity-catalog.spec.ts"],
  forbidOnly: !!process.env.CI,
  retries: 0,
  fullyParallel: false,
  reporter: process.env.CI ? "github" : "list",
  use: {
    baseURL: `http://127.0.0.1:${PORT}`,
    trace: "retain-on-failure",
    // Same determinism pin as `playwright.config.js` — see that file's own
    // comment for why (relative-age rendering reads the ambient TZ/locale
    // unless pinned at the browser-context level).
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
