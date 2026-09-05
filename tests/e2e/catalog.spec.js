// Headless e2e for the #691 playback catalog day-picker. The picker is the one
// viewer render path that shows ONLY in daemon mode (it needs /flow-days), so
// the demo-mode XSS gate (viewer-xss.spec.js) never exercises it — yet it
// renders record-derived content (mission names). This spec route-mocks the
// daemon endpoints, including a malicious mission name, and asserts the catalog
// renders it inertly + wires day navigation correctly.
const { test, expect } = require('@playwright/test');

test('catalog picker renders days + missions inertly and wires navigation', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));

  // boot() (mode=play, no flow-src) fetches /flow/<date>; return an empty day so
  // the daemon path succeeds (mode != no-daemon → the catalog button shows).
  await page.route('**/flow/2026-01-01', (r) =>
    r.fulfill({ contentType: 'application/json', body: '[]' })
  );
  // The day this spec drills into via the catalog — PlaybackLens's own
  // `/flow/<date>` fetch (the port's day-row destination; see the
  // navigation comment below). One real record, not an empty array: an
  // empty day renders PlaybackLens's OWN "no records for <date>" state,
  // not `.fleet-lens` — this spec needs the historical fleet hero to
  // actually paint to prove the drill-in landed somewhere real.
  await page.route('**/flow/2026-01-02', (r) =>
    r.fulfill({
      contentType: 'application/json',
      body: JSON.stringify([
        {
          ts: '2026-01-02T00:00:00Z', level: 'info', category: 'machinery',
          tier: 'local', stage: 'dispatch', action: 'machine.online',
          source: 'presence-reconciler', machine_id: 'demo-machine', machine_uid: 'demo-machine-uid',
        },
      ]),
    })
  );
  // The catalog: a real-shaped day plus an attacker-controlled mission name.
  await page.route('**/flow-days', (r) =>
    r.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({
        days: [
          { date: '2026-01-02', records: 12, dispatches: 3, missions: ['demo', "<img src=x onerror=window.__xss=1>"] },
          { date: '2026-01-01', records: 4, dispatches: 1, missions: [] },
        ],
        generated_at_ms: 0,
      }),
    })
  );

  await page.goto('/index-daemon.html');

  // (port note) Legacy upgrades a plain `#srcbadge` span into the catalog
  // trigger at boot time (`sb.dataset.act="catalog"; sb.classList.add
  // ("srcbtn")`, viewer.html:3937) — a JS-bolted-on affordance. The port's
  // `<Masthead>` mounts the REAL toggle unconditionally instead
  // (`CatalogPanel`'s own `.catalog-toggle` button, `aria-label="browse
  // history"` — see that component's module doc): no `#srcbadge` id exists
  // at all, and there's no boot-time upgrade step to wait on because the
  // button is interactive from first paint.
  await page.waitForSelector('.catalog-toggle', { timeout: 15_000 });

  // (#2412) The pill is now the ONE transport indicator, on every route —
  // this harness boots straight onto `#2026-01-01` (a replay), so the pill
  // already names that day with the replay glyph, and there is no separate
  // `#modebadge` beside it in any state.
  await expect(page.locator('.catalog-toggle')).toContainText('2026-01-01');
  await expect(page.locator('.catalog-toggle')).toContainText('▣');
  expect(await page.locator('#modebadge').count()).toBe(0);

  await page.click('.catalog-toggle');
  await page.waitForSelector('#catpanel .catrow');

  // Live row + two day rows.
  const rows = page.locator('#catpanel .catrow');
  await expect(rows).toHaveCount(3);
  await expect(rows.nth(0)).toContainText('live');
  await expect(rows.nth(1)).toContainText('2026-01-02');
  await expect(rows.nth(1)).toContainText('3 dispatches');
  // The mission name renders as TEXT, not as an injected element.
  await expect(rows.nth(1)).toContainText('demo');
  expect(await page.evaluate(() => window.__xss)).toBeUndefined();
  expect(await page.evaluate(() => document.querySelectorAll('img[src="x"],img[onerror]').length)).toBe(0);

  // A day row carries the navigation intent (data-act/data-arg), not an inline handler.
  const dayRow = page.locator('.catrow[data-arg="2026-01-02"]');
  await expect(dayRow).toHaveAttribute('data-act', 'goday');

  // Clicking it navigates to the day's playback view. Legacy does a real
  // `location.href="/play/"+date` (a server route, `.catalog-toggle`'s own
  // predecessor was a full-page bounce); the port's day row instead writes
  // `location.hash=date` (`CatalogPanel.tsx`'s own doc: "the NEW `playback`
  // route this packet adds") and renders the same in-SPA `FleetLens` over
  // that historical day (`PlaybackLens.tsx`'s own doc: "the fleet hero
  // rendered over one historical day, not a separate view") — no page
  // navigation at all. `/play/<date>` still exists as a real SERVER route
  // (a fresh boot straight onto that day, per this repo's daemon), it's
  // just not what a day-ROW CLICK does inside an already-booted SPA.
  await dayRow.click();
  await expect.poll(() => page.evaluate(() => location.hash)).toBe('#2026-01-02');
  await expect(page.locator('.fleet-lens')).toBeVisible();

  // (#2412) A past-date replay: the pill names the day with the replay
  // glyph in place of the dot, still with no separate badge.
  await expect(page.locator('.catalog-toggle')).toContainText('2026-01-02');
  await expect(page.locator('.catalog-toggle')).toContainText('▣');
  expect(await page.locator('#modebadge').count()).toBe(0);

  expect(pageErrors, `uncaught page errors: ${pageErrors.join(' | ')}`).toEqual([]);
});
