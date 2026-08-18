// Headless e2e smoke for the #1584 runs lens (which absorbed the #1247 Part 3
// lab observer lens). The static XSS harness has no daemon behind it, so
// `/runs` and `/lab/runs` aren't reachable there — these specs use the
// `index-lab.html` variant (playwright.config.js) which injects
// `darkmux-runs-src` + `darkmux-lab-runs-src` pointing at committed fixtures
// (tests/fixtures/runs-fixture.json, tests/fixtures/lab-runs-fixture.json),
// the same static-fixture-override pattern the missions/phases lens uses. The
// run-DETAIL endpoints (`/lab/run/detail` + `/lab/run/events`) have no meta
// override, so specs that drill into a run route-mock them (the catalog.spec
// pattern).
//
// Every `viewer.html:NNNN` citation in this file (retired #1806) points at
// the legacy file's last revision, recoverable with
// `git show v2.9.0:crates/darkmux-serve/assets/viewer.html` — not a file
// present anywhere in the current tree.
const { test, expect } = require('@playwright/test');

// Minimal-but-real-shaped mocks for one run's detail + events.
function mockRunDetail(page) {
  return Promise.all([
    page.route('**/lab/run/detail*', (r) =>
      r.fulfill({
        contentType: 'application/json',
        body: JSON.stringify({
          dir: 'demo-case/run2',
          funnels: [{
            case_id: 'demo-case-a', crew: 'demo-crew', mode: 'sequential',
            members: [], steps: [], bundles: 12, raw_flags: 18, deduped_flags: 14,
            flags: [], judged: [], confirmed: 5, needs_check: 2, archived: 7,
            fingerprint: {},
          }],
          scores: null,
        }),
      })
    ),
    page.route('**/lab/run/events*', (r) =>
      r.fulfill({
        contentType: 'application/json',
        body: JSON.stringify({
          lines: [{
            ts: '2026-01-01T00:00:00Z', level: 'info', category: 'work',
            tier: 'local', stage: 'dispatch', action: 'step result',
            handle: 'bundle', session_id: 'demo-case-a', source: 'review',
            payload: { step_id: 'bundle', kind: 'review.bundle', items_out: 12 },
          }],
          next_offset: 100,
          finished: true,
        }),
      })
    ),
  ]);
}

test('runs lens renders every kind in one flat list, newest first', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));

  await page.goto('/index-lab.html');
  await page.waitForSelector('#lens-runs');

  await page.click('#lens-runs');
  await expect(page.locator('#lens-runs')).toHaveClass(/\bon\b/);
  await expect(page.locator('#lens-fleet')).not.toHaveClass(/\bon\b/);
  // (#1247 deep-link) Lens navigation reflects into the address bar.
  await expect.poll(() => page.evaluate(() => location.hash)).toContain('lens=runs');

  // All six fixture runs, from all three sources, in ONE list — the point of
  // the lens: "what ran recently" without knowing which subsystem recorded it.
  await expect(page.locator('.labrunrow')).toHaveCount(6);
  await expect(page.locator('.runkind.lab')).toHaveCount(3);
  await expect(page.locator('.runkind.mission')).toHaveCount(1);
  await expect(page.locator('.runkind.dispatch')).toHaveCount(2);

  // Strictly newest-activity-first, with NO hoisting of `running` rows — a
  // lab run killed before writing scores.json stays `running` forever, so
  // hoisting would bury today's work under long-dead runs.
  const ids = await page.locator('.labrunrow .labruncrew').allTextContents();
  expect(ids).toEqual([
    'demo-live/gate',            // updated 1893456500 (running, and genuinely newest)
    'demo-case/run2',            // 1893456000
    'demo-case/run1',            // 1893452400
    'demo-mission',              // 1893451000
    'dispatch-demo-reviewer-1',  // 1893440600
    'ghost-session-abc',         // 1893430000 (running, but oldest -> last)
  ]);

  // The `running` badge never animates here: it means "no terminal artifact",
  // not "live right now". The relative time carries that distinction.
  await expect(page.locator('.labrunrow .labbadge.live')).toHaveCount(0);

  // An untracked ghost has no durable record to open, so its row offers no
  // drill-down rather than linking to a 404.
  const ghost = page.locator('.labrunrow', { hasText: 'ghost-session-abc' });
  await expect(ghost).toHaveClass(/\bflat\b/);
  await expect(ghost).toContainText('untracked');

  expect(pageErrors, `uncaught page errors: ${pageErrors.join(' | ')}`).toEqual([]);
});

