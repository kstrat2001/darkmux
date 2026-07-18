// (#1471) Headless e2e for the mission-graph EVENTS panel backfill fix.
//
// The bug: the EVENTS panel's `events` state was populated ONLY by the live SSE
// `onMessage` handler, which tails the flow stream from NOW — so a page refresh
// showed "no events for this mission yet" until a new record streamed, even for
// a mission with a rich history. The fix adds a one-shot backfill on load that
// fetches the day's records from `/flow/<date>`, filters them through the SAME
// `recordInMission` predicate the live path uses, and seeds the panel — deduped
// against any live rows that arrived in the race window.
//
// The mission-graph lens is a SEPARATE asset from viewer.html (its own vendored
// React Flow bundle), so this spec route-mocks the page's three same-origin data
// sources (graph.json snapshot, /flow/<date> backfill, the SSE stream) and
// fulfills the `/mission/<id>/graph` route with the served HTML shell so
// `missionIdFromPath()` sees the real path. The /vendor/* bundle falls through
// to the static server (see playwright.config.js's mission-graph harness).
const fs = require('fs');
const path = require('path');
const { test, expect } = require('@playwright/test');

const MISSION_ID = 'test-mission';
const GRAPH_PATH = `**/mission/${MISSION_ID}/graph`;
// The date the page fetches — todayUtc() === new Date().toISOString().slice(0,10).
const TODAY = new Date().toISOString().slice(0, 10);
// Match `/flow/<date>` (backfill) but NOT `/flow/<date>/stream` (live SSE).
const BACKFILL_RE = /\/flow\/\d{4}-\d{2}-\d{2}(\?.*)?$/;
const STREAM_RE = /\/flow\/\d{4}-\d{2}-\d{2}\/stream(\?.*)?$/;

const MISSION_GRAPH_HTML = fs.readFileSync(
  path.join(__dirname, '.served', 'mission-graph.html'),
  'utf8'
);

// A minimal but real-shaped graph snapshot: one phase, one task, one step. The
// backfill records below correlate to these ids through `recordInMission`.
const GRAPH = {
  mission_id: MISSION_ID,
  mission_status: 'active',
  nodes: [
    { id: 'phase-a', kind: 'phase', label: 'Phase A', status: 'running', depth: 0, steps: [] },
    {
      id: 'task-1', kind: 'task', label: 'Task One', parentId: 'phase-a',
      status: 'running', depth: 0,
      steps: [{ id: 'step-1', kind: 'dispatch.internal', label: 'Coder', status: 'complete' }],
    },
  ],
  edges: [],
  generated_at_ms: 0,
};

// Serve the page shell (real path so missionIdFromPath() resolves) + the graph
// snapshot. Returns nothing; callers add the /flow routes they need.
async function routeShellAndGraph(page) {
  await page.route(GRAPH_PATH, (r) =>
    r.fulfill({ contentType: 'text/html; charset=utf-8', body: MISSION_GRAPH_HTML })
  );
  await page.route(`**/mission/${MISSION_ID}/graph.json`, (r) =>
    r.fulfill({ contentType: 'application/json', body: JSON.stringify(GRAPH) })
  );
}

