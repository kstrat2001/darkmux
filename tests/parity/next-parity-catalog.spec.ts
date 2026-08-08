// @ts-nocheck
// Packet 4 acceptance: `/next`'s catalog panel + replay-by-query surfaces
// vs the legacy goldens recorded this packet
// (`goldens/catalog-open.txt`/`mission-replay.txt`/`playback-date.txt` —
// `session-task-list.txt` already existed, see the session test below for
// why it isn't byte-compared). Run via `bun run next-parity` (this file is
// registered in `next-playwright.config.js`'s `testMatch`, ADDITIVELY
// alongside `next-parity-runs.spec.ts`).
//
// Byte-parity scope, and why it's narrower than a full four-region compare
// for two of the three new goldens (documented here so nobody re-derives
// this from scratch, or worse, "fixes" the spec to force a false match):
//
//   - `catalog-open`: FULL byte parity, `#catpanel`'s region only (via the
//     new `extractCatalogOnlyText`/`catalogSectionOf` pair in
//     `lib/extract-next-lens.js`) — `CatalogPanel.tsx` is a genuine,
//     faithful port of `toggleCatalog()`'s row-building logic, so this is
//     the real spec-grade comparison this packet's brief asks for.
//   - `mission-replay` / `playback-date`: these goldens capture LEGACY's
//     `#stage` (the fleet-hero render), which `/next` doesn't own yet —
//     `FleetStrip` only covers `/fleet/machines/live` presence, not the
//     dispatch-lifecycle-derived hero rendering `fleet.txt`-family goldens
//     actually show (that's Packet 5's territory, tied to the live-tail/SSE
//     pipeline). Forcing byte parity here would mean building a SECOND
//     fleet-rendering pipeline inside this packet before the first one
//     exists — scope creep wearing this packet's clothes. So these two
//     tests assert `/next`'s own HONEST, narrower behavior instead
//     (`MissionReplay`/`PlaybackLens`'s own doc comments name the same
//     reasoning) — the goldens still earn their keep as the "collector"
//     record of what legacy does, for whichever future packet ports the
//     fleet-hero pipeline and can then extend this file to real parity.

import { test, expect } from "@playwright/test";
import { readFileSync, mkdirSync } from "node:fs";
import path from "node:path";
import { GOLDENS_DIR } from "./lib/paths.js";
import {
  extractCatalogOnlyText,
  catalogSectionOf,
  installFrozenClock,
  installCorpusRoutes,
  installBlankRoutes,
  loadMeta,
} from "./lib/extract-next-lens.js";

// Gitignored, repo-relative by default (`tests/parity/.gallery/4-catalog/`)
// — NOT an operator machine path (the corrected convention from the
// overnight runbook's own FOLLOW-UPS: packets 2 and 3 both originally
// hardcoded the operator's absolute scratchpad path here and had to be
// fixed post-QA). Override with `DARKMUX_GALLERY_DIR` for a real run; the
// `mkdirSync` lives in `beforeAll` (fires only when the suite actually
// RUNS), never at module scope.
const GALLERY_DIR = process.env.DARKMUX_GALLERY_DIR || path.join(__dirname, ".gallery", "4-catalog");
function shot(name: string) {
  return path.join(GALLERY_DIR, name);
}

function goldenText(label: string): string {
  return readFileSync(`${GOLDENS_DIR}/${label}.txt`, "utf8");
}

const CATALOG_SETTLED = "#catpanel .catrow";

