// #1868 packet 2 retarget of mission-graph-mission-isolation.spec.js against
// `MissionGraphLens` (`#mission=<id>`). Same #1641 fix this port carries
// verbatim (`graph.ts::recordInMission`/`applyRecordToMetrics`/
// `applyFlowRecord` all gate on a PRESENT `mission_id` being authoritative,
// same as the standalone page) — only the DOM surface for the assertions
// (the header meter, the events pane, the step-row status class) changed to
// this port's own selectors.
const { test, expect } = require('@playwright/test');

const MISSION_ID = 'm-a';
const OTHER_MISSION_ID = 'm-b';
const TODAY = new Date().toISOString().slice(0, 10);
const BACKFILL_RE = /\/flow\/\d{4}-\d{2}-\d{2}(?!\/stream)(\?.*)?$/;
const STREAM_RE = /\/flow\/\d{4}-\d{2}-\d{2}\/stream(\?.*)?$/;
const MISSION_RE = /\/flow-mission\/[^/?]+(\?.*)?$/;

const GRAPH = {
  mission_id: MISSION_ID,
  mission_status: 'active',
  nodes: [
    { id: 'phase-a', kind: 'phase', label: 'Investigate', status: 'running', depth: 0, steps: [] },
    {
      id: 'task-1', kind: 'task', label: 'Probe', parentId: 'phase-a', status: 'running', depth: 0,
      steps: [{ id: 's1', kind: 'dispatch.internal', label: 'Probe', status: 'running' }],
    },
  ],
  edges: [],
  generated_at_ms: 0,
};

const rec = (over) => ({ ts: `${TODAY}T10:00:00Z`, level: 'info', category: 'work', tier: 'local', stage: 'dispatch', handle: 's1', session_id: 'step-s1', ...over });

async function open(page, records) {
  const errors = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  await page.route(`**/mission/${MISSION_ID}/graph.json*`, (r) => r.fulfill({ contentType: 'application/json', body: JSON.stringify(GRAPH) }));
  await page.route(MISSION_RE, (r) => r.fulfill({ contentType: 'application/json', body: JSON.stringify({ records: [], count: 0, truncated: false, generated_at_ms: 0 }) }));
  await page.route(BACKFILL_RE, (r) => r.fulfill({ contentType: 'application/json', body: '[]' }));
  let hits = 0;
  await page.route(STREAM_RE, (r) => {
    const body = hits++ === 0 ? records.map((x) => `data: ${JSON.stringify(x)}\n\n`).join('') : '';
    r.fulfill({ contentType: 'text/event-stream', body });
  });
  await page.setViewportSize({ width: 1280, height: 900 });
  await page.goto(`/index-live.html#mission=${MISSION_ID}`);
  await expect(page.locator('.missionlens .mmeter')).toBeVisible();
  await page.waitForSelector('.missionlens .eventlog');
  return errors;
}

test("a record stamped for a DIFFERENT mission does not move this mission's step metrics or events", async ({ page }) => {
  const RECORDS = [
    rec({ action: 'dispatch start', mission_id: MISSION_ID, payload: {} }),
    rec({ action: 'telemetry.tokens', category: 'telemetry', source: 'tokens', mission_id: MISSION_ID, payload: { total_tokens: 4000 } }),
    // The collision: same session_id/handle, a DIFFERENT mission_id — exactly
    // what a concurrent run of the same config emits.
    rec({ action: 'telemetry.tokens', category: 'telemetry', source: 'tokens', mission_id: OTHER_MISSION_ID, payload: { total_tokens: 9000 } }),
    rec({ action: 'dispatch complete', mission_id: OTHER_MISSION_ID, payload: {} }),
    rec({ action: 'dispatch complete', mission_id: MISSION_ID, payload: {} }),
  ];
  const errors = await open(page, RECORDS);

  const meter = (await page.locator('.missionlens .mmeter').innerText()).replace(/\s+/g, ' ');
  expect(meter, `meter read: ${meter}`).toContain('4.0k tok');
  expect(meter, "the leaked mission-B tokens must never appear in m-a's total").not.toContain('13.0k');
  expect(meter, "the leaked mission-B tokens must never appear in m-a's total").not.toContain('9.0k');

  // Exactly the three records stamped for m-a — the two mission-B rows are
  // excluded, even though they share s1's handle/session_id.
  await expect(page.locator('.missionlens .eventlog__rec')).toHaveCount(3);
  await expect(page.locator('.missionlens .eventlog__qcount')).toContainText('3 events');

  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});

test("a foreign step-lifecycle record does not flip this mission's step status", async ({ page }) => {
  const RECORDS = [
    rec({ action: 'dispatch start', mission_id: MISSION_ID, payload: {} }),
    rec({ action: 'telemetry.tokens', category: 'telemetry', source: 'tokens', mission_id: MISSION_ID, payload: { total_tokens: 1000 } }),
    // Mission B finishes ITS copy of step s1 — stamped, foreign, and a real
    // status-transition action (`step complete` IS in STATUS_ACTIONS).
    rec({ action: 'step complete', mission_id: OTHER_MISSION_ID, payload: {} }),
  ];
  const errors = await open(page, RECORDS);

  // (#2117) NOT `toBeVisible()`: on the desktop-default canvas view this
  // row lives inside a React Flow node, which React Flow keeps
  // `visibility: hidden` until its own measurement pass completes on the
  // next animation frame — a race the 5s visibility wait sometimes lost in
  // headless Chromium, independent of whether the status logic was
  // correct. The claim under test is the STATUS class, not paint, and
  // `toHaveClass`/`not.toHaveClass` already retry until the element is
  // attached with the right `class` attribute — attachment is what this
  // test needs, not visibility.
  const stepRow = page.locator('.steprow').first();
  await expect(stepRow, "mission A's step must still be running — a foreign step-complete flipped it").toHaveClass(/s-running/);
  await expect(stepRow).not.toHaveClass(/s-complete/);

  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});

test('a record with NO mission_id still correlates — the legacy path', async ({ page }) => {
  const RECORDS = [
    rec({ action: 'dispatch start', payload: {} }),
    rec({ action: 'telemetry.tokens', category: 'telemetry', source: 'tokens', payload: { total_tokens: 2500 } }),
    rec({ action: 'dispatch complete', payload: {} }),
  ];
  const errors = await open(page, RECORDS);

  const meter = (await page.locator('.missionlens .mmeter').innerText()).replace(/\s+/g, ' ');
  expect(meter, `meter read: ${meter}`).toContain('2.5k tok');

  await expect(page.locator('.missionlens .eventlog__rec')).toHaveCount(3);
  await expect(page.locator('.missionlens .eventlog__qcount')).toContainText('3 events');
  await expect(page.locator('.missionlens .eventlog__empty')).toHaveCount(0);

  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});
