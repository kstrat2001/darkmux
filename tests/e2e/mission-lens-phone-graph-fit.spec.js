// #2376 — the mission graph on phones fit width-bound (React Flow's
// `fitView` sized the pane to the desktop layout's ~1378 flow-px width,
// landing at ~0.24 scale on a 358px-wide portrait pane) and never re-fit on
// rotation (`fitView` is an init-only boolean prop; nothing ever asked
// React Flow to recompute it after the pane resized).
//
// jsdom cannot measure a real `transform: scale(...)`, so — same as
// mission-lens-layout-geometry.spec.js — this lives here. Assertions read
// the ACTUAL rendered scale off `.react-flow__viewport`'s transform, not
// visibility: a graph can be "visible" while pinned at a stale, unreadable
// scale, which is exactly the bug this spec exists to catch.
//
// `hasTouch`/`isMobile` context options are required, not decoration:
// `useIsMobile`'s landscape-phone fallback gates on
// `matchMedia('(pointer: coarse)')`, which a plain viewport resize (no
// touch emulation) never sets — a probe run without these options measures
// desktop's `useIsMobile() === false` behavior at a phone-sized viewport,
// not the phone behavior the bug is actually about.
const { test, expect } = require('@playwright/test');

test.use({ hasTouch: true, isMobile: true });

const MISSION_ID = 'phone-graph-fit';

// The same 8-task, 3-phase graph mission-lens-layout-geometry.spec.js uses —
// large enough that the desktop layout's side-by-side depth columns really
// do run ~1378 flow-px wide, which is what forces the width-bound fit this
// spec is checking for.
function graphSnapshot() {
  const two = (id, label, parentId, depth) => ({
    id, kind: 'task', label, parentId, status: 'complete', depth,
    steps: [
      { id: `${id}-prompts`, kind: 'review.probe_prompts', label: 'Probe prompts', status: 'complete' },
      { id: `${id}-dispatch`, kind: 'dispatch.map', label: 'Dispatch (map)', status: 'complete', model: 'darkmux:qwen3.6-35b-a3b' },
    ],
  });
  return {
    mission_id: MISSION_ID,
    mission_status: 'finalized',
    nodes: [
      { id: 'phase-investigate', kind: 'phase', label: 'Investigate', status: 'complete', depth: 0, steps: [] },
      { id: 'bundle', kind: 'task', label: 'Bundle', parentId: 'phase-investigate', status: 'complete', depth: 0,
        steps: [{ id: 'bundle-1', kind: 'review.bundle', label: 'Bundle', status: 'complete' }] },
      two('probe-high', 'Probe high', 'phase-investigate', 1),
      two('probe-low', 'Probe low', 'phase-investigate', 1),
      two('probe-mid', 'Probe mid', 'phase-investigate', 1),
      { id: 'dedup', kind: 'task', label: 'Dedup', parentId: 'phase-investigate', status: 'complete', depth: 2,
        steps: [{ id: 'dedup-1', kind: 'review.dedup', label: 'Dedup', status: 'complete' }] },
      { id: 'phase-adjudicate', kind: 'phase', label: 'Adjudicate', status: 'complete', depth: 1, steps: [] },
      { id: 'judge', kind: 'task', label: 'Judge', parentId: 'phase-adjudicate', status: 'complete', depth: 0,
        steps: [{ id: 'judge-1', kind: 'review.judge', label: 'Judge', status: 'complete' }] },
      { id: 'phase-report', kind: 'phase', label: 'Report', status: 'complete', depth: 2, steps: [] },
      two('verify', 'Verify', 'phase-report', 0),
      { id: 'synthesis', kind: 'task', label: 'Synthesis', parentId: 'phase-report', status: 'complete', depth: 1,
        steps: [{ id: 'synth-1', kind: 'review.synthesis', label: 'Synthesis', status: 'complete' }] },
    ],
    edges: [
      { id: 'e1', source: 'bundle', target: 'probe-high', kind: 'depends' },
      { id: 'e2', source: 'bundle', target: 'probe-low', kind: 'depends' },
      { id: 'e3', source: 'bundle', target: 'probe-mid', kind: 'depends' },
      { id: 'e4', source: 'probe-high', target: 'dedup', kind: 'depends' },
      { id: 'e5', source: 'probe-low', target: 'dedup', kind: 'depends' },
      { id: 'e6', source: 'probe-mid', target: 'dedup', kind: 'depends' },
      { id: 'p1', source: 'phase-investigate', target: 'phase-adjudicate', kind: 'phase' },
      { id: 'p2', source: 'phase-adjudicate', target: 'phase-report', kind: 'phase' },
      { id: 'e7', source: 'verify', target: 'synthesis', kind: 'depends' },
    ],
    generated_at_ms: 0,
  };
}