test('EVENTS panel backfills existing mission records on load (not live-stream-only)', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));

  await routeShellAndGraph(page);

  // The day's records: three that belong to THIS mission (by step id, phase id,
  // and mission_id respectively) + one unrelated record that must be filtered
  // out by `recordInMission` (proving backfill uses the same filter as live).
  await page.route(BACKFILL_RE, (r) =>
    r.fulfill({
      contentType: 'application/json',
      body: JSON.stringify([
        { ts: `${TODAY}T10:00:00Z`, action: 'step start', handle: 'step-1', category: 'work', level: 'info' },
        { ts: `${TODAY}T10:00:05Z`, action: 'phase start', handle: 'phase-a', category: 'work', level: 'info' },
        { ts: `${TODAY}T10:00:10Z`, action: 'mission start', handle: 'm', mission_id: MISSION_ID, category: 'work', level: 'info' },
        { ts: `${TODAY}T10:00:15Z`, action: 'step start', handle: 'not-in-this-mission', category: 'work', level: 'info' },
      ]),
    })
  );
  // No live records during this test — an empty SSE body so the panel content is
  // purely the backfill.
  await page.route(STREAM_RE, (r) =>
    r.fulfill({ contentType: 'text/event-stream', body: '' })
  );

  await page.goto(`/mission/${MISSION_ID}/graph`);

  // The panel renders (desktop viewport → open by default). The core assertion:
  // it is NOT the empty state, and it shows exactly the 3 in-mission records.
  await page.waitForSelector('.evpanel');
  await expect(page.locator('.evpanel .evempty')).toHaveCount(0);
  await expect(page.locator('.evpanel .evrow')).toHaveCount(3);
  await expect(page.locator('.evpanel .evhd')).toContainText('events · 3');
  // The unrelated record was filtered out by `recordInMission`.
  await expect(page.locator('.evpanel .evrow .evh', { hasText: 'not-in-this-mission' })).toHaveCount(0);

  expect(pageErrors).toEqual([]);
});

test('a live-streamed record appends without duplicating a backfilled one', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));

  await routeShellAndGraph(page);

  // X (step-1) is present in BOTH the backfill AND the live stream — the exact
  // race the dedup guards (a record that already happened lands via SSE in the
  // window before the backfill fetch resolves). Z (task-1) is backfill-only; Y
  // (phase-a) is live-only and genuinely new.
  const recX = { ts: `${TODAY}T09:00:00Z`, action: 'step start', handle: 'step-1', category: 'work', level: 'info' };
  const recZ = { ts: `${TODAY}T08:59:00Z`, action: 'phase start', handle: 'task-1', category: 'work', level: 'info' };
  const recY = { ts: `${TODAY}T09:00:30Z`, action: 'step complete', handle: 'phase-a', category: 'work', level: 'info' };

  // Gate the backfill behind a manually-resolved promise so the live X+Y land
  // FIRST, deterministically (no fixed-delay race). This is the exact ordering
  // the dedup is designed for: live rows already present, backfill dedups
  // against them. A straight replace or a naive concat would double X.
  let releaseBackfill;
  const backfillGate = new Promise((res) => { releaseBackfill = res; });
  await page.route(BACKFILL_RE, async (r) => {
    await backfillGate;
    r.fulfill({ contentType: 'application/json', body: JSON.stringify([recX, recZ]) });
  });

  // Emit the two live frames on the FIRST stream connection only; later
  // EventSource reconnects get an empty body so the frames never replay (the
  // live onMessage path intentionally does not self-dedup — replaying would be
  // a test artifact, not the behavior under test).
  let streamHits = 0;
  await page.route(STREAM_RE, (r) => {
    const first = streamHits++ === 0;
    const body = first ? `data: ${JSON.stringify(recX)}\n\ndata: ${JSON.stringify(recY)}\n\n` : '';
    r.fulfill({ contentType: 'text/event-stream', body });
  });

  await page.goto(`/mission/${MISSION_ID}/graph`);
  await page.waitForSelector('.evpanel');

  // Live X + Y render FIRST (backfill still gated) — assert that ordering
  // explicitly, so the dedup precondition (live rows present) is proven, not
  // assumed.
  await expect(page.locator('.evpanel .evrow')).toHaveCount(2);
  releaseBackfill();

  // Final state: X once (deduped across live+backfill), plus Y (live) and Z
  // (backfill) — three distinct rows, not four.
  await expect(page.locator('.evpanel .evrow')).toHaveCount(3);
  // X's handle (step-1) appears exactly once — the dedup held.
  await expect(page.locator('.evpanel .evrow .evh', { hasText: /^step-1$/ })).toHaveCount(1);
  // The genuinely-new live record (phase-a / step complete) is present.
  await expect(page.locator('.evpanel .evrow', { hasText: 'step complete' })).toHaveCount(1);
  // The backfill-only record (task-1) is present.
  await expect(page.locator('.evpanel .evrow .evh', { hasText: /^task-1$/ })).toHaveCount(1);

  expect(pageErrors).toEqual([]);
});
