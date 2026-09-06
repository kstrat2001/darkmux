// #2417 round 3, MF-A (ORIGINAL finding) — the events chip overflowed the
// phone drawer at 390px once the "· N hidden" suffix (#2417 round 2) landed
// on a busy stream: `.eventlog__headbtns` was `inline-flex` with no wrap,
// and `.eventlog__qcount` was `white-space: nowrap`, so a long chip like
// "50 of 200 events · 900 hidden" pushed the row's right edge past the
// drawer's own bounds. The fix at the time was to let the chip WRAP to its
// own second line.
//
// SUPERSEDED (operator, 2026-09-06) — that second-line wrap made the
// toolbar three stacked rows (search; follow+filters; the wrapped chip),
// which the operator called out as wasted vertical space. The current
// design (`EventLogColumn.tsx`'s `compactCountLabel()`, `styles.css`'s
// `.phone-drawer__body .eventlog__headbtns`/`.eventlog__qcount`) instead
// keeps follow, the filters icon button and the count on ONE row: the count
// text is abbreviated on mobile ("50 of 1140 events · 4953 hidden" ->
// "50/1.1k · 4.9k hidden") and ellipsizes rather than wrapping if it still
// doesn't fit. This test now asserts the ONE-ROW outcome instead of the
// old wrap-to-second-line one — the geometry claim inverted (never wraps,
// stays on follow/filters' own row) along with the text format, so the
// assertions below are rewritten, not just retargeted.
const { test, expect } = require('@playwright/test');

const PHONE = { width: 390, height: 844 };

test.use({ viewport: PHONE, hasTouch: true, isMobile: true });

test.beforeEach(async ({ page }) => {
  page.on('pageerror', (e) => { throw new Error(`uncaught page error: ${e}`); });
});

test('the events count stays on the follow/filters row and never overflows the drawer', async ({ page }) => {
  await page.goto('/index-filters-overflow.html');
  // Open the bottom drawer's Events tab.
  await page.getByText(/Events ·/).click();
  await expect(page.locator('.eventlog__rec').first()).toBeVisible();

  const qcount = page.locator('#qcount');
  // Abbreviated form: "events"/"of" are dropped, and any 4+-digit run
  // (1140 hidden count here) becomes one-decimal "k" form — see
  // `compactCountLabel()`'s own doc for the exact transform.
  await expect(qcount).toHaveText(/ · [\d.]+k? hidden$/);

  const geo = await page.evaluate(() => {
    const q = document.getElementById('qcount');
    const clock = document.getElementById('follow');
    const head = document.querySelector('.phone-drawer__body .eventlog__head');
    const qBox = q.getBoundingClientRect();
    const clockBox = clock.getBoundingClientRect();
    const headBox = head.getBoundingClientRect();
    const headPaddingRight = parseFloat(getComputedStyle(head).paddingRight);
    return { qBox, clockBox, headRight: headBox.right, headPaddingRight };
  });

  // Never past the drawer's own right edge.
  expect(geo.qBox.right).toBeLessThanOrEqual(geo.headRight - geo.headPaddingRight + 1); // +1 rounding tolerance
  // Same row as the clock icon (SUPERSEDES the old "wraps to a second
  // line" claim) — their tops line up within a couple of px.
  expect(Math.abs(geo.qBox.top - geo.clockBox.top)).toBeLessThanOrEqual(2);
});
