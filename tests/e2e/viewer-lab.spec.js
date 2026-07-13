// Headless e2e smoke for the #1247 Part 3 lab observer lens. The static XSS
// harness has no daemon behind it, so `/lab/runs` isn't reachable there —
// these specs use the `index-lab.html` variant (playwright.config.js) which
// injects `darkmux-lab-runs-src` pointing at a committed fixture
// (tests/fixtures/lab-runs-fixture.json), the same static-fixture-override
// pattern the missions/phases lens already uses. The run-DETAIL endpoints
// (`/lab/run/detail` + `/lab/run/events`) have no meta override, so specs
// that drill into a run route-mock them (the catalog.spec pattern).
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
            tier: 'local', stage: 'dispatch', action: 'review.task',
            handle: 'demo-crew', session_id: 'demo-case-a', source: 'review',
            payload: { case_id: 'demo-case-a', crew: 'demo-crew', status: 'started', bundles: 12 },
          }],
          next_offset: 100,
          finished: true,
        }),
      })
    ),
  ]);
}

test('lab lens tab renders, the run list populates, and drilling a run updates the hash', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));
  await mockRunDetail(page);

  await page.goto('/index-lab.html');
  await page.waitForSelector('#lens-lab');

  await page.click('#lens-lab');
  await expect(page.locator('#lens-lab')).toHaveClass(/\bon\b/);
  await expect(page.locator('#lens-fleet')).not.toHaveClass(/\bon\b/);
  // (#1247 deep-link) Lens navigation reflects into the address bar.
  await expect.poll(() => page.evaluate(() => location.hash)).toContain('lens=lab');

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
  await expect(liveCard.locator('.labbadge.live')).toHaveCount(1);
  await expect(liveCard).toContainText('staffing pending');

  // Drilling into a run renders the detail view (pipeline strip + feed;
  // detail/events route-mocked above) and puts the run in the hash.
  await seriesCard.locator('.labrunrow').first().click();
  await expect(page.locator('#crumb')).toContainText('demo-case');
  await expect(page.locator('.labpipe .labstage').first()).toBeVisible();
  await expect.poll(() => page.evaluate(() => location.hash)).toContain('run=demo-case%2Frun2');

  // Navigating back to fleet clears the lab params from the hash.
  await page.click('#lens-fleet');
  await expect.poll(() => page.evaluate(() => location.hash)).not.toContain('lens=lab');

  expect(pageErrors, `uncaught page errors: ${pageErrors.join(' | ')}`).toEqual([]);
});

test('labKnobDiff surfaces a judge seat added or removed between runs', async ({ page }) => {
  // Direct unit-check of the client-side diff (frontier review, #1262): a
  // judge appearing/disappearing between runs is methodology drift and must
  // never render as "no knob change". Driven via page.evaluate against the
  // real function rather than a fixture third-run (which would ripple
  // through every series-count assertion above).
  await page.goto('/index-lab.html');
  await page.waitForSelector('#lens-lab');
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

test('deep link #lens=lab boots directly into the lab pane', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));

  await page.goto('/index-lab.html#lens=lab');
  // No tab click — boot itself must land in the lab lens.
  await expect(page.locator('#lens-lab')).toHaveClass(/\bon\b/);
  await expect(page.locator('.labtaskcard')).toHaveCount(2);

  expect(pageErrors, `uncaught page errors: ${pageErrors.join(' | ')}`).toEqual([]);
});

test('deep link #lens=lab&run=<dir> boots into that run detail', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));
  await mockRunDetail(page);

  await page.goto('/index-lab.html#lens=lab&run=demo-case%2Frun2');
  await expect(page.locator('#lens-lab')).toHaveClass(/\bon\b/);
  await expect(page.locator('.labpipe .labstage').first()).toBeVisible();
  await expect(page.locator('#crumb')).toContainText('demo-case/run2');

  expect(pageErrors, `uncaught page errors: ${pageErrors.join(' | ')}`).toEqual([]);
});

test('deep link with an unresolvable run falls back to the run list with a notice', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));
  // Detail endpoint rejects (the daemon 400s a bad/out-of-bounds dir; the
  // static harness would 404 — same fallback path either way).
  await page.route('**/lab/run/detail*', (r) => r.fulfill({ status: 400, body: 'bad dir' }));

  await page.goto('/index-lab.html#lens=lab&run=no-such-run');
  await expect(page.locator('#lens-lab')).toHaveClass(/\bon\b/);
  // Falls back to the run LIST (cards render) with the one-shot notice —
  // never a stuck "loading…" pane polling a failing request forever.
  await expect(page.locator('.labnotice')).toContainText('no-such-run');
  await expect(page.locator('.labtaskcard')).toHaveCount(2);

  expect(pageErrors, `uncaught page errors: ${pageErrors.join(' | ')}`).toEqual([]);
});
