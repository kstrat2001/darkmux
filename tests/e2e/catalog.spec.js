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

// (#2264) Reads the masthead's real rendered geometry plus what
// `env(safe-area-inset-top)` ACTUALLY resolves to on this page — measured
// from a throwaway probe element, never assumed from what CDP was asked for.
// Shared by the three masthead safe-area tests below so all three read the
// same numbers the same way.
async function readMastheadGeometry(page) {
  return page.evaluate(() => {
    const m = document.querySelector('.masthead');
    const brand = document.querySelector('.masthead__brand');
    const probe = document.createElement('div');
    probe.style.cssText = 'position:absolute;top:-9999px;left:0;width:1px;height:env(safe-area-inset-top, 0px)';
    document.body.appendChild(probe);
    const inset = probe.getBoundingClientRect().height;
    probe.remove();
    const cs = getComputedStyle(m);
    return {
      paddingTop: parseFloat(cs.paddingTop),
      // The un-inset half of the same shorthand — this element's own control
      // for "what is the masthead's vertical breathing room today".
      paddingBottom: parseFloat(cs.paddingBottom),
      inset,
      mastheadTop: m.getBoundingClientRect().top,
      brandTop: brand.getBoundingClientRect().top,
    };
  });
}

// (#2264) `viewport-fit=cover` (index.html) extends the standalone/PWA page
// under the iOS status bar, so `.masthead` must pad for
// `env(safe-area-inset-top)` — otherwise the wordmark/pill render UNDER the
// clock/battery. There is no notched device in CI, so this proves it the
// only way headless Chromium allows: `Emulation.setSafeAreaInsetsOverride`
// (verified against this harness's own Chromium build before writing the
// fix — it makes `env(safe-area-inset-top)` resolve to a real value) makes
// the inset real for the page, and the assertion reads the MASTHEAD's own
// computed padding + its brand's real screen position, not a CSS string.
//
// The base padding is READ, never hardcoded: `padding-bottom` is the
// un-inset half of the very same `padding: <v> <h>` shorthand the inset
// rule overrides on top (`padding: 8px 12px` at this breakpoint, `10px
// 16px` on the base rule), so it is this element's own control for what
// its vertical breathing room is today. The assertions therefore state the
// CONTRACT — `padding-top == base + inset` — and a design change to that
// breathing room moves both sides together instead of failing here with a
// message blaming the safe-area fix. (If a future rule ever sets
// `padding-bottom` independently, that control is gone and these need a
// different one.)
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

  const geo = await readMastheadGeometry(page);
  // The override actually reached the page: without this, an inset that
  // silently failed to apply would make the padding assertion below pass
  // vacuously at inset 0 — which is exactly the state the NEXT test is for.
  expect(geo.inset, `env(safe-area-inset-top) resolved to ${geo.inset}px — the CDP override never reached the page`).toBe(47);
  // `styles.css`'s <=768px rule: `padding-top: calc(8px + env(safe-area-inset-top, 0px))`,
  // asserted as base + inset off this element's own un-inset control.
  expect(geo.paddingTop, `masthead padding-top ${geo.paddingTop}px — want its ${geo.paddingBottom}px base + the ${geo.inset}px inset`).toBeCloseTo(geo.paddingBottom + geo.inset, 1);
  // The wordmark itself must clear the simulated status bar band, not just
  // the box around it — stated as the actual relationship (the brand sits
  // inside the masthead's padded content box, so it starts at or below that
  // box's top) rather than as `>= 47`, which the old assertion cleared by an
  // 8px accident of the base padding.
  expect(geo.brandTop, `brand top ${geo.brandTop}px — must start at or below the masthead's content box (${geo.mastheadTop + geo.paddingTop}px)`).toBeGreaterThanOrEqual(geo.mastheadTop + geo.paddingTop - 0.5);
});

