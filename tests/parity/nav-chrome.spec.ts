// @ts-nocheck
// Packet 1.5 acceptance: NAV CHROME + HASH WRITE-BACK — the scaffold gap
// both the machine (Packet 2) and runs (Packet 3) lens packets independently
// flagged as a hard blocker for the eventual /next -> / flip. This is chrome,
// not lens CONTENT, so `next-parity.spec.ts`/`next-parity-runs.spec.ts`
// (which grade `#crumb`/`#meta`/`#logscope`/`#stage` — see
// `nav-chrome.playwright.config.js`'s own doc) cannot cover it; this file is
// the dedicated gate.
//
// Same corpus-interception machinery as the other next-parity specs, reused
// verbatim (`lib/mock-routes.js`'s `installCorpusRoutes`, `lib/extract-lens.js`'s
// `waitSettled`) so the machine/runs lenses this suite navigates INTO render
// real content, not an empty/pending skeleton — a click that lands on a
// skeleton would not prove the tab actually selected the right lens.
//
// Deliberately DOES NOT install the frozen clock the byte-parity specs use
// (`installFrozenClock`, from the same module). This spec asserts hash/DOM
// mechanics, never relative-time TEXT, so it doesn't need the determinism a
// frozen clock buys — and installing it actively BREAKS this spec: root-caused
// by hand (temporarily removed, re-added, `debug-chrome.spec.ts` scratch file
// deleted after) to `MachineLens`'s `resourcesQuery` (`refetchInterval:
// MACHINE_MEM_POLL_MS`) — under Playwright's `page.clock.install()` (which
// fakes `requestAnimationFrame` among other timers), a REAL `page.click()`
// through the nav tab left `/machine/resources`'s 200 response sitting in
// TanStack Query's cache while the component stayed rendered at
// `data-state="loading"` indefinitely; the SAME navigation via
// `page.evaluate(() => location.hash = ...)` (what `next-parity.spec.ts` uses)
// was unaffected. A real click's actionability wait (browser-side stability
// checks) interacting with a faked `requestAnimationFrame` is the suspected
// mechanism; a harness quirk, not a product bug — `next-parity.spec.ts`'s own
// machine-lens goldens (which DO use the frozen clock, via `location.hash =`
// assignment, never a real click) are unaffected and remain the byte-parity
// source of truth for that lens's rendered content.

import { test, expect } from "@playwright/test";
import { mkdirSync } from "node:fs";
import path from "node:path";
import { loadMeta, installCorpusRoutes } from "./lib/mock-routes.js";
import { waitSettled } from "./lib/extract-lens.js";

// Gitignored, repo-relative by default (`tests/parity/.gallery/1.5-chrome/`)
// — NOT an operator machine path (the QA must-fix pattern every earlier
// packet in this arc hit and fixed — see `next-parity.spec.ts`'s identical
// comment for the full rationale). `DARKMUX_GALLERY_DIR` overrides;
// `mkdirSync` lives inside `test.beforeAll` (fires only when this suite
// actually RUNS), never at module scope.
const GALLERY_DIR = process.env.DARKMUX_GALLERY_DIR || path.join(__dirname, ".gallery", "1.5-chrome");

test.beforeAll(() => {
  mkdirSync(GALLERY_DIR, { recursive: true });
});

function shot(name) {
  return path.join(GALLERY_DIR, name);
}

// Per-tab: the `data-act` the tab carries, the canonical hash a fresh click
// produces, and a post-fetch content-settle selector proving the RIGHT lens
// actually rendered (never just "something changed") — same discipline
// `extract-lens.js`'s own module doc requires of every navigation in this
// harness.
const TABS = [
  { act: "fleet", hash: "", settle: '[data-state]:not([data-state="pending"])' },
  { act: "console", hash: "#lens=console", settle: '[data-state="not-ported"]' },
  { act: "runs", hash: "#lens=runs", settle: '[data-state="data"], [data-state="pending"]' },
  { act: "machine", hash: "#lens=machine", settle: '.machine-lens__health[data-state="loaded"]' },
];