test.describe("next-parity: catalog panel + replay-by-query (Packet 4)", () => {
  test.beforeAll(() => {
    mkdirSync(GALLERY_DIR, { recursive: true });
  });

  test("catalog panel matches catalog-open.txt's catalog region byte-for-byte", async ({ page }) => {
    const meta = loadMeta();
    await installFrozenClock(page, meta.frozen_clock_ms);
    installCorpusRoutes(page, meta);
    const pageErrors: string[] = [];
    page.on("pageerror", (e) => pageErrors.push(String(e)));

    await page.goto("/index.html");
    await page.click(".catalog-toggle");
    // `.catrow` never appears in the synchronous "loading…" placeholder
    // (only `.cathdr` does) — same reasoning as extract.spec.ts's own
    // comment for the legacy harness.
    await expect(page.locator(CATALOG_SETTLED).first()).toBeAttached({ timeout: 15000 });

    const got = await extractCatalogOnlyText(page);
    expect(got, "catalog panel must match legacy's #catpanel byte-for-byte").toBe(catalogSectionOf(goldenText("catalog-open")));
    expect(pageErrors, `pageerror events: ${pageErrors.join("; ")}`).toHaveLength(0);

    await page.screenshot({ path: shot("catalog-open.png"), fullPage: true });
  });

  // QA must-fix (post-first-review): legacy has THREE ways to close
  // `#catpanel`, this component originally had zero — dropped both
  // dismissal handlers AND had a geometry bug (`.catpanel` at a page-level
  // `top:48px` overlapped the toggle button itself once `#meta` landed
  // above it) that made even the toggle structurally unclickable while
  // open. `CatalogPanel.test.tsx` (vitest/jsdom) covers the handler LOGIC;
  // these three tests are the real-browser regression proof — in
  // particular the geometry fix, which jsdom can't catch at all (jsdom has
  // no layout engine, so `page.click()`'s real actionability check —
  // "is this element visible AND not covered by another element at its
  // center point" — is exactly the mechanism QA used to catch the original
  // bug, and exactly what a mocked/jsdom click can't reproduce).
  test.describe("dismissal (real browser)", () => {
    test("Escape closes the panel", async ({ page }) => {
      const meta = loadMeta();
      await installFrozenClock(page, meta.frozen_clock_ms);
      installCorpusRoutes(page, meta);

      await page.goto("/index.html");
      await page.click(".catalog-toggle");
      await expect(page.locator(CATALOG_SETTLED).first()).toBeAttached({ timeout: 15000 });

      await page.keyboard.press("Escape");
      await expect(page.locator("#catpanel")).toHaveCount(0);
    });

    test("a click outside the panel closes it", async ({ page }) => {
      const meta = loadMeta();
      await installFrozenClock(page, meta.frozen_clock_ms);
      installCorpusRoutes(page, meta);

      await page.goto("/index.html");
      await page.click(".catalog-toggle");
      await expect(page.locator(CATALOG_SETTLED).first()).toBeAttached({ timeout: 15000 });

      // #stage is real page content outside both the toggle and the panel.
      await page.click("#stage", { position: { x: 5, y: 5 } });
      await expect(page.locator("#catpanel")).toHaveCount(0);
    });

    test("the toggle stays clickable while the panel is open — the geometry regression test", async ({ page }) => {
      const meta = loadMeta();
      await installFrozenClock(page, meta.frozen_clock_ms);
      installCorpusRoutes(page, meta);

      await page.goto("/index.html");
      await page.click(".catalog-toggle");
      await expect(page.locator(CATALOG_SETTLED).first()).toBeAttached({ timeout: 15000 });

      // This is the exact reproduction of QA's original finding: Playwright's
      // `click()` performs a real actionability check (element is visible,
      // stable, and NOT obscured by another element at its center point)
      // before clicking — it would TIME OUT here if the panel still
      // overlapped the toggle, the same way QA's `elementFromPoint` probe
      // caught it. A short timeout is deliberate: this must succeed FAST if
      // the geometry is right, not eventually.
      await page.click(".catalog-toggle", { timeout: 3000 });
      await expect(page.locator("#catpanel")).toHaveCount(0);
    });
  });

  test("#mission=<known-corpus-mission>: real fetch, honest empty render (this corpus has no /flow-mission fixture)", async ({
    page,
  }) => {
    // See this file's own module doc for why this does NOT byte-compare
    // against `mission-replay.txt`'s `#stage` (legacy's fleet-hero render,
    // which `/next` doesn't own yet) — this asserts `/next`'s OWN honest
    // behavior: the fetch really happened, and a zero-record response
    // renders a real, named empty state rather than silently doing nothing.
    const meta = loadMeta();
    await installFrozenClock(page, meta.frozen_clock_ms);
    installCorpusRoutes(page, meta);
    const pageErrors: string[] = [];
    page.on("pageerror", (e) => pageErrors.push(String(e)));

    await page.goto("/index.html#mission=acp-ephemeral-pr-ship-1786152707367180000-5");
    await expect(page.getByText(/no records found for mission/i)).toBeVisible({ timeout: 15000 });
    expect(page.url()).toContain("/index.html");
    expect(pageErrors, `pageerror events: ${pageErrors.join("; ")}`).toHaveLength(0);

    await page.screenshot({ path: shot("mission-replay-empty.png"), fullPage: true });
  });

  test("bare #<date> hash: recognized as a named playback route, not silently rendered as fleet", async ({ page }) => {
    // Same scoping note as the mission test above — no #stage byte compare
    // against `playback-date.txt` (Packet 5's territory); this asserts the
    // route is recognized and named honestly (never blank, never
    // misclassified as `unknown` the way Packet 1–3 left it).
    const meta = loadMeta();
    await installFrozenClock(page, meta.frozen_clock_ms);
    installCorpusRoutes(page, meta);
    const pageErrors: string[] = [];
    page.on("pageerror", (e) => pageErrors.push(String(e)));

    await page.goto(`/index.html#${meta.captured_prev_date}`);
    await expect(page.getByText(new RegExp(`playback for ${meta.captured_prev_date}`, "i"))).toBeVisible({ timeout: 15000 });
    expect(pageErrors, `pageerror events: ${pageErrors.join("; ")}`).toHaveLength(0);

    await page.screenshot({ path: shot("playback-date-notice.png"), fullPage: true });
  });

  test("#session=task-list: real fetch, populated not-ported notice naming the real record count", async ({ page }) => {
    // The one replay-by-query golden that already existed before this
    // packet (`goldens/session-task-list.txt`, Packet 0a) — not byte-
    // compared here either, same reasoning: it's legacy's `renderSubsystem()`
    // full drill-in, a genuinely separate future lens (see
    // `SessionReplay.tsx`'s own doc). This proves the FETCH wiring is real
    // (the corpus's `flow-session-task-list.json` has 48 records) and the
    // notice names that real count, not a placeholder guess.
    const meta = loadMeta();
    await installFrozenClock(page, meta.frozen_clock_ms);
    installCorpusRoutes(page, meta);
    const pageErrors: string[] = [];
    page.on("pageerror", (e) => pageErrors.push(String(e)));

    await page.goto("/index.html#session=task-list");
    await expect(page.getByText(/48 records loaded/i)).toBeVisible({ timeout: 15000 });
    expect(pageErrors, `pageerror events: ${pageErrors.join("; ")}`).toHaveLength(0);

    await page.screenshot({ path: shot("session-replay-notice.png"), fullPage: true });
  });

  test("render-sanity: zero pageerror, real region heights, no 390px overflow", async ({ page }) => {
    const meta = loadMeta();
    await installFrozenClock(page, meta.frozen_clock_ms);
    installCorpusRoutes(page, meta);
    const pageErrors: string[] = [];
    const consoleErrors: string[] = [];
    page.on("pageerror", (e) => pageErrors.push(String(e)));
    page.on("console", (msg) => {
      if (msg.type() === "error") consoleErrors.push(msg.text());
    });

    // Set AFTER navigation, not relying on the device preset — same gotcha
    // note as next-parity-runs.spec.ts's own render-sanity test.
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto("/index.html");
    await page.click(".catalog-toggle");
    await expect(page.locator(CATALOG_SETTLED).first()).toBeAttached({ timeout: 15000 });

    const panelBox = await page.locator("#catpanel").boundingBox();
    expect(panelBox, "#catpanel must have a real bounding box").not.toBeNull();
    expect(panelBox!.height).toBeGreaterThan(20);

    const stageBox = await page.locator("#stage").boundingBox();
    expect(stageBox, "#stage must have a real bounding box").not.toBeNull();
    expect(stageBox!.height).toBeGreaterThan(20);

    // `document.body.scrollWidth`, NOT `document.documentElement.scrollWidth`
    // — see next-parity-runs.spec.ts's identical comment for the
    // `overflow-x:hidden`-clamps-documentElement gotcha this avoids. The
    // catalog panel's own `max-width: min(420px, 92vw)` is the property
    // under test here — it's what keeps a 300px-min-width absolute overlay
    // from overflowing a 390px viewport.
    const overflow = await page.evaluate(() => document.body.scrollWidth > document.documentElement.clientWidth);
    expect(overflow, "no horizontal document scroll at 390px, even with the catalog panel open").toBe(false);

    expect(pageErrors, `pageerror events: ${pageErrors.join("; ")}`).toHaveLength(0);
    expect(consoleErrors, `console.error events: ${consoleErrors.join("; ")}`).toHaveLength(0);

    const width = page.viewportSize()?.width;
    expect(width, "viewport must actually be 390px wide, not overridden by a device preset").toBe(390);

    await page.screenshot({ path: shot("catalog-390px.png"), fullPage: true });
  });
});

