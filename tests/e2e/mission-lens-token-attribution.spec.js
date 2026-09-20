// #1868 packet 2 retarget of mission-graph-token-attribution.spec.js against
// `MissionGraphLens` (`#mission=<id>`).
//
// (#2834) The #1626 three-state local/cloud/unknown attribution these tests
// were written against is WITHDRAWN from every rendered surface: it keyed on
// endpoint presence, which is not a cost fact. The flag survives in `graph.ts`
// as data; what these now assert is that the meter states a total and makes
// no claim about where the tokens ran. Tracked for a real design in #1521.
//
// Formerly: same #1626 three-state attribution this port carried in `graph.ts`
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

test('the meter states the total and makes no attribution claim (#2834)', async ({ page }) => {
  const errors = await open(page, RECORDS);
  const meter = (await page.locator('.missionlens .mmeter').innerText()).replace(/\s+/g, ' ');

  // (#2834) The local/cloud/unattributed split is withdrawn from both the
  // headline and the tooltip. It keyed on endpoint presence, which says a
  // model was reached over HTTP and nothing about what it cost — an
  // inference server on 127.0.0.1 has an endpoint. Moving the split into a
  // tooltip would not have made it true, so the tooltip is gone too.
  //
  // #1607's defect ("the whole 15k reads as local") is now unreachable by
  // construction rather than by guard: nothing here says local at all.
  // `15k`, not `15.00k`: the mission meter formats through `graph.ts`'s own
  // `fmtTok`, which drops to zero decimals at >=10,000 — NOT through
  // `lib/format.ts`'s `fmtN`, which #2842 gave two decimals in the
  // thousands. The two surfaces have separate formatters, and only the
  // fleet hero's moved here. #2845 tracks whether the mission meter should
  // follow; this assertion states what this surface renders TODAY so that a
  // change to it is a deliberate rebaseline rather than a silent drift.
  expect(meter, `meter read: ${meter}`).toContain('15k tok');
  for (const claim of ['local', 'cloud', 'unattributed']) {
    expect(meter.toLowerCase(), `meter must not claim "${claim}": ${meter}`).not.toContain(claim);
  }
  expect(
    await page.locator('.missionlens .mmeter').getAttribute('title'),
    'the split tooltip is withdrawn, not relocated',
  ).toBeNull();

  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});

test('drops a seat and still states only a total (#2834)', async ({ page }) => {
  const errors = await open(page, RECORDS.filter((r) => r.session_id !== 'step-local-1'));
  const meter = (await page.locator('.missionlens .mmeter').innerText()).replace(/\s+/g, ' ');
  // The figure changes with the records; the CLAIM does not — whatever the
  // mix of seats, the meter says how many tokens, never where they ran.
  expect(meter, `meter read: ${meter}`).toMatch(/tok\b/);
  for (const claim of ['local', 'cloud', 'unattributed']) {
    expect(meter.toLowerCase(), `meter must not claim "${claim}": ${meter}`).not.toContain(claim);
  }
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
  // (#2834) The point of this test is AGREEMENT between a page opened after
  // the run and one that watched it live — that the seeded-from-graph path
  // and the streamed path reach the same number. The number is the total
  // (3000 + 5000 + 7000); the split it used to assert is withdrawn.
  // `15k`, not `15.00k`: the mission meter formats through `graph.ts`'s own
  // `fmtTok`, which drops to zero decimals at >=10,000 — NOT through
  // `lib/format.ts`'s `fmtN`, which #2842 gave two decimals in the
  // thousands. The two surfaces have separate formatters, and only the
  // fleet hero's moved here. #2845 tracks whether the mission meter should
  // follow; this assertion states what this surface renders TODAY so that a
  // change to it is a deliberate rebaseline rather than a silent drift.
  expect(meter, `meter read: ${meter}`).toContain('15k tok');
  for (const claim of ['local', 'cloud', 'unattributed']) {
    expect(meter.toLowerCase(), `meter must not claim "${claim}": ${meter}`).not.toContain(claim);
  }
  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});
