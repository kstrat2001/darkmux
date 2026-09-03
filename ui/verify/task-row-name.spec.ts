import { test, expect } from "@playwright/test";

// #2280's red-first render proof. The other two specs in this directory
// (`live-render.spec.ts`, `machine-render.spec.ts`) talk to a THROWAWAY
// daemon on 8790 and never 8765 (see `playwright.config.ts`'s own doc) —
// this spec needs the opposite: the OPERATOR'S real daemon on 8765, because
// the defect only reproduces against real mission data (a 14-step task
// named "darkmux · unnamed-predicate" long enough to actually get squeezed;
// a synthetic fixture wouldn't exercise the same width pressure). It never
// talks to 8765 directly, though — `baseURL` here is the `bun run dev` vite
// server on 5273, which proxies API calls to 8765 (see `ui/vite.config.ts`).
// Run with: `DARKMUX_VERIFY_PORT=5273 npx playwright test verify/task-row-name.spec.ts --config verify/playwright.config.ts`
// (the dev server must already be running: `bun run dev`).
//
// A collapsed `.tltask .tlt-hd` row is a CSS grid (`auto 1fr auto auto
// auto` = dot, `.tlt-name`, `.tlt-count`, `.tlt-meter-cell`, `.tlt-chev`).
// Grid `auto` tracks size to their item's max-content width regardless of
// any `flex` property on the item itself (flex only governs sizing inside
// a flex container) — so #2281's `.mn-step-meter { flex: 0 1 auto; }` fix
// does nothing here; the meter cell's `auto` column still claims full
// intrinsic width and the `1fr` name column collapses to whatever's left.
const REAL_MISSION_ID = "crawl-1788402801-729335";

async function gotoCollapsedTaskRow(page: import("@playwright/test").Page, width: number, height: number) {
  await page.setViewportSize({ width, height });
  await page.goto(`/#mission=${REAL_MISSION_ID}`);
  // The row starts collapsed (no `.open` class) by default — don't click it.
  const row = page.locator(".missionlens .tltask .tlt-hd").first();
  await expect(row).toBeVisible({ timeout: 15_000 });
  // The mission's aggregate metrics (wall time / tokens / turns) arrive over
  // the live SSE tail AFTER the initial graph paint, not with it — a finished
  // mission's row briefly renders with an EMPTY `.tlt-meter-cell` before its
  // wall-time/token/turn numbers land. The defect (and the fix) is about
  // what happens once the meter has real content to fight the name column
  // for space, so wait for it before measuring anything.
  await expect(row.locator(".tlt-meter-cell")).not.toHaveText("", { timeout: 15_000 });
  return row;
}

