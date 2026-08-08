// @ts-nocheck
// The NEW-UI ACCEPTANCE GATE (Packet 2 — the machine lens, the first real
// lens ported onto the React + TanStack Query scaffold). Loads the
// COMMITTED BUILT `/next` artifact (`crates/darkmux-serve/assets/next.html`,
// served via `next-parity.playwright.config.js`'s dedicated webServer) the
// same way `extract.spec.ts` loads the legacy `viewer.html`: the SAME
// sanitized corpus fixtures via `page.route()` interception
// (`installCorpusRoutes`, `lib/mock-routes.js` — shared verbatim with the
// legacy extraction, not a lookalike reimplementation), the SAME frozen
// clock, the SAME extraction logic (`lib/extract-lens.js`'s
// `extractLensText`, also shared verbatim). Then it asserts the result
// against `goldens/machine.txt` / `goldens/machine-deeplink.txt` — the
// executable spec `extract.spec.ts` recorded from the legacy viewer.
//
// This is deliberately the FIRST file of its kind — later lens packets
// (missions/runs, catalog/replay, console panels, lab) ADD their own
// `test(...)` blocks to this same file rather than inventing a parallel
// harness; the config/route-interception/extraction machinery above is
// the reusable part, not per-lens.
//
// Byte equality is the default (no normalization beyond what
// `extractLensText` already applies for BOTH sides). If a future lens hits
// a fragment that is legitimately unreachable in the new UI (a genuine
// legacy-rendering artifact, not a port bug), the fix is a NAMED
// normalization with a comment justifying it here — never a silent fuzzy
// match.

import { test, expect } from "@playwright/test";
import { readFileSync } from "node:fs";
import path from "node:path";
import { GOLDENS_DIR } from "./lib/paths.js";
import { loadMeta, installCorpusRoutes, installBlankRoutes } from "./lib/mock-routes.js";
import { extractLensText, waitSettled, installFrozenClock, regionText } from "./lib/extract-lens.js";

// Overnight-runbook render-sanity contract (every UI packet's standing
// requirement): zero pageerror, `#stage` visible with real height, no
// horizontal document scroll at phone width. Gallery PNGs land here for the
// operator's morning review (not committed — binary bloat, see the runbook).
const GALLERY_DIR =
  process.env.DARKMUX_GALLERY_DIR ||
  "/private/tmp/claude-501/-Users-kain-de-projects-darkmux-public/652b2a6d-51b7-4543-9ddf-8ef250dd2a4d/scratchpad/ui-port-gallery/2-machine";

function screenshotPath(name) {
  return path.join(GALLERY_DIR, name);
}

function readGolden(label) {
  return readFileSync(`${GOLDENS_DIR}/${label}.txt`, "utf8");
}

test.describe.configure({ mode: "serial" });

test("next: click-navigation into #lens=machine matches goldens/machine.txt", async ({ page }) => {
  const meta = loadMeta();
  await installFrozenClock(page, meta.frozen_clock_ms);
  installCorpusRoutes(page, meta);

  // Boot at the default route first (no hash) — the React-port equivalent
  // of the legacy click-navigation path. This scaffold has no lens-nav-tab
  // UI yet (Packet 1 built only the FleetStrip proof region; a clickable
  // nav is a scaffold gap ledgered in the runbook, not this packet's job to
  // build) — setting `location.hash` is the operator-navigation ACTION the
  // click would otherwise trigger, exercised through `useHashRoute`'s
  // `hashchange` listener rather than a `.click()` on a tab that doesn't
  // exist yet. The distinction that actually matters for this golden pair
  // (click-transition vs fresh-boot — see the deep-link test below) is
  // preserved: this page has ALREADY booted once before the hash changes.
  await page.goto("/index.html");
  await page.evaluate(() => {
    location.hash = "#lens=machine";
  });
  await waitSettled(page, expect, '.machine-lens__health[data-state="loaded"]');
  await expect(page.locator("body")).not.toHaveClass(/booting/);

  const got = await extractLensText(page);
  expect(got).toBe(readGolden("machine"));
});

test("next: #lens=machine deep-link boot matches goldens/machine-deeplink.txt", async ({ page }) => {
  const meta = loadMeta();
  await installFrozenClock(page, meta.frozen_clock_ms);
  installCorpusRoutes(page, meta);

  // A FRESH boot with the hash already set — `useHashRoute`'s initial
  // `getSnapshot()` must resolve `{kind:"machine"}` on the FIRST render,
  // not depend on a `hashchange` event ever firing. Distinct code path from
  // the click-navigation test above (same reasoning as the legacy
  // `#lens=machine` deep-link golden — see `extract.spec.ts`'s comment on
  // that test).
  await page.goto("/index.html#lens=machine");
  await waitSettled(page, expect, '.machine-lens__health[data-state="loaded"]');
  await expect(page.locator("body")).not.toHaveClass(/booting/);

  const got = await extractLensText(page);
  expect(got).toBe(readGolden("machine-deeplink"));
});

