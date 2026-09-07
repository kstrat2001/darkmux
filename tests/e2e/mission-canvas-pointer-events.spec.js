// #2204: "nothing inside a React Flow node receives pointer events, so
// step/node drill-in is dead for real clicks" — proven live by the operator
// against a real mission via `document.elementsFromPoint()`: a step row's
// own geometry sat inside both its node and the visible canvas, computed
// `pointer-events` read `all` on the row/`.mnode`/`.react-flow__node`, and
// yet the node subtree was entirely absent from the hit stack — every real
// (non-forced) Playwright click on a canvas step row or task node timed out,
// while a SYNTHETIC `element.click()` on the same row "succeeded" (the false
// green a jsdom/force-click test would report).
//
// ROOT CAUSE (confirmed by reverting the fix and re-running this test): React
// Flow's own `NodeWrapper` renders `visibility: initialized ? 'visible' :
// 'hidden'`, where `initialized = !!node.width && !!node.height`
// (`@reactflow/core`'s `NodeWrapper`/`createNodeInternals`). `MissionCanvas`
// drives React Flow in CONTROLLED mode and rebuilds its `nodes` array once a
// second (`MissionGraphLens`'s `setInterval(() => setNow(Date.now()), 1000)`
// clock tick) — `createNodeInternals` spreads `{...node}` on every rebuild,
// carrying `handleBounds` forward but NOT `width`/`height` unless the caller
// re-stamps them, so an unpatched rebuild un-measures every node for a frame,
// its `.react-flow__node` (and, since `visibility` inherits, everything
// under it — `.mnode`, every `.mn-step-row`) goes `visibility: hidden`, and
// `document.elementsFromPoint()` skips the whole subtree, falling through to
// `.react-flow__pane` underneath — exactly the operator's evidence. This
// recurs every ~1s, indefinitely, for as long as the canvas is mounted; a
// real click landing in that ~one-frame window is simply lost.
//
// THE FIX ALREADY ON MAIN: #2325 (commit 9bc469b7 / PR #2327, "the mission
// graph stays painted — a controlled `nodes` update dropped React Flow's own
// node measurements", merged 2026-09-04, five days after this issue was
// filed) added `measuredDims.ts`'s `recordDimensions`/`withMeasuredDimensions`,
// wired into `MissionCanvas.tsx` via `onNodesChange`/`dimsRef`, which stamps
// the LAST KNOWN measurement back onto every rebuilt node so `initialized`
// (and therefore `visibility`) stays stable across the periodic rebuild.
// #2325 was filed and fixed for a different visible symptom (the canvas
// painting then going blank) than #2204 (clicks silently failing) — but it
// is the SAME defect and the SAME fix, landed without anyone connecting the
// two issues. This test is the missing regression coverage: it proves BOTH
// that a real click reaches the row AND that hit-testing survives the
// periodic rebuild that caused the original failure — deterministically,
// not by hoping a click lands in or out of a ~one-frame window.
//
// Verification performed (not inferred): reverting `MissionCanvas.tsx`'s use
// of `withMeasuredDimensions` (bypassing the #2325 fix, i.e. reproducing the
// pre-#2325 code) and running this test's `elementsFromPoint` sampling loop
// showed 3 misses in 3.5s of real wall-clock ticking, each landing almost
// exactly on a 1000ms boundary and each reporting `.react-flow__pane` as the
// topmost hit — the operator's exact signature. Restoring the fix and
// re-running showed 0 misses across 421 sampled animation frames. See this
// file's own two phases below for what actually gates the suite.
const { test, expect } = require('@playwright/test');

const MISSION_ID = 'graph-pointer-2204';
const TODAY = '2026-06-15';

const BACKFILL_RE = /\/flow\/\d{4}-\d{2}-\d{2}(?!\/stream)(\?.*)?$/;
const STREAM_RE = /\/flow\/\d{4}-\d{2}-\d{2}\/stream(\?.*)?$/;
const MISSION_RE = /\/flow-mission\/[^/?]+(\?.*)?$/;

function graphSnapshot() {
  const nodes = [{ id: 'phase-a', kind: 'phase', label: 'Review', status: 'running', depth: 0, steps: [] }];
  const edges = [];
  // Several sibling task nodes, each with one step — the operator's own
  // "five CRAWL.UNIT nodes" shape, not a single trivial node the bug might
  // not have room to show up on.
  for (let i = 0; i < 5; i++) {
    nodes.push({
      id: `task-${i}`, kind: 'task', label: `CRAWL.UNIT-${i}`, parentId: 'phase-a', status: 'running', depth: i,
      steps: [{ id: `step-${i}`, kind: 'crawl.unit', label: `Unit ${i}`, status: 'running', startedTs: 0, model: 'darkmux:qwen3.6-35b-a3b' }],
    });
    edges.push({ id: `contains-${i}`, source: 'phase-a', target: `task-${i}`, kind: 'contains' });
    if (i > 0) edges.push({ id: `dep-${i}`, source: `task-${i - 1}`, target: `task-${i}`, kind: 'depends_on' });
  }
  return { mission_id: MISSION_ID, mission_status: 'active', nodes, edges, generated_at_ms: 0 };
}

