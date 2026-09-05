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

// (U2-2) The detail pane's PAYLOAD key column, measured for the same reason
// as everything else in this file: `.rv__key` was a fixed 116px column with
// `text-overflow: ellipsis`, set before `--fs-scale: 1.18` (#2002) grew every
// glyph in the panel — so at 1456 a real field name rendered as
// "cpu speed lim…", hiding WHICH field a number belongs to. The phone rule
// below it already stacked label over value; only the desktop rule was left
// behind. Every unit test stayed green: the class name was always right.
test('(U2-2) a long payload field name wraps rather than being clipped', async ({ page }) => {
  await openLog(page);
  await page.locator('.eventlog__rec').first().click();
  await expect(page.locator('.rv__key').first()).toBeVisible();

  // The REAL element, its real stylesheet — only the text is substituted, so
  // this measures the column rather than whatever key this fixture happens
  // to carry. `cpu speed limit pct` is the name from the finding itself.
  const clipped = await page.evaluate(() => {
    const k = document.querySelector('.rv__key');
    k.textContent = 'cpu speed limit pct';
    // +1 for sub-pixel rounding; more than that is a real ellipsis.
    return k.scrollWidth > k.clientWidth + 1;
  });
  expect(clipped, 'the payload key column truncated a real field name').toBe(false);
});