// The SAME phone breakpoint, with NO inset simulated at all — every
// non-notched phone in the world (Android, an iPhone SE, a plain Safari tab
// on any of them): `env(safe-area-inset-top)` resolves to 0 and the fix must
// add nothing.
//
// This is a SEPARATE CSS declaration from the base rule the desktop test
// below covers, and it is the one that actually renders on a phone — so
// without this test the <=768px rule could hardcode the inset
// (`calc(8px + 47px)`, `env()` gone entirely) and BOTH other masthead tests
// would stay green while every non-notched phone got a permanent 47px dead
// band above the wordmark.
test('the masthead adds no phantom band on a phone with no safe-area inset', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await page.route('**/fleet/machines/live', (r) =>
    r.fulfill({ contentType: 'application/json', body: JSON.stringify({ machines: [], meta: { sources: { fleet: { state: 'ok' } }, complete: true } }) })
  );
  await page.route('**/fleet/sessions/live', (r) =>
    r.fulfill({ contentType: 'application/json', body: JSON.stringify({ sessions: [], meta: { sources: { fleet: { state: 'ok' } }, complete: true } }) })
  );

  await page.goto('/index-live.html');
  await page.waitForSelector('.masthead__brand', { timeout: 15_000 });

  const geo = await readMastheadGeometry(page);
  // Nothing overrides the inset here, so the page's own `env()` is 0 — the
  // premise of the whole test.
  expect(geo.inset, `env(safe-area-inset-top) resolved to ${geo.inset}px with no override — this test's premise is gone`).toBe(0);
  expect(geo.paddingTop, `masthead padding-top ${geo.paddingTop}px on an inset-free phone — want its plain ${geo.paddingBottom}px base, no band`).toBeCloseTo(geo.paddingBottom, 1);
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
  const geo = await readMastheadGeometry(page);
  expect(geo.inset, `env(safe-area-inset-top) resolved to ${geo.inset}px with no override — this test's premise is gone`).toBe(0);
  // `styles.css`'s base rule: `padding-top: calc(10px + env(safe-area-inset-top, 0px))`,
  // and no inset is simulated here — so it must equal its own un-inset base.
  expect(geo.paddingTop, `masthead padding-top ${geo.paddingTop}px on desktop — want its plain ${geo.paddingBottom}px base, no inset`).toBeCloseTo(geo.paddingBottom, 1);
});

// (#2264) The masthead is only HALF the status-bar band. It pads for the
// inset, but it SCROLLS AWAY — and what replaces it at the top of the
// viewport is `.app-shell__sticky`, the FLEET/CONSOLE/RUNS/MACHINE tab row,
// whose tabs are tap targets. `top: 0` on a sticky element IS the status-bar
// band under `viewport-fit=cover`, so before the fix a scrolled page parked
// the whole tab strip under the clock and the Dynamic Island.
//
// Both states are asserted, because the fix's whole risk is the first: `top`
// must move ONLY the stuck position, never open a gap at rest between the
// masthead (which already pads) and this row — that would be the double-pad.
//
// The page is `/index.html` (the static-playback harness) with a tall
// route-mocked panel body, because this assertion needs a page that actually
// SCROLLS at 390px — `scrollTo` on a short page is a silently vacuous test,
// so `scrollY` is asserted too.
const TALL_PANEL_ANSI = Array.from({ length: 200 }, (_, i) => `line ${i} of panel output`).join('\n') + '\n';

async function gotoScrollablePhonePage(page) {
  await page.route('**/panel/**', (r) =>
    r.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({
        panel: 'run-list', argv: ['run', 'list'], captured_ts_ms: Date.now(), gather_ms: 5,
        exit_code: 0, ansi_text: TALL_PANEL_ANSI, stderr_tail: '', cols: 100,
        cache_ttl_ms: 3000, age_ms: 0, auto_refresh: true,
      }),
    })
  );
  await page.goto('/index.html#lens=console');
  await page.waitForSelector('.app-shell__sticky', { timeout: 15_000 });
  await page.waitForSelector('.panelout', { timeout: 15_000 });
}

