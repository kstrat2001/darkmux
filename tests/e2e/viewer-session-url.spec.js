// (#1639) The session drill-down is a destination; destinations have URLs.
//
// This was the ONE level with no URL representation. Drilling in left
// `location.hash` untouched, so a reload — or a backgrounded PWA relaunch, or
// a tab reopened from history — silently dropped the operator back to the
// fleet hero with no error and no explanation. The documented `#dispatch=<id>`
// deep link had the mirror problem: `catalogQuery()` parsed it, the daemon
// returned correctly-scoped records, and `boot()` rendered the fleet hero
// anyway because only the `mission` kind was ever honored.
//
// Both halves are asserted here, in the order an operator hits them: drill
// writes the URL, boot reads it back, and navigating away clears it.
//
// (port note, read before touching this file) Legacy drove all four tests
// through `window.drillSession`/`window.goRuns` (imperative globals) and
// read back `state.level`/`state.session` (a global mutable object) — none
// of which exist on `/next`: React holds this state internally, and the
// port exposes no page globals at all (by design, not oversight — see
// `App.tsx`'s module doc on operator sovereignty / no hidden state).
//
// The port's actual `#dispatch=<id>` MECHANISM is real and independently
// verified (#1974): `route.ts` parses `dispatch=` (and the one-release
// `session=` alias) out of the hash, `App.tsx` renders
// `SessionReplay` for that route kind, and `hashSync.ts`'s `useSyncHash`
// writes the canonical `dispatch=<id>` form back on every route change — the
// exact same `location.hash` write every other drill-in in this app uses
// (`FleetLens`'s machine cards, `NavChrome`'s tabs). Three of the four tests
// below drive that mechanism directly via a boot-time deep link; the fourth
// (below) now drives the CLICK affordance itself.
//
// (drill-in packet) `MachineLens`'s run rows are still plain `<div>`s with
// no session-drill link (removed with the machine page's runs list in
// #1809; the "open →" link legacy's `recentRow()` details panel carried is
// a named, deliberate cut, not ported back). The click affordance this
// packet builds instead lives on the fleet lens's own activity-lane bars
// (`FleetLens.tsx`'s `.sbar` elements, `data-act="session"`) — legacy's OWN
// timeline bars are inert (no click handler anywhere in that code), so this
// is a genuine, documented WIDENING beyond legacy's address-bar behavior,
// not a port of an existing legacy control. See `FleetLens.tsx`'s own doc
// on the bar's `onClick` for the full reasoning.
const { test, expect } = require('@playwright/test');

// A minimal, real-shaped `/flow-session/<id>` response — a clean
// dispatch.start -> dispatch.complete pair, same record shape
// `tests/fixtures/savings-flow.jsonl` uses elsewhere in this suite.
// `SessionReplay` needs enough here that `runRegions()` doesn't hit its
// empty-count branch (`count: 0` renders "no records found", never the
// real `.session-run` this spec drills for).
function sessionRecords(id) {
  return {
    records: [
      {
        ts: '2026-08-03T12:00:00Z', level: 'info', category: 'work', tier: 'local', stage: 'dispatch',
        source: 'crew_dispatch', machine_id: 'fixture-box', machine_uid: 'FIXTURE-UID',
        action: 'dispatch start', handle: 'coder', session_id: id, payload: { runtime: 'internal' },
      },
      {
        ts: '2026-08-03T12:00:02Z', level: 'info', category: 'work', tier: 'local', stage: 'dispatch',
        source: 'crew_dispatch', machine_id: 'fixture-box', machine_uid: 'FIXTURE-UID',
        action: 'dispatch complete', handle: 'coder', session_id: id,
        payload: { runtime: 'internal', result_class: 'ok', total_turns: 1, total_tools: 0, total_tokens: 100 },
      },
    ],
    count: 2,
    truncated: false,
    generated_at_ms: 0,
  };
}

