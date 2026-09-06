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
// "50/1.1k · 5.0k hidden") and ellipsizes rather than wrapping if it still
// doesn't fit. This test now asserts the ONE-ROW outcome instead of the
// old wrap-to-second-line one — the geometry claim inverted (never wraps,
// stays on follow/filters' own row) along with the text format, so the
// assertions below are rewritten, not just retargeted.
//
// Pinned at BOTH 390 and 320 (operator, round 2, 2026-09-06) — a fix
// verified only at 390 can hide a regression at the narrower width the
// drawer also has to support (`phone-drawer-inset.spec.js`'s own
// horizontal-overflow tests use the same pair). `#fbtn`'s own top is
// checked alongside `#follow`'s, not just `#qcount`'s, so a future edit
// that misaligns just the filters icon button (rather than the count)
// would still be caught.
const { test, expect } = require('@playwright/test');

test.beforeEach(async ({ page }) => {
  page.on('pageerror', (e) => { throw new Error(`uncaught page error: ${e}`); });
});

for (const viewport of [
  { width: 390, height: 844 },
  { width: 320, height: 568 },
]) {
  test(`the events count stays on the follow/filters row and never overflows the drawer at ${viewport.width}px`, async ({ page }) => {
    await page.setViewportSize(viewport);
    await page.goto('/index-filters-overflow.html');
    // Open the bottom drawer's Events tab.
    await page.getByText(/Events ·/).click();
    await expect(page.locator('.eventlog__rec').first()).toBeVisible();

    const qcount = page.locator('#qcount');
    // Abbreviated form: "events"/"of" are dropped, and any 4+-digit run
    // (1140 records, hidden count here) becomes one-decimal "k" form —
    // see `compactCountLabel()`'s own doc for the exact transform.
    await expect(qcount).toHaveText(/ · [\d.]+k? hidden$/);

    const geo = await page.evaluate(() => {
      const q = document.getElementById('qcount');
      const clock = document.getElementById('follow');
      const fbtn = document.getElementById('fbtn');
      const row2 = document.querySelector('.phone-drawer__body .eventlog__headbtns');
      const head = document.querySelector('.phone-drawer__body .eventlog__head');
      const qBox = q.getBoundingClientRect();
      const clockBox = clock.getBoundingClientRect();
      const fbtnBox = fbtn.getBoundingClientRect();
      const row2Box = row2.getBoundingClientRect();
      const headBox = head.getBoundingClientRect();
      const headPaddingRight = parseFloat(getComputedStyle(head).paddingRight);
      return { qBox, clockBox, fbtnBox, row2Height: row2Box.height, headRight: headBox.right, headPaddingRight };
    });

    // Never past the drawer's own right edge.
    expect(geo.qBox.right).toBeLessThanOrEqual(geo.headRight - geo.headPaddingRight + 1); // +1 rounding tolerance
    // Same row as the clock icon AND the filters icon button (SUPERSEDES
    // the old "wraps to a second line" claim) — all three tops line up
    // within a couple of px.
    expect(Math.abs(geo.qBox.top - geo.clockBox.top)).toBeLessThanOrEqual(2);
    expect(Math.abs(geo.fbtnBox.top - geo.clockBox.top)).toBeLessThanOrEqual(2);
    // Row 2 (follow + filters + count) stays one line — the 44x44 button
    // fix (round 2) grew it from 36px to 44px tall; a regression that
    // silently reintroduced the old wrap-to-second-line behavior would
    // roughly double this.
    expect(geo.row2Height).toBeLessThanOrEqual(48);
  });
}
