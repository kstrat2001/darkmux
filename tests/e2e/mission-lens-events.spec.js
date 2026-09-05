// #1868 packet 2 retarget of mission-graph-events.spec.js against
// `MissionGraphLens` (`#mission=<id>`), whose events pane is `EventLogColumn`
// — the shared component (`.eventlog__rec` rows), not the standalone page's
// own `.evrow` markup. See `MissionGraphLens.tsx`'s own doc for why the
// SAME two backfill sources (`/flow-mission/<id>` + `/flow/<date>`) and the
// SAME `recordInMission` filter still apply — only the rendering surface
// changed.
//
// (mainstay-unification packet) Selectors are UNSCOPED `.eventlog*`, not
// `.missionlens .eventlog*` — the lens no longer mounts its own
// `EventLogColumn`; it reports its scoped events upward via `onEvents`, and
// `App.tsx` feeds the same shared/mainstay column every other route uses.
// There is exactly one instance on the page again (outside `.missionlens`'s
// own DOM subtree), same as the parity suite's own retarget
// (`tests/parity/lib/extract-graph.js`).
//
// One behavior is named-and-narrowed rather than ported verbatim: the
// original's cap-disclosure test asserted "events · 250 of N" (the
// standalone page's own `EVENTS_CAP`). `EventLogColumn` already has the
// SAME honest-disclosure feature (never silently truncate without saying
// so) but at its own `LOG_CAP` (50), phrased as "50 of N events" via
// `.eventlog__qcount` — the underlying behavior (cap disclosure, not the
// exact number) is what this spec proves.
const { test, expect } = require('@playwright/test');

const MISSION_ID = 'test-mission';
const TODAY = new Date().toISOString().slice(0, 10);
const MISSION_RE = /\/flow-mission\/[^/?]+(\?.*)?$/;
const DAY_RE = /\/flow\/\d{4}-\d{2}-\d{2}(?!\/stream)(\?.*)?$/;
const STREAM_RE = /\/flow\/\d{4}-\d{2}-\d{2}\/stream(\?.*)?$/;

// (#2416) This spec's fixtures use raw `action` strings ("step start",
// "phase start", "mission start", "step complete") that don't map to a
// known `activityOf()` branch and fall through to the label itself — none
// of them are in the event log's new default allowlist (reasoning/
// checkpoint/tool call/turn/dispatch error). This spec is about backfill/
// live-stream/cap-disclosure behavior, not the filter feature, so seed the
// global stored picks to show every activity these fixtures use — the same
// "everything visible" view the default used to be before #2416 curated it.
const SHOW_ALL_ACTIVITIES = [
  'reasoning', 'checkpoint', 'tool call', 'turn', 'heartbeat',
  'dispatch start', 'dispatch end', 'dispatch error', 'feedback', 'routing',
  'compaction', 'note', 'machine online', 'machine offline', 'session end',
  'detector', 'runtime', 'tokens', 'lms', 'host telemetry', 'telemetry',
  'other', 'step start', 'phase start', 'mission start', 'step complete',
];

test.beforeEach(async ({ page }) => {
  await page.addInitScript((acts) => {
    window.sessionStorage.setItem('dmux.eventfilters', JSON.stringify({
      version: 2,
      act: { include: acts, exclude: [] },
      cat: { include: [], exclude: [] },
      tier: { include: [], exclude: [] },
      src: { include: [], exclude: [] },
      q: '',
    }));
  }, SHOW_ALL_ACTIVITIES);
});

const mockEmpty = (page, re) => page.route(re, (r) => r.fulfill({ contentType: 'application/json', body: JSON.stringify([]) }));

const GRAPH = {
  mission_id: MISSION_ID,
  mission_status: 'active',
  nodes: [
    { id: 'phase-a', kind: 'phase', label: 'Phase A', status: 'running', depth: 0, steps: [] },
    {
      id: 'task-1', kind: 'task', label: 'Task One', parentId: 'phase-a',
      status: 'running', depth: 0,
      steps: [{ id: 'step-1', kind: 'dispatch.internal', label: 'Coder', status: 'complete' }],
    },
  ],
  edges: [],
  generated_at_ms: 0,
};