async function readStickyGeometry(page) {
  return page.evaluate(() => {
    const el = document.querySelector('.app-shell__sticky');
    const sticky = el.getBoundingClientRect();
    const masthead = document.querySelector('.masthead').getBoundingClientRect();
    const cs = getComputedStyle(el);
    const probe = document.createElement('div');
    probe.style.cssText = 'position:absolute;top:-9999px;left:0;width:1px;height:env(safe-area-inset-top, 0px)';
    document.body.appendChild(probe);
    const inset = probe.getBoundingClientRect().height;
    probe.remove();
    return {
      inset,
      stickyTop: sticky.top,
      // At rest the row's top and the masthead's bottom are the same edge —
      // any daylight here is a margin-shaped double-pad.
      gapUnderMasthead: sticky.top - masthead.bottom,
      // And the padding-shaped one, which the border-box measurement above
      // structurally CANNOT see (padding moves the row's content, not its top
      // edge): the same un-inset control the masthead tests use — this row's
      // `padding: 8px 16px` / `6px 12px` shorthand sets both, so an
      // inset-derived `padding-top` shows up as the two disagreeing. The
      // masthead already owns the inset; this row must never pad for it too.
      paddingTop: parseFloat(cs.paddingTop),
      paddingBottom: parseFloat(cs.paddingBottom),
      scrollY: window.scrollY,
    };
  });
}

test('the sticky tab row clears the iOS safe-area-inset-top once the page scrolls', async ({ page, context }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  const cdp = await context.newCDPSession(page);
  await cdp.send('Emulation.setSafeAreaInsetsOverride', {
    insets: { top: 47, right: 0, bottom: 34, left: 0 },
  });
  await gotoScrollablePhonePage(page);

  const rest = await readStickyGeometry(page);
  expect(rest.inset, `env(safe-area-inset-top) resolved to ${rest.inset}px — the CDP override never reached the page`).toBe(47);
  expect(rest.gapUnderMasthead, `a ${rest.gapUnderMasthead}px gap opened between the masthead and the tab row at rest — the sticky inset must move only the STUCK position`).toBeCloseTo(0, 1);
  expect(rest.paddingTop, `sticky row padding-top ${rest.paddingTop}px vs its ${rest.paddingBottom}px base — the row is padding for the inset the masthead already owns (a double-pad)`).toBeCloseTo(rest.paddingBottom, 1);

  await page.evaluate(() => window.scrollTo(0, 400));
  await page.waitForTimeout(200);
  const scrolled = await readStickyGeometry(page);
  // A short page would leave scrollY at 0 and make every assertion below
  // vacuously true against an unstuck row.
  expect(scrolled.scrollY, 'the page never scrolled — this test cannot say anything about the STUCK position').toBeGreaterThan(0);
  expect(scrolled.stickyTop, `sticky row stuck at ${scrolled.stickyTop}px — the ${scrolled.inset}px status-bar band must be above it, not on its tabs`).toBeCloseTo(scrolled.inset, 1);
});

// The same row on a phone with NO inset (Android, an iPhone SE, a plain
// Safari tab): it must still stick flush to the top of the viewport. The
// inset resolves to 0, so the fix is a no-op — nothing may be given away in
// vertical space on the devices that have no band to clear.
test('the sticky tab row still sticks flush to the top with no safe-area inset', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await gotoScrollablePhonePage(page);

  const rest = await readStickyGeometry(page);
  expect(rest.inset, `env(safe-area-inset-top) resolved to ${rest.inset}px with no override — this test's premise is gone`).toBe(0);
  expect(rest.gapUnderMasthead, `a ${rest.gapUnderMasthead}px gap opened between the masthead and the tab row at rest`).toBeCloseTo(0, 1);
  expect(rest.paddingTop, `sticky row padding-top ${rest.paddingTop}px vs its ${rest.paddingBottom}px base on an inset-free phone`).toBeCloseTo(rest.paddingBottom, 1);

  await page.evaluate(() => window.scrollTo(0, 400));
  await page.waitForTimeout(200);
  const scrolled = await readStickyGeometry(page);
  expect(scrolled.scrollY, 'the page never scrolled — this test cannot say anything about the STUCK position').toBeGreaterThan(0);
  expect(scrolled.stickyTop, `sticky row stuck at ${scrolled.stickyTop}px on an inset-free phone — want flush against the viewport top`).toBeCloseTo(0, 1);
});