test.describe("#2280 — collapsed task row keeps the task name readable", () => {
  test("390x844 (phone): the name keeps a readable minimum width", async ({ page }) => {
    await gotoCollapsedTaskRow(page, 390, 844);

    const nameEl = page.locator(".missionlens .tltask .tlt-hd .tlt-name").first();
    const nameBox = await nameEl.boundingBox();
    const fontSize = await nameEl.evaluate((el) => parseFloat(getComputedStyle(el).fontSize));

    expect(nameBox, "the .tlt-name element must have a real bounding box").not.toBeNull();
    // A monospace char is roughly 0.6em wide — 10 chars is the invariant's
    // stated minimum, so the box must be at least that wide. Loose enough
    // to not fight sub-pixel layout, tight enough that a 1-character-wide
    // name (the reported defect) fails it by a wide margin.
    expect(nameBox!.width, `name box was ${nameBox!.width}px, expected >= ${10 * fontSize * 0.6}px (10 chars)`).toBeGreaterThanOrEqual(10 * fontSize * 0.6);
  });

  test("390x844 (phone): no child of the meter cell paints past its own right edge", async ({ page }) => {
    await gotoCollapsedTaskRow(page, 390, 844);

    const cellBox = await page.locator(".missionlens .tltask .tlt-hd .tlt-meter-cell").first().boundingBox();
    expect(cellBox, "the .tlt-meter-cell element must have a real bounding box").not.toBeNull();

    const childRights = await page
      .locator(".missionlens .tltask .tlt-hd .tlt-meter-cell .mn-step-meter > *")
      .evaluateAll((nodes) => nodes.map((n) => n.getBoundingClientRect().right));

    for (const right of childRights) {
      expect(right, `a meter child's right edge (${right}px) exceeded the cell's right edge (${cellBox!.x + cellBox!.width}px)`).toBeLessThanOrEqual(cellBox!.x + cellBox!.width + 0.5);
    }

    // Same invariant restated at the document level, the same technique
    // `live-render.spec.ts` uses: an overflowing meter widens `body`
    // (`html`/`body` both clip `documentElement.scrollWidth`, so that one
    // reads clean regardless — see that spec's own comment on why `body`'s
    // is the real signal).
    const overflow = await page.evaluate(() => document.body.scrollWidth > document.documentElement.clientWidth);
    expect(overflow, "no horizontal document scroll at 390px").toBe(false);
  });

  test("700x900: the name keeps a readable minimum width", async ({ page }) => {
    await gotoCollapsedTaskRow(page, 700, 900);

    const nameEl = page.locator(".missionlens .tltask .tlt-hd .tlt-name").first();
    const nameBox = await nameEl.boundingBox();
    const fontSize = await nameEl.evaluate((el) => parseFloat(getComputedStyle(el).fontSize));

    expect(nameBox).not.toBeNull();
    expect(nameBox!.width, `name box was ${nameBox!.width}px, expected >= ${10 * fontSize * 0.6}px (10 chars)`).toBeGreaterThanOrEqual(10 * fontSize * 0.6);
  });

  test("390x844 (phone): the meter drops to its own line under the name", async ({ page }) => {
    await gotoCollapsedTaskRow(page, 390, 844);

    const nameBox = await page.locator(".missionlens .tltask .tlt-hd .tlt-name").first().boundingBox();
    const meterBox = await page.locator(".missionlens .tltask .tlt-hd .tlt-meter-cell").first().boundingBox();
    expect(nameBox).not.toBeNull();
    expect(meterBox).not.toBeNull();

    // Below the narrow breakpoint the meter is a SECOND grid row, so its top
    // sits at or below the name's bottom — never sharing the name's row.
    expect(meterBox!.y, `meter top (${meterBox!.y}) was not below name bottom (${nameBox!.y + nameBox!.height})`).toBeGreaterThanOrEqual(nameBox!.y + nameBox!.height - 1);
  });

  test("900x900 (above the 700px breakpoint): the meter shares the name's row", async ({ page }) => {
    // NOT 700x900 — `@media (max-width: 700px)` is inclusive of exactly
    // 700px, so the two-row phone layout is (correctly) still active there.
    // 900px is unambiguously above the breakpoint — but `isNarrowViewport`
    // (`timeline.ts`) uses the SAME <= 700px cutoff to pick the renderer
    // itself, so `MissionGraphLens` defaults to the desktop canvas at
    // 900px and `.tltask` never mounts. Force the timeline renderer via
    // its own "list" toggle (`MissionGraphLens.tsx`'s `evbtn`) so this test
    // exercises the SAME `.tlt-hd` grid the phone case does, just above
    // the CSS breakpoint that governs its layout.
    await page.setViewportSize({ width: 900, height: 900 });
    await page.goto(`/#mission=${REAL_MISSION_ID}`);
    await page.getByRole("button", { name: "list" }).click();
    const row = page.locator(".missionlens .tltask .tlt-hd").first();
    await expect(row).toBeVisible({ timeout: 15_000 });
    await expect(row.locator(".tlt-meter-cell")).not.toHaveText("", { timeout: 15_000 });

    const nameBox = await page.locator(".missionlens .tltask .tlt-hd .tlt-name").first().boundingBox();
    const meterBox = await page.locator(".missionlens .tltask .tlt-hd .tlt-meter-cell").first().boundingBox();
    expect(nameBox).not.toBeNull();
    expect(meterBox).not.toBeNull();

    // Above the breakpoint it's still the single-row layout — meter and
    // name occupy overlapping vertical spans (same grid row), not stacked
    // spans (below the breakpoint the meter's top starts at/after the
    // name's bottom — see the 390px case above).
    const overlaps = meterBox!.y < nameBox!.y + nameBox!.height && nameBox!.y < meterBox!.y + meterBox!.height;
    expect(overlaps, `meter (y=${meterBox!.y}, h=${meterBox!.height}) and name (y=${nameBox!.y}, h=${nameBox!.height}) do not vertically overlap — looks stacked, not sharing a row`).toBe(true);
  });
});
