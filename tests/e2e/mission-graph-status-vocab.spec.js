// The mission graph's status vocabulary (#1627, #1627).
//
// Found by a coverage-gap audit, not by the suite — which is the point. Three
// defects, and the first two were created by the fix for the third:
//
//   #1627 split `mission abort` from `mission close` on the Rust side, and
//   shipped without teaching this file either the new ACTION or the new
//   STATUS. So an abort record resolved to null and was dropped, `aborted`
//   ranked 0 (level with `planned`), and an aborted mission's own graph page
//   kept flying a "● live" pill. The lie the fix was filed to kill, one file
//   over, introduced by the fix.
//
//   #1628 is the general form and is worse: `statusRank` returned 0 for ANY
//   unrecognized status, and the merge ratchet keeps the page's value when
//   rank(old) >= rank(new). A `running` phase whose disk status became a value
//   this build has never heard of kept rendering `running` forever. That is
//   not misreading an absent value — it is DISCARDING a present, correct one.
const fs = require('fs');
const path = require('path');
const { test, expect } = require('@playwright/test');

const MISSION_ID = 'm-vocab';
const GRAPH_PATH = `**/mission/${MISSION_ID}/graph`;
const BACKFILL_RE = /\/flow\/\d{4}-\d{2}-\d{2}(\?.*)?$/;
const STREAM_RE = /\/flow\/\d{4}-\d{2}-\d{2}\/stream(\?.*)?$/;

// The lens is a separate asset from the viewer, so the spec serves it the same
// way every other mission-graph spec does (see mission-graph-truth.spec.js).
const MISSION_GRAPH_HTML = fs.readFileSync(
  path.join(__dirname, '..', '..', 'crates', 'darkmux-serve', 'assets', 'mission-graph.html'),
  'utf8'
);

function graph(missionStatus, phaseStatus) {
  // Shape mirrors `graphSnapshot()` in mission-graph-truth.spec.js: steps hang
  // off a TASK node with a `parentId`, not off the phase directly.
  return {
    mission_id: MISSION_ID,
    mission_status: missionStatus,
    nodes: [
      { id: 'phase-a', kind: 'phase', label: 'Investigate', status: phaseStatus, depth: 0, steps: [] },
      {
        id: 'task-1', kind: 'task', label: 'Probe', parentId: 'phase-a',
        status: phaseStatus, depth: 0,
        steps: [{ id: 'probe-1', kind: 'dispatch.internal', label: 'probe', status: phaseStatus }],
      },
    ],
    edges: [],
    generated_at_ms: 0,
  };
}

/// `bodies` is a list of successive graph.json responses — the second one is
/// what the 20s reconcile poll fetches, which is the only path that exercises
/// the merge ratchet and the one no spec had ever driven.
async function open(page, bodies) {
  const errors = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  await page.route(GRAPH_PATH, (r) =>
    r.fulfill({ contentType: 'text/html; charset=utf-8', body: MISSION_GRAPH_HTML })
  );
  let hit = 0;
  await page.route(`**/mission/${MISSION_ID}/graph.json*`, (r) => {
    const body = bodies[Math.min(hit, bodies.length - 1)];
    hit += 1;
    return r.fulfill({ contentType: 'application/json', body: JSON.stringify(body) });
  });
  await page.route(BACKFILL_RE, (r) => r.fulfill({ contentType: 'application/json', body: '[]' }));
  await page.route(STREAM_RE, (r) => r.fulfill({ contentType: 'text/event-stream', body: '' }));
  // Desktop width -> the React Flow canvas renderer (`.mnode`); the mobile
  // timeline is a different DOM and a different spec's concern.
  await page.setViewportSize({ width: 1280, height: 900 });
  await page.goto(`/mission/${MISSION_ID}/graph`);
  await expect(page.locator('.mnode').first()).toBeVisible();
  return { errors };
}

test('the status vocabulary knows `aborted` at all', async () => {
  // Asserted against the SOURCE, deliberately, and it is worth saying why.
  //
  // The rank drives the live-pill gate (`live && statusRank(...) >= 2`), but
  // the pill only reaches `.on` once SSE connects, and this spec's stream mock
  // never does — a first draft asserted `.livepill.on` and PASSED against the
  // reverted fix, true for a reason having nothing to do with the bug.
  // `statusRank` is not exposed on `window` either, so it cannot be called
  // from `page.evaluate`.
  //
  // Rather than invent a DOM assertion that cannot fail, this pins the two
  // table entries directly. Less elegant than a rendered assertion; it has the
  // one property that matters, which is that removing either entry fails it.
  // The BEHAVIOUR those entries drive is covered by the other two tests.
  expect(
    MISSION_GRAPH_HTML,
    '`mission abort` must map to a status, or the record is silently dropped'
  ).toContain('"mission abort": "aborted"');
  expect(
    MISSION_GRAPH_HTML,
    'aborted must rank TERMINAL (2), not fall through to 0 alongside `planned`'
  ).toMatch(/aborted:\s*2/);
});

