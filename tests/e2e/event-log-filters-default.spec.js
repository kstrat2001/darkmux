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
const PHONE = { width: 390, height: 844 };

// (2026-09-06, live review) `tests/fixtures/filters-default-flow.jsonl`
// carries 7 records across 4 distinct `act` facet values (`reasoning` x2,
// `dispatch start`, `machine online`, `heartbeat` x3), 2 `cat` values
// (`machinery`, `work`), 1 `tier` value (`local`), and 1 `src` value
// (`presence-reconciler`, the only record that carries a `source` field
// at all). `defaultFilterState` (`lib/eventFilters.ts`) leaves `cat`/
// `tier`/`src` fully selected — 0 hidden on each — and restricts `act` to
// `DEFAULT_ACTIVITIES` (reasoning/checkpoint/tool call/turn/dispatch
// error): only `reasoning` survives of this fixture's 4 activity values,
// hiding the other 3 (`heartbeat`, `dispatch start`, `machine online`).
// `activeFilterCount` sums `offered - selected` per facet, so the exact
// total here is 3 — not "some number >= 1", which can't see a regression
// back to the OLD per-facet count (capped at 1 per facet regardless of how
// many values it hid).
const EXPECTED_ACTIVE_FILTERS = 3;

async function assertFreshLoadDefaults(page) {
  await expect(page.locator('.eventlog__rec').first()).toBeVisible();

  const rows = page.locator('.eventlog__rec');
  // Only the two reasoning rows survive — heartbeat, dispatch start, and
  // machine online are ALL hidden by default (none is in
  // `DEFAULT_ACTIVITIES`), not just heartbeat.
  await expect(rows).toHaveCount(2);
  await expect(page.locator('.eventlog')).not.toContainText('heartbeat');
  await expect(page.locator('.eventlog')).toContainText('reasoning');

  const fbtn = page.locator('#fbtn');
  const ariaLabel = await fbtn.getAttribute('aria-label');
  expect(ariaLabel).toBe(`filters, ${EXPECTED_ACTIVE_FILTERS} active`);
}

test.describe('desktop', () => {
  test.use({ viewport: DESKTOP });

  test.beforeEach(async ({ page }) => {
    page.on('pageerror', (e) => { throw new Error(`uncaught page error: ${e}`); });
  });

  test('a fresh load with no seeded storage hides heartbeat, shows reasoning, and the Filters button names the exact hidden count', async ({ page }) => {
    await page.goto('/index-filters-default.html');
    await assertFreshLoadDefaults(page);
  });
});

test.describe('phone (390px)', () => {
  test.use({ viewport: PHONE });

  test.beforeEach(async ({ page }) => {
    page.on('pageerror', (e) => { throw new Error(`uncaught page error: ${e}`); });
  });

  test('a fresh load with no seeded storage hides heartbeat, shows reasoning, and the Filters button names the exact hidden count (phone drawer)', async ({ page }) => {
    await page.goto('/index-filters-default.html');
    // (`PhoneDrawer.tsx`'s own doc, and `chrome-order.spec.js`'s own
    // precedent) On phone the events pane lives inside PhoneDrawer's
    // "Events" tab, gated on `{open && <EventLogColumn .../>}` — closed,
    // it renders nothing. Tapping a closed tab opens the drawer to it.
    await page.click('[data-act="phone-drawer-tab-events"]');
    await assertFreshLoadDefaults(page);
  });
});
