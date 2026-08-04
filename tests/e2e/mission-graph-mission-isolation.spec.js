// (#1641) Cross-mission isolation for the mission-graph viewer.
//
// The bug: two missions launched from the SAME config share every
// CONFIG-scoped identity a flow record carries — `session_id`/`handle` are
// literal strings straight out of the mission config document (e.g.
// `session_id::task("review-probe-mid-task")` = "task-review-probe-mid-task",
// `handle: "review-probe-mid-step"`), byte-identical across concurrent runs.
// Before #1641, most producers also left `mission_id` unset, so the viewer's
// per-step metric fold (`applyRecordToMetrics` -> `stepForRecord`) and the
// events panel (`recordInMission`) had NOTHING to disambiguate on and fell
// back to the session_id/handle proxy match ALONE — one mission's step
// absorbed another's token counts, and a dead step got resurrected to
// "running" by the other mission's heartbeat.
//
// The fix is two-sided: producers now stamp `mission_id` wherever the
// launcher knows it (Rust-side, covered by mission_launch.rs's and
// review.rs's own unit tests), and the viewer now treats a PRESENT
// `mission_id` as authoritative — never falling through to the proxy match
// when it's there and wrong — while an ABSENT `mission_id` (every pre-#1641
// record, and anything a producer still misses) keeps working exactly as
// before. This spec proves the viewer half: a record stamped for mission B
// must not move mission A's numbers, and a legacy record with no mission_id
// at all must still correlate.
const fs = require('fs');
const path = require('path');
const { test, expect } = require('@playwright/test');

const MISSION_ID = 'm-a';
const OTHER_MISSION_ID = 'm-b';
const GRAPH_PATH = `**/mission/${MISSION_ID}/graph`;
const TODAY = new Date().toISOString().slice(0, 10);
const BACKFILL_RE = /\/flow\/\d{4}-\d{2}-\d{2}(\?.*)?$/;
const STREAM_RE = /\/flow\/\d{4}-\d{2}-\d{2}\/stream(\?.*)?$/;

const MISSION_GRAPH_HTML = fs.readFileSync(
  path.join(__dirname, '..', '..', 'crates', 'darkmux-serve', 'assets', 'mission-graph.html'),
  'utf8'
);

// One phase, one task, one step — deliberately named the way a review
// pipeline's probe stage would be (`s1`/`t1` stand in for the real
// `review-probe-mid-step`/`review-probe-mid-task`), so the collision this
// spec drives is the same SHAPE as the real bug: two missions, one config,
// one shared session_id/handle pair.
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

const rec = (over) => ({
  ts: `${TODAY}T10:00:00Z`, level: 'info', category: 'work', tier: 'local',
  stage: 'dispatch', handle: 's1', session_id: 'step-s1', ...over,
});

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
  let hits = 0;
  await page.route(STREAM_RE, (r) => {
    const body = hits++ === 0 ? records.map((x) => `data: ${JSON.stringify(x)}\n\n`).join('') : '';
    r.fulfill({ contentType: 'text/event-stream', body });
  });
  await page.setViewportSize({ width: 1280, height: 900 });
  await page.goto(`/mission/${MISSION_ID}/graph`);
  await expect(page.locator('.mmeter')).toBeVisible();
  await page.waitForSelector('.evpanel');
  return errors;
}

test('a record stamped for a DIFFERENT mission does not move this mission\'s step metrics or events', async ({ page }) => {
  const RECORDS = [
    // s1 (THIS mission, m-a) starts, then reports 4000 tokens correctly
    // stamped, then closes clean (no endpoint -> local).
    rec({ action: 'dispatch start', mission_id: MISSION_ID, payload: {} }),
    rec({
      action: 'telemetry.tokens', category: 'telemetry', source: 'tokens',
      mission_id: MISSION_ID, payload: { total_tokens: 4000 },
    }),
    // The collision: a record for the SAME session_id/handle (the config-
    // scoped identity two missions launched from one config would share)
    // but stamped for a DIFFERENT mission (m-b) — exactly what a concurrent
    // run of the same config would emit. Its 9000 tokens must NOT fold into
    // m-a's total, and it must not appear in m-a's events panel.
    rec({
      action: 'telemetry.tokens', category: 'telemetry', source: 'tokens',
      mission_id: OTHER_MISSION_ID, payload: { total_tokens: 9000 },
    }),
    rec({ action: 'dispatch complete', mission_id: OTHER_MISSION_ID, payload: {} }),
    // m-a's own step also closes clean, still correctly stamped.
    rec({ action: 'dispatch complete', mission_id: MISSION_ID, payload: {} }),
  ];
  const errors = await open(page, RECORDS);

  const meter = (await page.locator('.mmeter').innerText()).replace(/\s+/g, ' ');
  // Only m-a's 4000 folds in. If the mission-B record leaked through the old
  // proxy-only match, this would read "13.0k tok" (4000 + 9000). The local/
  // cloud split only renders once something is NOT cleanly local (#1626) —
  // this scenario is 100% local, so the bare total is the whole signal.
  expect(meter, `meter read: ${meter}`).toContain('4.0k tok');
  expect(meter, 'the leaked mission-B tokens must never appear in m-a\'s total').not.toContain('13.0k');
  expect(meter, 'the leaked mission-B tokens must never appear in m-a\'s total').not.toContain('9.0k');

  // The events panel: exactly the THREE records stamped for m-a (start,
  // tokens, complete) — the two mission-B rows are excluded, even though
  // they share s1's handle/session_id.
  await expect(page.locator('.evpanel .evrow')).toHaveCount(3);
  await expect(page.locator('.evpanel .evhd')).toContainText('events · 3');

  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});

test('a record with NO mission_id still correlates — the legacy path', async ({ page }) => {
  const RECORDS = [
    // Every record here is UNSTAMPED (no mission_id at all) — the shape of
    // every pre-#1641 record, and of any producer the fix doesn't yet
    // reach. These must keep matching by session_id/handle exactly as
    // before: this is not a stricter gate, only an authoritative one when
    // mission_id happens to be present.
    rec({ action: 'dispatch start', payload: {} }),
    rec({
      action: 'telemetry.tokens', category: 'telemetry', source: 'tokens',
      payload: { total_tokens: 2500 },
    }),
    rec({ action: 'dispatch complete', payload: {} }),
  ];
  const errors = await open(page, RECORDS);

  const meter = (await page.locator('.mmeter').innerText()).replace(/\s+/g, ' ');
  expect(meter, `meter read: ${meter}`).toContain('2.5k tok');

  await expect(page.locator('.evpanel .evrow')).toHaveCount(3);
  await expect(page.locator('.evpanel .evhd')).toContainText('events · 3');
  await expect(page.locator('.evpanel .evempty')).toHaveCount(0);

  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});
