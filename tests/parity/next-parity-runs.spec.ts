// @ts-nocheck
// Packet 3 acceptance: `/next`'s ported runs board vs the legacy goldens
// recorded in Packet 0a (plus the four this packet grew — see
// `goldens/runs-kind-mission.txt`, `runs-kind-dispatch.txt`, `runs-series.txt`,
// `runs-lens-boot.txt`). Run with `bunx playwright test --config
// next-playwright.config.js` (see `package.json`'s `next-parity` script) —
// NOT picked up by `playwright.config.js`'s `testMatch`, which is scoped to
// the legacy extractor/red-prove pair on purpose (see that file's own doc).
//
// Same interception + frozen-clock pattern as `extract.spec.ts`, reused
// verbatim via `lib/extract-next-lens.js` (see that module's doc for exactly
// which pieces are shared vs new, and why `#stage`-only is the scoped
// comparison this spec makes). This file is deliberately the ONLY thing
// packet-specific here — if `tests/parity/next-parity.spec.ts` (the shared
// multi-lens mechanism Packet 2 is building in parallel) exists by the time
// this merges, folding this file's `describe` block into it is a rename, not
// a rewrite: the fixture/lib imports below are already the shared surface.

import { test, expect } from "@playwright/test";
import { readFileSync, mkdirSync } from "node:fs";
import path from "node:path";
import { GOLDENS_DIR } from "./lib/paths.js";
import {
  extractStageOnlyText,
  stageSectionOf,
  waitSettled,
  installFrozenClock,
  installCorpusRoutes,
  installBlankRoutes,
  loadMeta,
} from "./lib/extract-next-lens.js";

const GALLERY_DIR =
  process.env.DARKMUX_GALLERY_DIR ||
  "/private/tmp/claude-501/-Users-kain-de-projects-darkmux-public/652b2a6d-51b7-4543-9ddf-8ef250dd2a4d/scratchpad/ui-port-gallery/3-runs";
mkdirSync(GALLERY_DIR, { recursive: true });
function shot(name: string) {
  return path.join(GALLERY_DIR, name);
}

function goldenStageText(label: string): string {
  const full = readFileSync(`${GOLDENS_DIR}/${label}.txt`, "utf8");
  return stageSectionOf(full);
}

// `[data-state="data"]` is `RunsBoard`'s own settle marker (present in BOTH
// the flat-list and the series-view success branches — see `RunsBoard.tsx`),
// the `/next` analog of legacy's `.lablist` — real content is in the DOM,
// not the `data-state="pending"` skeleton.
const SETTLED = '[data-state="data"]';

