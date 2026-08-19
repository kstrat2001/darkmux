// #1868 packet 2 retarget of mission-graph-error-nav.spec.js (the standalone
// `/mission/:id/graph` page's own nav-trap regression test) against the
// PORT: `MissionGraphLens`, reached via `#mission=<id>` inside `next.html`.
//
// The original bug (mobile-nav lineage #1403/#1495): the standalone page's
// loading/error branches used to render a bare full-screen `.msg` with no way
// back out, because that page is a chromeless home-screen PWA shell with no
// browser back. In the PORT this exact failure mode cannot recur BY
// CONSTRUCTION: `App.tsx` always renders `<Masthead>`/`<NavChrome>` around
// `#stage` regardless of which lens (or lens STATE) is showing — a real app
// shell, not a standalone document a lens can strand the operator inside.
// This spec still asserts that structural guarantee directly (rather than
// assuming it), plus the SAME 404-vs-500 honest-message distinction the
// original proved.
const { test, expect } = require('@playwright/test');

function routeFlowNoise(page) {
  return Promise.all([
    page.route(/\/flow\/\d{4}-\d{2}-\d{2}(\?.*)?$/, (r) =>
      r.fulfill({ contentType: 'application/json', body: '[]' })
    ),
    page.route(/\/flow\/\d{4}-\d{2}-\d{2}\/stream(\?.*)?$/, (r) =>
      r.fulfill({ contentType: 'text/event-stream', body: '' })
    ),
    page.route(/\/flow-mission\/[^/?]+(\?.*)?$/, (r) =>
      r.fulfill({ contentType: 'application/json', body: JSON.stringify({ records: [], count: 0, truncated: false, generated_at_ms: 0 }) })
    ),
  ]);
}

test('a 404 (deleted/untracked mission) shows a calm message WITH the app-shell nav — never a trapped blank screen', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));

  const missionId = 'stale-untracked-run';
  await page.setViewportSize({ width: 390, height: 844 });
  await routeFlowNoise(page);
  await page.route(`**/mission/${missionId}/graph.json`, (r) =>
    r.fulfill({ status: 404, contentType: 'text/plain', body: `no mission with id \`${missionId}\` found\n` })
  );

  await page.goto(`/index-live.html#mission=${missionId}`);

  // The app-shell nav (tabs + masthead) is present regardless of lens state —
  // the structural guarantee that replaces the standalone page's own
  // hand-maintained "keep .top on every branch" discipline.
  const navtabs = page.locator('.app-shell__navtabs');
  await expect(navtabs).toBeVisible();
  const fleetTab = page.getByRole('link', { name: 'fleet' });
  await expect(fleetTab).toBeVisible();
  const box = await fleetTab.boundingBox();
  expect(box).not.toBeNull();
  expect(box.width).toBeGreaterThan(0);
  expect(box.x).toBeGreaterThanOrEqual(0);
  expect(box.x).toBeLessThan(390);

  // The lens shows the CALM not-found message, not the raw server text.
  const alert = page.getByRole('alert');
  await expect(alert).toBeVisible();
  await expect(alert).toContainText(/ephemeral or cleared run/i);
  await expect(alert).not.toContainText('no mission with id');

  expect(pageErrors).toEqual([]);
});

test('a non-404 fetch failure still shows the nav, with the raw diagnostic text', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));

  const missionId = 'server-fault';
  await page.setViewportSize({ width: 390, height: 844 });
  await routeFlowNoise(page);
  await page.route(`**/mission/${missionId}/graph.json`, (r) =>
    r.fulfill({ status: 500, contentType: 'text/plain', body: 'failed to build mission graph: boom\n' })
  );

  await page.goto(`/index-live.html#mission=${missionId}`);

  await expect(page.locator('.app-shell__navtabs')).toBeVisible();
  const alert = page.getByRole('alert');
  await expect(alert).toContainText(/darkmux mission graph:/i);

  expect(pageErrors).toEqual([]);
});

test('a normal mission still loads fine (no regression) at mobile width', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));

  const missionId = 'healthy-mission';
  await page.setViewportSize({ width: 390, height: 844 });
  await routeFlowNoise(page);
  const graph = {
    mission_id: missionId,
    mission_status: 'active',
    nodes: [{ id: 'phase-a', kind: 'phase', label: 'Phase A', status: 'running', depth: 0, steps: [] }],
    edges: [],
    generated_at_ms: 0,
  };
  await page.route(`**/mission/${missionId}/graph.json`, (r) =>
    r.fulfill({ contentType: 'application/json', body: JSON.stringify(graph) })
  );

  await page.goto(`/index-live.html#mission=${missionId}`);

  await expect(page.locator('.app-shell__navtabs')).toBeVisible();
  await expect(page.getByRole('alert')).toHaveCount(0);
  await expect(page.locator('.missionlens .midname')).toHaveText(missionId);

  expect(pageErrors).toEqual([]);
});