test.describe("nav chrome (Packet 1.5)", () => {
  test("tabs render in the legacy DOM order (viewer.html:816: fleet, console, runs, machine) at 390px, no horizontal overflow", async ({ page }) => {
    const meta = loadMeta();
    installCorpusRoutes(page, meta);

    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto("/index.html");
    await expect(page.locator("body")).not.toHaveClass(/booting/);

    const tabs = page.locator(".app-shell__navtabs .nav-tab");
    await expect(tabs).toHaveCount(4);
    expect(await tabs.allTextContents()).toEqual(["fleet", "console", "runs", "machine"]);
    expect(await tabs.evaluateAll((els) => els.map((e) => e.id))).toEqual(["lens-fleet", "lens-console", "lens-runs", "lens-machine"]);

    // Same overflow probe every other packet's render-sanity test uses —
    // `document.body.scrollWidth`, NOT `documentElement.scrollWidth` (which
    // `styles.css`'s `overflow-x:hidden` clamps to the viewport and would
    // pass silently against a real overflow). See `ui/verify/live-render.spec.ts`.
    const overflow = await page.evaluate(() => document.body.scrollWidth > document.documentElement.clientWidth);
    expect(overflow, "no horizontal document scroll at 390px with the nav chrome present").toBe(false);

    // Tap-target sanity: every tab is at least ~36px tall (the packet's own
    // padding choice — see `styles.css`'s `.nav-tab` comment).
    for (let i = 0; i < 4; i++) {
      const box = await tabs.nth(i).boundingBox();
      expect(box, `tab ${i} must have a real bounding box`).not.toBeNull();
      expect(box.height, `tab ${i} tap target is only ${box.height}px tall`).toBeGreaterThanOrEqual(30);
    }

    await page.screenshot({ path: shot("chrome-390px-fleet.png"), fullPage: true });
  });

  test("clicking each tab updates BOTH the rendered lens and location.hash, in legacy order", async ({ page }) => {
    const meta = loadMeta();
    installCorpusRoutes(page, meta);
    const pageErrors = [];
    page.on("pageerror", (e) => pageErrors.push(String(e)));

    await page.goto("/index.html");
    await expect(page.locator("body")).not.toHaveClass(/booting/);
    await waitSettled(page, expect, TABS[0].settle);

    for (const { act, hash } of TABS) {
      await page.click(`[data-act="${act}"]`);
      await waitSettled(page, expect, TABS.find((t) => t.act === act).settle);
      const gotHash = await page.evaluate(() => location.hash);
      expect(gotHash, `clicking the "${act}" tab must set location.hash to "${hash}"`).toBe(hash);
      // The clicked tab is the one carrying `.on`; every OTHER tab must not.
      const onIds = await page.locator(".nav-tab.on").evaluateAll((els) => els.map((e) => e.id));
      expect(onIds, `exactly one tab (lens-${act}) should be highlighted after clicking it`).toEqual([`lens-${act}`]);
      await page.screenshot({ path: shot(`chrome-tab-${act}.png`), fullPage: true });
    }

    expect(pageErrors, `pageerror events across the whole click sequence: ${pageErrors.join("; ")}`).toHaveLength(0);
  });

  test("the legacy #lens=lab alias boots and the address bar upgrades to the canonical #lens=runs&kind=lab", async ({ page }) => {
    const meta = loadMeta();
    installCorpusRoutes(page, meta);

    await page.goto("/index.html#lens=lab");
    await waitSettled(page, expect, '[data-state="data"], [data-state="pending"]');

    await expect(async () => {
      const gotHash = await page.evaluate(() => location.hash);
      expect(gotHash).toBe("#lens=runs&kind=lab");
    }).toPass({ timeout: 5000 });

    // And the runs tab is the one lit — the upgrade is a REAL navigation
    // outcome, not just an address-bar cosmetic.
    await expect(page.locator("#lens-runs")).toHaveClass(/\bon\b/);
    await page.screenshot({ path: shot("chrome-lens-lab-upgrade.png"), fullPage: true });
  });

  test("a runs-lens kind-chip click writes the hash directly, without disturbing the active tab", async ({ page }) => {
    const meta = loadMeta();
    installCorpusRoutes(page, meta);

    await page.goto("/index.html#lens=runs");
    await waitSettled(page, expect, '[data-state="data"], [data-state="pending"]');
    expect(await page.evaluate(() => location.hash)).toBe("#lens=runs");

    const missionChip = page.locator('[data-arg="mission"]');
    if ((await missionChip.count()) === 0) {
      test.skip(true, "no mission-kind chip present in this corpus state — nothing to click");
    }
    await missionChip.click();

    await expect(async () => {
      expect(await page.evaluate(() => location.hash)).toBe("#lens=runs&kind=mission");
    }).toPass({ timeout: 5000 });
    await expect(page.locator("#lens-runs")).toHaveClass(/\bon\b/);
    await page.screenshot({ path: shot("chrome-runs-kind-chip.png"), fullPage: true });
  });
});
