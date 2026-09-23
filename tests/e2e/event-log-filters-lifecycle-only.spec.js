// #2512 acceptance: a corpus with NO model activity at all — every record
// is lifecycle/telemetry (dispatch/mission/phase/step bookends, machine
// online/offline, host telemetry) — must not read "0 events" on a fresh
// load with no seeded `sessionStorage`.
//
// `event-log-filters-default.spec.js` (#2416/#2417) already covers the
// curated-allowlist HAPPY path — a corpus that DOES contain a
// `DEFAULT_ACTIVITIES` member (`reasoning`) and confirms the curated
// denoise still applies (heartbeat/dispatch start/machine online hidden).
// This is the other half: a corpus where NONE of the 11 activity values
// this fixture carries is in `DEFAULT_ACTIVITIES` and none is
// failure-shaped, so the naive curated fold alone produces an EMPTY `act`
// set — the exact #2512 defect (`ui/src/lib/eventFilters.ts`'s
// `resolveActivitySet` backstop is what this proves live, in a real
// browser, through the real component wiring — not just the jsdom-free
// unit tests in `eventFilters.test.ts`).
//
// (operator, 2026-09-23) UPDATED for the periodic-sample exclusion: this
// fixture's one `machine.telemetry` ("host telemetry") record is now
// EXCLUDED from the backstop's fallback — showing it flooded a quiet
// fleet's real 24h window with sampler noise (see
// `PERIODIC_SAMPLE_ACTIVITIES` in `eventFilters.ts`). So the backstop now
// shows the other 10 lifecycle records, not all 11, and the chip/filter
// button correctly read that one record as hidden rather than claiming
// zero active filters.
const { test, expect } = require('@playwright/test');

const DESKTOP = { width: 1280, height: 900 };
const PHONE = { width: 390, height: 844 };

// The fixture (`tests/fixtures/filters-lifecycle-only-flow.jsonl`) carries
// 11 records, 9 distinct `act` facet values, all lifecycle/telemetry:
// machine online/offline, dispatch start/end, mission start/close, phase
// begin/complete, step start/complete, host telemetry. No stored picks
// means no explicit include/exclude for any of them, so the backstop shows
// every NON-PERIODIC one — 10 of the 11 rows (the `machine.telemetry` row
// stays hidden), and the Filters button reads "filters, 1 active" because
// that one value is the single facet member excluded from the fallback.
async function assertLifecycleOnlyDefaults(page) {
  await expect(page.locator('.eventlog__rec').first()).toBeVisible();

  const rows = page.locator('.eventlog__rec');
  await expect(rows).toHaveCount(10);

  // The literal bug signature this issue reported: "0 events" with
  // everything hidden. Assert its exact opposite, not just "some text".
  //
  // Desktop renders the full text as the chip's own content; the phone
  // drawer renders a `compactCountLabel`-shortened form ("10" instead of
  // "10 events" — see `EventLogColumn.tsx`'s own doc) and keeps the full
  // text only in `title`. Read both so this assertion holds on either
  // viewport without hardcoding which one carries the word "events".
  const chip = page.locator('.eventlog__qcount, .qcount').first();
  const chipReading = await chip.evaluate((el) => `${el.textContent ?? ''} ${el.getAttribute('title') ?? ''}`);
  expect(chipReading).toContain('10 events');
  // Word-boundary regex, not a plain substring: "10 events" itself contains
  // the substring "0 events", which a naive `.not.toContain('0 events')`
  // would wrongly flag.
  expect(chipReading).not.toMatch(/\b0 events\b/);
  // The one periodic-sample record (`machine.telemetry`) is correctly
  // hidden by the fallback now — honestly disclosed, not silently dropped.
  expect(chipReading).toContain('1 hidden');

  const fbtn = page.locator('#fbtn');
  const ariaLabel = await fbtn.getAttribute('aria-label');
  // One facet member (host telemetry) is excluded from the fallback, so
  // this is genuinely "1 active", not "filters" (zero).
  expect(ariaLabel).toBe('filters, 1 active');
}

test.describe('desktop', () => {
  test.use({ viewport: DESKTOP });

  test.beforeEach(async ({ page }) => {
    page.on('pageerror', (e) => { throw new Error(`uncaught page error: ${e}`); });
  });

  test('a fresh load of an all-lifecycle corpus with no seeded storage shows every record, not "0 events · N hidden"', async ({ page }) => {
    await page.goto('/index-filters-lifecycle-only.html');
    await assertLifecycleOnlyDefaults(page);
  });
});

test.describe('phone (390px)', () => {
  test.use({ viewport: PHONE });

  test.beforeEach(async ({ page }) => {
    page.on('pageerror', (e) => { throw new Error(`uncaught page error: ${e}`); });
  });

  test('a fresh load of an all-lifecycle corpus with no seeded storage shows every record (phone drawer)', async ({ page }) => {
    await page.goto('/index-filters-lifecycle-only.html');
    await page.click('[data-act="phone-drawer-tab-events"]');
    await assertLifecycleOnlyDefaults(page);
  });
});
