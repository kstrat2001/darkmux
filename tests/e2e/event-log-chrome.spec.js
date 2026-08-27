// The event log's collapse chrome, MEASURED in a real browser (#2029).
//
// Three defects shipped together and every UI unit test stayed green through
// all of them, because jsdom does not apply stylesheets: the suite asserts
// class NAMES, and all three class names were correct. What was wrong was the
// computed result.
//
//   1. Collapsed kept its 380px width. `.eventlog--collapsed` and `.eventlog`
//      are both single-class, so identical specificity — and the collapsed
//      rule sat ABOVE the base rule, so `width: 380px` won on source order.
//      The pane hid its content and reclaimed nothing.
//   2. The toggle was `position: absolute` and painted over the detail pane's
//      first line — it covered the record headline, and the empty-state
//      sentence when nothing was selected.
//   3. The follow button lost its base styling and wore a permanent accent
//      border, because an unrelated edit anchored on the first occurrence of
//      `.eventlog__fbtn` — the SECOND LINE of a grouped selector — and spliced
//      a rule into the middle of the group, orphaning `.eventlog__follow`.
//      Its `.on` class toggled correctly throughout; nothing rendered
//      differently, so it read as a dead control.
//
// All three were reported from the screen. This file is the only place any of
// them can be pinned — same reason `chrome-order.spec.js` exists.
const { test, expect } = require('@playwright/test');

const DESKTOP = { width: 1280, height: 900 };

test.use({ viewport: DESKTOP });

test.beforeEach(async ({ page }) => {
  page.on('pageerror', (e) => { throw new Error(`uncaught page error: ${e}`); });
});

async function openLog(page) {
  await page.goto('/index-lab.html');
  await expect(page.locator('.eventlog')).toBeVisible();
}

test('collapsing reclaims the width, it does not just blank the pane', async ({ page }) => {
  await openLog(page);
  const col = page.locator('.eventlog');
  const expanded = (await col.boundingBox()).width;
  expect(expanded).toBeGreaterThan(200);

  await page.click('[data-act="togglelog"]');
  const collapsed = (await col.boundingBox()).width;
  // The bug was `collapsed === expanded`. Asserting "much narrower" rather
  // than an exact px keeps this about the BEHAVIOR, so a future width tweak
  // does not fail it for the wrong reason.
  expect(collapsed).toBeLessThan(expanded / 4);

  await page.click('[data-act="togglelog"]');
  expect((await col.boundingBox()).width).toBeCloseTo(expanded, 0);
});

test('the collapse toggle never paints over the detail pane', async ({ page }) => {
  await openLog(page);
  const overlaps = async () =>
    page.evaluate(() => {
      const b = document.querySelector('[data-act="togglelog"]')?.getBoundingClientRect();
      const d = document.getElementById('detailbody')?.getBoundingClientRect();
      if (!b || !d) return false;
      return b.bottom > d.top && b.right > d.left && b.left < d.right && b.top < d.bottom;
    });
  expect(await overlaps()).toBe(false);
  await page.click('[data-act="togglelog"]');
  expect(await overlaps()).toBe(false);
});

test('the follow toggle LOOKS different in each state, not just class-different', async ({ page }) => {
  await openLog(page);
  const follow = page.locator('#follow');

  // The mouse must be moved OFF the control before every read. Playwright
  // leaves the cursor on a button after clicking it, and `:hover` paints the
  // same accent as `.on` — which made an earlier probe report "no change"
  // in BOTH states and nearly got filed as a fourth bug.
  const paint = async () => {
    await page.mouse.move(20, 700);
    return page.evaluate(() => {
      const el = document.getElementById('follow');
      const cs = getComputedStyle(el);
      return { on: el.className.includes('on'), color: cs.color, border: cs.borderColor };
    });
  };

  const on = await paint();
  expect(on.on).toBe(true);
  await follow.click();
  const off = await paint();
  expect(off.on).toBe(false);

  // The actual assertion: the RENDERING differs. Class alone was already
  // correct while the control looked identical in both states.
  expect(off.color).not.toBe(on.color);
  expect(off.border).not.toBe(on.border);
});
