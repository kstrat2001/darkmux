// #1868 packet 2 retarget of mission-graph-truth.spec.js against
// `MissionGraphLens` (`#mission=<id>`). Same three bugs this port's `graph.ts`
// carries the fixes for verbatim (`tsToMs`'s seconds/ms unit detection,
// `fmtModel`'s namespace-stripped chip, `applyRecordToMetrics`'s
// started-gate on live token folding) — only navigation + DOM selectors
// changed, and they are UNCHANGED selectors (`.tlt-step`/`.smodel`/
// `.mn-step-meter`/`.mnode .mn-step-row`), since `StepRow.tsx` keeps them
// identical to the standalone page on purpose.
const { test, expect } = require('@playwright/test');

const MISSION_ID = 'graph-truth';

// (#1913) `judgeTok`/`verifyTok` used to pin `${TODAY}T10:00:00Z` while
// `.gen`'s freshness (`STEP_LIVENESS_WINDOW_MS`, 20 minutes, graph.ts) was
// judged against REAL wall-clock now — so this test was red for roughly
// 13h40m of every UTC day (10:20 onward) and green the rest, on a schedule
// rather than intermittently. Freezing the page's clock via
// `page.clock.setFixedTime` before navigation makes the fixture-to-now
// distance an ASSERTED PARAMETER instead of something inherited from
// whenever the suite happens to run. `TODAY`/`STARTED_SECS` are derived
// from the SAME frozen instant, not from the real clock, so the whole
// fixture is internally consistent regardless of when the suite executes.
const FIXED_NOW_MS = Date.parse('2026-06-15T10:02:00.000Z'); // 2 minutes after the T10:00 token anchor, well inside the 20-minute window
const TODAY = '2026-06-15';
const STEP_LIVENESS_WINDOW_MS = 1200 * 1000; // graph.ts's own STEP_LIVENESS_WINDOW_MS

const BACKFILL_RE = /\/flow\/\d{4}-\d{2}-\d{2}(?!\/stream)(\?.*)?$/;
const STREAM_RE = /\/flow\/\d{4}-\d{2}-\d{2}\/stream(\?.*)?$/;
const MISSION_RE = /\/flow-mission\/[^/?]+(\?.*)?$/;

// A judge seat that STARTED ~95s before the frozen "now", stamped as a
// SECONDS epoch — exactly the server wire shape.
const STARTED_SECS = Math.floor(FIXED_NOW_MS / 1000) - 95;
function graphSnapshot() {
  return {
    mission_id: MISSION_ID,
    mission_status: 'active',
    nodes: [
      { id: 'phase-a', kind: 'phase', label: 'Review', status: 'running', depth: 0, steps: [] },
      {
        id: 'task-1', kind: 'task', label: 'Judge Wave', parentId: 'phase-a', status: 'running', depth: 0,
        steps: [
          { id: 'judge-1', kind: 'review.judge', label: 'Judge', status: 'running', startedTs: STARTED_SECS, model: 'darkmux:gpt-oss-120b' },
          { id: 'verify-1', kind: 'review.verify', label: 'Verify', status: 'planned', model: 'darkmux:devstral-small-2-2512' },
        ],
      },
    ],
    edges: [],
    generated_at_ms: 0,
  };
}

const judgeTok = { ts: `${TODAY}T10:00:00Z`, action: 'telemetry.tokens', category: 'telemetry', source: 'tokens', session_id: 'step-judge-1', level: 'info', payload: { total_tokens: 5000 } };
const verifyTok = { ts: `${TODAY}T10:00:01Z`, action: 'telemetry.tokens', category: 'telemetry', source: 'tokens', session_id: 'step-verify-1', level: 'info', payload: { total_tokens: 18000 } };

async function routeAll(page, streamRecords) {
  await page.route(`**/mission/${MISSION_ID}/graph.json`, (r) => r.fulfill({ contentType: 'application/json', body: JSON.stringify(graphSnapshot()) }));
  await page.route(MISSION_RE, (r) => r.fulfill({ contentType: 'application/json', body: JSON.stringify({ records: [], count: 0, truncated: false, generated_at_ms: 0 }) }));
  await page.route(BACKFILL_RE, (r) => r.fulfill({ contentType: 'application/json', body: '[]' }));
  let hits = 0;
  await page.route(STREAM_RE, (r) => {
    const first = hits++ === 0;
    const body = first ? streamRecords.map((x) => `data: ${JSON.stringify(x)}\n\n`).join('') : '';
    r.fulfill({ contentType: 'text/event-stream', body });
  });
}