test('aborted is visually distinct from finalized, not just textually', async ({ page }) => {
  // Colour is the fast read a status badge exists for. Without its own rule
  // `aborted` fell through to the same muted grey as `finalized`, so a
  // teardown and a success looked identical to anyone scanning rather than
  // reading — the #1627 defect on the surface built to prevent it.
  const { errors } = await open(page, [graph('aborted', 'abandoned')]);
  const aborted = await page
    .locator('.mstatus')
    .first()
    .evaluate((el) => getComputedStyle(el).color);

  await page.unrouteAll();
  await open(page, [graph('finalized', 'complete')]);
  const finalized = await page
    .locator('.mstatus')
    .first()
    .evaluate((el) => getComputedStyle(el).color);

  expect(aborted).not.toBe(finalized);
  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});

test('a status this build does not know wins the merge instead of being swallowed', async ({
  page,
}) => {
  // (#1628) The ratchet, exercised through the 20s reconcile — the only path
  // where a disk snapshot merges into the page's live state, and a path no
  // spec had ever driven.
  //
  // First response: a RUNNING phase. Second: the same phase carrying a status
  // this build has never heard of. Before the fix, rank("blocked") === 0 lost
  // to rank("running") === 1, so the page kept animating `running` forever.
  await page.clock.install();
  const { errors } = await open(page, [graph('active', 'running'), graph('active', 'blocked')]);

  await expect(page.locator('.mnode.s-running').first()).toBeVisible();

  await page.clock.fastForward(25_000);

  await expect(
    page.locator('.mnode.s-running'),
    'a real transition must not be discarded just because its value is unrankable'
  ).toHaveCount(0);
  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});

// ── (#1618) The layout fixes the operator found on his phone ────────────────
//
// The #1618 commit claimed these were "canvas rendering with no assertable
// seam... they want eyes on the phone". That was true of the COLOUR change and
// wrong about the rest: the depth rebase is a POSITIONING bug, and
// chrome-order.spec.js already proves this harness pins positions with measured
// boundingBox(). Untested because untried, not because untestable.

function twoPhaseGraph() {
  // Global depths 0..2 in phase A and 3 in phase B — the exact shape that
  // pushed later phases off-screen, because a task's column came from its depth
  // in the WHOLE DAG rather than within its own band.
  return {
    mission_id: MISSION_ID,
    mission_status: 'active',
    nodes: [
      { id: 'pa', kind: 'phase', label: 'Investigate', status: 'running', depth: 0, steps: [] },
      { id: 'ta', kind: 'task', label: 'Bundle', parentId: 'pa', status: 'complete', depth: 0, steps: [] },
      { id: 'tb', kind: 'task', label: 'Probe', parentId: 'pa', status: 'complete', depth: 1, steps: [] },
      { id: 'tc', kind: 'task', label: 'Dedup', parentId: 'pa', status: 'complete', depth: 2, steps: [] },
      { id: 'pb', kind: 'phase', label: 'Adjudicate', status: 'running', depth: 1, steps: [] },
      { id: 'td', kind: 'task', label: 'Judge', parentId: 'pb', status: 'running', depth: 3, steps: [] },
    ],
    edges: [],
    generated_at_ms: 0,
  };
}

test('a later phase starts at its OWN left edge, not indented off-screen', async ({ page }) => {
  // The reported symptom: "Last arrow to the last phase" looked broken, because
  // the phase-order edge ran down the LEFT while that band's only task sat at
  // global column 3 — past the right edge of the viewport.
  const { errors } = await open(page, [twoPhaseGraph()]);

  const first = await page.locator('.mnode', { hasText: 'Bundle' }).boundingBox();
  const later = await page.locator('.mnode', { hasText: 'Judge' }).boundingBox();
  expect(first, 'first-phase task must render').not.toBeNull();
  expect(later, 'later-phase task must render').not.toBeNull();

  // Judge is depth 3 globally but depth 0 within its own band, so it must line
  // up with the first band's first column — not three columns to its right.
  expect(
    Math.abs(later.x - first.x),
    `later phase indented ${Math.round(later.x - first.x)}px past the first — depth is not band-local`
  ).toBeLessThan(2);

  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});