test('kind chips filter the one list rather than navigating', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));

  await page.goto('/index-lab.html#lens=runs');
  await expect(page.locator('.labrunrow')).toHaveCount(6);

  await page.click('.runchip[data-arg="lab"]');
  await expect(page.locator('.labrunrow')).toHaveCount(3);
  await expect(page.locator('.runkind.mission')).toHaveCount(0);
  // The lens never changes — the filter is addressable state on it.
  await expect(page.locator('#lens-runs')).toHaveClass(/\bon\b/);
  await expect.poll(() => page.evaluate(() => location.hash)).toContain('kind=lab');

  await page.click('.runchip[data-arg="all"]');
  await expect(page.locator('.labrunrow')).toHaveCount(6);
  await expect.poll(() => page.evaluate(() => location.hash)).not.toContain('kind=');

  expect(pageErrors, `uncaught page errors: ${pageErrors.join(' | ')}`).toEqual([]);
});

test('the lab series view stays reachable, under the lab filter only', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));

  await page.goto('/index-lab.html#lens=runs');
  // The series toggle is lab-specific (it diffs recorded staffing snapshots),
  // so it does not exist while the list spans kinds.
  await expect(page.locator('[data-act="runsseries"]')).toHaveCount(0);

  await page.click('.runchip[data-arg="lab"]');
  await page.click('[data-act="runsseries"]');

  // Two task cards: the fixture's two `demo-case-a` runs group into one
  // series; the single `demo-case-b` live run is its own card.
  const cards = page.locator('.labtaskcard');
  await expect(cards).toHaveCount(2);

  const seriesCard = page.locator('.labtaskcard', { hasText: 'demo-case-a' });
  await expect(seriesCard.locator('.labrunrow')).toHaveCount(2);
  // Only the newer run gets a diff line (compared against the older one);
  // the knob diff between the fixture's two runs (probe k 1→2) renders as a
  // plain (single-variable) diff line, not the multi-variable warning.
  await expect(seriesCard.locator('.labdiffline')).toHaveCount(1);
  await expect(seriesCard.locator('.labdiffline')).toContainText('demo-probe.k 1→2');
  await expect(seriesCard.locator('.labdiffline.warn')).toHaveCount(0);

  const liveCard = page.locator('.labtaskcard', { hasText: 'demo-case-b' });
  await expect(liveCard).toContainText('staffing pending');

  expect(pageErrors, `uncaught page errors: ${pageErrors.join(' | ')}`).toEqual([]);
});

// (port note) `labKnobDiff surfaces a judge seat added or removed between
// runs` — REMOVED, not ported, and this is the finding, same shape as the
// LAB_FEED_CAP note below.
//
// The original drove `page.evaluate(() => labKnobDiff(...))` against a
// legacy GLOBAL function (viewer.html). The port has no page globals at
// all — `labKnobDiff` is a module-scoped export
// (`ui/src/lenses/lab/labSeries.ts`), unreachable from `page.evaluate` by
// design (React state/logic doesn't leak onto `window`), so this spec
// cannot work as written against `/next`.
//
// Real replacement coverage already exists, and it's a better home for
// this check than a browser ever was: `labSeries.test.ts`'s own
// `describe("labKnobDiff")` block, specifically `"reports a judge seat
// appearing or disappearing"` — asserting the EXACT scenario this e2e test
// did (`+judge (m)` / `-judge`), plus `"reports NO judge change when
// neither side has one"`, a case this e2e version never covered. It's a
// pure function of two plain objects; unit-testing it directly is strictly
// MORE precise than driving a browser to reach the same call, not a
// downgrade — see `ui/src/lenses/lab/labSeries.test.ts`.

test('deep link #lens=runs boots directly into the runs lens', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));

  await page.goto('/index-lab.html#lens=runs');
  // No tab click — boot itself must land in the runs lens.
  await expect(page.locator('#lens-runs')).toHaveClass(/\bon\b/);
  await expect(page.locator('.labrunrow')).toHaveCount(6);

  expect(pageErrors, `uncaught page errors: ${pageErrors.join(' | ')}`).toEqual([]);
});

test('a legacy #lens=lab bookmark still resolves, pre-filtered to Lab', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));

  // (#1584) Every bookmark and phone shortcut minted while the lab tab
  // existed must keep working — it lands on the same set of runs the old tab
  // showed, and the address bar is upgraded to the current spelling so what
  // the operator re-copies is the current form.
  await page.goto('/index-lab.html#lens=lab');
  await expect(page.locator('#lens-runs')).toHaveClass(/\bon\b/);
  await expect(page.locator('.labrunrow')).toHaveCount(3);
  await expect(page.locator('.runkind.lab')).toHaveCount(3);
  await expect.poll(() => page.evaluate(() => location.hash)).toContain('lens=runs');
  await expect.poll(() => page.evaluate(() => location.hash)).toContain('kind=lab');

  expect(pageErrors, `uncaught page errors: ${pageErrors.join(' | ')}`).toEqual([]);
});

