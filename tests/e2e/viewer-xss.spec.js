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
// SCOPE, stated honestly (#1800): the full every-render-path walk is the
// `test.fixme` below — it drills surfaces the React port does not have yet
// (recent-run expand, session drill, mission drill, the filters modal), and it
// is kept verbatim rather than narrowed, because quietly editing a security
// walk's selectors until it passes is how a gate keeps its name while losing
// its coverage. This file has been bitten by that once already; see the #1622
// and #1631 notes further down. What runs today covers the fleet view and the
// machine drill, and says so in its own name.
const { test, expect } = require('@playwright/test');

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

// (port gap, reported not papered over — read before touching this test)
// This test cannot complete against `/next` today, for a real reason: line
// 86 below (`page.locator('[data-act="filters"]').click()`) targets the
// checkbox-per-facet filter MODAL, which is a named, deliberate,
// documented cut in the port (`EventLogColumn.tsx`'s own module doc: "The
// filter MODAL is a named, deliberate cut, not a half-build" — `.fbtn`
// here toggles a one-shot "model only" quick filter directly instead of
// opening a modal with checkboxes). The same is true of the notes modal
// and the about/info modal (`#nmodalbg`/`#imodalbg`, `viewer-keyboard.spec.js`'s
// own module doc has the full accounting). There is no `[data-act="filters"]`
// element anywhere in the port to click, so this line hangs for the full
// 30s timeout rather than silently skipping — which is the CORRECT
// failure mode for an unconditional step whose surface has gone missing
// (see the file's own #1622 precedent below for why a bare
// `if (await X.count())` here would be the wrong fix, not a lesser one:
// it would make the walk "pass" having never reached the modal, exactly
// the silent-narrowing failure #1622/#1631 exist to prevent).
//
// Separately, and independently: the recent-run/session/mission
// drill-downs (lines 50-83) are ALSO all unreachable in the port today —
// confirmed live, not inferred (`walked.recentRun + walked.session +
// walked.mission` is 0 against this exact fixture) — because `MachineLens`'s
// run rows carry no session-drill affordance yet (`runLines.ts`'s own
// module doc names this cut) and there is no mission-drill affordance in
// this static harness either. So even past the filters-modal blocker, the
// ORIGINAL test's audit assertion (line 80-83) would still fail honestly.
//
// Kept here verbatim (fixme, not deleted or edited to lie about what it
// covers) as the tracked record of the full "across every view" claim.
// The surfaces that ARE real and walkable today — fleet + the local
// machine drill, which is genuinely NEW coverage as of the `.stagehdr`
// restoration below — get their own real, passing test underneath this
// one, so this file's net security coverage is a strict addition versus
// before this pass, not a wash.
test.fixme('viewer renders attacker-controlled flow records inertly across every view', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));

  await page.goto('/index.html');

  // boot() is async (fetches + parses the flow file) — wait for the fleet to render.
  await page.waitForSelector('[data-act="machine"]', { timeout: 15_000 });
  await assertInert(page, 'fleet');

  // Drill into a machine (its name/spec come from attacker-controlled fields).
  await page.locator('[data-act="machine"]').first().click();
  await page.waitForSelector('.stagehdr');
  await assertInert(page, 'machine');

  // (#1622) Each of these three used to be a bare `if (await X.count())`, so
  // if the surface stopped rendering AT ALL the block was SKIPPED, not failed
  // — the walk stayed green while silently covering less than its name claims.
  // The fixture guarantees each of these exists, so a zero count is a
  // regression upstream of the XSS check and must be loud.
  // (#1631) NOT hardened, deliberately, and this is the finding: the XSS
  // fixture renders ZERO recent-run rows, so this branch has never once
  // executed. The surface is UNWALKED, not merely conditionally walked.
  // Left soft rather than failing the security gate on a fixture gap; the
  // missing coverage is tracked separately.
  const rr = page.locator('details.rr').first();
  if (await rr.count()) {
    await rr.locator('summary').click();
    await assertInert(page, 'recent-run expanded');
  }

  // Drill into a session subsystem (handle/model/detector text rendered there).
  const sess = page.locator('[data-act="session"]').first();
  if (await sess.count()) {
    await sess.click();
    await page.waitForSelector('.sub');
    await assertInert(page, 'subsystem');
  }

  // Mission view (mission_id + per-dispatch role/machine/model rows).
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
  await page.locator('[data-act="filters"]').click();
  await assertInert(page, 'filters modal');

  // Full-text search forces the event log to render every matching record's fields.
  const search = page.locator('#fsearch');
  if (await search.count()) {
    await search.fill('img');
    await page.waitForTimeout(100);
    await assertInert(page, 'log search');
  }

  // No uncaught page errors anywhere in the walk (a broken handler / parse would surface here).
  expect(pageErrors, `uncaught page errors: ${pageErrors.join(' | ')}`).toEqual([]);
});

// The real, currently-walkable subset of the fixme'd test above: the fleet
// default view (machine cards + the savings hero, both record-derived) and
// a genuine drill into a SPECIFIC attacker-controlled machine card (not
// the nav tab — see the FleetLens comment on why `[data-act="machine"]`
// alone is ambiguous between the two; `[data-arg]` picks the card). This
// is real, additional coverage that did not exist before this pass (the
// card drill hung on a missing `.stagehdr` before that class was
// restored) — it does not claim to be the FULL "every view" walk the
// fixme'd test above names; see that test's own comment for the remaining
// gap (recent-run/session/mission drills, the filters modal, log search).
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
  // (#1809) A fleet-card click now routes by LOCALITY (`FleetLens.tsx`'s
  // `machineDrillHash`): local goes to the residency room, anything this
  // daemon can't confirm as itself — which, on THIS daemon-less static
  // harness, is every card, since there is no live `/machine/specs` probe
  // to confirm against — goes to the runs lens instead, pinned to that
  // machine. That's real, current product behavior for a daemon-less
  // build, and the runs lens renders the SAME attacker-controlled machine
  // name, so it earns its own inertness check here rather than being
  // skipped.
  await assertInert(page, 'runs lens (remote-machine drill destination)');

  // Deep-link straight into the machine lens too, so THAT render path
  // keeps its own real coverage regardless of which lens a click happens
  // to land on in this harness — see the comment above for why the click
  // no longer reliably reaches it here.
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