test('every phase container has a visible border, not one that matches the background', async ({
  page,
}) => {
  // A planned phase had no `.s-planned` rule, so it fell through to a
  // #1f1f24 border on a #0a0a0b background — a CONTRAST RATIO of about 1.2:1.
  // The LAST phase of a running mission is always planned, so the mission's own
  // destination was reliably the least visible thing on the canvas.
  //
  // Measured as a WCAG contrast ratio, not a luminance delta. A first draft
  // used `|lum(a) - lum(b)| > 12` and PASSED against the broken border, whose
  // raw delta is ~21 — the numbers looked far apart while the ratio was 1.2.
  // Perceived contrast is a ratio of gamma-decoded luminances; a subtraction of
  // gamma-encoded ones is not the same quantity and does not measure this bug.
  const { errors } = await open(page, [graph('active', 'planned')]);
  const box = page.locator('.phasegroup').first();
  await expect(box).toBeVisible();

  const ratio = await box.evaluate((el) => {
    const rel = (css) => {
      const [r, g, b] = css.match(/\d+/g).map(Number).slice(0, 3);
      const ch = (v) => {
        const c = v / 255;
        return c <= 0.03928 ? c / 12.92 : Math.pow((c + 0.055) / 1.055, 2.4);
      };
      return 0.2126 * ch(r) + 0.7152 * ch(g) + 0.0722 * ch(b);
    };
    const a = rel(getComputedStyle(el).borderTopColor);
    const b = rel(getComputedStyle(document.body).backgroundColor);
    const [hi, lo] = a > b ? [a, b] : [b, a];
    return (hi + 0.05) / (lo + 0.05);
  });

  // The broken border measures ~1.21; the fixed one ~1.65. 1.4 separates them
  // with room on both sides. Not a WCAG-compliance claim — this is a container
  // edge, not text — just "distinguishable from the canvas behind it".
  expect(ratio, `phase container border contrast is ${ratio.toFixed(2)}:1`).toBeGreaterThan(1.4);

  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});

test('the graph page never scrolls sideways on a phone', async ({ page }) => {
  // (#1618) The horizontal-overflow half of the minimap fix, which is the part
  // the operator actually experienced.
  //
  // NOT covered here: the minimap's own box. Two drafts tried — the first
  // wrapped every assertion in `if (await mm.count())` and skipped them all
  // (vacuous in exactly the way #1631 describes); the second forced
  // `dmux.mmap=1` via localStorage and the widget still did not mount in this
  // harness. Rather than ship a third guess or a test that cannot fail, the
  // cap is left unverified and said so. The page-level overflow assertion
  // below is real and would catch the reported symptom regardless of which
  // element caused it.
  const { errors } = await open(page, [twoPhaseGraph()]);
  await page.setViewportSize({ width: 390, height: 844 });
  // Below ~700px the lens swaps the React Flow canvas for the mobile TIMELINE
  // renderer, so `.mnode` does not exist here — wait on the timeline instead.
  await expect(page.locator('.tlphase, .tltask').first()).toBeVisible();

  const over = await page.evaluate(() => ({
    doc: document.documentElement.scrollWidth - document.documentElement.clientWidth,
    body: document.body.scrollWidth - document.body.clientWidth,
  }));
  expect(over.doc, `the graph page scrolls ${over.doc}px sideways on a phone`).toBeLessThanOrEqual(0);
  expect(over.body, 'the body scrolls sideways').toBeLessThanOrEqual(0);
  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});

// ── (#1640) A dead dispatch must stop claiming to be live ───────────────────

test('a step running since long ago stops pulsing and stops counting', async ({ page }) => {
  // The seventh instance of one root cause: a lifecycle state INFERRED rather
  // than recorded. A hard-killed dispatch — sleep, `kill -9`, power loss —
  // never writes its terminal, so `status: "running"` persists on disk and this
  // page pulsed a dot and ticked a clock upward forever.
  //
  // Stronger than the claim #1621 fixed on the runs lens, which merely shows a
  // badge. An animation plus a live counter is the most emphatic "this is
  // happening now" the UI can make, and it had no evidence behind it at all.
  const ancient = Math.floor(Date.now() / 1000) - 6 * 3600; // started 6h ago
  const g = graph('active', 'running');
  g.nodes[1].steps = [
    { id: 'probe-1', kind: 'dispatch.internal', label: 'probe',
      status: 'running', startedTs: ancient },
  ];
  const { errors } = await open(page, [g]);

  // No stream records arrive, so there is no signal of ANY kind for this step —
  // the only thing saying "running" is a disk field written six hours ago.
  await expect(
    page.locator('.mnode .gen'),
    'a step with no signal for hours must not render as generating'
  ).toHaveCount(0);

  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});

