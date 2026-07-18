// (mobile-nav lineage: #1403/#1495) Headless e2e for the mission-graph
// error/untracked-mission nav trap.
//
// Bug 1: when `/mission/<id>/graph.json` 404s (or any other fetch failure),
// the loading/error branches of App() returned a bare full-screen `.msg` with
// NO `.top` nav shell — on this chromeless home-screen PWA (#1403, no browser
// back) that stranded the user with no way back to the viewer. The fix keeps
// the same sticky `.top` bar (back link + brand) on every render branch; only
// the content area (`.body`) changes between loading/error/loaded.
//
// Bug 2: a 404 specifically (a well-formed id with no mission dir — an
// ephemeral run whose directory was cleared, or an "untracked" flow-only
// mission the missions index already flags with no durable plan behind it)
// now renders a calm, in-chrome message instead of the raw server error text
// ("no mission with id `...` found"), which is reserved for genuine faults
// (network failure, 500).
//
// Route-mocking convention matches mission-graph-events.spec.js /
// mission-graph-truth.spec.js: the page shell is fulfilled with the real
// mission-graph.html bytes (so `missionIdFromPath()` sees the real path),
// `/vendor/*` falls through to the static server (see playwright.config.js).
const fs = require('fs');
const path = require('path');
const { test, expect } = require('@playwright/test');

const MISSION_GRAPH_HTML = fs.readFileSync(
  path.join(__dirname, '.served', 'mission-graph.html'),
  'utf8'
);

function shellRoute(page, missionId) {
  return page.route(`**/mission/${missionId}/graph`, (r) =>
    r.fulfill({ contentType: 'text/html; charset=utf-8', body: MISSION_GRAPH_HTML })
  );
}

test('a 404 (deleted/untracked mission) shows a calm message WITH the nav — not a trapped blank screen', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));

  const missionId = 'stale-untracked-run';
  await page.setViewportSize({ width: 390, height: 844 });
  await shellRoute(page, missionId);
  await page.route(`**/mission/${missionId}/graph.json`, (r) =>
    r.fulfill({ status: 404, contentType: 'text/plain', body: `no mission with id \`${missionId}\` found\n` })
  );

  await page.goto(`/mission/${missionId}/graph`);

  // The nav shell is present and the back link is a real, tappable, on-screen
  // element — the exact thing that was missing before the fix.
  const top = page.locator('.top');
  await expect(top).toBeVisible();
  const back = page.locator('.top .nav a');
  await expect(back).toBeVisible();
  await expect(back).toHaveAttribute('href', '/');
  const box = await back.boundingBox();
  expect(box).not.toBeNull();
  expect(box.width).toBeGreaterThan(0);
  expect(box.height).toBeGreaterThan(0);
  // On-screen within the 390px viewport (not pushed off / collapsed to 0).
  expect(box.x).toBeGreaterThanOrEqual(0);
  expect(box.x).toBeLessThan(390);

  // The content area shows the CALM message (Bug 2), not the raw server text.
  const msg = page.locator('.body .msg');
  await expect(msg).toBeVisible();
  await expect(msg).toContainText("isn't available");
  await expect(msg).not.toContainText('no mission with id');

  expect(pageErrors).toEqual([]);
});

test('a non-404 fetch failure still shows the nav, with the raw diagnostic text', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));

  const missionId = 'server-fault';
  await page.setViewportSize({ width: 390, height: 844 });
  await shellRoute(page, missionId);
  await page.route(`**/mission/${missionId}/graph.json`, (r) =>
    r.fulfill({ status: 500, contentType: 'text/plain', body: 'failed to build mission graph: boom\n' })
  );

  await page.goto(`/mission/${missionId}/graph`);

  await expect(page.locator('.top')).toBeVisible();
  await expect(page.locator('.top .nav a')).toBeVisible();
  const msg = page.locator('.body .msg');
  await expect(msg).toContainText('graph.json fetch failed: 500');

  expect(pageErrors).toEqual([]);
});

test('a normal mission still loads fine (no regression) at mobile width', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));

  const missionId = 'healthy-mission';
  await page.setViewportSize({ width: 390, height: 844 });
  await shellRoute(page, missionId);
  const graph = {
    mission_id: missionId,
    mission_status: 'active',
    nodes: [
      { id: 'phase-a', kind: 'phase', label: 'Phase A', status: 'running', depth: 0, steps: [] },
    ],
    edges: [],
    generated_at_ms: 0,
  };
  await page.route(`**/mission/${missionId}/graph.json`, (r) =>
    r.fulfill({ contentType: 'application/json', body: JSON.stringify(graph) })
  );
  const today = new Date().toISOString().slice(0, 10);
  await page.route(new RegExp(`/flow/${today}(\\?.*)?$`), (r) =>
    r.fulfill({ contentType: 'application/json', body: '[]' })
  );
  await page.route(new RegExp(`/flow/${today}/stream(\\?.*)?$`), (r) =>
    r.fulfill({ contentType: 'text/event-stream', body: '' })
  );

  await page.goto(`/mission/${missionId}/graph`);

  await expect(page.locator('.top')).toBeVisible();
  await expect(page.locator('.top .nav a')).toBeVisible();
  await expect(page.locator('.body .msg')).toHaveCount(0);
  await expect(page.locator('.midname')).toHaveText(missionId);

  expect(pageErrors).toEqual([]);
});
