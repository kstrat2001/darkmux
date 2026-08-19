// (#1637) The browser half of the wire contract.
//
// Two of the six bad tests from 2026-08-04 came from one hole: nothing
// connected the Rust structs that emit `/runs` and `graph.json` to the JSON
// these specs hand-feed them. The graph node shape was hand-written wrong twice
// in a single session, and both times the only symptom was a page that rendered
// NOTHING — which reads as "my selector is wrong", not "my fixture is wrong".
//
// `tests/fixtures/generated/*.json` are now SERIALIZED FROM THE WIRE TYPES by
// `crates/darkmux-serve/src/wire_fixtures.rs`. A spec building on them cannot
// feed a shape the server could never produce, and a field added, renamed or
// retyped fails the Rust golden test until the fixture is regenerated — at
// which point every spec here sees the new shape.
//
// This file proves the generated fixtures actually DRIVE the viewer. A fixture
// nobody renders is just a file.
const fs = require('fs');
const path = require('path');
const { test, expect } = require('@playwright/test');

const GEN = path.join(__dirname, '..', 'fixtures', 'generated');
const graphFixture = JSON.parse(fs.readFileSync(path.join(GEN, 'mission-graph.json'), 'utf8'));
const runsRow = JSON.parse(fs.readFileSync(path.join(GEN, 'runs-row.json'), 'utf8'));

const MISSION_ID = graphFixture.mission_id;
// (#1868) `/flow-mission/<id>` + `/flow/<date>` — the SAME two backfill
// sources `MissionGraphLens.tsx` reads, and the SSE stream it subscribes to.
const MISSION_RE = /\/flow-mission\/[^/?]+(\?.*)?$/;
const BACKFILL_RE = /\/flow\/\d{4}-\d{2}-\d{2}(\?.*)?$/;
const STREAM_RE = /\/flow\/\d{4}-\d{2}-\d{2}\/stream(\?.*)?$/;

test('the generated graph fixture renders — the shape is the server\'s, not mine', async ({
  page,
}) => {
  const errors = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  await page.route(`**/mission/${MISSION_ID}/graph.json*`, (r) =>
    r.fulfill({ contentType: 'application/json', body: JSON.stringify(graphFixture) })
  );
  await page.route(MISSION_RE, (r) => r.fulfill({ contentType: 'application/json', body: '[]' }));
  await page.route(BACKFILL_RE, (r) => r.fulfill({ contentType: 'application/json', body: '[]' }));
  await page.route(STREAM_RE, (r) => r.fulfill({ contentType: 'text/event-stream', body: '' }));
  await page.setViewportSize({ width: 1280, height: 900 });
  // (#1868) The standalone `/mission/<id>/graph` page is retired; the graph
  // lens now renders IN-PLACE inside the port at `#mission=<id>`, the same
  // hash route `MissionGraphLens.tsx` owns — matching every mission-lens-*
  // spec's own navigation.
  await page.goto(`/index-live.html#mission=${MISSION_ID}`);

  // Rendering AT ALL is the assertion that matters. A hand-written shape the
  // server cannot produce yields an empty canvas, and every downstream
  // assertion in a spec built on it then passes or fails for reasons unrelated
  // to what it names.
  await expect(page.locator('.mnode').first()).toBeVisible();
  // Phases render as CONTAINERS (`.phasegroup`), tasks as cards (`.mnode`), so
  // the counts are separate. Asserting `.mnode === nodes.length` was wrong and
  // conveniently also failed — for the right reason, as it happens: the first
  // exemplar named a parent phase it did not include.
  const phases = graphFixture.nodes.filter((n) => n.kind === 'phase').length;
  const tasks = graphFixture.nodes.filter((n) => n.kind === 'task').length;
  await expect(page.locator('.mnode')).toHaveCount(tasks);
  await expect(page.locator('.phasegroup')).toHaveCount(phases);

  // Every task's parent must be a phase that EXISTS. A dangling parent is
  // unrenderable and the page degrades to silence rather than an error.
  const ids = new Set(graphFixture.nodes.map((n) => n.id));
  for (const n of graphFixture.nodes.filter((x) => x.parentId)) {
    expect(ids.has(n.parentId), `\`${n.id}\` names a parent that is not in the graph`).toBe(true);
  }

  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});

test('the generated graph carries every attribution state a spec needs', async () => {
  // The exemplar is not a happy path. #1626 needs all three of cloud /
  // local_ok / neither to exist, and #1481 needs a planned step that shows no
  // tokens. If a future edit trims the exemplar to something tidier, the specs
  // that depend on those states would silently stop exercising them — so the
  // states are asserted here rather than assumed.
  const steps = graphFixture.nodes.flatMap((n) => n.steps || []);
  expect(steps.some((s) => s.cloud === true), 'a hosted step').toBe(true);
  expect(steps.some((s) => s.localOk === true), 'a definitively-local step').toBe(true);
  expect(
    steps.some((s) => s.cloud === undefined && s.localOk === undefined && s.tokensFinal),
    'a step with tokens and NO attribution evidence — the errored-hosted shape'
  ).toBe(true);
  expect(
    steps.some((s) => s.status === 'planned' && s.tokensFinal === undefined),
    'a planned step with no tokens'
  ).toBe(true);

  // And the field a hand-written fixture omitted twice: steps hang off a TASK
  // node with a parentId, never off the phase directly.
  const task = graphFixture.nodes.find((n) => n.kind === 'task');
  expect(task, 'the exemplar must contain a task node').toBeTruthy();
  expect(task.parentId, 'a task must name its parent phase').toBeTruthy();
  expect((task.steps || []).length).toBeGreaterThan(0);
});

test('the generated runs row renders in the runs lens', async ({ page }) => {
  const errors = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  await page.route('**/runs*', (r) =>
    r.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({ generated_at_ms: Date.now(), runs: [runsRow] }),
    })
  );
  await page.goto('/index-lab.html#lens=runs');

  const row = page.locator('.labrunrow');
  await expect(row).toHaveCount(1);
  await expect(row).toContainText(runsRow.id);
  await expect(row.locator('.labbadge')).toHaveText(runsRow.status);

  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});
