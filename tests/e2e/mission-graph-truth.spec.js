// (#1483/#1481) Headless e2e for the mission-graph "graph tells the truth"
// batch — three viewer fixes that only reproduce against REAL wire data:
//
//   Bug 1 (elapsed units): the server stamps a step's `startedTs` in SECONDS,
//     but `tsToMs` returned a numeric epoch as-is and the elapsed math is in
//     ms — so `now(ms) - startedTs(seconds)` rendered "495161:10:35". This
//     fixture uses a genuine SECONDS epoch (the e2e's own live-mock elsewhere
//     uses ms and structurally can't catch this).
//   Bug 2 (model chip): the `.smodel` seat chip was CSS-crushed to one char by
//     Bug 1's oversized elapsed string eating the row width. With a sane
//     elapsed the full model name renders.
//   Bug 4 (phantom tokens): a live `telemetry.tokens` record resolving (via the
//     session->step map) onto a PLANNED, not-yet-started step must NOT pre-load
//     a token meter. A started step's tokens still fold (positive control).
//
// The lens route-mocks its three same-origin data sources (graph.json snapshot,
// /flow/<date> backfill, the SSE stream) exactly like mission-graph-events.spec.
const fs = require('fs');
const path = require('path');
const { test, expect } = require('@playwright/test');

const MISSION_ID = 'graph-truth';
const GRAPH_PATH = `**/mission/${MISSION_ID}/graph`;
const TODAY = new Date().toISOString().slice(0, 10);
const BACKFILL_RE = /\/flow\/\d{4}-\d{2}-\d{2}(\?.*)?$/;
const STREAM_RE = /\/flow\/\d{4}-\d{2}-\d{2}\/stream(\?.*)?$/;

const MISSION_GRAPH_HTML = fs.readFileSync(
  path.join(__dirname, '.served', 'mission-graph.html'),
  'utf8'
);

// A judge seat that STARTED ~95s ago, stamped as a SECONDS epoch — exactly the
// server wire shape (`now_unix()` = `as_secs()`). Plus a PLANNED verify seat
// (no startedTs) that shares the deep 35b model with the judge — the live
// collision that leaked judge tokens onto the not-yet-started verify.
const STARTED_SECS = Math.floor(Date.now() / 1000) - 95;
function graphSnapshot() {
  return {
    mission_id: MISSION_ID,
    mission_status: 'active',
    nodes: [
      { id: 'phase-a', kind: 'phase', label: 'Review', status: 'running', depth: 0, steps: [] },
      {
        id: 'task-1', kind: 'task', label: 'Judge Wave', parentId: 'phase-a',
        status: 'running', depth: 0,
        steps: [
          {
            id: 'judge-1', kind: 'review.judge', label: 'Judge', status: 'running',
            startedTs: STARTED_SECS, model: 'darkmux:gpt-oss-120b',
          },
          {
            id: 'verify-1', kind: 'review.verify', label: 'Verify', status: 'planned',
            model: 'darkmux:devstral-small-2-2512',
          },
        ],
      },
    ],
    edges: [],
    generated_at_ms: 0,
  };
}

// A token telemetry record for the RUNNING judge (must fold) and one for the
// PLANNED verify (must be gated out — the phantom).
const judgeTok = {
  ts: `${TODAY}T10:00:00Z`, action: 'telemetry.tokens', category: 'telemetry',
  source: 'tokens', session_id: 'step-judge-1', level: 'info', payload: { total_tokens: 5000 },
};
const verifyTok = {
  ts: `${TODAY}T10:00:01Z`, action: 'telemetry.tokens', category: 'telemetry',
  source: 'tokens', session_id: 'step-verify-1', level: 'info', payload: { total_tokens: 18000 },
};

async function routeAll(page, streamRecords) {
  await page.route(GRAPH_PATH, (r) =>
    r.fulfill({ contentType: 'text/html; charset=utf-8', body: MISSION_GRAPH_HTML })
  );
  await page.route(`**/mission/${MISSION_ID}/graph.json`, (r) =>
    r.fulfill({ contentType: 'application/json', body: JSON.stringify(graphSnapshot()) })
  );
  await page.route(BACKFILL_RE, (r) =>
    r.fulfill({ contentType: 'application/json', body: '[]' })
  );
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

  // Narrow viewport -> the mobile TIMELINE renderer (the surface the live bugs
  // were watched on), the same one the review screenshots capture at ~390px.
  await page.setViewportSize({ width: 390, height: 900 });
  await routeAll(page, [judgeTok, verifyTok]);
  await page.goto(`/mission/${MISSION_ID}/graph`);

  // Expand the task so its step rows (model chip + per-seat meter) render.
  const taskHd = page.locator('.tltask .tlt-hd');
  await taskHd.first().click();
  const judgeRow = page.locator('.tlt-step', { has: page.locator('.smodel', { hasText: 'gpt-oss-120b' }) });
  const verifyRow = page.locator('.tlt-step', { has: page.locator('.smodel', { hasText: 'devstral-small-2-2512' }) });
  await expect(judgeRow).toHaveCount(1);
  await expect(verifyRow).toHaveCount(1);

  // ── Bug 1: the running judge's elapsed clock is a sane m:ss, never the
  // 6-digit-hours nonsense the seconds/ms unit mismatch produced. ──
  const elapsed = (await judgeRow.locator('.mn-step-meter .gen').innerText()).trim();
  expect(elapsed).toMatch(/^\d{1,2}:\d{2}$/);
  // ~95s in -> 1:3x/1:4x. Certainly under an hour; the bug rendered 495161:..
  expect(elapsed).not.toMatch(/^\d{3,}:/);

  // ── Bug 2: the seat chip shows the FULL model name (fmtModel strips the
  // `darkmux:` namespace), not a single squished character. ──
  await expect(judgeRow.locator('.smodel')).toHaveText('gpt-oss-120b');
  await expect(verifyRow.locator('.smodel')).toHaveText('devstral-small-2-2512');

  // ── Bug 4: the PLANNED verify seat's 18k phantom is gated out — its meter is
  // the idle placeholder, never a token count. The RUNNING judge's own 5k DOES
  // fold (positive control: the gate blocks not-yet-started steps only). ──
  await expect(judgeRow.locator('.mn-step-meter .tok')).toContainText('tok');
  await expect(verifyRow.locator('.mn-step-meter .idle')).toHaveCount(1);
  await expect(verifyRow.locator('.mn-step-meter .tok')).toHaveCount(0);
  await expect(page.locator('.tltasks').first()).not.toContainText('18k');

  // The DESKTOP canvas node view is where the `.smodel` chip is a flex-shrink
  // sibling of the meter — the surface Bug 1's oversized elapsed string crushed
  // the chip to one char on. With the elapsed sane, the chip renders at its
  // designed width (the `darkmux:`-stripped name, ellipsized only by its own
  // 86px cap, never by a runaway sibling).
  await page.setViewportSize({ width: 1280, height: 900 });
  const canvasChip = page.locator('.mnode .mn-step-row .smodel', { hasText: 'gpt-os' }).first();
  await expect(canvasChip).toBeVisible();
  const box = await canvasChip.boundingBox();
  expect(box.width).toBeGreaterThan(30);

  expect(pageErrors).toEqual([]);
});
