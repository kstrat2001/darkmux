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

test('labKnobDiff surfaces a judge seat added or removed between runs', async ({ page }) => {
  // Direct unit-check of the client-side diff (frontier review, #1262): a
  // judge appearing/disappearing between runs is methodology drift and must
  // never render as "no knob change". Driven via page.evaluate against the
  // real function rather than a fixture third-run (which would ripple
  // through every series-count assertion above).
  await page.goto('/index-lab.html');
  await page.waitForSelector('#lens-runs');
  const diffs = await page.evaluate(() => {
    const probe = { name: 'p1', model: 'darkmux:m', k: 1, n_ctx: 1000, max_tokens: 100 };
    const judge = { name: 'j', model: 'darkmux:judge-model', k: 3, n_ctx: 2000, max_tokens: 200 };
    const withJudge = { crew: 'c', exec_mode: 's', staffing: { probes: [probe], judge } };
    const noJudge = { crew: 'c', exec_mode: 's', staffing: { probes: [probe], judge: null } };
    return {
      added: labKnobDiff(noJudge, withJudge),
      removed: labKnobDiff(withJudge, noJudge),
      unchanged: labKnobDiff(withJudge, withJudge),
    };
  });
  expect(diffs.added).toEqual(['+judge (judge-model)']);
  expect(diffs.removed).toEqual(['-judge']);
  expect(diffs.unchanged).toEqual([]);
});

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

test('deep link with an unresolvable run falls back to the run list with a notice', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));
  // Detail endpoint rejects (the daemon 400s a bad/out-of-bounds dir; the
  // static harness would 404 — same fallback path either way).
  await page.route('**/lab/run/detail*', (r) => r.fulfill({ status: 400, body: 'bad dir' }));

  await page.goto('/index-lab.html#lens=runs&run=no-such-run');
  await expect(page.locator('#lens-runs')).toHaveClass(/\bon\b/);
  // Falls back to the run LIST with the one-shot notice — never a stuck
  // "loading…" pane polling a failing request forever.
  await expect(page.locator('.labnotice')).toContainText('no-such-run');
  await expect(page.locator('.labrunrow')).toHaveCount(6);

  expect(pageErrors, `uncaught page errors: ${pageErrors.join(' | ')}`).toEqual([]);
});