test('deep link #lens=runs&run=<dir> boots into that run detail', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));
  await mockRunDetail(page);

  await page.goto('/index-lab.html#lens=runs&run=demo-case%2Frun2');
  await expect(page.locator('#lens-runs')).toHaveClass(/\bon\b/);
  await expect(page.locator('.labpipe .labstage').first()).toBeVisible();
  await expect(page.locator('#crumb')).toContainText('demo-case/run2');

  expect(pageErrors, `uncaught page errors: ${pageErrors.join(' | ')}`).toEqual([]);
});

test('drilling a lab row from the list opens its detail and updates the hash', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));
  await mockRunDetail(page);

  await page.goto('/index-lab.html#lens=runs');
  await page.locator('.labrunrow', { hasText: 'demo-case/run2' }).click();
  await expect(page.locator('#crumb')).toContainText('demo-case');
  await expect(page.locator('.labpipe .labstage').first()).toBeVisible();
  await expect.poll(() => page.evaluate(() => location.hash)).toContain('run=demo-case%2Frun2');

  // Navigating back to fleet clears the runs params from the hash.
  await page.click('#lens-fleet');
  await expect.poll(() => page.evaluate(() => location.hash)).not.toContain('lens=runs');
  await expect.poll(() => page.evaluate(() => location.hash)).not.toContain('run=');

  expect(pageErrors, `uncaught page errors: ${pageErrors.join(' | ')}`).toEqual([]);
});

// (drill-in packet) `LabRunDetail` now reports an unresolvable dir back to
// `RunsBoard` (`onUnresolvable`, `LabRunDetail.tsx`'s own doc) instead of
// staying on its own inline `data-state="error"` page forever — the port of
// legacy's `drillLabRun` fallback (`viewer.html:4101-4126`:
// `state.level="runs"; state.labRunDir=null;` plus a notice). The daemon-
// vs-static message split is legacy's own `missionGraphReachable()` branch,
// ported verbatim in `RunsBoard.tsx`'s `onLabRunUnresolvable` — this harness
// is a static-flow-src build (no daemon), so it takes the "needs a running
// daemon" arm, matching legacy's own reasoning for why "may have been
// removed" would be a dishonest message here.
test('deep link with an unresolvable run falls back to the run list with a notice', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));
  // Detail endpoint rejects (the daemon 400s a bad/out-of-bounds dir; the
  // static harness would 404 — same fallback path either way).
  await page.route('**/lab/run/detail*', (r) => r.fulfill({ status: 400, body: 'bad dir' }));

  await page.goto('/index-lab.html#lens=runs&run=no-such-run');
  await expect(page.locator('#lens-runs')).toHaveClass(/\bon\b/);
  // Falls back to the run LIST — never a stuck "loading…" pane polling a
  // failing request forever. That is the load-bearing behavior and it holds
  // in every context.
  await expect(page.locator('.labrunrow')).toHaveCount(6);
  // (#1584) This harness is a STATIC build with no daemon behind it, so the
  // honest reason is the missing capability, not a stale link. The
  // "may have been removed" wording is the WITH-daemon branch and is
  // unreachable here by construction — the predicate is
  // `missionGraphReachable()`, deliberately the same one the mission graph
  // uses, because `/play/<date>` is daemon-served playback where run detail
  // resolves fine and must keep the stale-link explanation.
  await expect(page.locator('.labnotice')).toContainText('needs a running daemon');
  await expect(page.locator('.labnotice')).not.toContainText('may have been removed');

  expect(pageErrors, `uncaught page errors: ${pageErrors.join(' | ')}`).toEqual([]);
});

// (#1584) Structural XSS tripwire for the runs lens. `viewer-xss.spec.js`
// injects hostile *flow records*; `/runs` is a separate wire shape reaching a
// separate render path (renderRunRow / runSubtitle / runStatusBadge), so it
// needs its own hostile-payload walk or the escaping is only ever verified by
// hand. Every attacker-controlled string field carries one of the three
// payload shapes that spec uses: an HTML-injection, a JS-string breakout, and
// a double-quoted-attribute breakout.
const XSS_HTML = '<img src=x onerror=window.__xss=1>';
const XSS_ATTR = '" onmouseover=window.__xss=1 x="';
const XSS_JS = "'); window.__xss=1;//";

const HOSTILE_RUNS = {
  generated_at_ms: 1893456600000,
  runs: [
    {
      id: XSS_HTML, kind: 'lab', status: 'running',
      machine: XSS_ATTR, role: XSS_JS, updated_ts: 1893456500, tracked: true,
    },
    {
      id: XSS_ATTR, kind: 'mission', status: 'complete',
      machine: XSS_HTML, model: XSS_HTML, route: XSS_ATTR,
      completed_ts: 1893456000, updated_ts: 1893456000, tracked: true,
    },
    {
      // Untracked -> the non-clickable branch, which assembles attributes
      // differently and so must be walked separately.
      id: XSS_JS, kind: 'dispatch', status: 'error',
      machine: XSS_HTML, role: XSS_ATTR, model: XSS_JS,
      updated_ts: 1893450000, tracked: false,
    },
  ],
};

