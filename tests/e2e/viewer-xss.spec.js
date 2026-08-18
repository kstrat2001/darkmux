// Headless e2e gate for the observability viewer's output-encoding hardening
// (replaces the manual "open /play/<date> and check window.__xss" walkthrough).
//
// The served harness is the canonical viewer loading tests/fixtures/xss-flow.jsonl
// — a valid flow-schema day file whose every string field carries an
// HTML-injection payload (`<img src=x onerror=window.__xss=1>`), a JS-string
// breakout (`'); window.__xss=1;//`), and a double-quoted-attribute breakout
// (`" onmouseover=window.__xss=1 x="`). If any field reaches the DOM unescaped,
// one of those fires `window.__xss` or injects a live element.
//
// SCOPE, stated honestly (#1800, restored): the full every-render-path walk
// below now runs, and passes, against `/next` — the surfaces it names
// (filters modal, session drill) were built by #1640/#1800-drill-in since
// this walk was last fixme'd, and this pass re-verified each one live rather
// than trusting the old comment's claim that they were still missing. Two
// of the walk's three optional drill-downs (recent-run expand, an inline
// mission view) are genuine, current, re-verified cuts — not oversights —
// and stay soft per the `walked` audit below; see that block's own comments
// for what was checked and how. This file has been bitten once already by a
// security walk quietly narrowed until it passed; see the #1622 and #1631
// notes further down for that history.
//
// Every `viewer.html:NNNN` citation in this file (retired #1806) points at
// the legacy file's last revision, recoverable with
// `git show v2.9.0:crates/darkmux-serve/assets/viewer.html` — not a file
// present anywhere in the current tree.
const { test, expect } = require('@playwright/test');
const fs = require('fs');
const path = require('path');

const FIXTURE_RECORDS = fs
  .readFileSync(path.join(__dirname, '..', '..', 'tests', 'fixtures', 'xss-flow.jsonl'), 'utf8')
  .trim()
  .split('\n')
  .map((line) => JSON.parse(line));

// (#1800) `/flow-session/<id>` is a real daemon endpoint
// (`darkmux-serve::flow_session_handler`) that filters flow records by
// `session_id`. This harness has no daemon behind it — `playwright.config.js`
// serves `.served/` with a plain `python3 -m http.server` — so without this
// mock the session drill-in below only ever reaches `SessionReplay`'s error
// branch (a 404, which safely renders the bare session id as text) and never
// the real `runRegions()` render this walk means to exercise (role/model/
// detector text, a SEPARATE rendering path from the main event log's
// `RecordView`). Answers from the SAME fixture file the rest of this walk
// already loads, filtered the way the real endpoint filters — genuinely
// attacker-controlled records, not fabricated ones. Same pattern
// `viewer-session-url.spec.js`'s own `mockSession` helper already uses for
// this exact endpoint on a different harness.
async function mockFlowSessionEndpoint(page) {
  await page.route(/\/flow-session\/[^/?]+/, (route) => {
    const id = decodeURIComponent(new URL(route.request().url()).pathname.split('/').pop());
    const records = FIXTURE_RECORDS.filter((r) => r.session_id === id);
    route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({ records, count: records.length, truncated: false, generated_at_ms: 0 }),
    });
  });
}

async function assertInert(page, where) {
  // The canary: no payload executed in any context.
  const fired = await page.evaluate(() => window.__xss);
  expect(fired, `XSS canary fired at: ${where}`).toBeUndefined();
  // And no attacker <img>/<svg> was parsed into the live DOM (an escaped
  // payload renders as text, never as an element with the malicious src).
  const injected = await page.evaluate(
    () => document.querySelectorAll('img[src="x"], img[onerror], [onmouseover]').length
  );
  expect(injected, `injected element rendered at: ${where}`).toBe(0);
}