async function routeGraph(page) {
  await page.route(`**/mission/${MISSION_ID}/graph.json`, (r) =>
    r.fulfill({ contentType: 'application/json', body: JSON.stringify(GRAPH) })
  );
}

test('EVENTS pane backfills existing mission records on load (not live-stream-only)', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));
  await page.setViewportSize({ width: 1280, height: 900 });
  await routeGraph(page);

  // The DAY file's records: three that belong to THIS mission (by step id,
  // phase id, and mission_id respectively) + one unrelated record that must
  // be filtered out by `recordInMission`.
  await mockEmpty(page, MISSION_RE);
  await page.route(DAY_RE, (r) =>
    r.fulfill({
      contentType: 'application/json',
      body: JSON.stringify([
        { ts: `${TODAY}T10:00:00Z`, action: 'step start', handle: 'step-1', category: 'work', level: 'info' },
        { ts: `${TODAY}T10:00:05Z`, action: 'phase start', handle: 'phase-a', category: 'work', level: 'info' },
        { ts: `${TODAY}T10:00:10Z`, action: 'mission start', handle: 'm', mission_id: MISSION_ID, category: 'work', level: 'info' },
        { ts: `${TODAY}T10:00:15Z`, action: 'step start', handle: 'not-in-this-mission', category: 'work', level: 'info' },
      ]),
    })
  );
  await page.route(STREAM_RE, (r) => r.fulfill({ contentType: 'text/event-stream', body: '' }));

  await page.goto(`/index-live.html#mission=${MISSION_ID}`);

  await page.waitForSelector('.eventlog');
  await expect(page.locator('.eventlog__empty')).toHaveCount(0);
  await expect(page.locator('.eventlog__rec')).toHaveCount(3);
  await expect(page.locator('.eventlog__qcount')).toContainText('3 events');
  // The unrelated record was filtered out by `recordInMission` — its handle
  // never appears as a row's `title` (this port's own subject attribute).
  const titles = await page.$$eval('.eventlog__rec', (els) => els.map((e) => e.getAttribute('title')));
  expect(titles).not.toContain('not-in-this-mission');

  expect(pageErrors).toEqual([]);
});

test('records from a PRIOR day still backfill — the cross-day case', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));
  await page.setViewportSize({ width: 1280, height: 900 });
  await routeGraph(page);

  const daysAgo = (n) => new Date(Date.now() - n * 86400000).toISOString().slice(0, 10);
  await page.route(MISSION_RE, (r) =>
    r.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({
        records: [
          { ts: `${daysAgo(3)}T10:00:00Z`, action: 'phase start', handle: 'phase-a', mission_id: MISSION_ID, category: 'work', level: 'info' },
          { ts: `${daysAgo(2)}T11:30:00Z`, action: 'mission start', handle: 'm', mission_id: MISSION_ID, category: 'work', level: 'info' },
        ],
      }),
    })
  );
  await mockEmpty(page, DAY_RE);
  await page.route(STREAM_RE, (r) => r.fulfill({ contentType: 'text/event-stream', body: '' }));

  await page.goto(`/index-live.html#mission=${MISSION_ID}`);

  await expect(page.locator('.eventlog__rec')).toHaveCount(2);
  await expect(page.locator('.eventlog')).not.toContainText('no events');
  const titles = await page.$$eval('.eventlog__rec', (els) => els.map((e) => e.getAttribute('title')));
  expect(titles).toContain('phase-a');

  // (#1868) The header must NOT claim a rolling "last 24h" window over
  // these records — they were selected by MISSION, spanning three days, not
  // by a 24h time window. `EventLogColumn`'s `historical` prop exists
  // precisely to drop that suffix for a fetched slice; passing it `false`
  // here (an earlier version of the port did) would have this exact
  // contradiction on screen: "events last 24h" over a 3-day-old record.
  await expect(page.locator('.eventlog__head h3')).not.toContainText(/last \d+h/i);

  expect(pageErrors).toEqual([]);
});

