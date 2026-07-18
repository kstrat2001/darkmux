// (#1483 emit half) Headless e2e for live turn/tool-call metrics on AGENTIC
// seat cards. The render half (turnRun/turnFinal fold) shipped in #1485/#1488;
// this proves the emit half wires end-to-end against REAL wire data:
//
//   * An AGENTIC seat (mission.coder) whose live `dispatch.turn` / `dispatch.tool`
//     records carry `payload.step_id` attributes to the seat card even though its
//     `session_id` is a shared `mission-run-<…>` session, NOT the `step-<id>`
//     default the viewer's session->step map resolves. Turns + tool-calls tick.
//   * The AUTHORITATIVE running count rides in the record (`turns_so_far` /
//     `tool_calls_so_far`): a single record stamped `turns_so_far: 3` reads "3
//     turns", not "1 turn" — the mid-dispatch-open catch-up the emit half adds.
//   * A SINGLE-SHOT seat (review.probe) legitimately has no turns/tools — it
//     emits only `telemetry.tokens`, so its card shows tokens and NEVER a turn/
//     tool count. Task-type-aware by construction.
//   * The running-only gate still holds: a `dispatch.turn` resolving onto a
//     PLANNED seat (no startedTs) does NOT tick — same #1481 Bug-4 gate the
//     token fold uses, now covering turns/tools.
//
// Route-mocks the three same-origin data sources exactly like the sibling
// mission-graph specs; the page shell + vendor bundle load from `.served`.
const fs = require('fs');
const path = require('path');
const { test, expect } = require('@playwright/test');

const MISSION_ID = 'agentic-turns';
const GRAPH_PATH = `**/mission/${MISSION_ID}/graph`;
const TODAY = new Date().toISOString().slice(0, 10);
const BACKFILL_RE = /\/flow\/\d{4}-\d{2}-\d{2}(\?.*)?$/;
const STREAM_RE = /\/flow\/\d{4}-\d{2}-\d{2}\/stream(\?.*)?$/;

const MISSION_GRAPH_HTML = fs.readFileSync(
  path.join(__dirname, '.served', 'mission-graph.html'),
  'utf8'
);

// Seconds epoch — the server wire shape (`now_unix()` = `as_secs()`). The coder
// + probe seats STARTED ~40s ago; the verify seat is PLANNED (no startedTs).
const STARTED_SECS = Math.floor(Date.now() / 1000) - 40;
function graphSnapshot() {
  return {
    mission_id: MISSION_ID,
    mission_status: 'active',
    nodes: [
      { id: 'phase-a', kind: 'phase', label: 'Build', status: 'running', depth: 0, steps: [] },
      {
        id: 'task-1', kind: 'task', label: 'Coder Phase', parentId: 'phase-a',
        status: 'running', depth: 0,
        steps: [
          {
            id: 'coder-1', kind: 'mission.coder', label: 'Coder', status: 'running',
            startedTs: STARTED_SECS, model: 'darkmux:qwen3-coder-next',
          },
          {
            id: 'probe-1', kind: 'review.probe', label: 'Probe', status: 'running',
            startedTs: STARTED_SECS, model: 'darkmux:gpt-oss-120b',
          },
          {
            id: 'verify-1', kind: 'mission.verify', label: 'Verify', status: 'planned',
            model: 'darkmux:devstral-small-2-2512',
          },
        ],
      },
    ],
    edges: [],
    generated_at_ms: 0,
  };
}

// AGENTIC coder seat — a shared mission-run session (NOT `step-coder-1`), so the
// records ONLY attribute via the stamped `payload.step_id`. One turn record
// stamped `turns_so_far: 3` (the authoritative running count a mid-dispatch-open
// page reads) + one tool record stamped `tool_calls_so_far: 2`.
const CODER_SESS = 'mission-run-agentic-turns-build-abc';
const coderTurn = {
  ts: `${TODAY}T10:00:00Z`, action: 'dispatch.turn', category: 'work', session_id: CODER_SESS,
  level: 'info', handle: 'coder', payload: { step_id: 'coder-1', turns_so_far: 3, turn_seq: 3 },
};
const coderTool = {
  ts: `${TODAY}T10:00:01Z`, action: 'dispatch.tool', category: 'work', session_id: CODER_SESS,
  level: 'info', handle: 'coder', payload: { step_id: 'coder-1', tool_calls_so_far: 2, tool_name: 'edit' },
};
// Single-shot probe seat — tokens only, never turns/tools.
const probeTok = {
  ts: `${TODAY}T10:00:02Z`, action: 'telemetry.tokens', category: 'telemetry', source: 'tokens',
  session_id: 'step-probe-1', level: 'info', payload: { total_tokens: 4000 },
};
// A turn record resolving onto the PLANNED verify seat — must be gated (started
// gate): the seat has no startedTs, so its live turns must NOT tick.
const verifyPhantomTurn = {
  ts: `${TODAY}T10:00:03Z`, action: 'dispatch.turn', category: 'work', session_id: CODER_SESS,
  level: 'info', handle: 'coder', payload: { step_id: 'verify-1', turns_so_far: 5, turn_seq: 5 },
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

test('agentic seat ticks turns + tool-calls; single-shot shows none; planned stays gated', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));

  // Mobile timeline renderer — the surface the live seat cards are watched on.
  await page.setViewportSize({ width: 390, height: 900 });
  await routeAll(page, [coderTurn, coderTool, probeTok, verifyPhantomTurn]);
  await page.goto(`/mission/${MISSION_ID}/graph`);

  const taskHd = page.locator('.tltask .tlt-hd');
  await taskHd.first().click();
  const coderRow = page.locator('.tlt-step', { has: page.locator('.smodel', { hasText: 'qwen3-coder-next' }) });
  const probeRow = page.locator('.tlt-step', { has: page.locator('.smodel', { hasText: 'gpt-oss-120b' }) });
  const verifyRow = page.locator('.tlt-step', { has: page.locator('.smodel', { hasText: 'devstral-small-2-2512' }) });
  await expect(coderRow).toHaveCount(1);
  await expect(probeRow).toHaveCount(1);
  await expect(verifyRow).toHaveCount(1);

  // ── Agentic seat: turns + tool-calls tick, off the AUTHORITATIVE running
  // count (one record each, stamped 3 and 2 — the += path would show 1). ──
  const coderMeter = coderRow.locator('.mn-step-meter');
  await expect(coderMeter).toContainText('3 turns');
  await expect(coderMeter).toContainText('2 tools');

  // ── Single-shot probe: tokens fold, but NEVER a turn/tool count. ──
  const probeMeter = probeRow.locator('.mn-step-meter');
  await expect(probeMeter.locator('.tok')).toContainText('tok');
  await expect(probeMeter).not.toContainText('turn');
  await expect(probeMeter).not.toContainText('tool');

  // ── Planned verify seat: the phantom turn is gated (no startedTs) — the meter
  // is the idle placeholder, never a turn/tool/token count. ──
  await expect(verifyRow.locator('.mn-step-meter .idle')).toHaveCount(1);
  await expect(verifyRow.locator('.mn-step-meter')).not.toContainText('turn');
  await expect(verifyRow.locator('.mn-step-meter')).not.toContainText('tool');

  expect(pageErrors).toEqual([]);
});