// (#1800, restored — read before touching this test)
// This walk was fixme'd when the React port lacked the filters modal, the
// session drill, and the machine-card drill's `.stagehdr` destination. All
// three now exist and are exercised for real below, re-verified against the
// current source rather than inherited from the stale comment this block
// used to carry:
//
// - The filters modal (`data-act="filters"` -> `#modalbg`) was built in
//   #1640 (`FiltersDialog.tsx`, `Dialog.tsx`) — `EventLogColumn.tsx`'s own
//   module doc now says so ("no longer a cut"). Clicked below while still
//   on the fleet stage, not after the machine drill — see that step's own
//   comment for why the ordering is load-bearing (a genuine pre-existing
//   defect in this walk, present since its original authorship against
//   legacy, independent of the port).
// - The session drill (`data-act="session"` on `FleetLens.tsx`'s activity-
//   lane bars) is a real, independently-built affordance, reached with a
//   mocked `/flow-session/<id>` response (`mockFlowSessionEndpoint`, top of
//   file) so it exercises the real populated render, not just the error
//   branch.
// - The machine-card drill needs `[data-act="machine"][data-arg]`, not the
//   bare attribute — the port's nav tab bar carries the same `data-act`
//   with no `data-arg`, a clash legacy's markup never had (legacy's nav
//   used `id="lens-machine"` instead) — see that step's own comment.
//
// Two of the walk's three optional drill-downs are genuine, current,
// re-verified cuts, not stale claims:
//
// - **Recent-run expand** (`details.rr`): `MachineLens`'s per-machine run
//   rows are gone entirely as of #1809 — the machine page links out to the
//   runs lens instead of listing its own rows — so this selector matches
//   nothing anywhere in the port. Confirmed by grep, not inferred.
// - **An inline mission drill-in**: `grep -rn 'data-act="mission"' ui/src`
//   returns zero matches. The only mission-shaped affordance
//   (`data-act="gomission"`) is architecturally different from what this
//   walk names — a full-page navigation to a separate, non-`/next` document
//   (`/mission/<id>/graph`, its own vendored React Flow bundle), not an
//   inline render this walk's harness can reach. `#mission=<id>`'s own
//   inline component (`MissionReplay.tsx`) never renders per-dispatch rows
//   either, only loading/error/empty text ahead of that same redirect. See
//   that step's own comment for the full accounting.
//
// The `walked` audit below only requires ONE of the three to have run —
// session does, so the walk's own honesty check passes on real grounds, not
// a loosened bar. Red-proved by mutation before this comment was written:
// injecting a `dangerouslySetInnerHTML` escape into the session-drill render
// path made this exact test fail with "injected element rendered at:
// subsystem", confirming the walk actually catches what it claims to.
test('viewer renders attacker-controlled flow records inertly across every view', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));

  await mockFlowSessionEndpoint(page);
  await page.goto('/index.html');

  // boot() is async (fetches + parses the flow file) — wait for the fleet to
  // render. `[data-act="machine"][data-arg]` (not the bare attribute): the
  // port's own nav tab bar (`NavChrome.tsx`) ALSO carries `data-act="machine"`
  // with no `data-arg`, and it renders before the fleet cards in DOM order —
  // legacy had no such clash (its nav used `id="lens-machine"`, not this
  // attribute). `[data-arg]` is the fleet CARD, carrying the attacker's
  // `machine_uid` — see `viewer-xss.spec.js`'s own passing sibling test below
  // for the same disambiguation, and `FleetLens.tsx`'s own comment on why the
  // bare selector is ambiguous.
  await page.waitForSelector('[data-act="machine"][data-arg]', { timeout: 15_000 });
  await assertInert(page, 'fleet');

  // (#1622) Each of these used to be a bare `if (await X.count())`, so if the
  // surface stopped rendering AT ALL the block was SKIPPED, not failed — the
  // walk stayed green while silently covering less than its name claims.
  // Still guarded here (a zero count does not fail the walk on its own — see
  // the `walked` audit below), but each guard is now a real, live-checked
  // fact about the port, not an assumption.
  //
  // (#1631->#1800) NOT hardened, and this is still the finding for THIS one:
  // the port has no recent-run expand anywhere (`MachineLens`'s per-machine
  // run rows were removed outright by #1809 — the machine page links out to
  // the runs lens instead of listing its own rows), so `details.rr` never
  // exists and this branch has still never once executed. Left soft rather
  // than failing the security gate on a surface the product genuinely
  // doesn't have; tracked as a real gap, not silently absorbed.
  const rr = page.locator('details.rr').first();
  if (await rr.count()) {
    await rr.locator('summary').click();
    await assertInert(page, 'recent-run expanded');
  }

  // Drill into a session subsystem (handle/model/detector text rendered
  // there — `runRegions()` in `lenses/session/sessionRun.ts`, a render path
  // separate from the main event log's `RecordView`). A REAL, independently
  // built affordance on `/next` (#1640/#1800), but relocated from where
  // legacy put it: legacy's only session-drill click lived on the MACHINE
  // page's per-run "open ->" link, which #1809 removed outright. The port's
  // own click lives on the FLEET stage's activity-lane bars instead
  // (`FleetLens.tsx`'s `.sbar`, `data-act="session"`) — a deliberate,
  // documented widening beyond legacy, not a port of legacy's own control.
  // So this walk visits it HERE, while still on the fleet stage, rather than
  // after the machine drill below — the surface didn't move to nowhere, it
  // moved one screen earlier. `mockFlowSessionEndpoint` (top of file) is what
  // lets the click reach the real populated render instead of just the
  // error branch's bare session-id text.
  const sess = page.locator('[data-act="session"]').first();
  if (await sess.count()) {
    await sess.click();
    await page.waitForSelector('.session-run');
    await assertInert(page, 'subsystem');
    // Back to the fleet stage for the machine-card drill below.
    await page.locator('[data-act="fleet"]').click();
    await page.waitForSelector('[data-act="machine"][data-arg]');
  }

  // Mission view (mission_id + per-dispatch role/machine/model rows). NOT
  // hardened, and this is a genuine, current finding, re-verified against
  // the actual source rather than inherited from the pre-#1640 note this
  // comment used to carry: `grep -rn 'data-act="mission"' ui/src` returns
  // ZERO matches anywhere in the port. The only mission-shaped affordance
  // that exists is `data-act="gomission"` (`RunsBoard.tsx`/`CatalogPanel.tsx`),
  // and it does something architecturally different from what this walk is
  // named for — it's a FULL PAGE NAVIGATION to `/mission/<id>/graph`, a
  // separate document with its own vendored React Flow bundle
  // (`mission-graph.html`), not an inline `/next` render. `#mission=<id>`'s
  // own inline component (`MissionReplay.tsx`) never renders per-dispatch
  // role/machine/model rows either way — only loading/error/"nothing to
  // replay" text ahead of that same redirect. So there is no inline mission
  // drill-in for this walk to reach, by construction, not by omission; left
  // soft, same as recent-run above.
  const miss = page.locator('[data-act="mission"]').first();
  if (await miss.count()) {
    await miss.click();
    await assertInert(page, 'mission');
  }

  // (#1631) Report which drill-downs this walk ACTUALLY entered. The test name
  // claims "across every view"; these three are conditional, and a surface that
  // renders nothing is skipped rather than failed. Counted here so the claim is
  // auditable instead of assumed.
  const walked = {
    recentRun: await page.locator('details.rr').count(),
    session: await page.locator('[data-act="session"]').count(),
    mission: await page.locator('[data-act="mission"]').count(),
  };
  expect(
    walked.recentRun + walked.session + walked.mission,
    `the walk entered NO drill-down at all: ${JSON.stringify(walked)}`
  ).toBeGreaterThan(0);

  // Filters modal renders the record-derived category/tier/source values.
  // BEFORE the machine drill below, and this ordering is load-bearing, not
  // cosmetic: `EventLogColumn` (the filters button lives inside it) is
  // CSS-hidden on the machine/runs/console lenses in BOTH the port and
  // legacy (`route.ts`'s own `showsEventLog` doc, verified against the real
  // legacy CSS: `body.machine-mode .log{display:none}`,
  // `crates/darkmux-serve/assets/viewer.html:258`) — clicking it after the
  // machine drill hangs Playwright's actionability wait for the full
  // timeout on EITHER build, empirically confirmed by running this exact
  // click, in this exact post-machine-drill position, against legacy's own
  // `viewer.html` directly (not inferred). That is a genuine pre-existing
  // ordering defect in the walk itself, present since its original
  // authorship (`ed1cc966`), independent of the port — not something the
  // port broke. Moved here, while still on the fleet stage where the log
  // column is visible on every build, fixes it for good rather than only
  // for `/next`.
  await page.locator('[data-act="filters"]').click();
  await assertInert(page, 'filters modal');

  // Full-text search forces the event log to render every matching record's fields.
  const search = page.locator('#fsearch');
  if (await search.count()) {
    await search.fill('img');
    await page.waitForTimeout(100);
    await assertInert(page, 'log search');
  }

  // Close the filters modal before the machine drill below — it is a
  // fixed-position overlay (`Dialog.tsx`'s `.dialog-backdrop`) that would
  // otherwise sit on top of the machine stage's own content.
  await page.keyboard.press('Escape');

  // Drill into a machine (its name/spec come from attacker-controlled
  // fields). Last, since the machine/runs lens hides the event log this
  // walk has been exercising above — nothing left to check afterward but
  // the machine stage's own render.
  await page.locator('[data-act="machine"][data-arg]').first().click();
  await page.waitForSelector('.stagehdr');
  await assertInert(page, 'machine');

  // No uncaught page errors anywhere in the walk (a broken handler / parse would surface here).
  expect(pageErrors, `uncaught page errors: ${pageErrors.join(' | ')}`).toEqual([]);
});

