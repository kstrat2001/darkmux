// #2417 round 3, MF-A — the events chip overflows the phone drawer at
// 390px once the "· N hidden" suffix (#2417 round 2) lands on a busy
// stream: `.eventlog__headbtns` was `inline-flex` with no wrap, and
// `.eventlog__qcount` was `white-space: nowrap`, so a long chip like
// "50 of 200 events · 900 hidden" pushed the row's right edge past the
// drawer's own bounds instead of wrapping to a second line. Desktop is
// untouched — this is a phone-drawer-scoped fix, verified at 390x844
// against a real fixture large enough (1101 records) to produce the long
// chip text that triggers it.
const { test, expect } = require('@playwright/test');

const PHONE = { width: 390, height: 844 };

test.use({ viewport: PHONE, hasTouch: true, isMobile: true });

test.beforeEach(async ({ page }) => {
  page.on('pageerror', (e) => { throw new Error(`uncaught page error: ${e}`); });
});

test('the events chip wraps under follow/filters in the phone drawer instead of overflowing', async ({ page }) => {
  await page.goto('/index-filters-overflow.html');
  // Open the bottom drawer's Events tab.
  await page.getByText(/Events ·/).click();
  await expect(page.locator('.eventlog__rec').first()).toBeVisible();

  const qcount = page.locator('#qcount');
  await expect(qcount).toHaveText(/ · \d+ hidden$/);

  const { qRight, headRight, headPaddingRight } = await page.evaluate(() => {
    const q = document.getElementById('qcount');
    const head = document.querySelector('.phone-drawer__body .eventlog__head');
    const qBox = q.getBoundingClientRect();
    const headBox = head.getBoundingClientRect();
    const headPaddingRight = parseFloat(getComputedStyle(head).paddingRight);
    return { qRight: qBox.right, headRight: headBox.right, headPaddingRight };
  });

  expect(qRight).toBeLessThanOrEqual(headRight - headPaddingRight + 1); // +1 rounding tolerance
});