test("the events pane discloses its cap instead of printing it as a total (EventLogColumn's own LOG_CAP, not the standalone page's 250)", async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));
  await page.setViewportSize({ width: 1280, height: 900 });
  await routeGraph(page);

  // N is deliberately ABOVE any cap this lens has ever pre-sliced to (an
  // earlier version of `MissionGraphLens` capped the scoped record list to
  // 250 BEFORE handing it to `EventLogColumn`, which then honestly reported
  // its own LOG_CAP against that already-capped length — "50 of 250"
  // instead of "50 of 400". Matches the legacy standalone page's own
  // regression test (`mission-graph-events.spec.js`), which used N=400 for
  // exactly this reason: N=80 alone never exercises a 250-record pre-cap.
  const N = 400;
  const records = Array.from({ length: N }, (_, i) => ({
    ts: `${TODAY}T${String(Math.floor(i / 3600)).padStart(2, '0')}:${String(Math.floor((i % 3600) / 60)).padStart(2, '0')}:${String(i % 60).padStart(2, '0')}Z`,
    action: 'step start', handle: 'step-1', category: 'work', level: 'info',
  }));
  await mockEmpty(page, MISSION_RE);
  await page.route(DAY_RE, (r) => r.fulfill({ contentType: 'application/json', body: JSON.stringify(records) }));
  await page.route(STREAM_RE, (r) => r.fulfill({ contentType: 'text/event-stream', body: '' }));

  await page.goto(`/index-live.html#mission=${MISSION_ID}`);

  await expect(page.locator('.eventlog__qcount')).toContainText(`50 of ${N} events`);
  await expect(page.locator('.eventlog__rec')).toHaveCount(50);
  expect(pageErrors).toEqual([]);
});

test('a live-streamed record appends without duplicating a backfilled one', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));
  await page.setViewportSize({ width: 1280, height: 900 });
  await routeGraph(page);

  // X (step-1) is present in BOTH the backfill AND the live stream. Z
  // (task-1) is backfill-only; Y (phase-a) is live-only and genuinely new.
  const recX = { ts: `${TODAY}T09:00:00Z`, action: 'step start', handle: 'step-1', category: 'work', level: 'info' };
  const recZ = { ts: `${TODAY}T08:59:00Z`, action: 'phase start', handle: 'task-1', category: 'work', level: 'info' };
  const recY = { ts: `${TODAY}T09:00:30Z`, action: 'step complete', handle: 'phase-a', category: 'work', level: 'info' };

  await mockEmpty(page, MISSION_RE);
  await page.route(DAY_RE, (r) => r.fulfill({ contentType: 'application/json', body: JSON.stringify([recX, recZ]) }));
  let streamHits = 0;
  await page.route(STREAM_RE, (r) => {
    const first = streamHits++ === 0;
    const body = first ? `data: ${JSON.stringify(recX)}\n\ndata: ${JSON.stringify(recY)}\n\n` : '';
    r.fulfill({ contentType: 'text/event-stream', body });
  });

  await page.goto(`/index-live.html#mission=${MISSION_ID}`);
  await page.waitForSelector('.eventlog');

  // Final state: X once (deduped across live+backfill), plus Y (live) and Z
  // (backfill) — three distinct rows, not four.
  await expect(page.locator('.eventlog__rec')).toHaveCount(3);
  const titles = await page.$$eval('.eventlog__rec', (els) => els.map((e) => e.getAttribute('title')));
  expect(titles.filter((t) => t === 'step-1')).toHaveLength(1);
  expect(titles).toContain('task-1');
  await expect(page.locator('.eventlog__rec', { hasText: 'step complete' })).toHaveCount(1);

  expect(pageErrors).toEqual([]);
});
