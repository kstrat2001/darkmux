// A daemon-less STATIC build must neither ASK a daemon for anything nor act
// as if it had (U5-1, U4-1) — both measured on the served demo, both
// invisible to every unit test in the tree.
//
//   U5-1: `#lens=fleet` on darkmux.com/demo fired `/fleet/machines/live`,
//         `/fleet/sessions/live` and `/machine/specs` — three 404s and their
//         console errors on a marketing page with no daemon anywhere near
//         it. `FleetLens` gated those on its own `historical` PROP, and
//         `App.tsx` renders `<FleetLens />` with no props at all, so the
//         prop's default (`false`) meant "live" on every static build. The
//         existing component test only ever passed `historical=true`, which
//         is exactly the case production never takes.
//
//   U4-1: on the same load the shared events column read "0 EVENTS" on
//         `#lens=runs`, `#lens=machine` and `#lens=console` while `#lens=fleet`
//         read "50 of 6092" — and clicking rewind made the count appear, so
//         the day was loaded the whole time. `#lens=fleet` resolves to the
//         PLAYBACK route on a static build and took the committed-file
//         branch; every explicitly-named lens kept its own route kind and
//         fell through to the live window, which is empty by construction
//         when there is no daemon to fill it.
//
// Both are properties of the COMPOSED app on a real static build — the shape
// this harness serves (`/index.html`: `next.html` + a `darkmux-flow-src`
// meta, the same injection `scripts/build-demo.sh` performs) — so this is
// where they can be pinned.
const { test, expect } = require('@playwright/test');

const DESKTOP = { width: 1456, height: 900 };

// Every endpoint that only a running daemon can answer, on the fleet screen.
const LIVE_ONLY = ['/fleet/machines/live', '/fleet/sessions/live', '/machine/specs'];

const LENSES = ['fleet', 'runs', 'machine', 'console'];

test.use({ viewport: DESKTOP });

test.beforeEach(async ({ page }) => {
  page.on('pageerror', (e) => { throw new Error(`uncaught page error: ${e}`); });
});

/** The events column's own count chip ("50 of 6092 events" / "0 events"). */
async function eventCountText(page) {
  return page.evaluate(() => {
    const el = [...document.querySelectorAll('*')].find(
      (e) => e.children.length === 0 && /^\d+\+? (of \d+\+? )?events?$/.test((e.textContent || '').trim()),
    );
    return el ? el.textContent.trim() : null;
  });
}

test('(U5-1) a static build asks a daemon for nothing on #lens=fleet', async ({ page }) => {
  const asked = [];
  const failed = [];
  const consoleErrors = [];
  page.on('request', (r) => {
    const u = new URL(r.url());
    if (LIVE_ONLY.includes(u.pathname)) asked.push(u.pathname);
  });
  page.on('response', (r) => { if (r.status() >= 400) failed.push(`${r.status()} ${r.url()}`); });
  page.on('console', (m) => { if (m.type() === 'error') consoleErrors.push(m.text()); });

  await page.goto('/index.html#lens=fleet');
  await expect(page.locator('.savrow')).toBeVisible();
  // The presence poll runs on a 5s interval when it runs at all — wait past
  // one tick so this cannot pass by being too quick to see the request.
  await page.waitForTimeout(6000);

  expect(asked, 'live-only endpoints requested on a daemon-less build').toEqual([]);
  expect(failed, 'failed requests on the demo').toEqual([]);
  expect(consoleErrors, 'console errors on the demo').toEqual([]);
});

test('(U4-1) every lens shows the same committed day in the events column, at rest', async ({ page }) => {
  const counts = {};
  for (const lens of LENSES) {
    await page.goto(`/index.html#lens=${lens}`);
    await expect(page.locator('.eventlog__rec').first()).toBeVisible();
    counts[lens] = await eventCountText(page);
  }
  // Non-zero everywhere…
  for (const lens of LENSES) {
    expect(counts[lens], `${lens} reported no events for a day the page had loaded`).not.toMatch(/^0 events$/);
  }
  // …and the SAME everywhere: the at-rest scope is the loaded day, and which
  // lens is showing does not change how much of it happened.
  const distinct = new Set(Object.values(counts));
  expect([...distinct], `per-lens counts disagreed: ${JSON.stringify(counts)}`).toHaveLength(1);
});