test.describe("next-parity: catalog panel red-prove (harness self-test, Packet 4)", () => {
  test("blank routes fail the catalog-open golden comparison", async ({ page }) => {
    await installFrozenClock(page, Date.UTC(2026, 0, 1));
    installBlankRoutes(page);

    await page.goto("/index.html");
    await page.click(".catalog-toggle");
    await expect(page.locator(CATALOG_SETTLED).first()).toBeAttached({ timeout: 15000 });
    const got = await extractCatalogOnlyText(page);
    expect(got, "blank-route extraction must NOT match the real catalog-open golden").not.toBe(
      catalogSectionOf(goldenText("catalog-open")),
    );
  });

  test("blank routes: mission replay renders a visible unreachable-daemon error, never a blank page", async ({ page }) => {
    // installBlankRoutes 404s EVERY api path uniformly (simulating total
    // daemon unreachability) — unlike installCorpusRoutes' /flow-mission/
    // fixture (Packet 4's own fix, see mock-routes.js's comment), which
    // deliberately answers 200+empty because that's what the REAL endpoint
    // does for an unmatched id. A blank/unreachable daemon is a genuinely
    // different case (an actual non-2xx), so MissionReplay's error branch —
    // not its empty branch — is the correct, honest render here.
    await installFrozenClock(page, Date.UTC(2026, 0, 1));
    installBlankRoutes(page);

    await page.goto("/index.html#mission=acp-ephemeral-pr-ship-1786152707367180000-5");
    await expect(page.getByText(/couldn't reach \/flow-mission\//i)).toBeVisible({ timeout: 15000 });
  });
});
