// The mission-graph header's local/cloud token split (#1626).
//
// #1607 fixed "unattributable tokens default to local" in viewer.html's savings
// hero. `mission-graph.html` computes the SAME split independently and never
// got the fix — two surfaces deriving one number, so the correction landed in
// one and not the other.
//
// The old rule was `endpoint present -> cloud, absent -> local`, and that
// DEFAULT was the defect: "could not determine" was reported as "off the
// meter", the one direction this number must never err in.
//
// A hosted step's `dispatch start` carries no endpoint — only a clean
// `dispatch complete` does. So:
//   - a hosted seat read as LOCAL for its entire live duration, then jumped;
//   - a hosted seat that ERRORED never emitted an endpoint at all, so its
//     tokens were credited to local PERMANENTLY.
//
// Remote-budget exhaustion makes the errored case ordinary, not exotic.
const fs = require('fs');
const path = require('path');
const { test, expect } = require('@playwright/test');

const MISSION_ID = 'm-tok';
const GRAPH_PATH = `**/mission/${MISSION_ID}/graph`;
const TODAY = new Date().toISOString().slice(0, 10);
const BACKFILL_RE = /\/flow\/\d{4}-\d{2}-\d{2}(\?.*)?$/;
const STREAM_RE = /\/flow\/\d{4}-\d{2}-\d{2}\/stream(\?.*)?$/;

const MISSION_GRAPH_HTML = fs.readFileSync(
  path.join(__dirname, '..', '..', 'crates', 'darkmux-serve', 'assets', 'mission-graph.html'),
  'utf8'
);

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

const rec = (over) => ({
  ts: `${TODAY}T10:00:00Z`, level: 'info', category: 'work', tier: 'local',
  stage: 'dispatch', handle: 'h', ...over,
});

// Each seat gets a `dispatch start` FIRST — required by the #1481 phantom-token
// gate (tokens only fold once a step has started), and faithful besides: a
// hosted seat's start carries NO endpoint, which is exactly what made it read
// as local for its whole live duration.
//
// 5000 tokens on a hosted seat still RUNNING (no terminal yet, so no endpoint
// has ever been written), 7000 on a hosted seat that ERRORED (no endpoint,
// ever), 3000 on a local seat that completed cleanly.
const RECORDS = [
  rec({ action: 'dispatch start', session_id: 'step-judge-1', payload: {} }),
  rec({ action: 'dispatch start', session_id: 'step-judge-2', payload: {} }),
  rec({ action: 'dispatch start', session_id: 'step-local-1', payload: {} }),
  rec({ action: 'telemetry.tokens', category: 'telemetry', source: 'tokens',
        session_id: 'step-judge-1', payload: { total_tokens: 5000 } }),
  rec({ action: 'telemetry.tokens', category: 'telemetry', source: 'tokens',
        session_id: 'step-judge-2', payload: { total_tokens: 7000 } }),
  rec({ action: 'dispatch error', session_id: 'step-judge-2', payload: {} }),
  rec({ action: 'telemetry.tokens', category: 'telemetry', source: 'tokens',
        session_id: 'step-local-1', payload: { total_tokens: 3000 } }),
  // The ONLY positive evidence of local: a clean terminal with no endpoint.
  rec({ action: 'dispatch complete', session_id: 'step-local-1', payload: {} }),
];

async function open(page, records) {
  const errors = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  await page.route(GRAPH_PATH, (r) =>
    r.fulfill({ contentType: 'text/html; charset=utf-8', body: MISSION_GRAPH_HTML })
  );
  await page.route(`**/mission/${MISSION_ID}/graph.json*`, (r) =>
    r.fulfill({ contentType: 'application/json', body: JSON.stringify(GRAPH) })
  );
  await page.route(BACKFILL_RE, (r) => r.fulfill({ contentType: 'application/json', body: '[]' }));
  // Records ride the STREAM, matching mission-graph-truth.spec.js — the
  // backfill path does not fold into the per-step metric accumulator.
  let hits = 0;
  await page.route(STREAM_RE, (r) => {
    const body = hits++ === 0 ? records.map((x) => `data: ${JSON.stringify(x)}\n\n`).join('') : '';
    r.fulfill({ contentType: 'text/event-stream', body });
  });
  await page.setViewportSize({ width: 1280, height: 900 });
  await page.goto(`/mission/${MISSION_ID}/graph`);
  await expect(page.locator('.mmeter')).toBeVisible();
  return errors;
}

