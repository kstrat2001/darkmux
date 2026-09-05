// #2416/#2417 round 2's curated activity default, measured in a real
// browser with NO seeded `sessionStorage` — the state an operator's FIRST
// EVER load of the page is actually in. Every unit test that exercises
// `defaultFilterState`/`restoreFilterState` runs in jsdom against the pure
// function directly; this is the one place the real DOM, real
// `window.sessionStorage`, and the real component wiring all have to agree
// that a fresh tab hides heartbeat, shows reasoning, and says so on the
// Filters button.
const { test, expect } = require('@playwright/test');

const DESKTOP = { width: 1280, height: 900 };

test.use({ viewport: DESKTOP });

test.beforeEach(async ({ page }) => {
  page.on('pageerror', (e) => { throw new Error(`uncaught page error: ${e}`); });
});

test('a fresh load with no seeded storage hides heartbeat, shows reasoning, and the Filters button is nonzero', async ({ page }) => {
  await page.goto('/index-filters-default.html');
  await expect(page.locator('.eventlog__rec').first()).toBeVisible();

  const rows = page.locator('.eventlog__rec');
  await expect(rows).toHaveCount(2); // only the two reasoning rows — heartbeat is hidden by default
  await expect(page.locator('.eventlog')).not.toContainText('heartbeat');
  await expect(page.locator('.eventlog')).toContainText('reasoning');

  const fbtn = page.locator('#fbtn');
  const ariaLabel = await fbtn.getAttribute('aria-label');
  expect(ariaLabel).toMatch(/^filters, \d+ active$/);
  const active = Number(ariaLabel.match(/(\d+) active/)[1]);
  expect(active).toBeGreaterThanOrEqual(1);
});
