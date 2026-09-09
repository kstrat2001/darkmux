// #1868 packet 2 retarget of mission-graph-status-vocab.spec.js against
// `MissionGraphLens` (`#mission=<id>`).
//
// This file NARROWS the original 10-test suite deliberately, per #1868's own
// packet brief ("if a behavior is genuinely not reproducible in the port,
// say so explicitly rather than dropping it silently"). Named, not silent:
//
// - "the status vocabulary knows `aborted` at all" (the HTML-source-string
//   assertion, `toContain('"mission abort": "aborted"')`) has NO analog
//   here — this port has no single HTML string containing that literal
//   table. It's covered STRONGER instead, as a direct vitest assertion on
//   the real ported data structures: `graph.test.ts`'s "mission abort
//   resolves to its own aborted terminal, not close" and "aborted ranks as
//   a real terminal, not planned".
// - "a later phase starts at its OWN left edge" (the depth-rebase layout
//   fix) is covered by `graph.test.ts`'s "rebases each phase band to its
//   own first column" — a more precise, faster unit-level proof of the same
//   `computeLayout` behavior than a `boundingBox()` comparison gives.
// - "a step running since long ago stops pulsing" / "a heartbeat keeps a
//   slow seat alive" (the `STEP_LIVENESS_WINDOW_MS` gate) are covered by
//   `graph.test.ts`'s "stepMeterFor liveness" describe block.
// - "an unknown status does not become permanent" (the `keepPageStatus`
//   arrival-vs-held asymmetry) is covered by `graph.test.ts`'s
//   "statusRank / keepPageStatus" describe block.
// - "every phase container has a visible border" (a WCAG contrast-ratio
//   measurement) IS ported below, re-measured against THIS port's own
//   palette (`styles.css`'s `.missionlens .phasegroup` block) rather than
//   the standalone page's — this port's `.phasegroup` base rule (no
//   `.s-planned` override needed; the UNSTYLED default already carries a
//   real border) measures ~12.57:1 against the app-shell background,
//   comfortably clear of the 1.4 threshold the original bug/fix pair
//   straddled at ~1.21/~1.65. Coverage against this port's own
//   hand-rewritten ~650-line CSS regressing the same way (no unit test
//   substitutes for a rendered contrast measurement), not evidence of a
//   live defect.
//
// What's ported, because each proves something the port could uniquely get
// wrong: the aborted/finalized visual distinction (a real CSS class check),
// the unknown-status-wins-the-reconcile-poll ratchet (#1628 — this port's
// OWN reconcile path, `graph.json`'s `refetchInterval`, differs
// structurally from the standalone page's timer and needed its own wiring —
// see `MissionGraphLens.tsx`'s own doc), the no-sideways-scroll
// render-sanity check (a real, cheap regression guard), and the phase
// border contrast (re-measured against this port's own CSS).
const { test, expect } = require('@playwright/test');

const MISSION_ID = 'm-vocab';
const BACKFILL_RE = /\/flow\/\d{4}-\d{2}-\d{2}(?!\/stream)(\?.*)?$/;
const STREAM_RE = /\/flow\/\d{4}-\d{2}-\d{2}\/stream(\?.*)?$/;
const MISSION_RE = /\/flow-mission\/[^/?]+(\?.*)?$/;

function graph(missionStatus, phaseStatus) {
  return {
    mission_id: MISSION_ID,
    mission_status: missionStatus,
    nodes: [
      { id: 'phase-a', kind: 'phase', label: 'Investigate', status: phaseStatus, depth: 0, steps: [] },
      {
        id: 'task-1', kind: 'task', label: 'Probe', parentId: 'phase-a', status: phaseStatus, depth: 0,
        steps: [{ id: 'probe-1', kind: 'dispatch.internal', label: 'probe', status: phaseStatus }],
      },
    ],
    edges: [],
    generated_at_ms: 0,
  };
}