async function routeAll(page) {
  await page.route(`**/mission/${MISSION_ID}/graph.json`, (r) =>
    r.fulfill({ contentType: 'application/json', body: JSON.stringify(graphSnapshot()) }));
  await page.route(/\/flow\/[^/]+\/mission\/.*/, (r) =>
    r.fulfill({ contentType: 'application/json', body: JSON.stringify({ records: [], count: 0, truncated: false, generated_at_ms: 0 }) }));
  await page.route(/\/flow\/[^/]+\/backfill.*/, (r) => r.fulfill({ contentType: 'application/json', body: '[]' }));
  await page.route(/\/flow\/[^/]+\/stream.*/, (r) => r.fulfill({ status: 204, body: '' }));
  await page.route(/\/(missions|phases|runs|lab\/runs|machine\/.*|presence.*|fleet\/.*)(\?.*)?$/, (r) =>
    r.fulfill({ contentType: 'application/json', body: '[]' }));
}

// A narrow viewport defaults to the list (timeline) renderer — same trap
// mission-lens-layout-geometry.spec.js's own comment documents: race the
// renderer pick, or a slow run clicks "switch renderer" on what is by then
// a wide viewport and flips the DEFAULT into the list renderer instead.
async function ensureCanvas(page) {
  await expect(page.locator('.missionlens')).toBeVisible();
  await page.waitForFunction(() => !!document.querySelector('.missionlens .canvas, .missionlens .tlt-hd'));
  if ((await page.locator('.missionlens .canvas').count()) === 0) {
    await page.locator('button[title="switch renderer"]').click();
  }
  await expect(page.locator('.missionlens .mnode').first()).toBeVisible({ timeout: 10000 });
}

// Reads the scale straight off React Flow's own `translate(...) scale(...)`
// transform string (`@reactflow/core`'s `Viewport` component) — the exact
// number `fitView()` computed, not an inference from bounding boxes.
async function getScale(page) {
  return page.evaluate(() => {
    const el = document.querySelector('.react-flow__viewport');
    if (!el) return null;
    const m = /scale\(([-\d.]+)\)/.exec(el.style.transform || '');
    return m ? parseFloat(m[1]) : null;
  });
}

test('phone portrait: the graph is no longer width-bound to a near-zero scale', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await routeAll(page);
  await page.goto(`/index-live.html#mission=${MISSION_ID}`);
  await ensureCanvas(page);
  // Retry-safe: poll the real transform rather than asserting once right
  // after mount, since `fitView`'s own first firing races React Flow's
  // node-measurement effect.
  await expect.poll(() => getScale(page), { message: 'fitView scale settles', timeout: 5000 }).not.toBeNull();
  const scale = await getScale(page);
  // The reported bug measured 0.236522 on this exact fixture at this exact
  // viewport. 0.3 leaves headroom above float jitter while staying well
  // below the old width-bound number — a regression back to the old
  // side-by-side desktop columns would land under 0.25 again.
  expect(scale, `portrait scale ${scale} is not meaningfully above the reported width-bound 0.236522`).toBeGreaterThan(0.3);
});

test('desktop: the fit is unaffected (inverted case — the phone layout branch must be a no-op here)', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await routeAll(page);
  await page.goto(`/index-live.html#mission=${MISSION_ID}`);
  await ensureCanvas(page);
  await expect.poll(() => getScale(page), { timeout: 5000 }).not.toBeNull();
  const scale = await getScale(page);
  // Measured on this fixture at this viewport: 0.492146. A regression that
  // wrongly applied the narrow (phone) layout to desktop would collapse the
  // side-by-side columns and shrink this well below 0.45.
  expect(scale, `desktop scale ${scale} moved — the narrow layout branch leaked into desktop`).toBeGreaterThan(0.45);
});

test('rotating a phone canvas RE-FITS instead of keeping the old scale (#2376)', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await routeAll(page);
  await page.goto(`/index-live.html#mission=${MISSION_ID}`);
  await ensureCanvas(page);
  await expect.poll(() => getScale(page), { timeout: 5000 }).not.toBeNull();
  const portraitScale = await getScale(page);

  // Rotate: 390x844 portrait -> 844x390 landscape. A phone rotated to
  // landscape is WIDER than the 768px breakpoint, which is exactly why
  // `useIsMobile` (see its own doc, #2108) tests `matchMedia('(pointer:
  // coarse)')` alongside width rather than width alone — this asserts the
  // canvas stays the phone (canvas) renderer through the rotation, not that
  // it silently reverted to a desktop chrome mid-flight.
  await page.setViewportSize({ width: 844, height: 390 });
  await expect(page.locator('.missionlens .canvas')).toBeVisible();

  // The core #2376 assertion: before this fix, `fitView` was an init-only
  // boolean prop and this number never moved off `portraitScale`, no matter
  // how long the test waited. `expect.poll` gives React Flow's own
  // ResizeObserver + the fix's `RefitOnResize` effect time to fire without
  // hard-coding a sleep.
  await expect
    .poll(() => getScale(page), { message: 'graph re-fits to the new pane after rotation', timeout: 5000 })
    .not.toBe(portraitScale);
  const landscapeScale = await getScale(page);
  expect(landscapeScale, 'landscape scale is a real number, not a stale/NaN transform').toBeGreaterThan(0);

  // Rotate back — the re-fit isn't a one-way trip; it recomputes again for
  // whatever the CURRENT pane is, every time.
  await page.setViewportSize({ width: 390, height: 844 });
  await expect
    .poll(() => getScale(page), { message: 'graph re-fits back on the return rotation', timeout: 5000 })
    .toBeCloseTo(portraitScale, 5);
});