test('elapsed reads a sane clock, model chip is full, and a planned step shows no phantom tokens', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));

  await page.clock.setFixedTime(FIXED_NOW_MS);
  await page.setViewportSize({ width: 390, height: 900 });
  await routeAll(page, [judgeTok, verifyTok]);
  await page.goto(`/index-live.html#mission=${MISSION_ID}`);

  const taskHd = page.locator('.tltask .tlt-hd');
  await taskHd.first().click();
  const judgeRow = page.locator('.tlt-step', { has: page.locator('.smodel', { hasText: 'gpt-oss-120b' }) });
  const verifyRow = page.locator('.tlt-step', { has: page.locator('.smodel', { hasText: 'devstral-small-2-2512' }) });
  await expect(judgeRow).toHaveCount(1);
  await expect(verifyRow).toHaveCount(1);

  // Bug 1: the running judge's elapsed clock is a sane m:ss.
  const elapsed = (await judgeRow.locator('.mn-step-meter .gen').innerText()).trim();
  expect(elapsed).toMatch(/^\d{1,2}:\d{2}$/);
  expect(elapsed).not.toMatch(/^\d{3,}:/);

  // Bug 2: the seat chip shows the FULL model name.
  await expect(judgeRow.locator('.smodel')).toHaveText('gpt-oss-120b');
  await expect(verifyRow.locator('.smodel')).toHaveText('devstral-small-2-2512');

  // Bug 4: the PLANNED verify seat's 18k phantom is gated out; the RUNNING
  // judge's own 5k DOES fold (positive control).
  await expect(judgeRow.locator('.mn-step-meter .tok')).toContainText('tok');
  await expect(verifyRow.locator('.mn-step-meter .idle')).toHaveCount(1);
  await expect(verifyRow.locator('.mn-step-meter .tok')).toHaveCount(0);
  await expect(page.locator('.tltasks').first()).not.toContainText('18k');

  // The desktop canvas node view: the chip renders at its designed width,
  // never crushed by an oversized elapsed string.
  await page.setViewportSize({ width: 1280, height: 900 });
  const canvasChip = page.locator('.mnode .mn-step-row .smodel', { hasText: 'gpt-os' }).first();
  await expect(canvasChip).toBeVisible();
  const box = await canvasChip.boundingBox();
  expect(box.width).toBeGreaterThan(30);

  expect(pageErrors).toEqual([]);
});

// (#1913) Both directions of the STEP_LIVENESS_WINDOW_MS boundary, pinned
// explicitly — unpinned before this fix, same as FLOW_LIVE_TTL_MS in
// FleetLens.test.tsx. `stepMeterFor` reads `lastSignal` (the judge's own
// token record) in preference to `startedTs` for freshness, so only the
// token's distance from "now" needs to move to walk the boundary.
test('a judge seat 1s under STEP_LIVENESS_WINDOW_MS still renders the generating meter', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));

  await page.clock.setFixedTime(FIXED_NOW_MS);
  await page.setViewportSize({ width: 390, height: 900 });
  const tok = {
    ts: new Date(FIXED_NOW_MS - (STEP_LIVENESS_WINDOW_MS - 1000)).toISOString(),
    action: 'telemetry.tokens', category: 'telemetry', source: 'tokens',
    session_id: 'step-judge-1', level: 'info', payload: { total_tokens: 1000 },
  };
  await routeAll(page, [tok]);
  await page.goto(`/index-live.html#mission=${MISSION_ID}`);

  const taskHd = page.locator('.tltask .tlt-hd');
  await taskHd.first().click();
  const judgeRow = page.locator('.tlt-step', { has: page.locator('.smodel', { hasText: 'gpt-oss-120b' }) });
  await expect(judgeRow).toHaveCount(1);
  await expect(judgeRow.locator('.mn-step-meter .gen')).toHaveCount(1);

  expect(pageErrors).toEqual([]);
});

test('a judge seat 1s past STEP_LIVENESS_WINDOW_MS renders no generating meter', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));

  await page.clock.setFixedTime(FIXED_NOW_MS);
  await page.setViewportSize({ width: 390, height: 900 });
  const tok = {
    ts: new Date(FIXED_NOW_MS - (STEP_LIVENESS_WINDOW_MS + 1000)).toISOString(),
    action: 'telemetry.tokens', category: 'telemetry', source: 'tokens',
    session_id: 'step-judge-1', level: 'info', payload: { total_tokens: 1000 },
  };
  await routeAll(page, [tok]);
  await page.goto(`/index-live.html#mission=${MISSION_ID}`);

  const taskHd = page.locator('.tltask .tlt-hd');
  await taskHd.first().click();
  const judgeRow = page.locator('.tlt-step', { has: page.locator('.smodel', { hasText: 'gpt-oss-120b' }) });
  await expect(judgeRow).toHaveCount(1);
  await expect(judgeRow.locator('.mn-step-meter .gen')).toHaveCount(0);

  expect(pageErrors).toEqual([]);
});
