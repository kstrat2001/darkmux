// (#1639) The session drill-down is a destination; destinations have URLs.
//
// This was the ONE level with no URL representation. Drilling in left
// `location.hash` untouched, so a reload — or a backgrounded PWA relaunch, or
// a tab reopened from history — silently dropped the operator back to the
// fleet hero with no error and no explanation. The documented `#session=<id>`
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
// The port's actual `#session=<id>` MECHANISM is real and independently
// verified: `route.ts` parses `session=` out of the hash, `App.tsx` renders
// `SessionReplay` for that route kind, and `hashSync.ts`'s `useSyncHash`
// writes the canonical `session=<id>` form back on every route change — the
// exact same `location.hash` write every other drill-in in this app uses
// (`FleetLens`'s machine cards, `NavChrome`'s tabs). What's genuinely
// UNBUILT is a CLICKABLE entry point into a session from anywhere in the
// UI — `MachineLens`'s run rows are plain `<div>`s with no session-drill
// link (removed with the machine page's runs list in #1809; the "open →" link legacy's
// `recentRow()` details panel carried is a named, deliberate cut, not
// ported yet). So three of the four tests below are rewritten to drive the
// SAME `location.hash` mechanism a future click affordance would use
// (documented per-test, not hidden), and the fourth — the one that is
// actually about clicking something to get IN — is `test.fixme`d with the
// gap named, not silently adapted around.
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

// (port gap, reported not papered over) There is currently no clickable
// affordance ANYWHERE in the port that enters a session view — the
// underlying mechanism (writing `location.hash = "session=<id>"`) is real,
// but nothing in the rendered UI does that write on the operator's behalf
// today. `MachineLens`'s run rows are the natural home for it (matching
// legacy's `data-act="session"` "open →" link) and don't have it yet — see
// this file's own module doc. Testing "drilling into a session writes it
// to the address bar" requires an actual drill action to exist; writing
// `location.hash` directly here would test the SAME mechanism the other
// three tests below already cover, under a name that claims to test a user
// interaction that doesn't exist in this build.
test.fixme('drilling into a session writes it to the address bar', async () => {});

test('booting on #session= restores the session view, not the fleet hero', async ({ page }) => {
  await mockSession(page, 'sess-in-flight');
  const errors = await bootLive(page, '#session=sess-in-flight');
  await expect(page.locator('.session-run')).toBeVisible();
  // The fleet hero's own marker must NOT be on the page — booting on a
  // session must not ALSO render fleet underneath/instead of it.
  await expect(page.locator('.fleet-lens')).toHaveCount(0);
  expect(await hash(page), 'the URL named a session; boot must honor it').toContain('session=sess-in-flight');
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
  const errors = await bootLive(page, '#session=sess-reload');
  await expect(page.locator('.session-run')).toBeVisible();

  await page.reload();
  await expect(page.locator('.session-run')).toBeVisible();
  expect(await hash(page), 'reload must not silently reset to the fleet hero').toContain('session=sess-reload');
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
  const errors = await bootLive(page, '#session=sess-leaving');
  await expect(page.locator('.session-run')).toBeVisible();
  expect(await hash(page)).toContain('session=sess-leaving');

  await page.click('#lens-runs');
  await expect.poll(() => hash(page)).not.toContain('sess-leaving');
  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});
