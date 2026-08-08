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
  // Dedicated to the runs lens (Packet 3) — `next-parity-runs` in
  // package.json. Packet 4's catalog suite got its OWN config +
  // script (`next-parity-catalog.playwright.config.js` /
  // `next-parity-catalog`, port 47922) after the back-merge with Packet 2's
  // machine lens, which introduced its own dedicated
  // `next-parity.playwright.config.js` / `next-parity` (port 47921) —
  // riding this file's testMatch post-merge would have made the
  // `next-parity-runs` script name lie about what it runs. Each lens suite
  // is individually invokable now; a future packet may fold all three into
  // one shared harness (a rename, not a rewrite) if that turns out cleaner.
  testMatch: ["next-parity-runs.spec.ts"],
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