// Red-prove — the SAME self-test discipline the legacy harness's
// redprove.spec.ts applies, run against the NEW UI: a blank/unreachable
// daemon must NOT produce text matching either golden. "A probe that passes
// without executing is worse than no probe" (operator doctrine) applies
// here exactly as it does to the legacy side.
test("next: blank daemon fails both machine-lens golden comparisons", async ({ page }) => {
  await installFrozenClock(page, Date.UTC(2026, 0, 1));
  installBlankRoutes(page);

  await page.goto("/index.html#lens=machine");
  // The blank page's `/machine/resources` 404s, so `resourcesQuery.data.ok`
  // is false and the health region never reaches `data-state="loaded"` —
  // wait on the error/loading terminal state instead (mirrors
  // redprove.spec.ts's `.none`-with-text distinction on the legacy side:
  // CSS state alone can't tell "settled-but-unreachable" from "still
  // loading", so this waits on whichever of the two non-loading states
  // actually appears).
  const settled = page.locator(
    '.machine-lens__health[data-state="loaded"], .machine-lens__health[data-state="error"]',
  );
  await waitSettled(page, expect, settled);

  const got = await extractLensText(page);
  expect(got, "redprove FAILED: a blank/unreachable daemon must not match the real machine golden").not.toBe(readGolden("machine"));
  expect(got, "redprove FAILED: a blank/unreachable daemon must not match the real machine-deeplink golden").not.toBe(
    readGolden("machine-deeplink"),
  );
});

test("next: render-sanity — zero pageerror, no horizontal scroll at 390px, real stage height", async ({ page }) => {
  const pageErrors = [];
  const consoleErrors = [];
  page.on("pageerror", (err) => pageErrors.push(String(err)));
  page.on("console", (msg) => {
    if (msg.type() === "error") consoleErrors.push(msg.text());
  });

  const meta = loadMeta();
  await installFrozenClock(page, meta.frozen_clock_ms);
  installCorpusRoutes(page, meta);

  // Playwright viewport trap (overnight runbook): `devices['Desktop Chrome']`
  // spread in the project's `use` (next-parity.playwright.config.js) would
  // override a config-level `viewport` — `page.setViewportSize()` called
  // HERE, at runtime, always wins regardless, which is why this is the safe
  // way to force 390px rather than fighting the config.
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/index.html#lens=machine");
  await waitSettled(page, expect, '.machine-lens__health[data-state="loaded"]');

  const stageBox = await page.locator("#stage").boundingBox();
  expect(stageBox, "the #stage region must have a real bounding box").not.toBeNull();
  expect(stageBox.height).toBeGreaterThan(20);

  // `document.body.scrollWidth`, NOT `document.documentElement.scrollWidth`
  // — `styles.css` sets `overflow-x: hidden` on both html AND body, which
  // CLAMPS `documentElement.scrollWidth` to the viewport width (see
  // `ui/verify/live-render.spec.ts`'s identical comment — this is the same
  // gotcha, ported verbatim into the parity harness's own render-sanity
  // check since the machine lens is dense enough (30 run rows, memcards) to
  // be a real overflow-risk candidate, unlike the fleet strip's few cards).
  const overflow = await page.evaluate(() => document.body.scrollWidth > document.documentElement.clientWidth);
  expect(overflow, "no horizontal document scroll at 390px").toBe(false);

  expect(pageErrors, `pageerror events: ${pageErrors.join("; ")}`).toHaveLength(0);
  expect(consoleErrors, `console.error events: ${consoleErrors.join("; ")}`).toHaveLength(0);

  await page.screenshot({ path: screenshotPath("machine-390px.png"), fullPage: true });
});

test("next: render-sanity screenshot at desktop width (populated, for review)", async ({ page }) => {
  const meta = loadMeta();
  await installFrozenClock(page, meta.frozen_clock_ms);
  installCorpusRoutes(page, meta);

  await page.setViewportSize({ width: 1280, height: 900 });
  await page.goto("/index.html#lens=machine");
  await waitSettled(page, expect, '.machine-lens__health[data-state="loaded"]');
  await page.screenshot({ path: screenshotPath("machine-1280px.png"), fullPage: true });
});

test("next: render-sanity screenshot of the deep-link boot path", async ({ page }) => {
  const meta = loadMeta();
  await installFrozenClock(page, meta.frozen_clock_ms);
  installCorpusRoutes(page, meta);

  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/index.html#lens=machine");
  await waitSettled(page, expect, '.machine-lens__health[data-state="loaded"]');
  await page.screenshot({ path: screenshotPath("machine-deeplink-390px.png"), fullPage: true });
});
