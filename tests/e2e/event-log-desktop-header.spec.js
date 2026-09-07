// (#2447, operator screenshot) The DESKTOP twin of `event-log-chip-wrap.spec.js`,
// which pins the same header on a phone. Both regressions this file guards
// were CSS-layout-only — invisible to every unit test, and each shipped
// because the fix for one skin was verified only on that skin:
//
//   1. #2108 moved the count pill INSIDE `.eventlog__headbtns` for the
//      phone's one-row toolbar. On desktop that group is `flex: none` and
//      the pill is `white-space: nowrap` at ~250px inside a ~380px events
//      column, so the pill could neither shrink nor wrap: it squeezed
//      "events last 24h" into three stacked lines and still ran past the
//      header's right edge.
//   2. The fix for (1) put the pill back beside the button group as a third
//      `<h3>` child — under `justify-content: space-between`, which pins the
//      first and last child and CENTERS the middle one. Whenever the pill
//      was short enough to share line 1 (the common case: "3 events"), the
//      follow and filters buttons detached from the right edge and floated
//      mid-header.
//
// So both cases are pinned here, on the two harnesses that produce them: a
// busy fixture whose chip is long enough to wrap, and a small one whose chip
// is not. The assertions are about POSITION, not text — the text is
// `EventLogColumn.test.tsx`'s job.
const { test, expect } = require('@playwright/test');

const DESKTOP = { width: 1440, height: 900 };

async function headerGeometry(page) {
  return page.evaluate(() => {
    const box = (el) => {
      const r = el.getBoundingClientRect();
      return { left: Math.round(r.left), right: Math.round(r.right), top: Math.round(r.top), height: Math.round(r.height) };
    };
    const h3 = document.querySelector('.eventlog__head h3');
    const style = getComputedStyle(h3);
    return {
      h3: box(h3),
      // The h3's own content box — the edge everything inside it must respect.
      contentRight: Math.round(h3.getBoundingClientRect().right - parseFloat(style.paddingRight || '0')),
      title: box(h3.querySelector('span')),
      btns: box(document.querySelector('.eventlog__headbtns')),
      pill: box(document.getElementById('qcount')),
    };
  });
}

test.beforeEach(async ({ page }) => {
  page.on('pageerror', (e) => { throw new Error(`uncaught page error: ${e}`); });
  await page.setViewportSize(DESKTOP);
});

test('a LONG count chip wraps to its own line instead of overflowing the header or stacking the title', async ({ page }) => {
  await page.goto('/index-filters-overflow.html');
  await expect(page.locator('.eventlog__rec').first()).toBeVisible();
  const g = await headerGeometry(page);

  // The chip really is the long form this case is about.
  await expect(page.locator('#qcount')).toHaveText(/hidden$/);
  expect(g.pill.right - g.pill.left).toBeGreaterThan(200);

  // (1) Nothing runs past the header's right edge. This is the exact
  // failure the operator screenshotted: +6px of the chip outside the box.
  expect(g.pill.right).toBeLessThanOrEqual(g.contentRight);
  expect(g.btns.right).toBeLessThanOrEqual(g.contentRight);

  // (2) The title keeps ONE line — it was three.
  expect(g.title.height).toBeLessThan(20);

  // (3) The chip wrapped: it sits BELOW the buttons, not beside them.
  //     Compared on the BOTTOM/TOP edges, not the tops — the chip is 20px
  //     and the buttons 25px, so equal tops is not what "same line" means
  //     here (see the short-chip test's centre comparison).
  expect(g.pill.top).toBeGreaterThanOrEqual(g.btns.top + g.btns.height);

  // (4) The buttons stay hard right on line 1 rather than floating mid-row.
  expect(g.contentRight - g.btns.right).toBeLessThanOrEqual(1);
});

test('a SHORT count chip shares line 1 with the buttons, and the buttons stay against the right edge', async ({ page }) => {
  await page.goto('/index-filters-default.html');
  await expect(page.locator('.eventlog__rec').first()).toBeVisible();
  const g = await headerGeometry(page);

  // The chip really is the short form this case is about.
  expect(g.pill.right - g.pill.left).toBeLessThan(180);

  // (1) One row: the chip is beside the buttons, not below them. The two
  //     are different heights (20px chip, 25px buttons) and the row is
  //     `align-items: center`, so their CENTRES coincide, not their tops.
  const centre = (b) => b.top + b.height / 2;
  expect(Math.abs(centre(g.pill) - centre(g.btns))).toBeLessThanOrEqual(1);

  // (2) The buttons sit immediately left of the chip. Under
  // `space-between` the middle child centers and this gap was ~80px.
  expect(g.pill.left - g.btns.right).toBeLessThanOrEqual(10);

  // (3) And the pair ends flush with the header's right edge.
  expect(g.contentRight - g.pill.right).toBeLessThanOrEqual(1);

  // (4) Still one line of title, and nothing outside the box.
  expect(g.title.height).toBeLessThan(20);
  expect(g.pill.right).toBeLessThanOrEqual(g.contentRight);
});