async function assertRunsInert(page, where) {
  const fired = await page.evaluate(() => window.__xss);
  expect(fired, `XSS canary fired at: ${where}`).toBeUndefined();
  const injected = await page.evaluate(
    () => document.querySelectorAll('img[src="x"], img[onerror], [onmouseover]').length
  );
  expect(injected, `injected element rendered at: ${where}`).toBe(0);
}

test('runs lens renders attacker-controlled /runs rows inertly', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));
  await page.route('**/runs-fixture.json', (r) =>
    r.fulfill({ contentType: 'application/json', body: JSON.stringify(HOSTILE_RUNS) })
  );

  await page.goto('/index-lab.html#lens=runs');
  await page.waitForSelector('.labrunrow');
  await expect(page.locator('.labrunrow')).toHaveCount(3);
  await assertRunsInert(page, 'runs list (all kinds)');

  // Two distinct machines in the payload, so the machine column renders —
  // otherwise the hostile `machine` field would never reach the DOM at all
  // and this walk would pass vacuously.
  await expect(page.locator('.labrunmeta').first()).toBeVisible();

  // Each kind filter re-renders the same rows through the same path; the
  // per-kind walk catches an escape that only one branch misses.
  for (const kind of ['lab', 'mission', 'dispatch']) {
    await page.click(`.runchip[data-arg="${kind}"]`);
    await page.waitForTimeout(100);
    await assertRunsInert(page, `runs list (kind=${kind})`);
  }

  // The hostile id also rides the hash on a drill, and the chip counts + the
  // header re-render on the way back.
  await page.click('.runchip[data-arg="all"]');
  await assertRunsInert(page, 'runs list (back to all)');

  expect(pageErrors, `uncaught page errors: ${pageErrors.join(' | ')}`).toEqual([]);
});

test('an empty lab slice explains WHERE it scanned, and only when empty', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));

  // (#1584/#1585) Deliberately NOT the pre-#1585 "restart with --lab-dir"
  // guidance: `lab_dir` has a config tier and a default now, so `configured:
  // false` can no longer occur on a real daemon and naming it would send the
  // operator after a setting that is already set. The actionable distinction
  // is whether the resolved dir EXISTS, which only the daemon can report.
  await page.route('**/runs-fixture.json', (r) =>
    r.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({ runs: [
        { id: 'm1', kind: 'mission', status: 'complete', updated_ts: 1893456000, tracked: true },
      ] }),
    })
  );
  await page.route('**/lab-runs-fixture.json', (r) =>
    r.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({ configured: true, dir: '/tmp/nope/runs', exists: false, runs: [] }),
    })
  );

  await page.goto('/index-lab.html#lens=runs');
  // A healthy board never carries an explanation of a problem it doesn't
  // have — the notice is scoped to the lab filter, and to it being empty.
  await expect(page.locator('.labnotice')).toHaveCount(0);

  await page.click('.runchip[data-arg="lab"]');
  await expect(page.locator('.labrunrow')).toHaveCount(0);
  await expect(page.locator('.labnotice')).toContainText('does not exist yet');
  await expect(page.locator('.labnotice')).toContainText('/tmp/nope/runs');

  expect(pageErrors, `uncaught page errors: ${pageErrors.join(' | ')}`).toEqual([]);
});

// (#1640) NOT TESTED HERE, deliberately, and this is the finding.
//
// The lab event feed's cap disclosure cannot be reached in this harness: the
// feed lives behind a lab-run drill-down that needs a real `/lab/run/detail`
// selection, and `.labfeedhdr` renders zero times from a route-mock alone. I
// wrote the test, probed it, and it was vacuous — an `if (await hdr.count())`
// that never entered its body while reporting green.
//
// Removing it rather than shipping a seventh test that passes for a reason it
// does not name. The fix itself (a named `LAB_FEED_CAP` and a "newest N of M"
// header) IS ported — `ui/src/lenses/runs/labRun.ts`'s `LAB_FEED_CAP` +
// `labFeedCountText`, unit-tested directly in `labRun.test.ts` — this file's
// gap is narrower than "untested": it's specifically that no E2E harness here
// can drive the real drill-down interaction to REACH that header live. The
// legacy `viewer.html` this comment originally pointed at is retired
// (#1806); its own copy of the same fix is recoverable with
// `git show v2.9.0:crates/darkmux-serve/assets/viewer.html` if the history is
// wanted, but the current, reviewable source is the port file named above.
// The harness work needed to exercise the drill-down is tracked with the
// other fixture gaps.