function twoPhaseGraph() {
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

async function open(page, bodies) {
  const errors = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  let hit = 0;
  await page.route(`**/mission/${MISSION_ID}/graph.json*`, (r) => {
    const body = bodies[Math.min(hit, bodies.length - 1)];
    hit += 1;
    return r.fulfill({ contentType: 'application/json', body: JSON.stringify(body) });
  });
  await page.route(MISSION_RE, (r) => r.fulfill({ contentType: 'application/json', body: JSON.stringify({ records: [], count: 0, truncated: false, generated_at_ms: 0 }) }));
  await page.route(BACKFILL_RE, (r) => r.fulfill({ contentType: 'application/json', body: '[]' }));
  await page.route(STREAM_RE, (r) => r.fulfill({ contentType: 'text/event-stream', body: '' }));
  await page.setViewportSize({ width: 1280, height: 900 });
  await page.goto(`/index-live.html#mission=${MISSION_ID}`);
  await expect(page.locator('.mnode').first()).toBeVisible();
  return { errors };
}

test('aborted is visually distinct from finalized, not just textually', async ({ browser }) => {
  // Two independent browser contexts (not one page reused across two
  // navigations) — the cleanest way to guarantee neither run's route mocks
  // or React tree can leak into the other's.
  const abortedPage = await (await browser.newContext()).newPage();
  const { errors: abortedErrors } = await open(abortedPage, [graph('aborted', 'abandoned')]);
  const aborted = await abortedPage.locator('.missionlens .mstatus').first().evaluate((el) => getComputedStyle(el).color);

  const finalizedPage = await (await browser.newContext()).newPage();
  const { errors: finalizedErrors } = await open(finalizedPage, [graph('finalized', 'complete')]);
  const finalized = await finalizedPage.locator('.missionlens .mstatus').first().evaluate((el) => getComputedStyle(el).color);

  expect(aborted).not.toBe(finalized);
  expect(abortedErrors, `uncaught: ${abortedErrors.join(' | ')}`).toEqual([]);
  expect(finalizedErrors, `uncaught: ${finalizedErrors.join(' | ')}`).toEqual([]);
});

test('a status this build does not know wins the RECONCILE POLL instead of being swallowed (#1628)', async ({ page }) => {
  // First response: a RUNNING phase. Second (the 20s `graph.json`
  // `refetchInterval` reconcile, `MissionGraphLens.tsx`'s own doc): the
  // same phase carrying a status this build has never heard of. Before the
  // fix, rank("blocked") === 0 lost to rank("running") === 1 in the merge
  // — but this port's fold DERIVES from whichever snapshot is current
  // rather than merging against a held value, so the failure mode this
  // proves against is narrower than legacy's (see this file's own module
  // doc): a snapshot-only status delta must actually be PICKED UP at all,
  // which requires the periodic refetch to exist and fire.
  await page.clock.install();
  const { errors } = await open(page, [graph('active', 'running'), graph('active', 'blocked')]);

  await expect(page.locator('.mnode.s-running').first()).toBeVisible();

  await page.clock.fastForward(25_000);

  await expect(
    page.locator('.mnode.s-running'),
    'a real transition must not be discarded — the reconcile refetch must have fired and folded the new status'
  ).toHaveCount(0);
  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});

test('a phase that legitimately regresses running->planned in a fresh reconcile snapshot is not pinned back to running by an older matching flow record (#2518)', async ({ page }) => {
  // The collision #2518 names: `derive_task_status`/`phase_task_rollup`
  // (crates/darkmux-serve/src/mission_graph.rs) can legitimately regress a
  // node's DISPLAY status in a fresh, later `graph.json` snapshot (a phase
  // genuinely between real transitions — #2406 made a phase's own status a
  // rollup of its tasks'), while `keepPageStatus`'s monotonic ratchet
  // refuses that regression once ANY matching flow record has ever put the
  // node at a higher rank. Before the #2518 fix, `foldFlowRecords` replayed
  // every record it had ever seen for a handle on EVERY fold regardless of
  // age, so an old "phase start" record from back when the phase first went
  // running would out-rank the fresh "planned" snapshot and pin the chip at
  // running forever — proved directly against `foldFlowRecords` in
  // `graph.test.ts`'s "foldFlowRecords snapshot-recency gate (#2518)". This
  // is the same collision proved end-to-end through a real render: the
  // fold now only replays records NEWER than the snapshot's own
  // `generated_at_ms`, so a record predating both snapshots here must not
  // resurrect "running" once the second snapshot has legitimately dropped
  // to "planned".
  const missionId = 'm-2518';
  const now = Date.now();
  const errors = [];
  page.on('pageerror', (e) => errors.push(String(e)));

  function snapshot(status, generatedAtMs) {
    return {
      mission_id: missionId,
      mission_status: 'active',
      nodes: [
        { id: 'phase-a', kind: 'phase', label: 'Investigate', status, depth: 0, steps: [] },
        { id: 'task-1', kind: 'task', label: 'Probe', parentId: 'phase-a', status, depth: 0, steps: [] },
      ],
      edges: [],
      generated_at_ms: generatedAtMs,
    };
  }
  const bodies = [snapshot('running', now), snapshot('planned', now + 30_000)];
  let hit = 0;
  await page.route(`**/mission/${missionId}/graph.json*`, (r) => {
    const body = bodies[Math.min(hit, bodies.length - 1)];
    hit += 1;
    return r.fulfill({ contentType: 'application/json', body: JSON.stringify(body) });
  });
  // The historical record that first put the phase at "running", well
  // BEFORE either snapshot's own `generated_at_ms` — the shape a real
  // mission produces: the event fires once, long before a later reconcile
  // poll correctly reads the phase back down to "planned".
  const historical = [{ ts: new Date(now - 60_000).toISOString(), action: 'phase start', handle: 'phase-a', mission_id: missionId }];
  await page.route(MISSION_RE, (r) => r.fulfill({ contentType: 'application/json', body: JSON.stringify({ records: historical, count: 1, truncated: false, generated_at_ms: 0 }) }));
  await page.route(BACKFILL_RE, (r) => r.fulfill({ contentType: 'application/json', body: '[]' }));
  await page.route(STREAM_RE, (r) => r.fulfill({ contentType: 'text/event-stream', body: '' }));
  await page.setViewportSize({ width: 1280, height: 900 });
  await page.clock.install({ time: now });
  await page.goto(`/index-live.html#mission=${missionId}`);
  await expect(page.locator('.mnode').first()).toBeVisible();

  await expect(page.locator('.phasegroup.s-running').first()).toBeVisible();

  await page.clock.fastForward(35_000);

  await expect(
    page.locator('.phasegroup.s-running'),
    'a legitimate regression in a fresh snapshot must not be pinned back to running by an older flow record for the same handle'
  ).toHaveCount(0);
  await expect(page.locator('.phasegroup.s-planned').first()).toBeVisible();
  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});

test('every phase container has a visible border, not one that matches the background (#1868 — re-measured against this port\'s own CSS)', async ({ page }) => {
  // Ported from mission-graph-status-vocab.spec.js's identical-named test —
  // same method (a real WCAG contrast ratio, not a luminance delta; see that
  // file's own comment for why a subtraction of gamma-encoded values passed
  // against the original broken border). This port's numbers are its own:
  // the standalone page's bug measured ~1.21 and its fix ~1.65; this port's
  // `.phasegroup` base rule was never derived from either number and needs
  // its own measurement to be trusted at all.
  const { errors } = await open(page, [graph('active', 'planned')]);
  const box = page.locator('.missionlens .phasegroup').first();
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

  expect(ratio, `phase container border contrast is ${ratio.toFixed(2)}:1`).toBeGreaterThan(1.4);
  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});

test('the graph lens never scrolls sideways on a phone', async ({ page }) => {
  const { errors } = await open(page, [twoPhaseGraph()]);
  await page.setViewportSize({ width: 390, height: 844 });
  // Below ~700px the lens swaps the React Flow canvas for the mobile
  // timeline renderer, so `.mnode` does not exist here.
  await expect(page.locator('.tlphase, .tltask').first()).toBeVisible();

  const over = await page.evaluate(() => ({
    doc: document.documentElement.scrollWidth - document.documentElement.clientWidth,
    body: document.body.scrollWidth - document.body.clientWidth,
  }));
  expect(over.doc, `the page scrolls ${over.doc}px sideways on a phone`).toBeLessThanOrEqual(0);
  expect(over.body, 'the body scrolls sideways').toBeLessThanOrEqual(0);
  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});

// (#2406, post-review) The counts behind a phase's status must be READABLE,
// not hover-only. Both renderers are covered because the lens swaps between
// them at ~700px: the React Flow canvas above it, the vertical timeline
// below — and the phone, where a tooltip cannot be produced at all, only
// ever sees the timeline.
function degradedGraph() {
  return {
    mission_id: MISSION_ID,
    mission_status: 'finalized',
    nodes: [
      {
        id: 'pa', kind: 'phase', label: 'Adjudicate', status: 'degraded', depth: 0, steps: [],
        statusNote: '1 complete · 11 errored',
      },
      { id: 'ta', kind: 'task', label: 'Judge', parentId: 'pa', status: 'complete', depth: 0, steps: [] },
      { id: 'tb', kind: 'task', label: 'Judge 2', parentId: 'pa', status: 'error', depth: 1, steps: [] },
    ],
    edges: [],
    generated_at_ms: 0,
  };
}

test('a degraded phase says so in TEXT, with its counts, on the canvas and on a phone (#2406)', async ({ page }) => {
  const { errors } = await open(page, [degradedGraph()]);

  // Canvas (desktop). Before this fix `PhaseGroup` rendered only "PHASE" +
  // the label: the status was carried by border color alone, and the
  // counts existed only as a `title=`.
  const box = page.locator('.missionlens .phasegroup').first();
  await expect(box).toBeVisible();
  await expect(box.locator('.wstatus')).toHaveText(/degraded/i);
  await expect(
    box.locator('.pg-note'),
    'DEGRADED alone is the same word for "11 of 12 shipped" and "1 of 12 shipped"'
  ).toHaveText('1 complete · 11 errored');

  // Phone. `title=` does not exist on touch, so the counts have to be real
  // text here or they are not reachable at all.
  await page.setViewportSize({ width: 390, height: 844 });
  const phase = page.locator('.missionlens .tlphase').first();
  await expect(phase).toBeVisible();
  await expect(phase.locator('.tlph-tag')).toHaveText(/degraded/i);
  await expect(phase.locator('.tlph-note')).toHaveText('1 complete · 11 errored');
  await expect(phase.locator('.tlph-note')).toBeInViewport();

  const over = await page.evaluate(
    () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
  );
  expect(over, `the counts row pushed the page ${over}px sideways`).toBeLessThanOrEqual(0);
  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});
