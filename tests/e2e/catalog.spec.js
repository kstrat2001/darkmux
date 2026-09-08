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

// (#2412 round 2, reviewer finding) A 44-char mission id rendered the pill
// 435px wide on a 390px phone with nothing to stop it, wrapping the
// masthead onto a SECOND row and shifting the tab strip + `#stage` down by
// that row's height. `.masthead__pilltext`'s phone-width `max-width` +
// ellipsis (`styles.css`) fixes it — this proves the geometry directly
// rather than trusting the CSS rule exists: the masthead's own height with
// the long id must equal its height on an ordinary live route (one row,
// not two), and the pill's own right edge must never cross the viewport.
test('a long mission id truncates on the phone instead of wrapping the masthead onto a second row', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 });

  await page.route('**/flow/**', (r) => r.fulfill({ contentType: 'application/json', body: '[]' }));
  await page.route('**/fleet/machines/live', (r) =>
    r.fulfill({ contentType: 'application/json', body: JSON.stringify({ machines: [], meta: { sources: { fleet: { state: 'ok' } }, complete: true } }) })
  );
  await page.route('**/fleet/sessions/live', (r) =>
    r.fulfill({ contentType: 'application/json', body: JSON.stringify({ sessions: [], meta: { sources: { fleet: { state: 'ok' } }, complete: true } }) })
  );

  await page.goto('/index-live.html');
  await page.waitForSelector('.catalog-toggle', { timeout: 15_000 });
  await expect(page.locator('.catalog-toggle')).toContainText('LIVE');
  const liveHeight = (await page.locator('.masthead').boundingBox()).height;

  const longId = 'acp-ephemeral-pr-ship-1786152707367180000-5';
  await page.route('**/flow-mission/**', (r) =>
    r.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({
        records: [
          { ts: '2026-08-07T09:00:00Z', category: 'dispatch', action: 'dispatch.start', mission_id: longId },
          { ts: '2026-08-07T09:31:00Z', category: 'mission', action: 'mission close', mission_id: longId },
        ],
        count: 2,
        truncated: false,
        generated_at_ms: 1,
      }),
    })
  );

  await page.goto(`/index-daemon.html#mission=${longId}`);
  await page.waitForSelector('.catalog-toggle', { timeout: 15_000 });
  await expect(page.locator('.masthead__pilltext')).toHaveAttribute('title', longId, { timeout: 15_000 });

  const mastheadBox = await page.locator('.masthead').boundingBox();
  const pillBox = await page.locator('.catalog-toggle').boundingBox();
  const viewport = page.viewportSize();

  expect(Math.round(mastheadBox.height), 'the masthead must stay ONE row — same height as the plain live state').toBe(Math.round(liveHeight));
  expect(pillBox.x + pillBox.width, "the pill's right edge must never cross the viewport").toBeLessThanOrEqual(viewport.width);

  // The replay glyph stays visible — never itself inside the truncating,
  // ellipsized span.
  await expect(page.locator('.masthead__pilldot--replay')).toBeVisible();
  await expect(page.locator('.masthead__pilldot--replay')).toHaveText('▣');
});

// (#2264) `viewport-fit=cover` (index.html) extends the standalone/PWA page
// under the iOS status bar, so `.masthead` must pad for
// `env(safe-area-inset-top)` — otherwise the wordmark/pill render UNDER the
// clock/battery. There is no notched device in CI, so this proves it the
// only way headless Chromium allows: `Emulation.setSafeAreaInsetsOverride`
// (verified against this harness's own Chromium build before writing the
// fix — it makes `env(safe-area-inset-top)` resolve to a real value) makes
// the inset real for the page, and the assertion reads the MASTHEAD's own
// computed padding + its brand's real screen position, not a CSS string.
test('the masthead pads for the iOS safe-area-inset-top on a phone', async ({ page, context }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  const cdp = await context.newCDPSession(page);
  await cdp.send('Emulation.setSafeAreaInsetsOverride', {
    insets: { top: 47, right: 0, bottom: 34, left: 0 },
  });

  await page.route('**/fleet/machines/live', (r) =>
    r.fulfill({ contentType: 'application/json', body: JSON.stringify({ machines: [], meta: { sources: { fleet: { state: 'ok' } }, complete: true } }) })
  );
  await page.route('**/fleet/sessions/live', (r) =>
    r.fulfill({ contentType: 'application/json', body: JSON.stringify({ sessions: [], meta: { sources: { fleet: { state: 'ok' } }, complete: true } }) })
  );

  await page.goto('/index-live.html');
  await page.waitForSelector('.masthead__brand', { timeout: 15_000 });

  const geo = await page.evaluate(() => {
    const m = document.querySelector('.masthead');
    const brand = document.querySelector('.masthead__brand');
    return {
      paddingTop: parseFloat(getComputedStyle(m).paddingTop),
      brandTop: Math.round(brand.getBoundingClientRect().top),
    };
  });
  // `styles.css`'s <=768px rule: `padding-top: calc(8px + env(safe-area-inset-top, 0px))`.
  expect(Math.round(geo.paddingTop), `masthead padding-top ${geo.paddingTop}px — want 8 + the 47px inset`).toBe(8 + 47);
  // The wordmark itself must clear the simulated status bar band, not just
  // the box around it.
  expect(geo.brandTop, `brand top ${geo.brandTop}px — must clear the 47px inset`).toBeGreaterThanOrEqual(47);
});

// The SAME masthead, on desktop, with NO inset simulated at all (the real
// shape of every non-notched device: `env(safe-area-inset-top)` resolves to
// 0 with nothing to override) — the fix must not add phantom padding where
// there is no inset to clear.
test('the masthead is unaffected by the safe-area fix on desktop', async ({ page }) => {
  await page.setViewportSize({ width: 1456, height: 900 });
  await page.route('**/fleet/machines/live', (r) =>
    r.fulfill({ contentType: 'application/json', body: JSON.stringify({ machines: [], meta: { sources: { fleet: { state: 'ok' } }, complete: true } }) })
  );
  await page.route('**/fleet/sessions/live', (r) =>
    r.fulfill({ contentType: 'application/json', body: JSON.stringify({ sessions: [], meta: { sources: { fleet: { state: 'ok' } }, complete: true } }) })
  );

  await page.goto('/index-live.html');
  await page.waitForSelector('.masthead__brand', { timeout: 15_000 });
  const paddingTop = await page.evaluate(() => parseFloat(getComputedStyle(document.querySelector('.masthead')).paddingTop));
  // `styles.css`'s base rule: `padding-top: calc(10px + env(safe-area-inset-top, 0px))`, and no inset is simulated here.
  expect(Math.round(paddingTop), `masthead padding-top ${paddingTop}px on desktop — want the plain 10px, no inset`).toBe(10);
});