test.describe("next-parity: runs board (Packet 3)", () => {
  test("kind=all deep-link boot matches runs.txt's #stage", async ({ page }) => {
    const meta = loadMeta();
    await installFrozenClock(page, meta.frozen_clock_ms);
    installCorpusRoutes(page, meta);
    const pageErrors: string[] = [];
    page.on("pageerror", (e) => pageErrors.push(String(e)));

    await page.goto("/index.html#lens=runs");
    await expect(page.locator(SETTLED)).toBeAttached({ timeout: 15000 });

    const got = await extractStageOnlyText(page);
    expect(got, "runs board (kind=all) must match the legacy #stage byte-for-byte").toBe(goldenStageText("runs"));
    // Also matches the DEDICATED boot golden — same content, proving the
    // deep-link boot path itself (not just a click sequence) renders right.
    expect(got).toBe(goldenStageText("runs-lens-boot"));
    expect(pageErrors, `pageerror events: ${pageErrors.join("; ")}`).toHaveLength(0);

    await page.screenshot({ path: shot("runs-kind-all.png"), fullPage: true });
  });

  test("kind=mission deep-link boot matches runs-kind-mission.txt's #stage", async ({ page }) => {
    const meta = loadMeta();
    await installFrozenClock(page, meta.frozen_clock_ms);
    installCorpusRoutes(page, meta);
    await page.goto("/index.html#lens=runs&kind=mission");
    await expect(page.locator(SETTLED)).toBeAttached({ timeout: 15000 });
    expect(await extractStageOnlyText(page)).toBe(goldenStageText("runs-kind-mission"));
    await page.screenshot({ path: shot("runs-kind-mission.png"), fullPage: true });
  });

  test("kind=dispatch deep-link boot matches runs-kind-dispatch.txt's #stage", async ({ page }) => {
    const meta = loadMeta();
    await installFrozenClock(page, meta.frozen_clock_ms);
    installCorpusRoutes(page, meta);
    await page.goto("/index.html#lens=runs&kind=dispatch");
    await expect(page.locator(SETTLED)).toBeAttached({ timeout: 15000 });
    expect(await extractStageOnlyText(page)).toBe(goldenStageText("runs-kind-dispatch"));
    await page.screenshot({ path: shot("runs-kind-dispatch.png"), fullPage: true });
  });

  test("kind=lab deep-link boot matches runs-kind-lab.txt's #stage", async ({ page }) => {
    const meta = loadMeta();
    await installFrozenClock(page, meta.frozen_clock_ms);
    installCorpusRoutes(page, meta);
    await page.goto("/index.html#lens=runs&kind=lab");
    await expect(page.locator(SETTLED)).toBeAttached({ timeout: 15000 });
    expect(await extractStageOnlyText(page)).toBe(goldenStageText("runs-kind-lab"));
    await page.screenshot({ path: shot("runs-kind-lab.png"), fullPage: true });
  });

  test("kind=lab + ◧ series toggle matches runs-series.txt's #stage", async ({ page }) => {
    const meta = loadMeta();
    await installFrozenClock(page, meta.frozen_clock_ms);
    installCorpusRoutes(page, meta);
    await page.goto("/index.html#lens=runs&kind=lab");
    await expect(page.locator(SETTLED)).toBeAttached({ timeout: 15000 });
    const beforeToggle = await extractStageOnlyText(page);

    await page.click('[data-arg="series"]');
    // Belt-and-suspenders (same pattern as `waitSettled`'s `previousText`
    // check in the legacy harness): confirm the toggle's `.on` class landed
    // AND the stage text actually changed, so a click that silently no-opped
    // can't pass this test by accident.
    await expect(page.locator('[data-arg="series"].on')).toBeAttached({ timeout: 15000 });
    await expect(async () => {
      const now = await extractStageOnlyText(page);
      if (now === beforeToggle) throw new Error("stage text unchanged after clicking the series toggle");
    }).toPass({ timeout: 15000 });

    expect(await extractStageOnlyText(page)).toBe(goldenStageText("runs-series"));
    await page.screenshot({ path: shot("runs-series.png"), fullPage: true });
  });

  test("render-sanity: zero pageerror, real #stage height, no 390px overflow", async ({ page }) => {
    const meta = loadMeta();
    await installFrozenClock(page, meta.frozen_clock_ms);
    installCorpusRoutes(page, meta);
    const pageErrors: string[] = [];
    const consoleErrors: string[] = [];
    page.on("pageerror", (e) => pageErrors.push(String(e)));
    page.on("console", (msg) => {
      if (msg.type() === "error") consoleErrors.push(msg.text());
    });

    // `devices['Desktop Chrome']` (this config's own project) sets its own
    // viewport — setting it explicitly here AFTER navigation, not relying on
    // the device preset, is what makes a 390px screenshot actually 390px
    // wide (see the packet brief's own warning about this exact gotcha).
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto("/index.html#lens=runs");
    await expect(page.locator(SETTLED)).toBeAttached({ timeout: 15000 });

    const stageBox = await page.locator("#stage").boundingBox();
    expect(stageBox, "#stage must have a real bounding box").not.toBeNull();
    expect(stageBox!.height).toBeGreaterThan(20);

    // `document.body.scrollWidth`, NOT `document.documentElement.scrollWidth`
    // — `styles.css` sets `overflow-x:hidden` on both `html` and `body`,
    // which clamps `documentElement.scrollWidth` to the viewport and would
    // let a genuinely overflowing child pass silently. See
    // `ui/verify/live-render.spec.ts`'s identical comment (Packet 1 QA
    // finding) — this is the same gotcha, re-asserted for this lens.
    const overflow = await page.evaluate(() => document.body.scrollWidth > document.documentElement.clientWidth);
    expect(overflow, "no horizontal document scroll at 390px").toBe(false);

    expect(pageErrors, `pageerror events: ${pageErrors.join("; ")}`).toHaveLength(0);
    expect(consoleErrors, `console.error events: ${consoleErrors.join("; ")}`).toHaveLength(0);

    const width = page.viewportSize()?.width;
    expect(width, "viewport must actually be 390px wide, not overridden by a device preset").toBe(390);

    await page.screenshot({ path: shot("runs-board-390px.png"), fullPage: true });
  });
});

test.describe("next-parity: runs board red-prove (harness self-test)", () => {
  test("blank routes fail every runs golden comparison", async ({ page }) => {
    await installFrozenClock(page, Date.UTC(2026, 0, 1));
    installBlankRoutes(page);

    await page.goto("/index.html#lens=runs");
    await expect(page.locator(SETTLED)).toBeAttached({ timeout: 15000 });
    const blankAll = await extractStageOnlyText(page);
    expect(blankAll, "blank-route extraction must NOT match the real runs.txt golden").not.toBe(goldenStageText("runs"));

    await page.goto("/index.html#lens=runs&kind=mission");
    await expect(page.locator(SETTLED)).toBeAttached({ timeout: 15000 });
    expect(await extractStageOnlyText(page)).not.toBe(goldenStageText("runs-kind-mission"));

    await page.goto("/index.html#lens=runs&kind=dispatch");
    await expect(page.locator(SETTLED)).toBeAttached({ timeout: 15000 });
    expect(await extractStageOnlyText(page)).not.toBe(goldenStageText("runs-kind-dispatch"));

    await page.goto("/index.html#lens=runs&kind=lab");
    await expect(page.locator(SETTLED)).toBeAttached({ timeout: 15000 });
    expect(await extractStageOnlyText(page)).not.toBe(goldenStageText("runs-kind-lab"));

    const beforeToggle = await extractStageOnlyText(page);
    await page.click('[data-arg="series"]');
    await expect(page.locator('[data-arg="series"].on')).toBeAttached({ timeout: 15000 });
    await expect(async () => {
      const now = await extractStageOnlyText(page);
      if (now === beforeToggle) throw new Error("stage text unchanged after clicking the series toggle");
    }).toPass({ timeout: 15000 });
    expect(await extractStageOnlyText(page)).not.toBe(goldenStageText("runs-series"));
  });
});
