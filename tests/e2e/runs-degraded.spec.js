// The runs lens against DEGRADED data (#1622).
//
// Every UI defect the operator found by hand on 2026-08-03 (#1618, #1620,
// #1621) lived in a state the suite never constructed: a mission mid-run, a
// manifest with fields missing, rendered pixels. The fixtures were all terminal
// and well-formed, so 2684 passing tests were never going to catch any of it —
// the greenness gave FALSE confidence rather than none.
//
// The runs lens in particular had ZERO specs, which is exactly where #1621
// lived: 49 long-dead runs rendered as `running` because "not finished" was
// read as "in flight". This file is that gap closed for the lens.
//
// The governing rule, stated once: ABSENCE OF DATA MUST NEVER BECOME A
// CONFIDENT ANSWER. A missing field renders as missing. It does not become the
// most alarming value, the most reassuring one, or a crash.
const { test, expect } = require('@playwright/test');
const fixture = require('../fixtures/runs-degraded-fixture.json');

async function openRuns(page, body = fixture) {
  const errors = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  await page.route('**/runs*', (r) =>
    r.fulfill({ contentType: 'application/json', body: JSON.stringify(body) })
  );
  await page.goto('/index-lab.html#lens=runs');
  await expect(page.locator('.labrunrow').first()).toBeVisible();
  return errors;
}

test('a row with fields missing renders without inventing them', async ({ page }) => {
  const errors = await openRuns(page);

  // Every row in the fixture must render. A malformed row that silently
  // vanishes is worse than one that renders oddly: the operator cannot act on
  // a run they cannot see, and nothing tells them the list is incomplete.
  await expect(page.locator('.labrunrow')).toHaveCount(fixture.runs.length);

  // The row missing `status` must not acquire one. `running` in particular is
  // a claim about the PRESENT — the #1621 defect was exactly this, an unknown
  // state rendered as the most alarming available value.
  const noStatus = page.locator('.labrunrow', { hasText: 'degraded/no-status-field' });
  await expect(noStatus).toHaveCount(1);
  await expect(noStatus).not.toContainText('running');

  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});

test('a status this viewer does not know renders as itself, not as running', async ({ page }) => {
  // Forward-compat, the viewer half of #1611's lesson: a newer darkmux may emit
  // a status this build has never heard of. Coercing it to a known value is how
  // a fleet-version skew turns into a wrong claim about live work.
  const errors = await openRuns(page);
  const row = page.locator('.labrunrow', { hasText: 'degraded/status-from-a-newer-darkmux' });
  await expect(row).toHaveCount(1);
  await expect(row).toContainText('quiesced');
  await expect(row).not.toContainText('running');
  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});

test('an abandoned run is visibly not a running one', async ({ page }) => {
  // The #1621 fix makes a stale unfinished run report `abandoned`. That is only
  // worth anything if the lens renders the two distinguishably — otherwise the
  // correction stops at the API boundary and the operator sees no change.
  const errors = await openRuns(page);
  const live = page.locator('.labrunrow', { hasText: 'healthy/live' }).locator('.labbadge');
  const dead = page.locator('.labrunrow', { hasText: 'degraded/abandoned-stale' }).locator('.labbadge');
  await expect(live).toHaveText('running');
  await expect(dead).toHaveText('abandoned');
  await expect(live).not.toHaveClass(await dead.getAttribute('class'));
  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});

test('a row with no timestamps at all still renders and stays inert', async ({ page }) => {
  // `runsAgo` has nothing to work with here. It must yield nothing rather than
  // "NaN ago" or "56 years ago" — a fabricated age is the same class of lie as
  // a fabricated status, just quieter.
  const errors = await openRuns(page);
  const row = page.locator('.labrunrow', { hasText: 'degraded/no-timestamps-at-all' });
  await expect(row).toHaveCount(1);
  await expect(row).not.toContainText('NaN');
  await expect(row).not.toContainText('Invalid');
  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});

test('an untracked run is not offered as clickable', async ({ page }) => {
  // There is no durable record behind a ghost, so a link would open a 404.
  const errors = await openRuns(page);
  const ghost = page.locator('.labrunrow', { hasText: 'degraded/untracked-ghost' });
  await expect(ghost).toHaveClass(/flat/);
  await expect(ghost).not.toHaveAttribute('role', 'button');
  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});

test('degraded rows are still rendered inertly, not as markup', async ({ page }) => {
  // `/runs` ids come from run directory names and mission ids, neither of which
  // is guaranteed sanitary. The XSS gate covers the flow-record path; this is
  // the same guarantee on the runs path, which has its own renderer.
  const errors = await openRuns(page);

  // No ELEMENT was created from the payload — that is the whole guarantee.
  await expect(page.locator('.labrunrow img')).toHaveCount(0);
  await expect(page.locator('.labrunrow b')).toHaveCount(0);
  await expect(page.locator('.labrunrow [onerror]')).toHaveCount(0);

  // ...and the hostile id is still SHOWN, as text. A renderer that "sanitized"
  // by dropping the row would hide a real run from the operator, which is the
  // other way to fail this.
  //
  // NB an earlier draft asserted the page HTML does not CONTAIN `onerror=`.
  // That was wrong and failed against correct code: escaped text legitimately
  // contains those characters as visible content. The question is whether an
  // element exists, never whether a substring appears.
  const evil = page.locator('.labrunrow', { hasText: 'degraded/' }).filter({ hasText: 'onerror' });
  await expect(evil).toHaveCount(1);
  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});

test('an empty runs payload reads as empty, never as an error', async ({ page }) => {
  const errors = await openRuns(page, { generated_at_ms: 1893456600000, runs: [] }).catch(() => []);
  // `openRuns` waits for a row, which will not appear — so re-drive it here
  // without that wait rather than asserting on a timeout.
  await page.route('**/runs*', (r) =>
    r.fulfill({ contentType: 'application/json', body: JSON.stringify({ runs: [] }) })
  );
  await page.goto('/index-lab.html#lens=runs');
  await expect(page.locator('.labrunrow')).toHaveCount(0);
  await expect(page.locator('#stage')).not.toContainText('undefined');
  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});