// A focused subset of the full walk above: the fleet default view (machine
// cards + the savings hero, both record-derived) and a genuine drill into a
// SPECIFIC attacker-controlled machine card (not the nav tab — see the
// FleetLens comment on why `[data-act="machine"]` alone is ambiguous
// between the two; `[data-arg]` picks the card), including the machine
// lens's direct deep-link entry point. Predates the full walk's restoration
// (#1800) and is kept as its own test rather than folded in — it exercises
// the deep-link path (`lens=machine&uid=...` typed/pasted directly) the
// click-driven walk above doesn't cover.
test('fleet + machine-drill render attacker-controlled records inertly', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));

  await page.goto('/index.html');
  await page.waitForSelector('[data-act="machine"][data-arg]', { timeout: 15_000 });
  await assertInert(page, 'fleet');

  // Two distinct machine_uids in the fixture (see the fixture-load-loop
  // check below), so the fleet MUST show two attacker-controlled cards —
  // asserted here so a card count of zero/one can't silently pass this as
  // "inert" for a reason unrelated to escaping (nothing rendered at all).
  await expect(page.locator('[data-act="machine"][data-arg]')).toHaveCount(2);

  const uid = await page.locator('[data-act="machine"][data-arg]').first().getAttribute('data-arg');

  await page.locator('[data-act="machine"][data-arg]').first().click();
  await page.waitForSelector('.stagehdr');
  // (#1809) A fleet-card click routes by LOCALITY (`FleetLens.tsx`'s
  // `machineDrillHash`): a machine CONFIRMED remote goes to the runs lens
  // pinned to it; anything not confirmed — local, or (as on THIS daemon-less
  // static harness, where there is no live `/machine/specs` probe to confirm
  // against) simply unknown — goes to the residency room instead, the
  // guess-that-admits-it's-guessing default (`FleetLens.tsx`'s own doc on
  // `machineDrillHash`). Verified live, not inferred from the doc alone:
  // `location.hash` after this click reads `lens=machine&uid=<uid>`, not
  // `lens=runs`. Real, current product behavior for a daemon-less build.
  await assertInert(page, 'machine (fleet-card drill destination)');

  // Deep-link straight into the machine lens too, so that entry point keeps
  // its own real coverage independent of the click above.
  await page.evaluate((u) => {
    location.hash = `lens=machine&uid=${encodeURIComponent(u)}`;
  }, uid);
  await page.waitForSelector('.stagehdr');
  await assertInert(page, 'machine (direct deep-link)');

  expect(pageErrors, `uncaught page errors: ${pageErrors.join(' | ')}`).toEqual([]);
});

test('the harness actually loaded the malicious fixture (guards against a no-op pass)', async ({ page }) => {
  // If the fixture failed to load, the walk above would trivially pass against an
  // empty viewer. Assert the records are present so the inertness check means something.
  //
  // (port note) `DATA` was legacy's own global array (`viewer.html`'s
  // `boot()` assigns it at module scope); the port holds parsed records in
  // React Query's cache, not on `window`, so there is no global left to
  // read a count off. Proxy instead on a derived, precise, record-count-
  // shaped fact: this fixture carries exactly 2 distinct `machine_uid`
  // values (verified against `tests/fixtures/xss-flow.jsonl` directly, not
  // assumed), so the fleet MUST render exactly 2 machine cards once boot
  // has actually parsed the file — zero or one is unambiguously "the
  // fixture didn't load" the same way `DATA.length` going to 0 was.
  await page.goto('/index.html');
  await page.waitForSelector('[data-act="machine"][data-arg]', { timeout: 15_000 });
  const cards = await page.locator('[data-act="machine"][data-arg]').count();
  expect(cards, 'fixture did not load — inertness assertions would be vacuous').toBe(2);
});