// A dispatch bookend on task-0's step so `stepDispatchSessions` resolves a
// real dispatch id — a successful click navigates to `#dispatch=<id>`,
// giving the click phase (below) a concrete, checkable destination.
const dispatchStart = {
  ts: `${TODAY}T10:00:00Z`, action: 'dispatch.start', category: 'lifecycle', source: 'runtime',
  session_id: 'unit-session-0', mission_id: MISSION_ID, level: 'info', payload: { step_id: 'step-0' },
};

async function routeAll(page) {
  await page.route(`**/mission/${MISSION_ID}/graph.json`, (r) => r.fulfill({ contentType: 'application/json', body: JSON.stringify(graphSnapshot()) }));
  await page.route(MISSION_RE, (r) => r.fulfill({ contentType: 'application/json', body: JSON.stringify({ records: [], count: 0, truncated: false, generated_at_ms: 0 }) }));
  await page.route(BACKFILL_RE, (r) => r.fulfill({ contentType: 'application/json', body: '[]' }));
  let hits = 0;
  await page.route(STREAM_RE, (r) => {
    const first = hits++ === 0;
    r.fulfill({ contentType: 'text/event-stream', body: first ? `data: ${JSON.stringify(dispatchStart)}\n\n` : '' });
  });
}

test('canvas step rows stay hit-testable across the periodic clock-driven rebuild, and a real click navigates', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));

  // Desktop viewport: `viewMode==="auto"` + not-mobile resolves to the
  // CANVAS (graph) renderer per `timelineActive()` — the renderer this bug
  // is about. Deliberately NOT freezing the clock (no `page.clock`): the
  // regression this test guards is driven BY the real `setInterval` tick
  // that rebuilds `MissionCanvas`'s node array once a second — a frozen
  // clock would silence that tick and the whole test would prove nothing.
  await page.setViewportSize({ width: 1280, height: 900 });
  await routeAll(page);
  await page.goto(`/index-live.html#mission=${MISSION_ID}`);

  const firstRow = page.locator('.mnode .mn-step-row').first();
  await expect(firstRow).toBeVisible();

  // ── Phase 1 — deterministic regression guard ──────────────────────────
  // Sample `document.elementsFromPoint()` at every step row's own center on
  // every animation frame for long enough to span at least two of the
  // once-a-second rebuilds. A single `click()` call could get lucky (or
  // unlucky) against a ~one-frame window; this can't — it either sees the
  // node in the hit stack on EVERY frame, or it doesn't.
  const sample = await page.evaluate(() => {
    return new Promise((resolve) => {
      let frames = 0;
      let misses = 0;
      const missSamples = [];
      const start = performance.now();
      function tick() {
        frames++;
        for (const el of document.querySelectorAll('.mnode .mn-step-row')) {
          const r = el.getBoundingClientRect();
          if (r.width === 0 || r.height === 0) continue;
          const cx = r.left + r.width / 2;
          const cy = r.top + r.height / 2;
          const stack = document.elementsFromPoint(cx, cy);
          if (!stack.includes(el)) {
            misses++;
            if (missSamples.length < 5) {
              missSamples.push({ t: Math.round(performance.now() - start), top: (stack[0] && (stack[0].className || stack[0].tagName)) || null });
            }
          }
        }
        if (performance.now() - start < 2600) requestAnimationFrame(tick);
        else resolve({ frames, misses, missSamples });
      }
      requestAnimationFrame(tick);
    });
  });
  expect(sample.frames).toBeGreaterThan(30); // sanity: rAF actually ran
  expect(sample.misses, `step rows dropped out of the hit stack: ${JSON.stringify(sample.missSamples)}`).toBe(0);

  // ── Phase 2 — the user-facing proof ────────────────────────────────────
  // A REAL click, not `force: true` — Playwright's own actionability check
  // (which includes hit-testing) must pass on its own, and the resulting
  // navigation is the concrete behavior an operator experiences.
  const targetRow = page.locator('.mnode .mn-step-row').filter({ hasText: 'Unit 0' });
  await targetRow.click({ timeout: 5000 });
  await expect.poll(() => page.evaluate(() => location.hash)).toBe('#dispatch=unit-session-0');

  expect(pageErrors).toEqual([]);
});