async function mockSession(page, id) {
  await page.route(`**/flow-session/${encodeURIComponent(id)}`, (r) =>
    r.fulfill({ contentType: 'application/json', body: JSON.stringify(sessionRecords(id)) })
  );
}

// Boot-time deep links need the DAEMON shape: `boot()`/`route.ts` only
// reaches the `session` route kind for a page with no static flow-src
// pinned (see `route.ts`'s own doc). `index-live.html` carries no
// flow-src (playwright.config.js) and its fetches are stubbed here.
async function bootLive(page, hash) {
  const errors = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  await page.route(/\/flow\/\d{4}-\d{2}-\d{2}(\?.*)?$/, (r) =>
    r.fulfill({ contentType: 'application/json', body: '[]' })
  );
  await page.route(/\/flow\/\d{4}-\d{2}-\d{2}\/stream(\?.*)?$/, (r) =>
    r.fulfill({ contentType: 'text/event-stream', body: '' })
  );
  await page.route('**/runs*', (r) =>
    r.fulfill({ contentType: 'application/json', body: '{"generated_at_ms":0,"runs":[]}' })
  );
  await page.goto(`/index-live.html${hash || ''}`);
  await page.waitForSelector('.app-shell');
  return errors;
}

const hash = (page) => page.evaluate(() => location.hash);

// Boots the live fleet hero with ONE session on ONE machine, recent enough
// (5 minutes ago) to land inside the 24h activity-timeline window
// (`timeline.ts`'s `LIVE_WINDOW_MS`/default preset) — enough for a real
// `.sbar` bar to render, matching this port's own machine-uid/session-id
// field shapes (`machine_uid`/`session_id`, the `dispatch start`
// space-spelling `normalizeAction` dots). `mockSession` (above) covers the
// `/flow-session/<id>` fetch the click's destination needs.
async function bootLiveWithSession(page, sid) {
  const errors = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  // A CLOSED session (start, then complete a minute later) rather than a
  // still-open one — deliberately: an open bar's right edge sits at
  // `pct(tMax)`, the SAME x-position `timeline.ts` computes for the
  // playhead (`playheadPct: pct(tMax)`), so an open bar's own tip is
  // exactly where `.ph` renders — the click lands on the playhead div, not
  // the bar, on any date/window where "now" is close to the record's own
  // timestamp (i.e. every real run of this test). A closed bar's right
  // edge sits at its OWN close time, clearly left of the live playhead.
  const startTs = new Date(Date.now() - 10 * 60_000).toISOString();
  const completeTs = new Date(Date.now() - 9 * 60_000).toISOString();
  // A trailing, session-less record a minute ago — later than the bar's own
  // close. Without this, the bar's own `dispatch.complete` would ALSO be the
  // window's single latest record, and `timeline.ts` positions the playhead
  // at `pct(tMax)` where `tMax` is that SAME latest-record timestamp — so
  // the bar's right edge and the playhead would coincide again, for the
  // identical reason a still-open bar's edge does (see the comment above).
  const laterTs = new Date(Date.now() - 60_000).toISOString();
  const records = [
    {
      ts: startTs, level: 'info', category: 'work', tier: 'local', stage: 'dispatch',
      source: 'crew_dispatch', machine_id: 'fixture-box', machine_uid: 'FIXTURE-UID',
      action: 'dispatch start', handle: 'coder', session_id: sid, payload: { runtime: 'internal' },
    },
    {
      ts: completeTs, level: 'info', category: 'work', tier: 'local', stage: 'dispatch',
      source: 'crew_dispatch', machine_id: 'fixture-box', machine_uid: 'FIXTURE-UID',
      action: 'dispatch complete', handle: 'coder', session_id: sid,
      payload: { runtime: 'internal', result_class: 'ok', total_turns: 1, total_tools: 0, total_tokens: 100 },
    },
    {
      ts: laterTs, level: 'info', category: 'machinery', tier: 'local', stage: 'dispatch',
      source: 'presence-reconciler', machine_id: 'fixture-box', machine_uid: 'FIXTURE-UID', action: 'machine.online',
    },
  ];
  await page.route(/\/flow\/\d{4}-\d{2}-\d{2}(\?.*)?$/, (r) =>
    r.fulfill({ contentType: 'application/json', body: JSON.stringify(records) })
  );
  await page.route(/\/flow\/\d{4}-\d{2}-\d{2}\/stream(\?.*)?$/, (r) =>
    r.fulfill({ contentType: 'text/event-stream', body: '' })
  );
  await mockSession(page, sid);
  await page.goto('/index-live.html');
  await page.waitForSelector('.app-shell');
  return errors;
}

