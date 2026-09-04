// #1868 packet 2 retarget of mission-graph-token-attribution.spec.js against
// `MissionGraphLens` (`#mission=<id>`). Same #1626 three-state local/cloud/
// unknown attribution this port carries verbatim in `graph.ts`
// (`applyRecordToMetrics`/`missionTotals`/`seedMetricsFromGraph`) — only the
// header meter's selector (`.missionlens .mmeter`) changed.
const { test, expect } = require('@playwright/test');

const MISSION_ID = 'm-tok';
const TODAY = new Date().toISOString().slice(0, 10);
const BACKFILL_RE = /\/flow\/\d{4}-\d{2}-\d{2}(?!\/stream)(\?.*)?$/;
const STREAM_RE = /\/flow\/\d{4}-\d{2}-\d{2}\/stream(\?.*)?$/;
const MISSION_RE = /\/flow-mission\/[^/?]+(\?.*)?$/;

const GRAPH = {
  mission_id: MISSION_ID,
  mission_status: 'active',
  nodes: [
    { id: 'phase-a', kind: 'phase', label: 'Adjudicate', status: 'running', depth: 0, steps: [] },
    {
      id: 'task-1', kind: 'task', label: 'Judge', parentId: 'phase-a', status: 'running', depth: 0,
      steps: [
        { id: 'judge-1', kind: 'review.judge', label: 'Judge', status: 'running' },
        { id: 'judge-2', kind: 'review.judge', label: 'Judge 2', status: 'error' },
        { id: 'local-1', kind: 'dispatch.internal', label: 'Local', status: 'complete' },
      ],
    },
  ],
  edges: [],
  generated_at_ms: 0,
};

const rec = (over) => ({ ts: `${TODAY}T10:00:00Z`, level: 'info', category: 'work', tier: 'local', stage: 'dispatch', handle: 'h', ...over });

const RECORDS = [
  rec({ action: 'dispatch start', session_id: 'step-judge-1', payload: {} }),
  rec({ action: 'dispatch start', session_id: 'step-judge-2', payload: {} }),
  rec({ action: 'dispatch start', session_id: 'step-local-1', payload: {} }),
  rec({ action: 'telemetry.tokens', category: 'telemetry', source: 'tokens', session_id: 'step-judge-1', payload: { total_tokens: 5000 } }),
  rec({ action: 'telemetry.tokens', category: 'telemetry', source: 'tokens', session_id: 'step-judge-2', payload: { total_tokens: 7000 } }),
  rec({ action: 'dispatch error', session_id: 'step-judge-2', payload: {} }),
  rec({ action: 'telemetry.tokens', category: 'telemetry', source: 'tokens', session_id: 'step-local-1', payload: { total_tokens: 3000 } }),
  rec({ action: 'dispatch complete', session_id: 'step-local-1', payload: {} }),
];

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
  return errors;
}

test('tokens with no endpoint evidence are not credited to local', async ({ page }) => {
  const errors = await open(page, RECORDS);
  const meter = (await page.locator('.missionlens .mmeter').innerText()).replace(/\s+/g, ' ');

  // (#2332) The header shows the total and the cloud share; the three-way
  // split lives in the meter's tooltip. The #1607 guard is unchanged in
  // spirit: cloud tokens must never be counted as local.
  const split = await page.locator('.missionlens .mmeter').getAttribute('title');
  expect(meter, `meter read: ${meter}`).toContain('15k tok');
  expect(meter, 'no attribution word in the headline when nothing is cloud').not.toContain('local');
  expect(split, `split read: ${split}`).toContain('3.0k local');
  expect(split).toContain('unattributed');
  expect(split, 'the whole 15k must never read as local — that is the #1607 defect').not.toContain('15.0k local');

  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});

test('the split is shown even when nothing is attributed to cloud', async ({ page }) => {
  const errors = await open(page, RECORDS.filter((r) => r.session_id !== 'step-local-1'));
  const meter = (await page.locator('.missionlens .mmeter').innerText()).replace(/\s+/g, ' ');
  const split = await page.locator('.missionlens .mmeter').getAttribute('title');
  expect(split, `split read: ${split}`).toContain('unattributed');
  expect(split).toContain('0 local');
  expect(meter, 'no cloud share → no attribution word in the headline').not.toContain('cloud');
  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});

test('a page opened AFTER the run agrees with one watched live', async ({ page }) => {
  const errors = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  const finished = JSON.parse(JSON.stringify(GRAPH));
  finished.nodes[1].steps = [
    { id: 'local-1', kind: 'dispatch.internal', label: 'Local', status: 'complete', startedTs: 1700000000, tokensFinal: 3000, localOk: true },
    { id: 'judge-1', kind: 'review.judge', label: 'Judge', status: 'complete', startedTs: 1700000000, tokensFinal: 5000, cloud: true },
    { id: 'judge-2', kind: 'review.judge', label: 'Judge 2', status: 'error', startedTs: 1700000000, tokensFinal: 7000 },
  ];
  await page.route(`**/mission/${MISSION_ID}/graph.json*`, (r) => r.fulfill({ contentType: 'application/json', body: JSON.stringify(finished) }));
  await page.route(MISSION_RE, (r) => r.fulfill({ contentType: 'application/json', body: JSON.stringify({ records: [], count: 0, truncated: false, generated_at_ms: 0 }) }));
  await page.route(BACKFILL_RE, (r) => r.fulfill({ contentType: 'application/json', body: '[]' }));
  await page.route(STREAM_RE, (r) => r.fulfill({ contentType: 'text/event-stream', body: '' }));
  await page.setViewportSize({ width: 1280, height: 900 });
  await page.goto(`/index-live.html#mission=${MISSION_ID}`);
  await expect(page.locator('.missionlens .mmeter')).toBeVisible();

  const meter = (await page.locator('.missionlens .mmeter').innerText()).replace(/\s+/g, ' ');
  const split = await page.locator('.missionlens .mmeter').getAttribute('title');
  expect(meter, `meter read: ${meter}`).toContain('5.0k cloud');
  expect(split, `split read: ${split}`).toContain('3.0k local');
  expect(split, 'the errored hosted seat stays unattributed').toContain('unattributed');
  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});