test('a step that just started still reads as live', async ({ page }) => {
  // The other half, and the one that matters more: the window must be generous
  // enough that a slow-but-genuinely-live seat is never called dead. A false
  // "stalled" is worse than a late-clearing animation.
  const justNow = Math.floor(Date.now() / 1000) - 30;
  const g = graph('active', 'running');
  g.nodes[1].steps = [
    { id: 'probe-1', kind: 'dispatch.internal', label: 'probe',
      status: 'running', startedTs: justNow },
  ];
  const { errors } = await open(page, [g]);

  await expect(
    page.locator('.mnode .gen'),
    'a step started 30s ago is live and must still animate'
  ).toHaveCount(1);

  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});

// ── (#1640) The two ways my own #1640/#1628 fixes were wrong ────────────────
//
// Found by a frontier audit told to assume my fixes carried the same error rate
// as my tests. It was right twice.

test('a heartbeat keeps a slow seat alive — the signal that moves no counter', async ({ page }) => {
  // #1640's first cut stamped arrival time INSIDE the metrics object, which the
  // no-change guard then discarded: it compares counters and bookends only, so
  // every record that moves no counter was thrown away without recording that
  // we heard from the step.
  //
  // That is not an edge case. This machine's real flow files hold 35,136
  // `dispatch.turn.heartbeat` records — the runtime's DESIGNATED proof-of-work
  // signal — plus 5,909 `dispatch.reasoning` and 4,187 `telemetry.context`.
  // A live seat mid-turn, heartbeating the whole way, read as stalled. The fix's
  // own comment called that the worse failure.
  const old = Math.floor(Date.now() / 1000) - 6 * 3600;
  const g = graph('active', 'running');
  g.nodes[1].steps = [
    { id: 'probe-1', kind: 'dispatch.internal', label: 'probe', status: 'running', startedTs: old },
  ];

  const errors = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  await page.route(GRAPH_PATH, (r) =>
    r.fulfill({ contentType: 'text/html; charset=utf-8', body: MISSION_GRAPH_HTML })
  );
  await page.route(`**/mission/${MISSION_ID}/graph.json*`, (r) =>
    r.fulfill({ contentType: 'application/json', body: JSON.stringify(g) })
  );
  await page.route(BACKFILL_RE, (r) => r.fulfill({ contentType: 'application/json', body: '[]' }));
  // A heartbeat NOW for a step that started six hours ago. It moves no counter.
  const today = new Date().toISOString().slice(0, 10);
  const beat = {
    ts: `${today}T10:00:00Z`, action: 'dispatch.turn.heartbeat', level: 'info',
    category: 'work', tier: 'local', stage: 'dispatch', handle: 'h',
    session_id: 'step-probe-1', payload: {},
  };
  await page.route(STREAM_RE, (r) =>
    r.fulfill({ contentType: 'text/event-stream', body: `data: ${JSON.stringify(beat)}\n\n` })
  );
  await page.setViewportSize({ width: 1280, height: 900 });
  await page.goto(`/mission/${MISSION_ID}/graph`);
  await expect(page.locator('.mnode').first()).toBeVisible();

  await expect(
    page.locator('.mnode .gen'),
    'a heartbeat is proof of work — the seat is live and must still animate'
  ).toHaveCount(1);

  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});

test('an unknown status does not become permanent', async ({ page }) => {
  // #1628's first cut ranked unknown at 99 so it would win on arrival. It did —
  // and then won FOREVER as the held value, because the ratchet keeps the page's
  // value when its rank is >=. A real terminal arriving later could never
  // displace it, across the reconcile poll, a second merge, and manual refresh.
  // Only a full browser reload cleared it, which a chromeless home-screen page
  // cannot assume.
  //
  // Three snapshots: running -> unknown -> complete. The last must land.
  await page.clock.install();
  const { errors } = await open(page, [
    graph('active', 'running'),
    graph('active', 'blocked'),
    graph('active', 'complete'),
  ]);
  await expect(page.locator('.mnode.s-running').first()).toBeVisible();

  await page.clock.fastForward(25_000);
  await expect(page.locator('.mnode.s-running')).toHaveCount(0);

  await page.clock.fastForward(25_000);
  await expect(
    page.locator('.mnode.s-complete'),
    'a real terminal must displace an unknown the page is holding'
  ).toHaveCount(1);

  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});