test('drilling into a session writes it to the address bar', async ({ page }) => {
  const sid = 'sess-clickable';
  const errors = await bootLiveWithSession(page, sid);

  const bar = page.locator(`.sbar[title*="${sid}"]`);
  await expect(bar).toBeVisible();
  await bar.click();

  await expect(page.locator('.session-run')).toBeVisible();
  expect(await hash(page), 'clicking the activity-lane bar must write #dispatch=<id>').toContain(`dispatch=${sid}`);
  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});

test('booting on the LEGACY #session= alias restores the dispatch view AND rewrites the URL to #dispatch= (#1974)', async ({ page }) => {
  // Deliberately boots on the OLD spelling. This is the browser-level proof
  // that the one-release alias in `route.ts` actually resolves for a real
  // bookmark, and that `useSyncHash`'s write-back then rewrites the address
  // bar to the canonical form — the two halves that together make the alias
  // temporary rather than permanent. The unit tests cover each half in
  // isolation (`route.test.ts`, `hashSync.test.ts`); only this one proves
  // they compose across a real boot.
  await mockSession(page, 'sess-in-flight');
  const errors = await bootLive(page, '#session=sess-in-flight');
  await expect(page.locator('.session-run')).toBeVisible();
  // The fleet hero's own marker must NOT be on the page — booting on a
  // dispatch must not ALSO render fleet underneath/instead of it.
  await expect(page.locator('.fleet-lens')).toHaveCount(0);
  await expect
    .poll(() => hash(page), { message: 'a legacy #session= bookmark must be honored AND rewritten to #dispatch=' })
    .toContain('dispatch=sess-in-flight');
  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});

test('a drill survives a reload — the whole point', async ({ page }) => {
  // The end-to-end path the issue describes: land on a session, Cmd+R,
  // still there. Asserted as one flow rather than trusting a boot-only
  // check implies a reload behaves the same way — `useSyncHash`'s
  // `replaceState` write and `useHashRoute`'s `hashchange`-driven read are
  // different code paths, and nothing else pins that a value written by
  // one is read back correctly by the other after a real navigation event
  // (not just a same-document hash mutation).
  await mockSession(page, 'sess-reload');
  const errors = await bootLive(page, '#dispatch=sess-reload');
  await expect(page.locator('.session-run')).toBeVisible();

  await page.reload();
  await expect(page.locator('.session-run')).toBeVisible();
  expect(await hash(page), 'reload must not silently reset to the fleet hero').toContain('dispatch=sess-reload');
  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});

test('navigating up out of a session clears it from the URL', async ({ page }) => {
  // Without this the id lingers, and the NEXT reload drops the operator
  // back into a view they deliberately left — the same failure, inverted.
  // No "leave a session" click affordance exists either (same gap named
  // above), so this drives the one real, always-present way to change
  // route while sitting on a session view: a `NavChrome` tab click — the
  // exact mechanism `useSyncHash`'s write-back is built to react to.
  await mockSession(page, 'sess-leaving');
  const errors = await bootLive(page, '#dispatch=sess-leaving');
  await expect(page.locator('.session-run')).toBeVisible();
  expect(await hash(page)).toContain('dispatch=sess-leaving');

  await page.click('#lens-runs');
  await expect.poll(() => hash(page)).not.toContain('sess-leaving');
  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});
