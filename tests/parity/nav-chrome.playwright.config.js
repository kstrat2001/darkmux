const { defineConfig, devices } = require("@playwright/test");
const fs = require("fs");
const path = require("path");

// Packet 1.5 acceptance gate: the nav-chrome + hash write-back behavior
// ITSELF, distinct from `next-parity.spec.ts` (byte-parity of lens
// CONTENT — the extractor scopes to `#crumb`/`#meta`/`#logscope`/`#stage`
// and never touches the chrome this packet adds) and
// `next-parity-runs.spec.ts` (`#stage`-only). Neither of those specs can
// grade chrome — a tab bar, its DOM order, and hash mechanics are outside
// every region either extractor reads BY DESIGN (the chrome lives in a
// NEW `.app-shell__navtabs` sibling, deliberately outside all four
// extracted regions — see `App.tsx`'s module doc). So this config exists
// for exactly the same reason `next-playwright.config.js` does for the
// runs board: one packet, one dedicated file, folded into the shared
// `next-parity.spec.ts` mechanism by a later packet if that ever makes
// sense (see that file's own precedent comment).
//
// Same webServer-serves-a-static-copy pattern as its three next-parity
// siblings; own port (47922 — distinct from 47919/47920/47921/47823/
// ui/verify's 8790+) so all of these can run concurrently.
const REPO_ROOT = path.join(__dirname, "..", "..");
const NEXT_HTML = path.join(REPO_ROOT, "crates", "darkmux-serve", "assets", "next.html");
const SERVED = path.join(__dirname, ".served-next-chrome");
const PORT = 47922;

(function stageArtifact() {
  if (!fs.existsSync(NEXT_HTML)) {
    throw new Error(
      `nav-chrome.playwright.config: ${NEXT_HTML} does not exist — run \`cd ui && bun run build\` first (the artifact is committed, not built by this config).`,
    );
  }
  fs.mkdirSync(SERVED, { recursive: true });
  fs.copyFileSync(NEXT_HTML, path.join(SERVED, "index.html"));
})();

module.exports = defineConfig({
  testDir: __dirname,
  testMatch: ["nav-chrome.spec.ts"],
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