test('tokens with no endpoint evidence are not credited to local', async ({ page }) => {
  const errors = await open(page, RECORDS);
  const meter = (await page.locator('.mmeter').innerText()).replace(/\s+/g, ' ');

  // Only the seat with a clean, endpoint-free terminal is local: 3000.
  // The running hosted seat (5000) and the errored hosted seat (7000) have no
  // endpoint evidence either way, so 12000 is unattributed — and must be SAID,
  // not silently added to the off-the-meter claim.
  expect(meter, `meter read: ${meter}`).toContain('3.0k local');
  expect(meter).toContain('unattributed');
  expect(
    meter,
    'the whole 15k must never read as local — that is the #1607 defect'
  ).not.toContain('15.0k local');

  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});

test('the split is shown even when nothing is attributed to cloud', async ({ page }) => {
  // The old render gated the breakdown on `tot.cloud` being non-zero, so a
  // mission whose tokens were ENTIRELY unattributed showed a bare total with
  // no hint that none of it had been attributed — the same silence the two-way
  // split produced, one layer up.
  const errors = await open(page, RECORDS.filter((r) => r.session_id !== 'step-local-1'));
  const meter = (await page.locator('.mmeter').innerText()).replace(/\s+/g, ' ');
  expect(meter, `meter read: ${meter}`).toContain('unattributed');
  expect(meter).toContain('0 local');
  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});

test('a page opened AFTER the run agrees with one watched live', async ({ page }) => {
  // (#1626 follow-up) SSE is tail-from-now, so opening a finished run's graph
  // gets its totals from the server BACKFILL, not the live fold. The first cut
  // of this fix touched only the live path — `seedMetricsFromGraph` set no
  // `localOk` at all — so every step on a page opened after the fact would have
  // read as unattributed. That replaces a wrong number with a useless one,
  // which an operator would reasonably report as a NEW regression.
  //
  // Here the snapshot carries the server's own evidence and NO stream records
  // arrive, so this drives the backfill path exclusively.
  const errors = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  const finished = JSON.parse(JSON.stringify(GRAPH));
  finished.nodes[1].steps = [
    // Server says: clean terminal, no endpoint -> definitively local.
    { id: 'local-1', kind: 'dispatch.internal', label: 'Local', status: 'complete',
      startedTs: 1700000000, tokensFinal: 3000, localOk: true },
    // Server says: hosted -> cloud.
    { id: 'judge-1', kind: 'review.judge', label: 'Judge', status: 'complete',
      startedTs: 1700000000, tokensFinal: 5000, cloud: true },
    // Server says NEITHER -> unknown. An errored hosted seat looks exactly
    // like this, and must not be credited to local.
    { id: 'judge-2', kind: 'review.judge', label: 'Judge 2', status: 'error',
      startedTs: 1700000000, tokensFinal: 7000 },
  ];
  await page.route(GRAPH_PATH, (r) =>
    r.fulfill({ contentType: 'text/html; charset=utf-8', body: MISSION_GRAPH_HTML })
  );
  await page.route(`**/mission/${MISSION_ID}/graph.json*`, (r) =>
    r.fulfill({ contentType: 'application/json', body: JSON.stringify(finished) })
  );
  await page.route(BACKFILL_RE, (r) => r.fulfill({ contentType: 'application/json', body: '[]' }));
  await page.route(STREAM_RE, (r) => r.fulfill({ contentType: 'text/event-stream', body: '' }));
  await page.setViewportSize({ width: 1280, height: 900 });
  await page.goto(`/mission/${MISSION_ID}/graph`);
  await expect(page.locator('.mmeter')).toBeVisible();

  const meter = (await page.locator('.mmeter').innerText()).replace(/\s+/g, ' ');
  expect(meter, `meter read: ${meter}`).toContain('3.0k local');
  expect(meter).toContain('5.0k cloud');
  expect(meter, 'the errored hosted seat stays unattributed').toContain('unattributed');
  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});
